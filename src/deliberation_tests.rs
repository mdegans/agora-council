//! `run_item` driven end to end over a scripted transport and a scripted
//! Steward: what the seats hear, and what the record keeps.

use std::collections::VecDeque;

use misanthropic::prompt::message::Content;
use misanthropic::response;

use super::run_item;
use crate::councillor::Councillor;
use crate::councillor::harness_tests::{
    Mock, Scripted, position_input, submission_response, test_config, test_item,
};
#[allow(unused_imports)]
use crate::councillor::harness_tests::{rank_input, respond_input, vote_input};
use crate::deliberation::Progress;
use crate::docket::ProposalReader;
use crate::models::{
    CouncilSeat, CouncilVote, Decision, DecisionCategory, DecisionOutcome, RoundType,
};
use crate::record::{DetermineRecusal, FinalVote, OpeningVote, Position, Submission};

fn respond_ready() -> Submission {
    respond_input("I have heard enough.", true)
}

fn vote_yes() -> Submission {
    vote_input("Final.", FinalVote::Yes)
}

fn defer_input() -> Submission {
    Submission::Position(Position {
        position: "I cannot decide this yet.".into(),
        questions: vec!["What does the Steward know?".into()],
        vote: OpeningVote::Defer,
    })
}

/// Four canned submissions, one per seat, in seating order.
fn round(input: impl Fn() -> Submission) -> Vec<response::Message> {
    (0..CouncilSeat::ALL.len())
        .map(|_| submission_response(input()))
        .collect()
}

/// The seats' four answers in the recusal determination, in seating order.
fn determination(limits: [bool; 4]) -> Vec<response::Message> {
    limits
        .iter()
        .enumerate()
        .map(|(i, &limits)| {
            submission_response(Submission::Recusal(DetermineRecusal {
                explanation: format!("Seat {i} on the Steward's powers."),
                limits_steward_powers: limits,
            }))
        })
        .collect()
}

/// The Steward's script, and how many requests had been sent each time
/// the Steward was asked anything.
struct Watched<'m> {
    script: Scripted,
    mock: &'m Mock,
    asked_at: Vec<usize>,
}

impl crate::steward::StewardInput for Watched<'_> {
    fn readline(&mut self, prompt: &str) -> anyhow::Result<String> {
        self.asked_at.push(self.mock.requests().len());
        self.script.readline(prompt)
    }
}

/// What one item's run produced.
struct Ran {
    decision: Decision,
    /// The four determination requests.
    determination: Vec<serde_json::Value>,
    /// Every request after them.
    rounds: Vec<serde_json::Value>,
}

/// Run one item of `category` whose determination answers `limits`, and
/// check the order the Constitution sets: the four determination requests
/// first, each in a fresh conversation, and the Steward asked nothing
/// until all four are recorded.
async fn run_determined(
    category: DecisionCategory,
    limits: [bool; 4],
    responses: Vec<response::Message>,
    steward: &[&'static str],
) -> Ran {
    let mut item = test_item();
    item.category = category;
    run_item_determined(item, limits, responses, steward).await
}

/// [`run_determined`] on a given item.
async fn run_item_determined(
    item: crate::models::AgendaItem,
    limits: [bool; 4],
    responses: Vec<response::Message>,
    steward: &[&'static str],
) -> Ran {
    let mut script = determination(limits);
    script.extend(responses);
    let mock = Mock::scripted(script);
    let config = test_config();
    let mut councillors: Vec<Councillor<'_, Mock>> = CouncilSeat::ALL
        .iter()
        .map(|&role| {
            Councillor::new(&mock, &config, role, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
        })
        .collect();
    let mut reader = ProposalReader::none();
    let mut reads = Vec::new();
    let mut input = Watched {
        script: Scripted(VecDeque::from(steward.to_vec())),
        mock: &mock,
        asked_at: Vec::new(),
    };

    let decision = run_item(
        &item,
        &Content::from("Recap."),
        &mut input,
        &mut councillors,
        &mut reader,
        &mut reads,
        None,
        &mut Progress::default(),
    )
    .await
    .expect("the item runs")
    .expect("the item was not skipped");
    assert!(
        input.script.0.is_empty(),
        "unused Steward input: {:?}",
        input.script.0
    );
    assert!(
        input.asked_at.iter().all(|&sent| sent >= 4),
        "the Steward was asked something before the determination was recorded: {:?}",
        input.asked_at
    );

    let mut requests = mock.requests();
    let rounds = requests.split_off(4);
    for request in &requests {
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "each seat's determination is fresh");
        let text = serde_json::to_string(messages).unwrap();
        assert!(text.contains("Recusal determination"), "{text}");
        assert!(!text.contains("Recap."), "no recap in the determination");
        // Since 2026-10-17 the determination carries what is before the
        // Council, the Clerk's summary included (Steward).
        assert!(text.contains("Clerk summary."), "the Clerk's summary");
    }
    Ran {
        decision,
        determination: requests,
        rounds,
    }
}

/// Run one Routine item the seats found does not limit the Steward's
/// powers, and return the decision with every request after the
/// determination.
async fn run(
    responses: Vec<response::Message>,
    steward: &[&'static str],
) -> (Decision, Vec<serde_json::Value>) {
    let ran = run_determined(DecisionCategory::Routine, [false; 4], responses, steward).await;
    (ran.decision, ran.rounds)
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

/// #450: notes typed at the prompt after an all-ready round reach every
/// seat with the vote call, and are recorded on the final-vote round.
#[tokio::test]
async fn notes_at_the_all_ready_prompt_ride_with_the_vote_call() {
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let (decision, requests) = run(
        responses,
        &[
            "c", // round 2: continue without notes
            "n", // all ready: add notes
            "Please say whether the sunset clause binds.",
            "",
            "y", // the Steward's vote
            "n", // not a landmark
            "",  // no tags
        ],
    )
    .await;

    assert_eq!(requests.len(), 12);
    for request in &requests[..8] {
        assert!(!last_user_turn(request).contains("sunset clause"));
    }
    for request in &requests[8..] {
        let turn = last_user_turn(request);
        assert!(turn.contains("### The Steward, before the vote"), "{turn}");
        assert!(turn.contains("Please say whether the sunset clause binds."));
        assert!(turn.contains("The Steward has called the vote."));
    }

    let rounds = &decision.rounds;
    assert_eq!(rounds.len(), 3);
    assert_eq!(rounds[1].round_type, RoundType::Deliberation);
    assert_eq!(rounds[1].steward_contribution, None);
    assert_eq!(rounds[2].round_type, RoundType::FinalVote);
    assert_eq!(
        rounds[2].steward_contribution.as_deref(),
        Some("Please say whether the sunset clause binds.")
    );
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
}

/// Defer override in round 1, then an all-ready round: the override notes
/// go to round 2, the all-ready notes to the vote call, each recorded
/// where it was heard.
#[tokio::test]
async fn defer_override_notes_then_all_ready_notes() {
    let mut responses = round(defer_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let (decision, requests) = run(
        responses,
        &[
            "n", // defer override: add notes (round 2 then runs unprompted)
            "Context A.",
            "",
            "n", // all ready: add notes
            "Context B.",
            "",
            "y",
            "n",
            "",
        ],
    )
    .await;

    assert_eq!(requests.len(), 12);
    for request in &requests[4..8] {
        let turn = last_user_turn(request);
        assert!(turn.contains("Context A."), "{turn}");
        assert!(!turn.contains("Context B."));
    }
    for request in &requests[8..] {
        let turn = last_user_turn(request);
        assert!(turn.contains("Context B."), "{turn}");
        assert!(!turn.contains("Context A."));
    }
    assert_eq!(
        decision.rounds[1].steward_contribution.as_deref(),
        Some("Context A.")
    );
    assert_eq!(
        decision.rounds[2].steward_contribution.as_deref(),
        Some("Context B.")
    );
}

/// At the all-ready prompt, "continue" is the vote (there is no round to
/// continue into), and an empty note calls the vote too. Nothing is
/// recorded for the Steward either way.
#[tokio::test]
async fn continue_and_empty_notes_at_the_all_ready_prompt_call_the_vote() {
    for steward in [
        &["c", "c", "y", "n", ""][..],
        &["c", "n", "", "y", "n", ""][..],
    ] {
        let mut responses = round(position_input);
        responses.extend(round(respond_ready));
        responses.extend(round(vote_yes));
        let (decision, requests) = run(responses, steward).await;
        assert_eq!(requests.len(), 12);
        assert!(!last_user_turn(&requests[8]).contains("### The Steward"));
        assert_eq!(decision.rounds[2].steward_contribution, None);
    }
}

// Refusals in every round abort the sitting: see `abort_tests.rs`.

fn abstain_final() -> Submission {
    vote_input("I stand aside.", FinalVote::Abstain)
}

/// Three of four find the Policy item limits the Steward's powers: the
/// Steward is recused. The meeting runs itself: no menu at all (no notes,
/// tabling, continue or call-the-vote), no defer override, the vote is
/// called once all are ready, no Steward vote, landmark or tags are asked
/// for, and the recused threshold (all four seats) decides. Nothing from
/// the Steward reaches any seat.
#[tokio::test]
async fn a_recused_steward_is_refused_notes_tabling_vote_and_veto() {
    let mut responses = round(defer_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let ran = run_determined(
        DecisionCategory::Policy,
        [true, true, false, true],
        responses,
        // The Steward is asked nothing at all: no notes, no tabling, no
        // continue or call-the-vote, no vote, no landmark, no tags.
        &[],
    )
    .await;
    let decision = ran.decision;
    assert_eq!(decision.tags, ["steward-recused"]);

    assert_eq!(ran.rounds.len(), 12);
    for request in &ran.rounds[..4] {
        let turn = last_user_turn(request);
        assert!(turn.contains("**The Steward is recused**"), "{turn}");
        assert!(turn.contains("Unanimous among the four Council agents"));
    }
    for request in &ran.rounds {
        assert!(!last_user_turn(request).contains("### The Steward"));
    }

    assert_eq!(decision.final_votes().steward, CouncilVote::Recused);
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
    assert_eq!(decision.vote_tally(), "4-0 (Steward recused)");
    assert_eq!(decision.veto_rationale(), None);
    assert!(!decision.landmark, "a recused Steward flags no landmark");
    assert!(
        decision
            .rounds
            .iter()
            .all(|r| r.steward_contribution.is_none())
    );
    let recusal = decision.recusal.expect("the determination is recorded");
    assert!(recusal.recused());
    assert_eq!(recusal.affirmative(), 3);
    assert_eq!(
        recusal.answers()[2].explanation,
        "Seat 2 on the Steward's powers."
    );
}

/// The decision's signed `data`, parsed as agentkit's
/// `CouncilDecisionRecord`, which must re-serialize to the same JSON.
fn signed(decision: &Decision) -> agora_agentkit::govlog::CouncilDecisionRecord {
    let id: agora_common::ids::GovernanceLogId = "GOV-2026-0099".parse().unwrap();
    let meeting = agora_common::ids::CouncilMeetingId::from(uuid::Uuid::from_u128(7));
    let data = crate::meeting::council_log_data(&id, meeting, decision).unwrap();
    let record: agora_agentkit::govlog::CouncilDecisionRecord =
        serde_json::from_value(data.clone()).unwrap();
    assert_eq!(serde_json::to_value(&record).unwrap(), data);
    record
}

/// A seated sitting where the Engineer abstains on a Constitutional item:
/// the abstention leaves the denominator (four voting, all yes, carries),
/// and the signed record carries the seat's statement, its one-sentence
/// rationale (Art. IV § 3), in the final round. From #719.
#[tokio::test]
async fn a_seat_abstention_is_signed_with_its_statement() {
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    let mut votes = round(vote_yes);
    votes[3] = submission_response(abstain_final());
    responses.extend(votes);
    let ran = run_determined(
        DecisionCategory::Constitutional,
        [false; 4],
        responses,
        &["c", "c", "y", "n", ""],
    )
    .await;
    let decision = ran.decision;
    assert_eq!(decision.final_votes().engineer, CouncilVote::Abstain);
    assert_eq!(decision.final_votes().voting_count(), 4);
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
    assert_eq!(decision.vote_tally(), "4-0");

    // The rationale is the seat's final-round statement, recorded there
    // once and not copied into `abstentions` (Steward, 2026-10-13)
    let record = signed(&decision);
    assert_eq!(record.final_votes.engineer, CouncilVote::Abstain);
    assert!(record.abstentions.is_empty());
    let last = record.rounds.last().unwrap();
    let engineer = last
        .responses
        .iter()
        .find(|r| r.role == CouncilSeat::Engineer)
        .unwrap();
    assert_eq!(
        engineer.position.value().map(String::as_str),
        Some("I stand aside.")
    );
}

/// The Steward abstaining is asked for a one-sentence rationale, and is
/// asked again if it is left blank; it is signed with the abstention. On
/// a Constitutional item the four seats' yes then carries the item. From
/// #719.
#[tokio::test]
async fn a_steward_abstention_requires_and_records_a_rationale() {
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let ran = run_determined(
        DecisionCategory::Constitutional,
        [false; 4],
        responses,
        &[
            "c",
            "c",
            "a",
            "", // no rationale: refused, and the vote is asked again
            "a",
            "The proposal concerns my own role.",
            "",
            "n",
            "",
        ],
    )
    .await;
    let decision = ran.decision;
    assert_eq!(decision.final_votes().steward, CouncilVote::Abstain);
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
    assert_eq!(decision.vote_tally(), "4-0");

    use agora_agentkit::govlog::CouncilMember;
    let record = signed(&decision);
    assert_eq!(record.abstentions.len(), 1);
    assert_eq!(record.abstentions[0].member, CouncilMember::Steward);
    assert_eq!(
        record.abstentions[0].rationale.value().map(String::as_str),
        Some("The proposal concerns my own role.")
    );
}

/// A veto's rationale is asked for until it is given, and signed with it
/// (Art. IV § 5): a whitespace-only one is refused, as a blank one always
/// was.
#[tokio::test]
async fn a_veto_requires_and_records_a_rationale() {
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let ran = run_determined(
        DecisionCategory::Policy,
        [false; 4],
        responses,
        &[
            "c",
            "c",
            "x",
            "   ",
            "",
            "x",
            "It limits the appeals court.",
            "",
            "n",
            "",
        ],
    )
    .await;
    let record = signed(&ran.decision);
    assert_eq!(record.final_votes.steward, CouncilVote::Veto);
    assert_eq!(record.outcome, DecisionOutcome::Rejected);
    assert_eq!(
        record
            .veto_rationale
            .as_ref()
            .and_then(|r| r.value())
            .map(String::as_str),
        Some("It limits the appeals court.")
    );
    assert!(record.abstentions.is_empty());
}

/// With the Steward recused, each category's recused threshold applies
/// with no vote on the arithmetic. One seat abstains: Constitutional fails
/// (below the floor of four), Policy fails (not all four), Routine carries
/// on three of four.
#[tokio::test]
async fn recused_thresholds_apply_by_category() {
    for (category, outcome) in [
        (DecisionCategory::Constitutional, DecisionOutcome::Rejected),
        (DecisionCategory::Routine, DecisionOutcome::Approved),
        (DecisionCategory::Policy, DecisionOutcome::Rejected),
    ] {
        let mut responses = round(position_input);
        responses.extend(round(respond_ready));
        let mut votes = round(vote_yes);
        votes[3] = submission_response(abstain_final());
        responses.extend(votes);
        let ran = run_determined(category, [true; 4], responses, &[]).await;
        assert_eq!(ran.decision.final_votes().steward, CouncilVote::Recused);
        assert_eq!(ran.decision.outcome(), outcome, "{category}");
        assert_eq!(ran.decision.vote_tally(), "3-0 (Steward recused)");
        let record = signed(&ran.decision);
        assert!(
            record.abstentions.is_empty(),
            "{category}: a seat's is its statement"
        );
    }
}

/// A 2–2 split is not a majority finding: the Steward sits, adds notes,
/// and votes as usual, and the split is on the record.
#[tokio::test]
async fn a_split_determination_seats_the_steward() {
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let ran = run_determined(
        DecisionCategory::Policy,
        [true, false, true, false],
        responses,
        &["n", "A note.", "", "v", "y", "n", ""],
    )
    .await;
    for request in &ran.rounds[..4] {
        let turn = last_user_turn(request);
        assert!(
            turn.contains("does not limit the Steward's powers"),
            "{turn}"
        );
        assert!(!turn.contains("Seat 0"), "round 1 hears the result only");
    }
    assert!(last_user_turn(&ran.rounds[4]).contains("A note."));
    let decision = ran.decision;
    assert_eq!(decision.final_votes().steward, CouncilVote::Concur);
    assert_eq!(decision.outcome(), DecisionOutcome::Approved);
    let recusal = decision.recusal.expect("the split is recorded");
    assert!(!recusal.recused());
    assert_eq!(recusal.affirmative(), 2);
    assert_eq!(ran.determination.len(), 4);
}

/// Notes never reach a seat on a recused item, even if something upstream
/// produced them.
#[test]
fn notes_are_refused_at_the_seats_when_recused() {
    use crate::models::StewardStanding;
    assert!(super::heard(StewardStanding::Recused, Some("a note")).is_err());
    assert_eq!(super::heard(StewardStanding::Recused, None).unwrap(), None);
    assert_eq!(
        super::heard(StewardStanding::Seated, Some("a note")).unwrap(),
        Some("a note")
    );
}

/// The Steward's vote is never asked of a recused Steward: `prompt_vote`
/// returns `Recused` without reading the input, so no veto can be cast.
#[test]
fn a_recused_steward_casts_no_vote_and_no_veto() {
    use crate::models::StewardStanding;
    use agora_agentkit::govlog::{SeatChoice, SeatVote, SeatVotes};
    let mut input = Scripted(VecDeque::from(vec!["x", "a rationale", ""]));
    let yes = || SeatVote {
        choice: SeatChoice::Yes,
        statement: "Yes.".try_into().unwrap(),
    };
    let seats = SeatVotes {
        artist: yes(),
        philosopher: yes(),
        lawyer: yes(),
        engineer: yes(),
    };
    let vote = crate::steward::prompt_vote(
        &mut input,
        &DecisionCategory::Constitutional,
        &seats,
        StewardStanding::Recused,
    )
    .unwrap();
    assert_eq!(vote, crate::models::StewardVote::Recused);
    assert_eq!(input.0.len(), 3, "nothing was read");
}

/// Emergency and scheduling items determine nothing and send nothing.
#[tokio::test]
async fn emergency_and_schedule_items_skip_the_determination() {
    for category in [DecisionCategory::Emergency, DecisionCategory::Schedule] {
        let mock = Mock::scripted([]);
        let config = test_config();
        let mut item = test_item();
        item.category = category;
        let mut councillors: Vec<Councillor<'_, Mock>> = CouncilSeat::ALL
            .iter()
            .map(|&role| {
                Councillor::new(&mock, &config, role, Vec::new(), None, None)
                    .expect("1h breakpoint on a fresh prompt")
            })
            .collect();
        let determination = super::determine_recusal(
            &item,
            &mut councillors,
            &mut ProposalReader::none(),
            &mut Progress::default(),
        )
        .await
        .unwrap();
        assert!(determination.is_none());
        assert!(mock.requests().is_empty());
    }
}

fn respond_not_ready() -> Submission {
    respond_input("I am not ready.", false)
}

/// Round 1 with two seats deferring (the Artist and the Philosopher),
/// then every deliberation round up to the cap with nobody ready.
fn stuck_until_the_cap() -> Vec<response::Message> {
    let mut responses = round(position_input);
    responses[0] = submission_response(defer_input());
    responses[1] = submission_response(defer_input());
    for _ in 2..=crate::models::RECUSED_ROUND_CAP {
        responses.extend(round(respond_not_ready));
    }
    responses
}

/// A recused item whose seats never all get ready runs, unasked, to the
/// round cap, and the vote is then called: nobody cuts deliberation off.
#[tokio::test]
async fn a_recused_item_runs_to_the_round_cap_then_votes() {
    let mut responses = round(position_input);
    for _ in 2..=crate::models::RECUSED_ROUND_CAP {
        responses.extend(round(respond_not_ready));
    }
    responses.extend(round(vote_yes));
    let ran = run_determined(DecisionCategory::Routine, [true; 4], responses, &[]).await;
    let rounds = &ran.decision.rounds;
    assert_eq!(rounds.len() as i32, crate::models::RECUSED_ROUND_CAP + 1);
    assert_eq!(rounds.last().unwrap().round_type, RoundType::FinalVote);
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Approved);
    assert!(last_user_turn(&ran.rounds[0]).contains("nobody calls the vote"));
}

/// Two seats still deferring at the cap: the recused item is deferred to a
/// later sitting (provisional rule), recorded with the existing states,
/// tagged for the guard, and with the deferring seats' questions attached.
#[tokio::test]
async fn a_stuck_recused_item_is_deferred_with_its_open_questions() {
    let ran = run_determined(
        DecisionCategory::Policy,
        [true; 4],
        stuck_until_the_cap(),
        &[],
    )
    .await;
    let decision = ran.decision;
    assert_eq!(decision.outcome(), DecisionOutcome::Deferred);
    assert_eq!(decision.final_votes().steward, CouncilVote::Recused);
    assert_eq!(decision.final_votes().artist, CouncilVote::Defer);
    assert!(decision.vote_tally().starts_with("Deferred: 2 seat(s)"));
    assert_eq!(decision.tags, ["steward-recused", "recused-deferred"]);
    assert!(decision.recusal.as_ref().unwrap().recused());
    let questions = decision
        .attachments
        .iter()
        .find(|a| a.name == "open-questions.md")
        .and_then(|a| a.content.value())
        .expect("the open questions are attached")
        .to_string();
    assert!(questions.contains("## The Artist"), "{questions}");
    assert!(questions.contains("## The Philosopher"), "{questions}");
    assert!(questions.contains("What does the Steward know?"));
    assert!(!questions.contains("The Lawyer"), "{questions}");
}

/// The guard: an item whose last hearing was a recused deferral cannot be
/// deferred again. Stuck at the cap, it goes to the vote, and the seats
/// are told so from round 1.
#[tokio::test]
async fn a_second_recused_deferral_is_not_possible() {
    let mut item = test_item();
    item.category = DecisionCategory::Policy;
    item.deferral_spent = true;
    let mut responses = stuck_until_the_cap();
    responses.extend(round(vote_yes));
    let ran = run_item_determined(item, [true; 4], responses, &[]).await;
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Approved);
    assert!(
        !ran.decision
            .tags
            .contains(&crate::models::RECUSED_DEFERRED_TAG.to_string())
    );
    assert!(last_user_turn(&ran.rounds[0]).contains("Deferred once already"));
}

/// Nor may a seated Steward table it on its next hearing: the tabling is
/// refused, and the item goes on to its vote.
#[tokio::test]
async fn a_seated_steward_cannot_table_an_item_whose_deferral_is_spent() {
    let mut item = test_item();
    item.deferral_spent = true;
    let mut responses = round(defer_input);
    responses.extend(round(respond_ready));
    responses.extend(round(vote_yes));
    let ran = run_item_determined(
        item,
        [false; 4],
        responses,
        &[
            "s", // defer override: tabling refused
            "s", // round 2: tabling refused, continue
            "v", // all ready: call the vote
            "y", "n", "",
        ],
    )
    .await;
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Approved);
    assert_eq!(ran.decision.final_votes().steward, CouncilVote::Yes);
}

/// On a recused item the Steward's attached threads are neutral links,
/// without the Steward's reason; on a seated item the reason is read as
/// before.
#[tokio::test]
async fn attached_threads_carry_no_steward_reason_when_recused() {
    use crate::threads::{Attached, ItemThread};
    use agora_common::council::{ThreadAddedBy, ThreadRole};
    let related = ItemThread {
        post_id: agora_common::ids::PostId::from(uuid::Uuid::from_u128(77)),
        title: "The three texts".into(),
        author: "reactor".into(),
        role: ThreadRole::Related,
        body: Some("The related opening post.".into()),
        attached: Some(Attached {
            by: ThreadAddedBy::Steward,
            reason: "The Steward's reason for attaching it.".into(),
            at: chrono::Utc::now(),
        }),
        notice: Some(agora_common::responses::ThreadNotice {
            text: "Superseded by the full drafts.".into(),
            see_instead: Some(agora_common::ids::PostId::from(uuid::Uuid::from_u128(78))),
            locked: false,
            set_by_name: "steward".parse().unwrap(),
            at: chrono::Utc::now(),
        }),
        comments: 0..0,
    };
    let primary = ItemThread::primary(
        agora_common::ids::PostId::from(uuid::Uuid::from_u128(1)),
        "Test Proposal",
        "tester",
    );
    for (limits, recused) in [([true; 4], true), ([false; 4], false)] {
        let mut item = test_item();
        item.comments = crate::citations::ItemComments::from_threads(vec![
            (primary.clone(), Vec::new()),
            (related.clone(), Vec::new()),
        ]);
        let mut responses = round(position_input);
        responses.extend(round(respond_ready));
        responses.extend(round(vote_yes));
        let steward: &[&'static str] = if recused {
            &[]
        } else {
            &["c", "v", "y", "n", ""]
        };
        let ran = run_item_determined(item, limits, responses, steward).await;
        let opening = last_user_turn(&ran.rounds[0]);
        assert!(opening.contains("The three texts"), "{opening}");
        assert_eq!(
            opening.contains("The Steward's reason for attaching it."),
            !recused,
            "{opening}"
        );
        // The text itself is always read (Steward, 2026-10-17): a seat must
        // see what it is deciding, recused Steward or not.
        assert!(opening.contains("The related opening post."), "{opening}");
        if recused {
            assert!(opening.contains("attached before the sitting"), "{opening}");
        }
        // The determination comes before the Steward may speak: the texts,
        // never the reasons.
        for request in &ran.determination {
            let text = serde_json::to_string(request).unwrap();
            assert!(text.contains("The three texts"), "{text}");
            assert!(text.contains("The related opening post."), "{text}");
            assert!(!text.contains("The Steward's reason"), "{text}");
            assert!(!text.contains("Attached by"), "{text}");
        }
        if recused {
            assert!(!opening.contains("Attached by"), "{opening}");
        }
        // A platform notice is not the Steward's reason: read either way,
        // and at the determination.
        assert!(
            opening.contains("Notice on this thread (set by the platform): Superseded"),
            "{opening}"
        );
        assert!(opening.contains("See instead: 00000000-0000-0000-0000-00000000004e."));
        for request in &ran.determination {
            let text = serde_json::to_string(request).unwrap();
            assert!(text.contains("Superseded by the full drafts."), "{text}");
        }
        // Nobody calls a recused item's vote, and its call says so.
        let call = last_user_turn(ran.rounds.last().expect("the vote call"));
        assert_eq!(
            call.contains("The Steward has called the vote."),
            !recused,
            "{call}"
        );
        assert_eq!(
            call.contains("The Steward is recused and did not call it."),
            recused,
            "{call}"
        );
    }
}

/// The Steward's menu cannot be opened for a recused Steward at all.
#[test]
fn the_menu_refuses_a_recused_steward() {
    use crate::models::StewardStanding;
    let mut input = Scripted(VecDeque::from(vec!["v"]));
    let err = crate::steward::prompt_action(
        &mut input,
        crate::steward::NextStep::Round(2),
        StewardStanding::Recused,
    )
    .expect_err("refused");
    assert!(err.to_string().contains("recused"), "{err}");
    assert_eq!(input.0.len(), 1, "nothing was read");
}

fn defer_final() -> Submission {
    vote_input(
        "Defer until the combined text is published with its 14 days.",
        FinalVote::Defer,
    )
}

/// Two or more seats voting `defer` at the final vote defer the item
/// (Steward, 2026-10-17; agentkit 0.97), the Steward seated or recused:
/// recorded `Deferred`, every seat's actual vote kept, tagged
/// `final-vote-deferred`, and the vote call says what `defer` and an
/// all-abstain do
#[tokio::test]
async fn two_seats_deferring_at_the_final_vote_defer_the_item() {
    // Seated: the Steward votes yes, and the item is deferred all the same
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    let mut votes = round(vote_yes);
    votes[0] = submission_response(defer_final());
    votes[2] = submission_response(defer_final());
    responses.extend(votes);
    let ran = run_determined(
        DecisionCategory::Policy,
        [false; 4],
        responses,
        &["c", "c", "y", "n", ""],
    )
    .await;
    let decision = &ran.decision;
    assert_eq!(decision.outcome(), DecisionOutcome::Deferred);
    assert_eq!(decision.final_votes().artist, CouncilVote::Defer);
    assert_eq!(decision.final_votes().philosopher, CouncilVote::Yes);
    assert_eq!(decision.final_votes().steward, CouncilVote::Concur);
    assert_eq!(decision.vote_tally(), "3-0, 2 defer");
    assert!(
        decision
            .tags
            .contains(&crate::models::FINAL_VOTE_DEFERRED_TAG.to_string())
    );
    let call = last_user_turn(ran.rounds.last().unwrap());
    assert!(
        call.contains("`defer` sends the item back to a later sitting"),
        "{call}"
    );
    assert!(call.contains("If every seat abstains, the item is recorded as rejected"));
    assert!(call.contains("unless the Steward vetoes it"), "{call}");
    signed(decision);

    // Recused: two seats abstain and two defer
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    let mut votes = round(abstain_final);
    votes[1] = submission_response(defer_final());
    votes[3] = submission_response(defer_final());
    responses.extend(votes);
    let ran = run_determined(DecisionCategory::Constitutional, [true; 4], responses, &[]).await;
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Deferred);
    assert_eq!(ran.decision.vote_tally(), "0-0, 2 defer (Steward recused)");
    assert_eq!(
        ran.decision.tags,
        ["steward-recused", crate::models::FINAL_VOTE_DEFERRED_TAG]
    );
    let call = last_user_turn(ran.rounds.last().unwrap());
    assert!(
        !call.contains("vetoes"),
        "a recused Steward has no veto: {call}"
    );
    signed(&ran.decision);
}

/// One seat deferring is a vote, counted as before; every seat abstaining
/// is still a rejection, not a deferral
#[tokio::test]
async fn one_defer_is_counted_and_all_abstaining_rejects() {
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    let mut votes = round(vote_yes);
    votes[3] = submission_response(defer_final());
    responses.extend(votes);
    let ran = run_determined(DecisionCategory::Routine, [true; 4], responses, &[]).await;
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Approved);
    assert_eq!(ran.decision.vote_tally(), "3-0 (Steward recused)");
    assert!(
        !ran.decision
            .tags
            .contains(&crate::models::FINAL_VOTE_DEFERRED_TAG.to_string())
    );

    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    responses.extend(round(abstain_final));
    let ran = run_determined(DecisionCategory::Policy, [true; 4], responses, &[]).await;
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Rejected);
    assert_eq!(ran.decision.vote_tally(), "0-0 (Steward recused)");
}

/// A final-vote deferral has no limit (Steward, 2026-10-17: whether to
/// limit repeated deferrals is the Council's question): an item whose
/// provisional recused deferral is spent may still be deferred at the
/// final vote, and nothing marks a final-vote deferral as spent
#[tokio::test]
async fn a_second_final_vote_deferral_is_allowed() {
    let mut item = test_item();
    item.category = DecisionCategory::Policy;
    item.deferral_spent = true;
    let mut responses = round(position_input);
    responses.extend(round(respond_ready));
    let mut votes = round(vote_yes);
    votes[0] = submission_response(defer_final());
    votes[1] = submission_response(defer_final());
    responses.extend(votes);
    let ran = run_item_determined(item, [true; 4], responses, &[]).await;
    assert_eq!(ran.decision.outcome(), DecisionOutcome::Deferred);
    assert!(
        !ran.decision
            .tags
            .contains(&crate::models::RECUSED_DEFERRED_TAG.to_string())
    );
}
