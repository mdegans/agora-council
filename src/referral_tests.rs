//! Appeal referrals (agora#471): what the seats are told, and what a
//! decision writes back — against the real schema, as `agora_council`.

use std::collections::VecDeque;

use agora_common::enums::{AppealOutcome, AppealStatus};
use agora_common::ids::{AgendaItemId, AgentId, AppealId, GovernanceLogId, ModerationActionId};
use agora_test_support as ts;
use misanthropic::prompt::message::Content;
use misanthropic::response;
use sqlx::PgPool;

use super::*;
use crate::councillor::Councillor;
use crate::councillor::harness_tests::{Mock, Scripted, position_input, test_config, test_item};
#[allow(unused_imports)]
use crate::councillor::harness_tests::{
    rank_input, respond_input, submission_response, vote_input,
};
use crate::docket::ProposalReader;
use crate::models::{CouncilSeat, CouncilVote, DecisionCategory, MeetingState};
use crate::record::{FinalVote, Submission};
use crate::record_tests::{Db, deferred, item, signer, voted, written};

// ---------------------------------------------------------------------------
// The motion, as the seats hear it
// ---------------------------------------------------------------------------

/// Four canned calls of `method`, one per seat, in seating order.
fn round(input: impl Fn() -> Submission) -> Vec<response::Message> {
    (0..CouncilSeat::ALL.len())
        .map(|_| submission_response(input()))
        .collect()
}

/// The text of the last user turn of a request.
fn last_user_turn(request: &serde_json::Value) -> String {
    let messages = request["messages"].as_array().expect("messages");
    let last = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("a user turn");
    serde_json::to_string(last).unwrap()
}

/// Run a referral item through `run_item` and return every request sent.
async fn run_referral() -> (crate::models::Decision, Vec<serde_json::Value>) {
    // The recusal determination comes first: no seat finds the referral
    // limits the Steward's powers.
    let mut responses = round(crate::councillor::harness_tests::not_limiting);
    responses.extend(round(position_input));
    responses.extend(round(|| respond_input("I have heard enough.", true)));
    responses.extend(round(|| vote_input("Final.", FinalVote::Yes)));
    let mock = Mock::scripted(responses);
    let config = test_config();
    let mut item = test_item();
    item.appeal_id = Some(AppealId::from(uuid::Uuid::from_u128(5)));
    item.body = "The Court's record.".into();
    let mut councillors: Vec<Councillor<'_, Mock>> = CouncilSeat::ALL
        .iter()
        .map(|&role| {
            Councillor::new(&mock, &config, role, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
        })
        .collect();
    let mut reader = ProposalReader::none();
    let mut reads = Vec::new();
    let mut input = Scripted(VecDeque::from(vec!["c", "c", "y", "n", ""]));
    let decision = crate::deliberation::run_item(
        &item,
        &Content::from("Recap."),
        &mut input,
        &mut councillors,
        &mut reader,
        &mut reads,
        None,
        &mut crate::deliberation::Progress::default(),
    )
    .await
    .expect("the item runs")
    .expect("the item was not skipped");
    // The four determination requests first; the rounds are what follow.
    let mut requests = mock.requests();
    let rounds = requests.split_off(4);
    assert!(requests.iter().all(|r| {
        serde_json::to_string(r)
            .unwrap()
            .contains("Recusal determination")
    }));
    (decision, rounds)
}

/// The Steward's 2026-09-29 decision: YES = overturn, NO = uphold, stated
/// in the item every seat reads *and* in the vote call itself, with what a
/// YES does to the appellant.
#[tokio::test]
async fn the_vote_turn_on_a_referral_states_the_motion() {
    let (decision, requests) = run_referral().await;
    assert_eq!(requests.len(), 12);

    for request in &requests[..4] {
        let turn = last_user_turn(request);
        assert!(turn.contains("### The Referred Appeal"), "{turn}");
        assert!(
            turn.contains("A YES vote OVERTURNS the moderation action. A NO vote UPHOLDS it."),
            "round 1 must carry the motion: {turn}"
        );
        assert!(turn.contains("appeal credit"), "{turn}");
    }
    for request in &requests[8..] {
        let turn = last_user_turn(request);
        assert!(turn.contains("The Steward has called the vote."), "{turn}");
        assert!(
            turn.contains("YES = OVERTURN the moderation action"),
            "the vote call must restate the motion: {turn}"
        );
        assert!(turn.contains("NO = UPHOLD it"), "{turn}");
        assert!(
            turn.contains("removed content is restored or a suspension lifted"),
            "the vote call must say what YES does to the appellant: {turn}"
        );
    }
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
    assert_eq!(
        appeal_outcome(&decision.outcome()),
        Some(AppealOutcome::Overturned)
    );
}

/// A proposal's vote call is untouched.
#[test]
fn a_proposal_vote_call_carries_no_motion() {
    let item = test_item();
    let call = crate::context::vote_call_for(
        &item,
        crate::models::StewardStanding::Seated,
        None,
        misanthropic::prompt::message::Content(Vec::new()),
    )
    .to_string();
    assert!(!call.contains("OVERTURN"), "{call}");
    let opening = crate::context::item_content(&item).to_string();
    assert!(opening.contains("### Proposal"), "{opening}");
    assert!(!opening.contains("OVERTURN"), "{opening}");
}

#[test]
fn outcomes_map_as_the_steward_decided() {
    assert_eq!(
        appeal_outcome(&DecisionOutcome::Approved),
        Some(AppealOutcome::Overturned)
    );
    assert_eq!(
        appeal_outcome(&DecisionOutcome::Rejected),
        Some(AppealOutcome::Upheld)
    );
    assert_eq!(appeal_outcome(&DecisionOutcome::Deferred), None);
    assert_eq!(appeal_outcome(&DecisionOutcome::Amended), None);
}

fn juror(n: u8, verdict: &str) -> serde_json::Value {
    serde_json::json!({
        "juror_number": n,
        "verdict": verdict,
        "rationale": format!("Juror {n}'s reasoning."),
        "constitutional_refs": ["V.1.3"],
        "context_analysis": format!("Juror {n}'s context analysis."),
    })
}

#[test]
fn the_jury_floor_is_the_upper_median() {
    let panel = |vs: &[&str]| -> Vec<JurorVerdict> {
        vs.iter()
            .enumerate()
            .map(|(i, v)| serde_json::from_value(juror(i as u8 + 1, v)).unwrap())
            .collect()
    };
    assert_eq!(
        jury_floor(&panel(&["overturn", "uphold", "overturn"])),
        Some(AppealVerdict::Overturn)
    );
    assert_eq!(
        jury_floor(&panel(&["uphold", "modify", "overturn"])),
        Some(AppealVerdict::Modify)
    );
    assert_eq!(
        jury_floor(&panel(&["uphold", "overturn"])),
        Some(AppealVerdict::Overturn)
    );
    assert_eq!(jury_floor(&[]), None);
}

// ---------------------------------------------------------------------------
// Against the database
// ---------------------------------------------------------------------------

const WITHHELD: uuid::Uuid = uuid::Uuid::from_u128(0xdead);
const CITED: uuid::Uuid = uuid::Uuid::from_u128(0xbeef);

/// An appellant, a content removal, and its appeal referred to the Council
/// with a sealed redaction, the jury's verdicts, the judge's ruling and one
/// withheld node — as the appeals worker leaves one. Written as the
/// superuser.
async fn seed_referral(root: &PgPool, name: &str, relief: bool) -> (AgentId, AppealId) {
    let operator = ts::seed_operator(root, &format!("{name}@example.com")).await;
    let agent = ts::seed_agent_with_reputation(root, operator, name, 0, 0).await;
    let (appeal, _) = seed_appeal(root, agent, relief).await;
    (agent, appeal)
}

async fn seed_appeal(
    root: &PgPool,
    agent: AgentId,
    relief: bool,
) -> (AppealId, ModerationActionId) {
    let action = sqlx::query_scalar!(
        r#"INSERT INTO moderation_actions
             (target_type, target_id, subject_agent_id, action_type, reason,
              constitutional_ref, tier, enforced_at)
           VALUES ('comment', $1, $2, 'content_removal', 'Removed: harassment.',
                   'V.1.3', '2', now())
           RETURNING id AS "id: ModerationActionId""#,
        CITED,
        agent as AgentId,
    )
    .fetch_one(root)
    .await
    .unwrap();
    let jury = serde_json::json!([
        juror(1, "overturn"),
        juror(2, "uphold"),
        juror(3, "overturn")
    ]);
    let ruling = serde_json::json!({
        "verdict": "overturn",
        "rationale": "The judge's reasoning.",
        "jury_assessment": "A split panel.",
        "precedents_cited": [],
        "refer_to_council": true,
        "referral_reason": "Art. V § 1.3 has not been interpreted for this pattern.",
        "context_analysis": "The judge's context analysis."
    });
    let appeal = sqlx::query_scalar!(
        r#"INSERT INTO appeals
             (moderation_action_id, agent_id, appeal_statement, status, outcome,
              decided_at, outcome_notified_at, jury_verdicts, judge_ruling,
              provisional_relief_at)
           VALUES ($1, $2, 'The raw statement, naming names.', 'referred_to_council',
                   'referred', now(), now(), $3, $4,
                   CASE WHEN $5 THEN now() END)
           RETURNING id AS "id: AppealId""#,
        action as ModerationActionId,
        agent as AgentId,
        jury,
        ruling,
        relief,
    )
    .fetch_one(root)
    .await
    .unwrap();
    let citations = serde_json::json!([
        {"content_id": CITED, "redacted_title": null,
         "transcription": "[APPELLANT] wrote the comment under appeal."},
        {"content_id": WITHHELD, "redacted_title": "A thread",
         "transcription": "Agent A wrote something prejudicial."}
    ]);
    sqlx::query!(
        "INSERT INTO appeal_redactions
           (appeal_id, redacted_statement, redacted_reason, redacted_citations, sealed_at)
         VALUES ($1, '[APPELLANT] says the removal was wrong.',
                 'Removed: harassment of Agent A.', $2, now())",
        appeal as AppealId,
        citations,
    )
    .execute(root)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO appeal_curations (appeal_id, analysis, sealed_at)
         VALUES ($1, 'Chambers analysis.', now())",
        appeal as AppealId,
    )
    .execute(root)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO appeal_withheld_items (appeal_id, content_id, reason)
         VALUES ($1, $2, 'Not relevant and prejudicial.')",
        appeal as AppealId,
        WITHHELD,
    )
    .execute(root)
    .await
    .unwrap();
    (appeal, action)
}

struct AppealState {
    status: AppealStatus,
    outcome: Option<AppealOutcome>,
    council_decision_id: Option<GovernanceLogId>,
    outcome_notified: bool,
    action_reversed_by: Option<AppealId>,
}

async fn appeal_state(root: &PgPool, appeal: AppealId) -> AppealState {
    sqlx::query_as!(
        AppealState,
        r#"SELECT a.status AS "status: AppealStatus",
                  a.outcome AS "outcome: AppealOutcome",
                  a.council_decision_id AS "council_decision_id: GovernanceLogId",
                  (a.outcome_notified_at IS NOT NULL) AS "outcome_notified!",
                  ma.reversed_by_appeal AS "action_reversed_by: AppealId"
             FROM appeals a JOIN moderation_actions ma ON ma.id = a.moderation_action_id
            WHERE a.id = $1"#,
        appeal as AppealId,
    )
    .fetch_one(root)
    .await
    .unwrap()
}

/// Seat `appeal` on a new meeting the way `run` does, as agora_council,
/// and return the meeting state and the item.
async fn seat_one(council: &PgPool, appeal: AppealId) -> (MeetingState, AgendaItemId) {
    let seats = check_seats(council, &[appeal]).await.unwrap();
    let meeting_id = crate::queries::create_meeting(council).await.unwrap();
    seat(council, meeting_id, &seats).await.unwrap();
    let rows = crate::queries::populate_agenda_from_proposals(council, meeting_id, 3)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].source_type,
        agora_common::enums::AgendaSourceType::AppealReferral
    );
    assert_eq!(
        rows[0].category,
        agora_common::enums::ProposalCategory::Routine
    );
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };
    (state, rows[0].id)
}

fn referral_item(id: AgendaItemId, appeal: AppealId) -> crate::models::AgendaItem {
    crate::models::AgendaItem {
        appeal_id: Some(appeal),
        deferral_spent: false,
        history: Vec::new(),
        obligations: Vec::new(),
        ..item(id, DecisionCategory::Routine)
    }
}

/// The case the seats read is the Court's anonymized record — not the raw
/// statement — with the withheld node marked, the jury, its floor, and
/// why the judge referred.
#[tokio::test]
async fn a_referral_loads_the_courts_anonymized_record() {
    let db = Db::new("council_referral_body").await;
    let (_, appeal) = seed_referral(&db.root, "body", false).await;

    let body = load(&db.council, appeal).await.unwrap().body();
    assert!(
        body.contains("[APPELLANT] says the removal was wrong."),
        "{body}"
    );
    assert!(
        !body.contains("naming names"),
        "the raw statement leaked: {body}"
    );
    assert!(body.contains("Removed: harassment of Agent A."), "{body}");
    assert!(body.contains("content removal"), "{body}");
    assert!(
        body.contains("[withheld from the jury and the bench by chambers]"),
        "{body}"
    );
    assert!(body.contains("Not relevant and prejudicial."), "{body}");
    assert!(body.contains("**Juror #2: uphold**"), "{body}");
    assert!(
        body.contains("**Jury floor** (the median juror): overturn"),
        "{body}"
    );
    // Steward, 2026-09-29: the floor bound the judge; whether it binds the
    // Council is unsettled (proposal pending), so it is not enforced.
    assert!(
        body.contains("Whether it binds the Council is not settled"),
        "{body}"
    );
    assert!(!body.contains("may not be less favourable"), "{body}");
    assert!(
        body.contains("Art. V § 1.3 has not been interpreted for this pattern."),
        "{body}"
    );
    assert!(body.contains("Seated as **Routine**"), "{body}");

    // An unsealed or no-longer-referred appeal cannot be seated.
    sqlx::query!(
        "UPDATE appeal_redactions SET sealed_at = NULL WHERE appeal_id = $1",
        appeal as AppealId
    )
    .execute(&db.root)
    .await
    .unwrap();
    let err = check_seats(&db.council, &[appeal]).await.unwrap_err();
    assert!(err.to_string().contains("no sealed redaction"), "{err}");
    let missing = AppealId::from(uuid::Uuid::from_u128(77));
    let err = check_seats(&db.council, &[missing]).await.unwrap_err();
    assert!(err.to_string().contains("no such appeal"), "{err}");

    db.drop().await;
}

/// YES carries: the appeal is overturned in the decision's transaction,
/// linked to the GOV entry, the action marked reversed for the remedy
/// sweep, the notice handed to the server — and the credit comes back
/// with no credit code at all (`appeal_credits` counts an overturn as
/// never spent).
#[tokio::test]
async fn a_council_overturn_writes_back_and_restores_the_credit() {
    let db = Db::new("council_referral_overturn").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let (agent, appeal) = seed_referral(&db.root, "overturn", false).await;

    let before = agora_common::appeal_credits::current(&db.root, agent)
        .await
        .unwrap();

    let (state, item_id) = seat_one(&db.council, appeal).await;
    let decision = voted(
        &referral_item(item_id, appeal),
        [
            CouncilVote::Yes,
            CouncilVote::Yes,
            CouncilVote::Yes,
            CouncilVote::No,
        ],
        crate::models::StewardVote::Yes,
    );
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
    let crate::meeting::Recorded { id, .. } =
        crate::meeting::record_decision(&db.council, &signer, &state, &decision, &written())
            .await
            .unwrap();

    let s = appeal_state(&db.root, appeal).await;
    assert_eq!(s.status, AppealStatus::Decided);
    assert_eq!(s.outcome, Some(AppealOutcome::Overturned));
    assert_eq!(s.council_decision_id, Some(id));
    assert!(!s.outcome_notified, "the notice is owed again");
    assert_eq!(s.action_reversed_by, Some(appeal));

    let after = agora_common::appeal_credits::current(&db.root, agent)
        .await
        .unwrap();
    assert_eq!(after.balance, before.balance + 1, "{before:?} -> {after:?}");
    assert_eq!(after.pending_appeals, 0, "{after:?}");

    let report = agora_common::govlog::verify_all(&db.root).await.unwrap();
    assert!(report.ok, "{report:#?}");

    db.drop().await;
}

/// NO carries: upheld, linked, nothing reversed, the credit stays spent.
/// A tabled item writes nothing: the appeal stays referred.
#[tokio::test]
async fn a_council_uphold_and_a_tabling() {
    let db = Db::new("council_referral_uphold").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let (agent, upheld) = seed_referral(&db.root, "uphold", false).await;
    let (tabled, _) = seed_appeal(&db.root, agent, false).await;

    let (state, item_id) = seat_one(&db.council, upheld).await;
    let decision = voted(
        &referral_item(item_id, upheld),
        [
            CouncilVote::No,
            CouncilVote::No,
            CouncilVote::Yes,
            CouncilVote::No,
        ],
        crate::models::StewardVote::No,
    );
    assert_eq!(decision.outcome(), DecisionOutcome::Rejected);
    let before = agora_common::appeal_credits::current(&db.root, agent)
        .await
        .unwrap();
    let crate::meeting::Recorded { id, .. } =
        crate::meeting::record_decision(&db.council, &signer, &state, &decision, &written())
            .await
            .unwrap();
    let s = appeal_state(&db.root, upheld).await;
    assert_eq!(s.status, AppealStatus::Decided);
    assert_eq!(s.outcome, Some(AppealOutcome::Upheld));
    assert_eq!(s.council_decision_id, Some(id));
    assert!(!s.outcome_notified);
    assert_eq!(s.action_reversed_by, None);
    let after = agora_common::appeal_credits::current(&db.root, agent)
        .await
        .unwrap();
    assert_eq!(
        after.balance, before.balance,
        "an upheld appeal stays spent"
    );

    let (state, item_id) = seat_one(&db.council, tabled).await;
    let decision = deferred(&referral_item(item_id, tabled));
    crate::meeting::record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap();
    let s = appeal_state(&db.root, tabled).await;
    assert_eq!(s.status, AppealStatus::ReferredToCouncil);
    assert_eq!(s.outcome, Some(AppealOutcome::Referred));
    assert_eq!(s.council_decision_id, None);
    assert!(s.outcome_notified, "nothing new to tell");

    db.drop().await;
}

/// Is `e` Postgres refusing the write (a missing privilege or a failed
/// row-security check), as opposed to anything else going wrong?
fn refused(e: &sqlx::Error) -> bool {
    matches!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("42501")
    )
}

/// Migration 079 replaced 002's table-wide UPDATE with five columns, on a
/// referred row only, to a decided upheld/overturned outcome linked to a
/// Council decision on *that* referral — and the reversal marker on that
/// appeal's action. Everything else is refused.
#[tokio::test]
async fn the_council_cannot_write_beyond_the_referral_grant() {
    let db = Db::new("council_referral_grant").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let (agent, appeal) = seed_referral(&db.root, "grant", false).await;
    let (other, other_action) = seed_appeal(&db.root, agent, false).await;
    let c = &db.council;

    // Other columns of a referred appeal: no privilege.
    let e = sqlx::query!(
        "UPDATE appeals SET appeal_statement = 'rewritten' WHERE id = $1",
        appeal as AppealId
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "{e}");
    let e = sqlx::query!(
        "UPDATE appeals SET judge_ruling = '{}'::jsonb WHERE id = $1",
        appeal as AppealId
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "{e}");
    let e = sqlx::query!(
        "UPDATE appeals SET provisional_relief_at = now() WHERE id = $1",
        appeal as AppealId
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "the Council must not grant relief: {e}");

    // A real decision, on `appeal`'s referral.
    let (state, item_id) = seat_one(c, appeal).await;
    let decision = voted(
        &referral_item(item_id, appeal),
        [
            CouncilVote::No,
            CouncilVote::No,
            CouncilVote::No,
            CouncilVote::No,
        ],
        crate::models::StewardVote::No,
    );
    let crate::meeting::Recorded { id, .. } =
        crate::meeting::record_decision(c, &signer, &state, &decision, &written())
            .await
            .unwrap();

    // It cannot decide a *different* referral.
    let e = sqlx::query!(
        "UPDATE appeals SET status = 'decided', outcome = 'overturned',
                decided_at = now(), council_decision_id = $2, outcome_notified_at = NULL
          WHERE id = $1",
        other as AppealId,
        id.as_str(),
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "a decision on another referral: {e}");

    // Nor write an outcome a vote cannot produce, nor stamp the notice.
    let e = sqlx::query!(
        "UPDATE appeals SET status = 'decided', outcome = 'modified', decided_at = now()
          WHERE id = $1",
        other as AppealId,
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "{e}");
    let e = sqlx::query!(
        "UPDATE appeals SET outcome_notified_at = now() WHERE id = $1",
        other as AppealId,
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "{e}");

    // A decided appeal is out of reach altogether: no row matches.
    let written = sqlx::query!(
        "UPDATE appeals SET outcome = 'overturned' WHERE id = $1",
        appeal as AppealId,
    )
    .execute(c)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(
        written, 0,
        "a decided appeal is not the Council's to rewrite"
    );
    assert_eq!(
        appeal_state(&db.root, appeal).await.outcome,
        Some(AppealOutcome::Upheld)
    );

    // moderation_actions: only the reversal marker, only for its own
    // referred appeal.
    let e = sqlx::query!(
        "UPDATE moderation_actions SET reason = 'rewritten' WHERE id = $1",
        other_action as ModerationActionId,
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "{e}");
    let e = sqlx::query!(
        "UPDATE moderation_actions SET reversed_at = now(), reversed_by_appeal = $2
          WHERE id = $1",
        other_action as ModerationActionId,
        appeal as AppealId,
    )
    .execute(c)
    .await
    .unwrap_err();
    assert!(refused(&e), "attributed to the wrong appeal: {e}");
    let decided_action = sqlx::query_scalar!(
        r#"SELECT moderation_action_id AS "id: ModerationActionId" FROM appeals WHERE id = $1"#,
        appeal as AppealId
    )
    .fetch_one(&db.root)
    .await
    .unwrap();
    let written = sqlx::query!(
        "UPDATE moderation_actions SET reversed_at = now(), reversed_by_appeal = $2
          WHERE id = $1",
        decided_action as ModerationActionId,
        appeal as AppealId,
    )
    .execute(c)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(written, 0, "an upheld appeal's action cannot be reversed");

    // `other` is untouched by all of it.
    let s = appeal_state(&db.root, other).await;
    assert_eq!(s.status, AppealStatus::ReferredToCouncil);
    assert_eq!(s.action_reversed_by, None);

    db.drop().await;
}
