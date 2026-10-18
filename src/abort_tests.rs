//! A refusal aborts the sitting — in an opening round, a deliberation
//! round, the final vote, a ranking, and from the Clerk and the Lawyer's
//! meeting summary — and each abort leaves the same things behind: a
//! sealed, owner-only dump whose hashes match, a Steward's record draft,
//! and a marker that blocks the next sitting until `resolve` clears it.
//! A seat's decision emitted out of declared order aborts the same way,
//! with the raw response body sealed and the two key orders on the record.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use misanthropic::prompt::message::Content;
use misanthropic::response;

use agora_common::enums::ModelRole;
use agora_common::govlog::StewardRecord;
use agora_common::ids::AgendaItemId;
use agora_common::prompt_archive::sha256_hex;
use agora_common::refusal::Refused;

use super::*;
use crate::councillor::Councillor;
use crate::councillor::harness_tests::{
    Mock, Scripted, one_candidate_docket, position_input, refusal_response, submission_response,
    test_config, test_item, text_response, thread_comment, tool_response,
};
#[allow(unused_imports)]
use crate::councillor::harness_tests::{rank_input, respond_input, vote_input};
use crate::deliberation::sit_item;
use crate::docket::ProposalReader;
use crate::models::DecisionCategory;
use crate::record::{FinalVote, Submission};
use crate::steward::StewardInput;

/// A scratch directory for one test, removed when it drops.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agora-council-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// The test configuration, writing only in here.
    pub(crate) fn config(&self) -> Config {
        let mut config = test_config();
        config.refusals_dir = self.0.join("secrets/council/refusals");
        config.stops_dir = config.refusals_dir.clone();
        config.records_dir = self.0.join("governance/records");
        config
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Answers the Steward from owned strings (the dump's name is not
/// `'static`).
struct Typed(VecDeque<String>);

impl StewardInput for Typed {
    fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
        self.0
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("the Steward script ran out"))
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Everything an abort must leave behind, checked; then the marker blocks
/// a sitting until `resolve` clears it. Returns the manifest.
///
/// `last_request` is the request the refusal came back for, as the mock
/// saw it on the wire.
pub(crate) fn assert_aborted_and_sealed(
    config: &Config,
    err: &anyhow::Error,
    stage: &Stage,
    role: ModelRole,
    last_request: &serde_json::Value,
    expected_files: &[&str],
) -> Manifest {
    let aborted = SittingAborted::find(err).unwrap_or_else(|| panic!("not an abort: {err:#}"));
    assert_eq!(&aborted.stage, stage);
    assert_eq!(aborted.cause.role(), role);
    let sealed = aborted.sealed.as_ref().expect("sealed");

    // Owner-only, and under the configured root.
    assert_eq!(sealed.dir.parent(), Some(config.refusals_dir.as_path()));
    assert_eq!(mode(&sealed.dir), 0o700, "the dump directory is 0700");
    assert_eq!(mode(&config.refusals_dir), 0o700);
    let manifest_path = sealed.dir.join(MANIFEST);
    assert_eq!(mode(&manifest_path), 0o600);
    let manifest_bytes = std::fs::read(&manifest_path).unwrap();
    assert_eq!(sha256_hex(&manifest_bytes), sealed.manifest_sha256);
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();

    // Every file listed, 0600, and its hash matches.
    let names: Vec<&str> = manifest.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, expected_files);
    for file in &manifest.files {
        let path = sealed.dir.join(&file.name);
        assert_eq!(mode(&path), 0o600, "{}", file.name);
        assert_eq!(
            sha256_hex(&std::fs::read(&path).unwrap()),
            file.sha256,
            "{}",
            file.name
        );
    }
    let on_disk: usize = std::fs::read_dir(&sealed.dir).unwrap().count();
    assert_eq!(
        on_disk,
        expected_files.len() + 1,
        "the files and the manifest"
    );

    // The refused request, exactly as sent.
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sealed.dir.join("refused-request.json")).unwrap())
            .unwrap();
    assert_eq!(&request, last_request);

    // The manifest's facts.
    assert_eq!(&manifest.stage, stage);
    let refusal = manifest.cause.as_refusal().expect("sealed as a refusal");
    assert_eq!(refusal.role, role);
    assert_eq!(refusal.category.as_deref(), Some("cyber"));
    assert_eq!(
        refusal.explanation.as_deref(),
        Some("Flagged by a safety classifier.")
    );
    // The manifest on disk keeps the shape it had before emission
    // breaches were sealed too: a top-level `refusal` object.
    let raw: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(raw["refusal"]["category"], "cyber", "{raw}");
    assert_eq!(manifest.seat_models.len(), 4);
    assert!(manifest.resolution.is_none(), "pending investigation");

    // The Steward's record: facts, hashes, nothing presumed.
    let record_path = manifest.record.clone().expect("a record was drafted");
    assert!(record_path.starts_with(&config.records_dir));
    let draft: RecordDraft = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(draft.kind, "council_refusal");
    for file in &manifest.files {
        assert!(draft.body.contains(&file.sha256), "{} hash", file.name);
    }
    assert!(draft.body.contains("No injection is presumed"));
    assert!(draft.body.contains("GOV-2026-0011"));
    assert!(draft.body.contains(&refusal.response_id));
    for word in ["declined", "refused to", "abstained", "injection was"] {
        assert!(!draft.body.contains(word), "the record says {word:?}");
    }
    let mut record = StewardRecord::new(draft.kind, draft.title, draft.body);
    record.concerns = draft.concerns;
    record.participants = draft.participants;
    agora_common::govlog::check_record(&record).expect("a valid record request");

    // The marker blocks the next sitting...
    let waiting = pending(&config.refusals_dir).unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].dir, sealed.dir);
    let blocked = ensure_none_pending(config).expect_err("the next sitting is blocked");
    assert!(
        blocked.to_string().contains("pending investigation"),
        "{blocked}"
    );

    // ...until the Steward resolves it, confirming by the dump's name.
    let name = sealed
        .dir
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let resolved = resolve(
        config,
        &sealed.dir,
        Resolution::Cleaned,
        "The offending comment was removed from the thread.",
        &mut Typed(VecDeque::from([name])),
    )
    .unwrap();
    assert_eq!(resolved.resolution, Resolution::Cleaned);
    assert!(pending(&config.refusals_dir).unwrap().is_empty());
    ensure_none_pending(config).expect("resolved: the Council may sit");
    let after: Manifest = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(after.resolution, Some(resolved));
    assert_eq!(mode(&manifest_path), 0o600);
    manifest
}

fn respond_ready() -> Submission {
    respond_input("I have heard enough.", true)
}

fn vote_yes() -> Submission {
    vote_input("Final.", FinalVote::Yes)
}

fn rank_one() -> Submission {
    rank_input(vec![1])
}

fn four(input: fn() -> Submission) -> Vec<response::Message> {
    (0..4).map(|_| submission_response(input())).collect()
}

const SEATS: [&str; 7] = [
    "refusal.json",
    "refused-request.json",
    "seat-artist.json",
    "seat-philosopher.json",
    "seat-lawyer.json",
    "seat-engineer.json",
    "clerk-briefing.json",
];

/// The sealed files on an item that ran its recusal determination: each
/// seat's deliberation and its isolated determination conversation.
const SEATS_DETERMINED: [&str; 11] = [
    "refusal.json",
    "refused-request.json",
    "seat-artist.json",
    "seat-artist-recusal.json",
    "seat-philosopher.json",
    "seat-philosopher-recusal.json",
    "seat-lawyer.json",
    "seat-lawyer-recusal.json",
    "seat-engineer.json",
    "seat-engineer-recusal.json",
    "clerk-briefing.json",
];

/// Sit one item over `responses`, after the recusal determination where
/// the item's category makes one (no seat finds the item limits the
/// Steward's powers); it must abort. Returns the error and every request
/// sent after the determination.
async fn sit_to_abort(
    config: &Config,
    item: &AgendaItem,
    docket: Option<&crate::docket::Docket>,
    threads: Option<(PostId, Vec<crate::queries::CommentRow>)>,
    responses: Vec<response::Message>,
    steward: &[&'static str],
) -> (anyhow::Error, Vec<serde_json::Value>) {
    let determined = item.category.determines_recusal();
    let mut script = if determined {
        four(crate::councillor::harness_tests::not_limiting)
    } else {
        Vec::new()
    };
    script.extend(responses);
    let (err, mut requests) =
        sit_to_abort_from_the_start(config, item, docket, threads, script, steward).await;
    if determined {
        requests = requests.split_off(4);
    }
    (err, requests)
}

/// Sit one item over `responses`, from its very first request; it must
/// abort. Returns the error and every request sent.
async fn sit_to_abort_from_the_start(
    config: &Config,
    item: &AgendaItem,
    docket: Option<&crate::docket::Docket>,
    threads: Option<(PostId, Vec<crate::queries::CommentRow>)>,
    responses: Vec<response::Message>,
    steward: &[&'static str],
) -> (anyhow::Error, Vec<serde_json::Value>) {
    let mock = Mock::scripted(responses);
    let rankable = docket.map(|d| d.rankable());
    let mut councillors: Vec<Councillor<'_, Mock>> = CouncilSeat::ALL
        .iter()
        .map(|&role| {
            Councillor::new(&mock, config, role, Vec::new(), rankable.clone(), None)
                .expect("1h breakpoint on a fresh prompt")
        })
        .collect();
    let mut reader = ProposalReader::new(None, docket);
    if let Some((post, comments)) = threads {
        reader.threads.insert(post, comments);
    }
    let mut reads = Vec::new();
    let mut input = Scripted(VecDeque::from(steward.to_vec()));
    let err = sit_item(
        config,
        item,
        &Content::from("Recap."),
        &mut input,
        &mut councillors,
        &mut reader,
        &mut reads,
        rankable.as_ref(),
    )
    .await
    .expect_err("a refusal aborts the sitting");
    assert!(input.0.is_empty(), "unused Steward input: {:?}", input.0);
    (err, mock.requests())
}

#[tokio::test]
async fn a_refusal_in_the_opening_round_aborts() {
    let scratch = Scratch::new("opening");
    let config = scratch.config();
    let item = test_item();
    let responses = vec![submission_response(position_input()), refusal_response()];
    let (err, requests) = sit_to_abort(&config, &item, None, None, responses, &[]).await;
    assert_eq!(
        requests.len(),
        2,
        "nothing after the refusal, nothing retried"
    );
    let stage = Stage::Round {
        number: 1,
        kind: RoundKind::Opening,
        seat: CouncilSeat::Philosopher,
    };
    assert_aborted_and_sealed(
        &config,
        &err,
        &stage,
        ModelRole::Philosopher,
        requests.last().unwrap(),
        &SEATS_DETERMINED,
    );
}

/// A refusal in the recusal determination aborts like any other, before
/// the Steward has been asked anything: sealed with the determination
/// stage, the refused determination request, and the conversations that
/// existed (the seats that had answered, the refused seat's, and every
/// seat's still-unopened deliberation).
#[tokio::test]
async fn a_refusal_in_the_recusal_determination_aborts() {
    let scratch = Scratch::new("recusal");
    let config = scratch.config();
    let item = test_item();
    let responses = vec![
        submission_response(crate::councillor::harness_tests::not_limiting()),
        refusal_response(),
    ];
    let (err, requests) =
        sit_to_abort_from_the_start(&config, &item, None, None, responses, &[]).await;
    assert_eq!(requests.len(), 2, "nothing after the refusal");
    let refused = serde_json::to_string(&requests[1]).unwrap();
    assert!(refused.contains("Recusal determination"), "{refused}");
    let stage = Stage::RecusalDetermination {
        seat: CouncilSeat::Philosopher,
    };
    let manifest = assert_aborted_and_sealed(
        &config,
        &err,
        &stage,
        ModelRole::Philosopher,
        requests.last().unwrap(),
        &[
            "refusal.json",
            "refused-request.json",
            "seat-artist.json",
            "seat-artist-recusal.json",
            "seat-philosopher.json",
            "seat-philosopher-recusal.json",
            "seat-lawyer.json",
            "seat-engineer.json",
            "clerk-briefing.json",
        ],
    );
    assert_eq!(manifest.subject, Subject::AgendaItem(item.id));
}

#[tokio::test]
async fn a_refusal_in_a_deliberation_round_aborts() {
    let scratch = Scratch::new("deliberation");
    let config = scratch.config();
    let item = test_item();
    let mut responses = four(position_input);
    responses.extend(four(respond_ready).into_iter().take(2));
    responses.push(refusal_response());
    let (err, requests) = sit_to_abort(&config, &item, None, None, responses, &["c"]).await;
    assert_eq!(requests.len(), 7);
    let stage = Stage::Round {
        number: 2,
        kind: RoundKind::Deliberation,
        seat: CouncilSeat::Lawyer,
    };
    let manifest = assert_aborted_and_sealed(
        &config,
        &err,
        &stage,
        ModelRole::Lawyer,
        requests.last().unwrap(),
        &SEATS_DETERMINED,
    );
    assert_eq!(manifest.subject, Subject::AgendaItem(item.id));
}

#[tokio::test]
async fn a_refusal_on_the_final_vote_aborts_with_no_menu() {
    let scratch = Scratch::new("final-vote");
    let config = scratch.config();
    let item = test_item();
    let mut responses = four(position_input);
    responses.extend(four(respond_ready));
    responses.extend(four(vote_yes).into_iter().take(3));
    responses.push(refusal_response());
    // Round 2, then the vote. No retry/table/abort prompt follows the
    // refusal: an unused or missing Steward answer would fail the run.
    let (err, requests) = sit_to_abort(&config, &item, None, None, responses, &["c", "c"]).await;
    assert_eq!(requests.len(), 12);
    let stage = Stage::Round {
        number: 3,
        kind: RoundKind::FinalVote,
        seat: CouncilSeat::Engineer,
    };
    assert_aborted_and_sealed(
        &config,
        &err,
        &stage,
        ModelRole::Engineer,
        requests.last().unwrap(),
        &SEATS_DETERMINED,
    );
}

#[tokio::test]
async fn a_refusal_on_a_ranking_aborts() {
    let scratch = Scratch::new("ranking");
    let config = scratch.config();
    let (docket, _) = one_candidate_docket();
    let mut item = test_item();
    item.category = DecisionCategory::Schedule;
    let mut responses = four(position_input);
    responses.extend(four(respond_ready));
    responses.push(submission_response(rank_one()));
    responses.push(refusal_response());
    let (err, requests) =
        sit_to_abort(&config, &item, Some(&docket), None, responses, &["c", "c"]).await;
    assert_eq!(requests.len(), 10);
    let stage = Stage::Round {
        number: 3,
        kind: RoundKind::Ranking,
        seat: CouncilSeat::Philosopher,
    };
    assert_aborted_and_sealed(
        &config,
        &err,
        &stage,
        ModelRole::Philosopher,
        requests.last().unwrap(),
        &SEATS,
    );
}

/// The Clerk's `read_proposal` summary draws the refusal, on the Artist's
/// opening turn: that aborts the sitting like a seat's own refusal. The
/// sealed request is the Clerk's, and the manifest says the Clerk was
/// asked.
#[tokio::test]
async fn a_clerk_refusal_mid_round_aborts() {
    let scratch = Scratch::new("clerk-read");
    let config = scratch.config();
    let (docket, post_id) = one_candidate_docket();
    let mut item = test_item();
    item.category = DecisionCategory::Schedule;
    let mut clerk_refusal = text_response("Partial summary.");
    clerk_refusal.stop_reason = Some(response::StopReason::Refusal);
    clerk_refusal.stop_details = crate::councillor::harness_tests::cyber_refusal();
    let responses = vec![
        tool_response(
            "toolu_read",
            "read_proposal",
            crate::record::ReadProposal { number: 1 },
        ),
        clerk_refusal,
    ];
    let (err, requests) = sit_to_abort(
        &config,
        &item,
        Some(&docket),
        Some((post_id, vec![thread_comment(1, "A comment.")])),
        responses,
        &[],
    )
    .await;
    assert_eq!(requests.len(), 2);
    assert!(requests[1].get("tools").is_none(), "the Clerk's request");
    let stage = Stage::Round {
        number: 1,
        kind: RoundKind::Opening,
        seat: CouncilSeat::Artist,
    };
    assert_aborted_and_sealed(
        &config,
        &err,
        &stage,
        ModelRole::Clerk,
        requests.last().unwrap(),
        &SEATS,
    );
}

/// The Clerk's briefing (the comment summary, before any seat speaks)
/// draws a refusal: the error is typed, and sealing it leaves no seat
/// files, only the request and the briefing so far.
#[tokio::test]
async fn a_clerk_refusal_at_the_briefing_is_sealed() {
    let scratch = Scratch::new("clerk-briefing");
    let config = scratch.config();
    let mut item = test_item();
    item.comment_summary = None;
    let mut clerk_refusal = text_response("Partial.");
    clerk_refusal.stop_reason = Some(response::StopReason::Refusal);
    clerk_refusal.stop_details = crate::councillor::harness_tests::cyber_refusal();
    let mock = Mock::scripted([clerk_refusal]);
    let err = crate::clerk::summarize_comments(
        &mock,
        None,
        &item.title,
        &item.body,
        &crate::citations::ItemComments::new(vec![thread_comment(1, "A comment.")]),
    )
    .await
    .expect_err("a refusal");
    let refused = Refused::find(&err).expect("typed");
    let err = seal(
        &config,
        Moment::item(&item, Stage::Briefing),
        refused,
        &[],
        Some(ClerkBriefing::of(&item)),
    )
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_aborted_and_sealed(
        &config,
        &err,
        &Stage::Briefing,
        ModelRole::Clerk,
        &requests[0],
        &[
            "refusal.json",
            "refused-request.json",
            "clerk-briefing.json",
        ],
    );
}

/// A dump whose manifest is missing or does not parse (a crash mid-seal)
/// blocks the sitting too: the guard fails closed.
#[test]
fn an_unreadable_dump_counts_as_pending() {
    let scratch = Scratch::new("unreadable");
    let config = scratch.config();
    let dir = config.refusals_dir.join("20260929T000000Z-x");
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(pending(&config.refusals_dir).unwrap().len(), 1);
    std::fs::write(dir.join(MANIFEST), b"{ not json").unwrap();
    let p = pending(&config.refusals_dir).unwrap();
    assert_eq!(p.len(), 1);
    assert!(p[0].manifest.is_err());
    assert!(ensure_none_pending(&config).is_err());
}

#[test]
fn no_refusals_dir_means_nothing_pending() {
    let scratch = Scratch::new("empty");
    let config = scratch.config();
    assert!(pending(&config.refusals_dir).unwrap().is_empty());
    ensure_none_pending(&config).unwrap();
}

/// Seal one refusal for the resolution tests below.
async fn sealed_one(config: &Config) -> PathBuf {
    let item = test_item();
    let request = misanthropic::Prompt::default().model("claude-opus-4-6");
    let mut response = refusal_response();
    response.model = "claude-opus-4-6".into();
    let refused = Refused::new(&response, &request, ModelRole::Engineer);
    let err = seal(
        config,
        Moment::item(
            &item,
            Stage::Round {
                number: 1,
                kind: RoundKind::Opening,
                seat: CouncilSeat::Engineer,
            },
        ),
        &refused,
        &[],
        None,
    )
    .await;
    SittingAborted::find(&err)
        .unwrap()
        .sealed
        .as_ref()
        .unwrap()
        .dir
        .clone()
}

#[tokio::test]
async fn resolve_needs_the_typed_name() {
    let scratch = Scratch::new("confirm");
    let config = scratch.config();
    let dir = sealed_one(&config).await;
    let err = resolve(
        &config,
        &dir,
        Resolution::Cleaned,
        "Removed.",
        &mut Typed(VecDeque::from(["yes".to_string()])),
    )
    .expect_err("not confirmed");
    assert!(err.to_string().contains("not confirmed"), "{err}");
    assert_eq!(pending(&config.refusals_dir).unwrap().len(), 1);
}

/// `reverted` is refused while the seat still runs the model the refusal
/// came from: the revert is a reviewed commit to `SEAT_MODEL_OVERRIDE`
/// first, then the resolution.
#[tokio::test]
async fn reverted_needs_the_seat_actually_reverted() {
    let scratch = Scratch::new("reverted");
    let config = scratch.config();
    let dir = sealed_one(&config).await;
    let err = resolve(
        &config,
        &dir,
        Resolution::Reverted,
        "Reverted the seat.",
        &mut Typed(VecDeque::new()),
    )
    .expect_err("the seat was not reverted");
    assert!(err.to_string().contains("SEAT_MODEL_OVERRIDE"), "{err}");

    // With the seat on another model now, it resolves.
    let mut reverted = config;
    reverted.model = "claude-opus-4-5".into();
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();
    let resolved = resolve(
        &reverted,
        &dir,
        Resolution::Reverted,
        "Reverted the seat.",
        &mut Typed(VecDeque::from([name])),
    )
    .unwrap();
    assert_eq!(resolved.seat_model_now.as_deref(), Some("claude-opus-4-5"));
}

#[tokio::test]
async fn resolve_refuses_tampered_evidence_and_a_second_resolution() {
    let scratch = Scratch::new("tamper");
    let config = scratch.config();
    let dir = sealed_one(&config).await;
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();

    let path = dir.join("refused-request.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{}").unwrap();
    let err = resolve(
        &config,
        &dir,
        Resolution::Cleaned,
        "Removed.",
        &mut Typed(VecDeque::from([name.clone()])),
    )
    .expect_err("tampered");
    assert!(err.to_string().contains("no longer matches"), "{err}");
    std::fs::write(&path, original).unwrap();

    resolve(
        &config,
        &dir,
        Resolution::Cleaned,
        "Removed.",
        &mut Typed(VecDeque::from([name.clone()])),
    )
    .unwrap();
    let err = resolve(
        &config,
        &dir,
        Resolution::Cleaned,
        "Again.",
        &mut Typed(VecDeque::from([name])),
    )
    .expect_err("already resolved");
    assert!(err.to_string().contains("already resolved"), "{err}");
}

#[test]
fn resolution_parses_only_its_three_words() {
    assert_eq!(
        "cleaned".parse::<Resolution>().unwrap(),
        Resolution::Cleaned
    );
    assert_eq!(
        "reverted".parse::<Resolution>().unwrap(),
        Resolution::Reverted
    );
    assert_eq!(
        "reverified".parse::<Resolution>().unwrap(),
        Resolution::Reverified
    );
    assert!("abstained".parse::<Resolution>().is_err());
}

/// A refusal on the Clerk's summary of a decision, which now comes before
/// anything is recorded: the record says nothing was recorded or signed.
#[test]
fn a_decision_summary_record_says_nothing_was_recorded() {
    let manifest = Manifest {
        version: 1,
        sealed_at: Utc::now(),
        subject: Subject::AgendaItem(AgendaItemId::from(uuid::Uuid::nil())),
        proposal: None,
        title: "Test Proposal".into(),
        stage: Stage::DecisionSummary,
        cause: CauseFacts::Refusal(RefusalFacts {
            role: ModelRole::Clerk,
            model: "claude-haiku-4-5-20251001".into(),
            response_id: "msg_x".into(),
            category: None,
            explanation: None,
            output_before_refusal: false,
        }),
        seat_models: Vec::new(),
        clerk_model: "claude-sonnet-5".into(),
        git_sha: None,
        git_dirty: None,
        files: Vec::new(),
        record: None,
        recorded: None,
        resolution: None,
    };
    let draft = record_draft(&manifest);
    assert!(draft.concerns.is_empty());
    assert!(draft.body.contains("Nothing was recorded or signed"));
    assert!(draft.body.contains("not given"));
}

/// Gap C, end to end: a seat's final vote comes back with `vote` before
/// `reasoning`. The sitting is sealed as aborted exactly as a refusal is,
/// no decision is made, and the record says which seat, which method, the
/// declared order and the emitted order. The sealed dump holds the body
/// byte for byte. The next sitting is blocked until the Steward resolves
/// it `reverified`; `cleaned` (a refusal's resolution) is refused.
#[tokio::test]
async fn an_out_of_order_vote_aborts_the_sitting_sealed_with_the_reason() {
    let scratch = Scratch::new("unsound-emission");
    let config = scratch.config();
    let item = test_item();
    // Out of declared order, so not a typed submission: the literal bytes.
    let out_of_order = crate::wire::RawBody::new(crate::wire::tests::REVERSED_VOTE)
        .parse()
        .expect("the reversed vote parses");
    let body = serde_json::to_vec(&out_of_order).unwrap();
    let mut responses = four(position_input);
    responses.extend(four(respond_ready));
    responses.extend(four(vote_yes).into_iter().take(1));
    responses.push(out_of_order);
    // Two answers: proceed to round 2, then call the vote. Nothing after
    // the breach asks the Steward anything.
    let (err, requests) = sit_to_abort(&config, &item, None, None, responses, &["c", "c"]).await;
    assert_eq!(
        requests.len(),
        10,
        "nothing after the breach, nothing retried"
    );

    let stage = Stage::Round {
        number: 3,
        kind: RoundKind::FinalVote,
        seat: CouncilSeat::Philosopher,
    };
    let aborted = SittingAborted::find(&err).unwrap_or_else(|| panic!("not an abort: {err:#}"));
    assert_eq!(aborted.stage, stage);
    assert_eq!(aborted.subject, Subject::AgendaItem(item.id));
    let shown = err.to_string();
    assert!(shown.contains("out of declared order"), "{shown}");
    assert!(!shown.contains("I support this"), "{shown}");

    let sealed = aborted.sealed.as_ref().expect("sealed");
    assert_eq!(mode(&sealed.dir), 0o700);
    let manifest_bytes = std::fs::read(sealed.dir.join(MANIFEST)).unwrap();
    assert_eq!(sha256_hex(&manifest_bytes), sealed.manifest_sha256);
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert!(raw.get("refusal").is_none(), "{raw}");
    assert_eq!(raw["unsound_emission"]["method"], "vote", "{raw}");

    // The facts: which seat, which method, both orders.
    let emission = manifest.cause.as_emission().expect("sealed as a breach");
    assert_eq!(emission.role, ModelRole::Philosopher);
    assert_eq!(emission.method, "vote");
    assert_eq!(emission.declared, ["submission", "statement", "vote"]);
    assert_eq!(emission.emitted, ["submission", "vote", "statement"]);
    assert!(
        emission
            .violation
            .contains("`vote` was emitted before `statement`")
    );
    assert_eq!(aborted.cause, manifest.cause);

    // The evidence: the body byte for byte, the request as sent, every
    // seat's conversation (without the breach's call), the briefing.
    let names: Vec<&str> = manifest.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "unsound-emission.json",
            "request.json",
            "raw-response.json",
            "seat-artist.json",
            "seat-artist-recusal.json",
            "seat-philosopher.json",
            "seat-philosopher-recusal.json",
            "seat-lawyer.json",
            "seat-lawyer-recusal.json",
            "seat-engineer.json",
            "seat-engineer-recusal.json",
            "clerk-briefing.json",
        ]
    );
    for file in &manifest.files {
        let path = sealed.dir.join(&file.name);
        assert_eq!(mode(&path), 0o600, "{}", file.name);
        assert_eq!(sha256_hex(&std::fs::read(&path).unwrap()), file.sha256);
    }
    assert_eq!(
        std::fs::read(sealed.dir.join("raw-response.json")).unwrap(),
        body,
        "the raw body is sealed as received"
    );
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sealed.dir.join("request.json")).unwrap()).unwrap();
    assert_eq!(&request, requests.last().unwrap());
    let seat = std::fs::read_to_string(sealed.dir.join("seat-philosopher.json")).unwrap();
    assert!(!seat.contains("toolu_bad"), "the breach's call was seated");

    // The record says plainly why the sitting stopped.
    let record_path = manifest.record.clone().expect("a record was drafted");
    assert!(record_path.starts_with(&config.records_dir));
    assert!(
        record_path
            .to_string_lossy()
            .contains("council-unsound-emission"),
        "{}",
        record_path.display()
    );
    let draft: RecordDraft = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(draft.kind, "council_unsound_emission");
    assert!(
        draft.title.contains("out of declared order"),
        "{}",
        draft.title
    );
    for expected in [
        "the Philosopher (a Council seat)",
        "**Method**: `vote`",
        "**Declared order**: `submission`, `statement`, `vote`",
        "**Emitted order**: `submission`, `vote`, `statement`",
        "Nothing from the call was recorded",
        "No decision was recorded for this item",
        "§ 4.5",
        "msg_rev",
    ] {
        assert!(
            draft.body.contains(expected),
            "missing {expected:?}:\n{}",
            draft.body
        );
    }
    for file in &manifest.files {
        assert!(draft.body.contains(&file.sha256), "{} hash", file.name);
    }
    assert!(!draft.body.contains("I support this"), "the seat's words");
    let mut record = StewardRecord::new(draft.kind, draft.title, draft.body);
    record.participants = draft.participants;
    agora_common::govlog::check_record(&record).expect("a valid record request");

    // Blocks the next sitting; `cleaned` is a refusal's resolution and is
    // refused here; `reverified` clears it.
    ensure_none_pending(&config).expect_err("the next sitting is blocked");
    let name = sealed
        .dir
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let err = resolve(
        &config,
        &sealed.dir,
        Resolution::Cleaned,
        "Removed.",
        &mut Typed(VecDeque::from([name.clone()])),
    )
    .expect_err("`cleaned` is for a refusal");
    assert!(err.to_string().contains("reverified"), "{err}");
    let resolved = resolve(
        &config,
        &sealed.dir,
        Resolution::Reverified,
        "Replayed the archived prompts with strict on: declared order held 20/20.",
        &mut Typed(VecDeque::from([name])),
    )
    .unwrap();
    assert_eq!(resolved.resolution, Resolution::Reverified);
    ensure_none_pending(&config).expect("resolved: the Council may sit");
}

/// `reverified` is an emission breach's resolution, not a refusal's.
#[tokio::test]
async fn reverified_is_refused_for_a_refusal() {
    let scratch = Scratch::new("reverified-refusal");
    let config = scratch.config();
    let dir = sealed_one(&config).await;
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();
    let err = resolve(
        &config,
        &dir,
        Resolution::Reverified,
        "Re-verified.",
        &mut Typed(VecDeque::from([name])),
    )
    .expect_err("not for a refusal");
    assert!(err.to_string().contains("`cleaned`"), "{err}");
    assert_eq!(pending(&config.refusals_dir).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// A Steward stop (Ctrl-C during the sitting; `crate::stop`)
// ---------------------------------------------------------------------------

/// One answer at a Steward prompt, or a failed read.
enum Said {
    Text(&'static str),
    /// Ctrl-C: the sitting's terminal returns the stop at an ordinary
    /// prompt and rustyline's `Interrupted` at the reason prompt.
    CtrlC,
    /// Ctrl-D.
    Eof,
}

/// The Steward at the sitting's terminal, armed with `stop`.
struct Steward {
    said: VecDeque<Said>,
    stop: crate::stop::StopSignal,
}

impl Steward {
    fn next(&mut self, at_reason: bool) -> anyhow::Result<String> {
        match self.said.pop_front().expect("the Steward script ran out") {
            Said::Text(s) => Ok(s.to_string()),
            Said::CtrlC if at_reason => Err(rustyline::error::ReadlineError::Interrupted.into()),
            Said::CtrlC => {
                self.stop.trigger();
                Err(crate::stop::StewardStopRequested {
                    during: crate::stop::StopDuring::StewardPrompt,
                }
                .into())
            }
            Said::Eof => Err(rustyline::error::ReadlineError::Eof.into()),
        }
    }
}

impl StewardInput for Steward {
    fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
        self.next(false)
    }
    fn readline_reason(&mut self, _prompt: &str) -> anyhow::Result<String> {
        self.next(true)
    }
    fn stop_signal(&self) -> Option<crate::stop::StopSignal> {
        Some(self.stop.clone())
    }
}

/// [`Mock`], except that its request number `at` (0-based) never comes
/// back: the Steward presses Ctrl-C while it is in flight.
struct CtrlCAt<'m> {
    mock: &'m Mock,
    at: usize,
    stop: crate::stop::StopSignal,
}

#[async_trait::async_trait]
impl misanthropic::transport::Transport<misanthropic::prompt::CachedPrompt> for CtrlCAt<'_> {
    type Error = crate::councillor::harness_tests::ScriptExhausted;

    async fn send(
        &self,
        prompt: &misanthropic::prompt::CachedPrompt,
    ) -> Result<response::Message, Self::Error> {
        self.mock.send(prompt).await
    }

    async fn models(&self) -> Result<misanthropic::model::Models, Self::Error> {
        self.mock.models().await
    }
}

impl crate::wire::RawTransport for CtrlCAt<'_> {
    async fn send_raw(
        &self,
        prompt: &misanthropic::prompt::CachedPrompt,
    ) -> Result<crate::wire::RawBody, Self::Error> {
        if self.mock.requests().len() == self.at {
            self.stop.trigger();
            std::future::pending::<()>().await;
        }
        self.mock.send_raw(prompt).await
    }
}

/// Sit `item` until the Steward stops it; return the error and how many
/// requests the mock answered.
async fn sit_to_stop(
    config: &Config,
    item: &AgendaItem,
    responses: Vec<response::Message>,
    ctrl_c_at: Option<usize>,
    said: Vec<Said>,
) -> (anyhow::Error, usize) {
    let meeting = CouncilMeetingId::from(uuid::Uuid::from_u128(7));
    let stop = crate::stop::StopSignal::new(Some(meeting));
    let mock = Mock::scripted(responses);
    let transport = CtrlCAt {
        mock: &mock,
        at: ctrl_c_at.unwrap_or(usize::MAX),
        stop: stop.clone(),
    };
    let mut councillors: Vec<Councillor<'_, CtrlCAt<'_>>> = CouncilSeat::ALL
        .iter()
        .map(|&role| {
            Councillor::new(&transport, config, role, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
        })
        .collect();
    let mut reader = ProposalReader::new(None, None);
    let mut reads = Vec::new();
    let mut input = Steward {
        said: said.into(),
        stop,
    };
    let err = sit_item(
        config,
        item,
        &Content::from("Recap."),
        &mut input,
        &mut councillors,
        &mut reader,
        &mut reads,
        None,
    )
    .await
    .expect_err("the Steward stopped the sitting");
    assert!(input.said.is_empty(), "unused Steward input");
    (err, mock.requests().len())
}

/// What a stop must leave behind: the seal, a record carrying the reason
/// verbatim and a decided item's attachments, a marker that blocks the
/// next sitting, and a resolution only by the Council having heard it.
fn assert_stopped_and_sealed(
    config: &Config,
    err: &anyhow::Error,
    stage: &Stage,
    during: crate::stop::StopDuring,
    reason: &str,
) -> Manifest {
    let aborted = SittingAborted::find(err).unwrap_or_else(|| panic!("not an abort: {err:#}"));
    assert_eq!(&aborted.stage, stage);
    assert_eq!(aborted.cause.role(), ModelRole::Steward);
    let shown = aborted.to_string();
    assert!(shown.contains("stopped by the Steward"), "{shown}");
    let sealed = aborted.sealed.as_ref().expect("sealed");
    assert_eq!(mode(&sealed.dir), 0o700);
    let manifest_bytes = std::fs::read(sealed.dir.join(MANIFEST)).unwrap();
    assert_eq!(sha256_hex(&manifest_bytes), sealed.manifest_sha256);
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
    let names: Vec<&str> = manifest.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names[0], "steward-stop.json");
    assert_eq!(names.last(), Some(&"clerk-briefing.json"));
    assert!(names.contains(&"seat-artist.json"), "{names:?}");
    for file in &manifest.files {
        let path = sealed.dir.join(&file.name);
        assert_eq!(mode(&path), 0o600, "{}", file.name);
        assert_eq!(sha256_hex(&std::fs::read(&path).unwrap()), file.sha256);
    }

    let stop = manifest.cause.as_stop().expect("sealed as a Steward stop");
    assert_eq!(stop.reason.as_deref(), Some(reason));
    assert_eq!(stop.during, during);
    assert_eq!(
        stop.meeting,
        Some(CouncilMeetingId::from(uuid::Uuid::from_u128(7)))
    );
    let raw: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(raw["steward_stop"]["reason"], reason, "{raw}");

    // The record: drafted beside its attachments, the reason verbatim, and
    // the very record `meeting::run` appends.
    let record_path = manifest.record.clone().expect("a record was drafted");
    let draft: RecordDraft = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(draft.kind, "council_steward_stop");
    assert!(
        draft.body.contains(&format!("> {reason}")),
        "{}",
        draft.body
    );
    assert!(draft.body.contains("not an emergency action"));
    assert!(draft.body.contains("Art. IV § 2"));
    assert!(
        draft
            .body
            .contains("blocked until the Council has heard this stop")
    );
    assert!(draft.body.contains("00000000-0000-0000-0000-000000000007"));
    for file in &manifest.files {
        assert!(draft.body.contains(&file.sha256), "{} hash", file.name);
    }
    let record = sealed.append.as_ref().expect("a stop's record is appended");
    agora_common::govlog::check_record(record).expect("a valid record");
    assert_eq!(record.body, draft.body);
    assert_eq!(record.attachments.len(), draft.attachments.len());
    for (attached, listed) in record.attachments.iter().zip(&draft.attachments) {
        assert_eq!(attached.name, listed.name);
        let beside = record_path.parent().unwrap().join(&listed.file);
        assert_eq!(std::fs::read_to_string(beside).unwrap(), attached.content);
    }
    let attached: Vec<&str> = record.attachments.iter().map(|a| a.name.as_str()).collect();
    assert!(
        attached.contains(&"clerk-thread-summary.md"),
        "the briefing, from briefing::attachments: {attached:?}"
    );

    // The marker blocks the next sitting, and only `heard` clears it.
    assert_eq!(pending(&config.refusals_dir).unwrap().len(), 1);
    ensure_none_pending(config).expect_err("the next sitting is blocked");
    let name = sealed
        .dir
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    for wrong in [
        Resolution::Cleaned,
        Resolution::Reverified,
        Resolution::Reverted,
    ] {
        let refused = resolve(
            config,
            &sealed.dir,
            wrong.clone(),
            "Not how a stop is resolved.",
            &mut Typed(VecDeque::from([name.clone()])),
        )
        .expect_err("a stop is resolved only as heard");
        assert!(refused.to_string().contains("heard"), "{wrong}: {refused}");
    }
    assert_eq!(pending(&config.refusals_dir).unwrap().len(), 1);
    let heard: Resolution = "heard:GOV-2026-0018".parse().unwrap();
    let resolved = resolve(
        config,
        &sealed.dir,
        heard.clone(),
        "The Council heard the stop at its next sitting.",
        &mut Typed(VecDeque::from([name])),
    )
    .unwrap();
    assert_eq!(resolved.resolution, heard);
    ensure_none_pending(config).expect("heard: the Council may sit");
    manifest
}

#[tokio::test]
async fn ctrl_c_during_a_seats_call_cancels_it_asks_a_reason_and_seals() {
    let scratch = Scratch::new("stop-in-flight");
    let config = scratch.config();
    let item = test_item();
    // The determination, then the Artist's opening; Ctrl-C while the
    // Philosopher's opening is in flight. The reason prompt refuses an
    // empty line, a second Ctrl-C and Ctrl-D before it takes one.
    let mut responses = four(crate::councillor::harness_tests::not_limiting);
    responses.push(submission_response(position_input()));
    let reason = "The Engineer's seat runs the wrong model.";
    let (err, answered) = sit_to_stop(
        &config,
        &item,
        responses,
        Some(5),
        vec![
            Said::Text(""),
            Said::CtrlC,
            Said::Eof,
            Said::Text("   "),
            Said::Text("  The Engineer's seat runs the wrong model.  "),
        ],
    )
    .await;
    assert_eq!(answered, 5, "the call in flight never came back");
    let stage = Stage::Round {
        number: 1,
        kind: RoundKind::Opening,
        seat: CouncilSeat::Philosopher,
    };
    let manifest = assert_stopped_and_sealed(
        &config,
        &err,
        &stage,
        crate::stop::StopDuring::ModelCall,
        reason,
    );
    // The record carries the recusal question, the four answers and the
    // Artist's opening: what a decided item's record would, so far.
    let record_path = manifest.record.unwrap();
    let draft: RecordDraft = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    let names: Vec<&str> = draft.attachments.iter().map(|a| a.name.as_str()).collect();
    for name in [
        "recusal-question.md",
        "recusal-determinations.json",
        "rounds.json",
    ] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    let record_dir = record_path.parent().unwrap();
    let votes: Vec<agora_agentkit::govlog::RecusalVote> = serde_json::from_str(
        &std::fs::read_to_string(record_dir.join("recusal-determinations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(votes.len(), 4);
    let rounds: Vec<crate::models::CouncilRound> =
        serde_json::from_str(&std::fs::read_to_string(record_dir.join("rounds.json")).unwrap())
            .unwrap();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].responses.len(), 1, "only the Artist spoke");
}

#[tokio::test]
async fn ctrl_c_at_the_stewards_menu_is_the_same_stop() {
    let scratch = Scratch::new("stop-at-prompt");
    let config = scratch.config();
    let item = test_item();
    // Round 1 runs; at the round-2 menu the Steward presses Ctrl-C.
    let mut responses = four(crate::councillor::harness_tests::not_limiting);
    responses.extend(four(position_input));
    let (err, answered) = sit_to_stop(
        &config,
        &item,
        responses,
        None,
        vec![Said::CtrlC, Said::Text("Stopping to fix the agenda.")],
    )
    .await;
    assert_eq!(answered, 8);
    let stage = Stage::Round {
        number: 1,
        kind: RoundKind::Opening,
        seat: CouncilSeat::Engineer,
    };
    assert_stopped_and_sealed(
        &config,
        &err,
        &stage,
        crate::stop::StopDuring::StewardPrompt,
        "Stopping to fix the agenda.",
    );
}

#[test]
fn heard_names_a_decision_or_a_record() {
    let expected: GovernanceLogId = "GOV-2026-0018".parse().unwrap();
    for form in [
        "heard:GOV-2026-0018",
        "heard GOV-2026-0018",
        "heard=GOV-2026-0018",
    ] {
        assert_eq!(
            form.parse::<Resolution>().unwrap(),
            Resolution::Heard(expected.clone()),
            "{form}"
        );
    }
    assert!(matches!(
        "heard:REC-2026-0009".parse::<Resolution>(),
        Ok(Resolution::Heard(_))
    ));
    assert!("heard".parse::<Resolution>().is_err());
    assert!("heard:APP-2026-0003".parse::<Resolution>().is_err());
    assert!("heard:nonsense".parse::<Resolution>().is_err());
    // The manifest keeps the id.
    let json = serde_json::to_value(Resolution::Heard(expected)).unwrap();
    assert_eq!(json, serde_json::json!({ "heard": "GOV-2026-0018" }));
}

#[tokio::test]
async fn heard_is_refused_for_a_refusal() {
    let scratch = Scratch::new("heard-refusal");
    let config = scratch.config();
    let dir = sealed_one(&config).await;
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();
    let err = resolve(
        &config,
        &dir,
        "heard:GOV-2026-0018".parse().unwrap(),
        "Wrong kind.",
        &mut Typed(VecDeque::from([name])),
    )
    .expect_err("`heard` is for a stop");
    assert!(err.to_string().contains("Steward stop"), "{err}");
}

/// A stop sealed under a separate `stops_dir` (a rehearsal's) blocks
/// nothing: refusals and emission breaches still seal in `refusals_dir`
#[tokio::test]
async fn a_stop_in_a_rehearsal_seals_apart_and_blocks_no_sitting() {
    let scratch = Scratch::new("stop-apart");
    let mut config = scratch.config();
    config.stops_dir = scratch.0.join("rehearsal/stops");
    let item = test_item();
    let mut responses = four(crate::councillor::harness_tests::not_limiting);
    responses.push(submission_response(position_input()));
    let (err, _) = sit_to_stop(
        &config,
        &item,
        responses,
        Some(5),
        vec![Said::Text("Rehearsal stopped to change the prompt.")],
    )
    .await;
    let aborted = SittingAborted::find(&err).expect("an abort");
    let sealed = aborted.sealed.as_ref().expect("sealed");
    assert_eq!(sealed.dir.parent(), Some(config.stops_dir.as_path()));
    assert!(pending(&config.refusals_dir).unwrap().is_empty());
    ensure_none_pending(&config).expect("a rehearsal's stop holds no sitting");
}
