//! The decision's summary, in the seats' own conversations, against a
//! scripted transport and a scripted Steward: no model is ever called.
//!
//! Every seat votes first (one exchange each), so each conversation has a
//! history the summary turns must append to without touching.

use std::collections::VecDeque;

use misanthropic::prompt::message::Block;
use misanthropic::response;
use misanthropic::response::StopReason;

use super::*;
use crate::abort::SittingAborted;
use crate::councillor::harness_tests::{Mock, canned_from, cyber_refusal, vote_input};
use crate::models::{CouncilDecisionRecord, RoundType, StewardVote};
use crate::record::FinalVote;
use agora_common::govlog::SummaryAcceptance;
use agora_common::ids::AgendaItemId;

/// Answers the Steward's prompts from a script.
struct Scripted(VecDeque<&'static str>);

impl StewardInput for Scripted {
    fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
        self.0
            .pop_front()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the Steward script ran out"))
    }
}

/// A submission as the API returns it: the seat's thinking, then the
/// format's JSON
fn answer(submission: Submission) -> response::Message {
    let thought = Block::Thought {
        thought: "Checked against the record.".into(),
        signature: "EqQB".into(),
    };
    canned_from(
        "claude-opus-4-6",
        "msg_test",
        Content(vec![
            thought,
            Block::text(serde_json::to_string(&submission).unwrap()),
        ]),
        StopReason::EndTurn,
    )
}

fn draft(note: &str, summary: &str) -> response::Message {
    answer(Submission::Summary(SummaryDraft {
        note_to_reviewers: note.into(),
        summary: summary.into(),
    }))
}

fn seat(accept: bool, comment: &str) -> response::Message {
    answer(Submission::SummaryReview(SeatReview {
        comment: comment.into(),
        accept,
    }))
}

fn refused() -> response::Message {
    let mut refused = draft("", "Refused text.");
    refused.stop_reason = Some(StopReason::Refusal);
    refused.stop_details = cyber_refusal();
    refused
}

/// Each seat's final vote, in seating order: the history every summary
/// turn appends to
fn votes() -> Vec<response::Message> {
    CouncilSeat::ALL
        .iter()
        .map(|seat| {
            answer(vote_input(
                &format!("The {seat} votes yes."),
                FinalVote::Yes,
            ))
        })
        .collect()
}

fn item() -> AgendaItem {
    crate::record_tests::item(
        AgendaItemId::from(uuid::Uuid::from_u128(7)),
        crate::models::DecisionCategory::Policy,
    )
}

/// A Policy item carried with the Steward's concurrence
fn seated(item: &AgendaItem) -> Decision {
    crate::record_tests::voted(item, [CouncilVote::Yes; 4], StewardVote::Concur)
}

fn id() -> GovernanceLogId {
    "GOV-2026-0019".parse().unwrap()
}

fn record_of(decision: &Decision) -> String {
    decision_record(&id(), decision, None)
}

/// The four seats, each having voted (the first four requests), then the
/// summary written in their conversations
async fn write(
    mock: &Mock,
    config: &crate::config::Config,
    item: &AgendaItem,
    decision: &Decision,
    steward: &mut impl StewardInput,
) -> anyhow::Result<Written> {
    let mut councillors: Vec<Councillor<'_, Mock>> = CouncilSeat::ALL
        .iter()
        .map(|&role| Councillor::new(mock, config, role, Vec::new(), None, None).unwrap())
        .collect();
    let mut reader = ProposalReader::new(None, None);
    for councillor in &mut councillors {
        councillor
            .exchange(
                RoundType::FinalVote,
                Content::from("The Steward has called the vote."),
                &mut reader,
            )
            .await
            .unwrap();
    }
    write_summary(
        config,
        item,
        decision,
        &record_of(decision),
        &mut councillors,
        &mut reader,
        steward,
    )
    .await
}

/// What `meeting::signed_log_data` signs, typed back, with the record's
/// rules checked
fn signed(decision: &Decision, written: &Written) -> CouncilDecisionRecord {
    let meeting = agora_common::ids::CouncilMeetingId::from(uuid::Uuid::nil());
    let data = crate::meeting::signed_log_data(&id(), meeting, decision, written).unwrap();
    assert_eq!(CouncilDecisionRecord::check_data(&data), Ok(()));
    let record: CouncilDecisionRecord = serde_json::from_value(data).unwrap();
    assert_eq!(record.check(), Ok(()));
    record
}

/// `value` with every `cache_control` removed: the rolling breakpoints move
/// along the tail of an append-only conversation, and are not content.
fn without_cache(value: &serde_json::Value) -> serde_json::Value {
    let mut value = value.clone();
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                map.remove("cache_control");
                map.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut value);
    value
}

/// `later` continues `earlier`: everything but the messages identical
/// (system, tools, tool choice, format, thinking: the cached prefix), and
/// `earlier`'s messages a prefix of `later`'s, byte for byte once the
/// moving cache breakpoints are set aside; `appended` more after them.
/// Returns the messages appended.
fn appends(
    earlier: &serde_json::Value,
    later: &serde_json::Value,
    appended: usize,
) -> Vec<serde_json::Value> {
    let (earlier, later) = (without_cache(earlier), without_cache(later));
    for key in [
        "system",
        "tools",
        "tool_choice",
        "output_config",
        "thinking",
        "model",
    ] {
        assert_eq!(earlier[key], later[key], "{key} changed");
    }
    let before = earlier["messages"].as_array().unwrap();
    let after = later["messages"].as_array().unwrap();
    assert_eq!(after.len(), before.len() + appended, "appended {appended}");
    assert_eq!(
        serde_json::to_string(&after[..before.len()]).unwrap(),
        serde_json::to_string(before).unwrap(),
        "nothing removed or rewritten"
    );
    after[before.len()..].to_vec()
}

/// The two after-vote shapes are in the one response format every seat
/// has from its first turn, `$ref`-free, no `pattern`, each with its words
/// before its decision, and no field asking for reasoning
#[test]
fn the_after_vote_shapes_are_in_the_seats_format() {
    let orders = crate::record::declared_orders();
    let order = |name: &str| {
        orders
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} is in the format"))
            .1
            .clone()
    };
    assert_eq!(
        order("summary"),
        ["submission", "note_to_reviewers", "summary"]
    );
    assert_eq!(order("summary_review"), ["submission", "comment", "accept"]);
    let schema =
        serde_json::to_value(Submission::output_config()).unwrap()["format"]["schema"].to_string();
    assert!(
        !schema.contains("$ref") && !schema.contains("$defs"),
        "{schema}"
    );
    assert!(!schema.contains("\"pattern\""), "{schema}");
    for word in ["reasoning", "thinking", "scratchpad"] {
        assert!(!schema.contains(&format!("\"{word}\"")), "{word}");
    }
}

/// The Steward accepts the first draft: it is written in the Lawyer's own
/// conversation, appended after its vote, and signed accepted by the
/// Steward
#[tokio::test]
async fn a_seated_steward_accepts_and_the_record_carries_the_summary() {
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(draft("", "The Council approved the policy 4-0."));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(["a"].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.outcome, Outcome::Accepted { draft: 1 });
    assert_eq!(
        written.text.as_deref(),
        Some("The Council approved the policy 4-0.")
    );
    assert_eq!(written.review, None);

    let record = signed(&decision, &written);
    let summary = record.summary.expect("signed with the summary");
    assert_eq!(summary.written_by, CouncilSeat::Lawyer);
    assert_eq!(summary.accepted_by, SummaryAcceptance::Steward);

    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    // The Lawyer voted third; its draft is its vote's request, the vote
    // as it answered, and one user turn
    let added = appends(&requests[2], &requests[4], 2);
    assert_eq!(added[0]["role"], "assistant");
    assert_eq!(added[1]["role"], "user");
    let turn = added[1].to_string();
    assert!(turn.contains("# The Decision's Summary"), "the charge");
    assert!(
        turn.contains("GOV-2026-0019: A test proposal"),
        "the record"
    );
    assert!(turn.contains("Its reviewers: the Steward."), "who reviews");
    assert!(
        !turn.contains("You are **The Lawyer**"),
        "no second identity"
    );
}

/// The Steward's input goes to the Lawyer as one more turn; the Lawyer
/// may decline it, and what is signed is the Lawyer's revision
#[tokio::test]
async fn steward_input_is_revised_by_the_lawyer() {
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(draft("", "Draft one."));
    script.push(draft(
        "I declined the second point: the record shows 4-0.",
        "Draft two.",
    ));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(["i", "Fix the tally.", "Also say it was 3-1.", "", "a"].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text.as_deref(), Some("Draft two."));
    assert_eq!(written.outcome, Outcome::Accepted { draft: 2 });

    let requests = mock.requests();
    assert_eq!(requests.len(), 6);
    let added = appends(&requests[4], &requests[5], 2);
    let input = added[1].to_string();
    assert!(input.contains("Fix the tally.\\nAlso say it was 3-1."));
    assert!(input.contains("You may decline any of it"));
    signed(&decision, &written);
}

/// A dissent: the Lawyer's text is signed unchanged, and the Steward's
/// dissent goes on the record as `summary-review.md`, under his name
#[tokio::test]
async fn a_dissent_is_attached_to_the_record() {
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(draft("", "The Lawyer's text."));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(["d", "I read the tally differently.", ""].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text.as_deref(), Some("The Lawyer's text."));
    let review = written.review.clone().expect("the dissent is attached");
    assert_eq!(review.name, REVIEW_ATTACHMENT);
    let Redactable::Value(content) = &review.content else {
        unreachable!()
    };
    assert!(content.contains("I read the tally differently."));
    let record = signed(&decision, &written);
    assert_eq!(record.summary.unwrap().served_text(), "The Lawyer's text.");
}

/// Withheld: the decision is signed without a summary, which the rules
/// admit, and the record says why
#[tokio::test]
async fn a_withheld_summary_signs_the_decision_without_one() {
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(draft("", "Draft."));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(["x", "w"].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text, None);
    assert_eq!(written.outcome, Outcome::Withheld { draft: 1 });
    let record = signed(&decision, &written);
    assert_eq!(record.summary, None);
    assert!(
        record
            .attachments
            .iter()
            .any(|a| a.name == REVIEW_ATTACHMENT)
    );
}

/// A failed turn (here the transport gives out) is the Steward's to
/// retry: the same conversation is sent again, nothing appended
#[tokio::test]
async fn a_failed_draft_is_sent_again_unchanged_when_the_steward_asks() {
    // The scripted transport runs out after the votes: the draft's first
    // send fails (a 400-class error, not retried by `retry_recoverable`).
    // The Steward asks again, which sends the very same request.
    let item = item();
    let decision = seated(&item);
    let mock = Mock::scripted(votes());
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(["r", "w"].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text, None, "withheld after the retry failed too");
    assert!(steward.0.is_empty(), "asked twice");
    let requests = mock.requests();
    assert_eq!(requests.len(), 6, "four votes, the draft, the retry");
    assert_eq!(
        without_cache(&requests[4]),
        without_cache(&requests[5]),
        "the retry appends nothing"
    );
}

/// A blank draft fails the corpus gate: it goes back to the Lawyer,
/// appended, before the Steward sees anything. (A UUID no longer fails
/// it: Steward, 2026-10-17.)
#[tokio::test]
async fn a_draft_failing_the_gate_goes_back_to_the_lawyer() {
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(draft("", "  \n"));
    script.push(draft("Wrote the summary.", "Approved 4-0."));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(["a"].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text.as_deref(), Some("Approved 4-0."));
    assert_eq!(written.outcome, Outcome::Accepted { draft: 2 });
    let requests = mock.requests();
    let added = appends(&requests[4], &requests[5], 2);
    assert!(added[1].to_string().contains("empty"), "{}", added[1]);
}

/// The Steward recused: the three seats review in their own
/// conversations, appended after their votes; one asks a correction, the
/// Lawyer revises, all three accept the revision; nothing is read from the
/// Steward, and the record is signed accepted by the seats
#[tokio::test]
async fn a_recused_stewards_item_is_accepted_by_the_three_seats() {
    let item = item();
    let decision = crate::record_tests::recused(&item);
    assert_eq!(decision.final_votes().steward, CouncilVote::Recused);
    let mut script = votes();
    script.extend([
        draft("", "Draft one."),
        seat(true, "Accurate."),
        seat(false, "The tally is 4-0, not 3-1."),
        seat(true, "Accurate."),
        draft(
            "Corrected the tally, as the Philosopher asked.",
            "Draft two.",
        ),
        seat(true, "Accepted."),
        seat(true, "Accepted: the tally is right now."),
        seat(true, "Accepted."),
    ]);
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(VecDeque::new());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text.as_deref(), Some("Draft two."));
    assert_eq!(
        written.outcome,
        Outcome::AcceptedBySeats { draft: 2, round: 2 }
    );
    let record = signed(&decision, &written);
    assert_eq!(
        record.summary.unwrap().accepted_by,
        SummaryAcceptance::Seats {
            seats: RECUSED_ACCEPTORS.to_vec()
        }
    );
    let review = record
        .attachments
        .iter()
        .find(|a| a.name == REVIEW_ATTACHMENT)
        .expect("the seats' review is on the record");
    let Redactable::Value(review) = &review.content else {
        unreachable!()
    };
    assert!(review.contains("The tally is 4-0, not 3-1."));
    assert!(review.contains("## Round 2: draft 2"));

    // votes 0..4 (Artist, Philosopher, Lawyer, Engineer); draft 4;
    // reviews 5, 6, 7; revision 8; reviews 9, 10, 11
    let requests = mock.requests();
    assert_eq!(requests.len(), 12);
    for (vote, first, second, name) in [
        (0, 5, 9, "Artist"),
        (1, 6, 10, "Philosopher"),
        (3, 7, 11, "Engineer"),
    ] {
        // The first review continues the seat's own conversation
        let added = appends(&requests[vote], &requests[first], 2);
        let turn = added[1].to_string();
        assert!(
            turn.contains("# Reviewing the Decision's Summary"),
            "{name}"
        );
        assert!(turn.contains("GOV-2026-0019: A test proposal"), "{name}");
        assert!(turn.contains("Draft one."), "{name}");
        assert!(
            !turn.contains("You are **The"),
            "{name}: no second identity"
        );
        // and the second continues the first
        let added = appends(&requests[first], &requests[second], 2);
        let turn = added[1].to_string();
        assert!(turn.contains("The tally is 4-0, not 3-1."), "{name}");
        assert!(turn.contains("Draft two."), "{name}");
        assert!(turn.contains("Corrected the tally"), "{name}");
    }
    // The Lawyer's revision continues its draft
    let added = appends(&requests[4], &requests[8], 2);
    assert!(added[1].to_string().contains("The tally is 4-0, not 3-1."));
}

/// At the round cap without agreement, the decision is signed without a
/// summary, and every round's comments and the last draft are attached
#[tokio::test]
async fn unagreed_seats_sign_the_decision_without_a_summary() {
    let item = item();
    let decision = crate::record_tests::recused(&item);
    let round = |n: u32| {
        vec![
            seat(true, "Fine."),
            seat(
                false,
                Box::leak(format!("Still wrong ({n}).").into_boxed_str()),
            ),
            seat(true, "Fine."),
        ]
    };
    let mut script = votes();
    script.push(draft("", "Draft one."));
    script.extend(round(1));
    script.push(draft("Declined.", "Draft two."));
    script.extend(round(2));
    script.push(draft("Declined again.", "Draft three."));
    script.extend(round(3));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let mut steward = Scripted(VecDeque::new());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .unwrap();
    assert_eq!(written.text, None);
    assert_eq!(written.outcome, Outcome::Unagreed { draft: 3 });
    assert_eq!(
        mock.requests().len(),
        16,
        "four votes and three rounds, no more"
    );
    let record = signed(&decision, &written);
    assert_eq!(record.summary, None);
    let review = record
        .attachments
        .iter()
        .find(|a| a.name == REVIEW_ATTACHMENT)
        .expect("the disagreement is on the record");
    let Redactable::Value(review) = &review.content else {
        unreachable!()
    };
    for n in 1..=3 {
        assert!(review.contains(&format!("Still wrong ({n}).")), "{review}");
    }
    assert!(review.contains("without a summary"));
    assert!(
        review.contains("Draft three."),
        "the last draft, for the record"
    );
}

/// A refused draft aborts the sitting, every seat's conversation sealed;
/// the Steward is not asked and nothing is retried
#[tokio::test]
async fn a_refused_draft_aborts_the_sitting() {
    let scratch = crate::abort::tests::Scratch::new("decision-summary-refused");
    let config = scratch.config();
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(refused());
    script.push(draft("", "Never sent."));
    let mock = Mock::scripted(script);
    let mut steward = Scripted(["r", "a"].into());
    let err = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .expect_err("a refusal aborts");
    let requests = mock.requests();
    assert_eq!(requests.len(), 5, "no retry, no re-sample");
    assert_eq!(steward.0.len(), 2, "the Steward was not asked");
    crate::abort::tests::assert_aborted_and_sealed(
        &config,
        &err,
        &Stage::DecisionSummary,
        agora_common::enums::ModelRole::Lawyer,
        &requests[4],
        &[
            "refusal.json",
            "refused-request.json",
            "seat-artist.json",
            "seat-philosopher.json",
            "seat-lawyer.json",
            "seat-engineer.json",
            "clerk-briefing.json",
        ],
    );
}

/// A seat's review that draws a refusal aborts too
#[tokio::test]
async fn a_refused_review_aborts_the_sitting() {
    let scratch = crate::abort::tests::Scratch::new("decision-summary-review-refused");
    let config = scratch.config();
    let item = item();
    let decision = crate::record_tests::recused(&item);
    let mut script = votes();
    script.extend([draft("", "Draft one."), seat(true, "Fine."), refused()]);
    let mock = Mock::scripted(script);
    let mut steward = Scripted(VecDeque::new());
    let err = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .expect_err("a refusal aborts");
    let requests = mock.requests();
    assert_eq!(requests.len(), 7);
    crate::abort::tests::assert_aborted_and_sealed(
        &config,
        &err,
        &Stage::DecisionSummary,
        agora_common::enums::ModelRole::Philosopher,
        &requests[6],
        &[
            "refusal.json",
            "refused-request.json",
            "seat-artist.json",
            "seat-philosopher.json",
            "seat-lawyer.json",
            "seat-engineer.json",
            "clerk-briefing.json",
        ],
    );
}

/// The Steward at the terminal, with the sitting's stop signal
struct Stopping {
    stop: crate::stop::StopSignal,
    lines: VecDeque<&'static str>,
}

impl StewardInput for Stopping {
    fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
        self.lines
            .pop_front()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the Steward script ran out"))
    }
    fn stop_signal(&self) -> Option<crate::stop::StopSignal> {
        Some(self.stop.clone())
    }
}

/// A Steward stop during the summary never discards the vote, recused or
/// seated: the summary is not written, the decision is to be signed
/// without one with `summary-review.md` giving the stop and the reason,
/// and the stop's facts say where the Steward stood
#[tokio::test]
async fn a_stop_during_the_summary_keeps_the_vote() {
    for (decision, recused) in [
        (seated(&item()), false),
        (crate::record_tests::recused(&item()), true),
    ] {
        let item = item();
        let config = crate::config::Config::for_tests();
        let stop = crate::stop::StopSignal::new(None);
        stop.trigger();
        let mut script = votes();
        script.push(draft("", "Never read."));
        let mock = Mock::scripted(script);
        let mut steward = Stopping {
            stop,
            lines: ["The record needs another look."].into(),
        };
        let written = write(&mock, &config, &item, &decision, &mut steward)
            .await
            .expect("the vote stands");
        assert_eq!(
            mock.requests().len(),
            4,
            "only the votes: the stop came first"
        );
        let Outcome::Stopped { facts } = &written.outcome else {
            panic!("{:?}", written.outcome)
        };
        assert_eq!(
            facts.reason.as_deref(),
            Some("The record needs another look.")
        );
        assert_eq!(facts.steward_recused, Some(recused));
        assert_eq!(written.text, None);
        let record = signed(&decision, &written);
        assert_eq!(record.summary, None);
        let review = record
            .attachments
            .iter()
            .find(|a| a.name == REVIEW_ATTACHMENT)
            .expect("the stop is on the record");
        let Redactable::Value(text) = &review.content else {
            unreachable!()
        };
        assert!(text.contains("The record needs another look."), "{text}");
        assert!(text.contains("never discards a vote"), "{text}");
        assert_eq!(text.contains("recused"), recused, "{text}");
    }
}

/// The stop is then sealed after the signing, at `SummaryStopped`: its
/// record says the decision is signed and where the Steward stood, and the
/// stop cannot be resolved as heard in its own record
#[tokio::test]
async fn a_stop_after_the_vote_is_sealed_as_signed_and_not_heard_in_its_own_record() {
    let scratch = crate::abort::tests::Scratch::new("decision-summary-stop-sealed");
    let config = scratch.config();
    let item = item();
    let decision = crate::record_tests::recused(&item);
    let facts = crate::stop::StopFacts {
        reason: Some("The record needs another look.".into()),
        during: crate::stop::StopDuring::StewardPrompt,
        stopped_at: chrono::Utc::now(),
        reason_at: chrono::Utc::now(),
        meeting: None,
        steward_recused: Some(true),
    };
    let mock = Mock::scripted([]);
    let councillor =
        Councillor::new(&mock, &config, CouncilSeat::Lawyer, Vec::new(), None, None).unwrap();
    let conversations = vec![(
        crate::abort::Holder::Seat(CouncilSeat::Lawyer),
        councillor.conversation().clone(),
    )];
    let err = crate::meeting::seal_summary_stop(
        &config,
        &item,
        &decision,
        &facts,
        &conversations,
        Stage::SummaryStopped { decision: id() },
    )
    .await;
    let aborted = SittingAborted::find(&err).expect("sealed");
    assert_eq!(aborted.stage, Stage::SummaryStopped { decision: id() });
    let sealed = aborted.sealed.as_ref().expect("sealed");
    let record = sealed.append.as_ref().expect("a stop's record is appended");
    assert!(
        record.body.contains("signed as GOV-2026-0019"),
        "{}",
        record.body
    );
    assert!(
        record.body.contains("**without a summary**"),
        "{}",
        record.body
    );
    assert!(
        record
            .body
            .contains("**The Steward's standing on the item**: recused"),
        "{}",
        record.body
    );

    // Appended as REC-2026-0099: that id cannot be where it was heard
    let own: GovernanceLogId = "REC-2026-0099".parse().unwrap();
    crate::abort::note_recorded(&sealed.dir, &own).unwrap();
    let name = sealed
        .dir
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    struct Typed(VecDeque<String>);
    impl StewardInput for Typed {
        fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
            self.0
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("the Steward script ran out"))
        }
    }
    let refused = crate::abort::resolve(
        &config,
        &sealed.dir,
        crate::abort::Resolution::Heard(own),
        "Heard.",
        &mut Typed(VecDeque::from([name.clone()])),
    )
    .expect_err("its own record is refused");
    assert!(refused.to_string().contains("own record"), "{refused}");
    crate::abort::resolve(
        &config,
        &sealed.dir,
        crate::abort::Resolution::Heard("GOV-2026-0020".parse().unwrap()),
        "Heard at the next sitting.",
        &mut Typed(VecDeque::from([name])),
    )
    .expect("heard in a decision");
}

/// Recused, drafts that keep failing the corpus gate end the summary: the
/// decision is signed without one, the reason attached; seated, the
/// Steward is asked and may withhold
#[tokio::test]
async fn drafts_that_keep_failing_the_gate_do_not_lose_the_vote() {
    let blank = "  \n";
    let item = item();
    let config = crate::config::Config::for_tests();

    let decision = crate::record_tests::recused(&item);
    let mut script = votes();
    script.extend((0..=GATE_REVISIONS).map(|_| draft("", blank)));
    let mock = Mock::scripted(script);
    let written = write(
        &mock,
        &config,
        &item,
        &decision,
        &mut Scripted(VecDeque::new()),
    )
    .await
    .expect("the vote stands");
    assert!(
        matches!(&written.outcome, Outcome::Failed { reason } if reason.contains("corpus gate")),
        "{:?}",
        written.outcome
    );
    let record = signed(&decision, &written);
    assert_eq!(record.summary, None);
    assert!(
        record
            .attachments
            .iter()
            .any(|a| a.name == REVIEW_ATTACHMENT)
    );

    let decision = seated(&item);
    let mut script = votes();
    script.extend((0..=GATE_REVISIONS).map(|_| draft("", blank)));
    let mock = Mock::scripted(script);
    let mut steward = Scripted(["w"].into());
    let written = write(&mock, &config, &item, &decision, &mut steward)
        .await
        .expect("the vote stands");
    assert_eq!(written.outcome, Outcome::Withheld { draft: 3 });
    assert!(steward.0.is_empty(), "the Steward was asked");
}

/// The Steward's terminal failing at the review signs the decision without
/// a summary rather than losing it
#[tokio::test]
async fn a_failed_terminal_at_the_review_does_not_lose_the_vote() {
    let item = item();
    let decision = seated(&item);
    let mut script = votes();
    script.push(draft("", "Draft."));
    let mock = Mock::scripted(script);
    let config = crate::config::Config::for_tests();
    let written = write(
        &mock,
        &config,
        &item,
        &decision,
        &mut Scripted(VecDeque::new()),
    )
    .await
    .expect("the vote stands");
    assert!(
        matches!(&written.outcome, Outcome::Failed { .. }),
        "{:?}",
        written.outcome
    );
    signed(&decision, &written);
}

/// The record the Lawyer is given: no UUID on a scheduling item (short ids
/// instead), the recused threshold on a recused item, the recusal
/// determination with every answer, what each seat voted for, the
/// obligations before the hearing, and the attachments' names
#[test]
fn the_decision_record_carries_what_the_summary_needs() {
    let mut scheduling = item();
    scheduling.category = crate::models::DecisionCategory::Schedule;
    let ranked = crate::record_tests::schedule(&scheduling);
    let text = decision_record(&id(), &ranked, Some(&scheduling));
    assert!(
        !agora_common::anonymize::contains_uuid(&text),
        "short ids, not UUIDs: {text}"
    );
    assert!(text.contains("(post 00000000)"), "{text}");

    let item = item();
    let mut decision = crate::record_tests::recused(&item);
    decision.attachments.push(CouncilAttachment {
        name: "clerk-thread-summary.md".into(),
        note: "The Clerk's summary.".into(),
        content: Redactable::Value("…".into()),
    });
    let text = decision_record(&id(), &decision, Some(&item));
    let recused = decision.category.recused_threshold().unwrap();
    assert!(text.contains(&format!("(threshold: {recused})")), "{text}");
    assert!(text.contains("### The recusal determination"), "{text}");
    assert!(text.contains("the Steward is recused"), "{text}");
    for seat in CouncilSeat::ALL {
        assert!(
            text.contains(&format!("**The {seat}**: ")),
            "{seat}'s answer: {text}"
        );
    }
    assert!(text.contains("### What each seat voted for"), "{text}");
    assert!(text.contains("- `clerk-thread-summary.md`: The Clerk's summary."));
}

/// `--set-summary` drafts a revision request `govlog revise` reads: an
/// `add` at `/summary` with the acceptance the item requires, and the
/// revised record keeps the record's rules. Adding to a record signed
/// without one and replacing one both work.
#[test]
fn set_summary_drafts_a_revision_of_the_signed_summary() {
    let item = item();
    let meeting = agora_common::ids::CouncilMeetingId::from(uuid::Uuid::nil());
    for (decision, accepted) in [
        (seated(&item), SummaryAcceptance::Steward),
        (
            crate::record_tests::recused(&item),
            SummaryAcceptance::Seats {
                seats: RECUSED_ACCEPTORS.to_vec(),
            },
        ),
    ] {
        let unsummarized = crate::meeting::council_log_data(&id(), meeting, &decision).unwrap();
        let file = crate::meeting::summary_revision_of(
            &id(),
            &unsummarized,
            "The corrected summary.\n",
            "the Lawyer, at the 2026-10-24 sitting",
        )
        .unwrap();
        assert_eq!(file.target, id());
        assert!(
            file.note.starts_with("Adds the decision's summary"),
            "{}",
            file.note
        );
        assert!(file.note.contains("the Lawyer, at the 2026-10-24 sitting"));
        // What `govlog revise` parses, `deny_unknown_fields` and all
        let text = serde_json::to_string(&file).unwrap();
        let read: agora_common::govlog::RevisionFile = serde_json::from_str(&text).unwrap();
        let revised = agora_common::govlog::apply_patch(&unsummarized, &read.patch).unwrap();
        assert_eq!(CouncilDecisionRecord::check_data(&revised), Ok(()));
        let summary = agora_common::govlog::DecisionSummary::of_data(&revised)
            .unwrap()
            .unwrap();
        assert_eq!(summary.served_text(), "The corrected summary.");
        assert_eq!(summary.accepted_by, accepted);

        let again =
            crate::meeting::summary_revision_of(&id(), &revised, "Corrected again.", "s").unwrap();
        assert!(again.note.starts_with("Replaces"), "{}", again.note);
    }
}
