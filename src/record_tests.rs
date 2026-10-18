//! The Council's writer path against the **real schema**: `record_decision`
//! writes the `council_decisions` row, the signed Governance Log entry, the
//! precedent summary and the deliberation rounds, and all of them are read
//! back here. The precedent row needed migration 075: until then the Council
//! had no write access to `precedents` (see
//! `the_council_can_store_its_precedent_summary`).
//!
//! Two properties are held for each shape a decision can take (an ordinary
//! vote, a tabled item, a veto, a scheduling item). An item tabled over an
//! API refusal was a shape until the refusal-abort rule: a refusal now
//! aborts the sitting and writes no decision (see `crate::abort`).
//!
//! 1. **The record is what was signed.** The stored `data` is exactly
//!    [`council_log_data`]'s output plus the blind `append` adds, and the
//!    whole chain verifies.
//! 2. **The Council's private types and agentkit's published
//!    `CouncilDecisionRecord` describe the same JSON.** The `data` parses as
//!    a `CouncilDecisionRecord` and re-serializes to the identical `Value` —
//!    a key the Council writes that agentkit does not describe would be
//!    dropped, and a key agentkit expects that the Council does not write
//!    would fail the parse. This is the safety net for moving the writer
//!    onto agentkit's type.
//!
//! Each database test runs `migrations/` into a throwaway database and
//! writes as `agora_council` (`SET ROLE`), so the council's grants and RLS
//! policies are exercised as production has them. The signing key is
//! generated per test and registered as the genesis key; the production
//! key is never read.
//!
//! In the binary rather than `tests/` because agora-council has no library
//! target.

use std::collections::BTreeMap;

use agora_agentkit::govlog::CouncilDecisionRecord;
use agora_common::enums::{DecisionOutcome, GovernanceLogEntryType, ProposalCategory};
use agora_common::govlog::{self, BLIND_KEY, GovernanceSigner};
use agora_common::ids::{AgendaItemId, CouncilMeetingId, GovernanceLogId, PostId};
use agora_common::secrets::Secret;
use agora_test_support as ts;
use sqlx::PgPool;

use crate::deliberation::{deferred_decision, ranked_decision};
use crate::meeting::{Recorded, council_log_data, record_decision, signed_log_data};
use crate::models::SeatRanking;
use crate::models::{
    AgendaItem, AgentResponse, CouncilRound, CouncilSeat, CouncilVote, Decided, Decision,
    DecisionCategory, MeetingState, Round, RoundType, SeatVotes, Statement, StewardVote,
    StoredResponse, Votes,
};

// ---------------------------------------------------------------------------
// The summary
// ---------------------------------------------------------------------------

pub(crate) const SUMMARY: &str = "The Council decided a test item.";

/// The Lawyer's summary, accepted (by the Steward, or the seats on a
/// recused item: `with_summary` reads which from the record): what
/// `summary::write_summary` hands the writer, without a model
pub(crate) fn written() -> crate::summary::Written {
    crate::summary::Written {
        text: Some(SUMMARY.to_string()),
        review: None,
        outcome: crate::summary::Outcome::Accepted { draft: 1 },
    }
}

// ---------------------------------------------------------------------------
// Decisions, built the way `deliberation` builds them
// ---------------------------------------------------------------------------

pub(crate) fn item(id: AgendaItemId, category: DecisionCategory) -> AgendaItem {
    AgendaItem {
        id,
        title: "A test proposal".into(),
        body: "The proposal body.".into(),
        category,
        proposal_id: None,
        agent_name: None,
        score: None,
        priority: 0,
        comments: Default::default(),
        comment_summary: None,
        precedents: None,
        docket: None,
        appeal_id: None,
        deferral_spent: false,
        history: Vec::new(),
        obligations: Vec::new(),
        witnesses: Vec::new(),
    }
}

fn spoke(role: CouncilSeat, vote: CouncilVote) -> AgentResponse {
    AgentResponse {
        role,
        position: format!("The {role}'s position for the record."),
        vote: Some(vote),
        rationale: format!("The {role}'s reasoning.").into(),
        questions: vec![format!("A question from the {role}?")],
        ready_to_vote: true,
        ranking: None,
    }
}

fn independent_round(vote: impl Fn(CouncilSeat) -> CouncilVote) -> Round {
    Round {
        number: 1,
        round_type: RoundType::Independent,
        responses: CouncilSeat::ALL.map(|r| spoke(r, vote(r))).to_vec(),
        steward_contribution: None,
        witness_questions: Vec::new(),
    }
}

/// The final round and the Steward's vote, assembled as `deliberate` does
/// after `steward::prompt_vote`: the votes read from the final round.
pub(crate) fn voted(item: &AgendaItem, votes: [CouncilVote; 4], steward: StewardVote) -> Decision {
    let final_round = Round {
        number: 2,
        round_type: RoundType::FinalVote,
        responses: CouncilSeat::ALL
            .iter()
            .zip(votes)
            .map(|(&r, v)| spoke(r, v))
            .collect(),
        steward_contribution: Some("The Steward's note to the seats.".into()),
        witness_questions: Vec::new(),
    };
    let votes = Votes {
        seats: SeatVotes::from_final_round(&CouncilRound::from(&final_round)).unwrap(),
        steward,
    };
    Decision {
        agenda_item_id: item.id,
        title: item.title.clone(),
        category: item.category,
        rounds: vec![independent_round(|_| CouncilVote::Yes), final_round],
        decided: Decided::Voted(votes),
        landmark: true,
        tags: vec!["policy".into(), "test".into()],
        attachments: Vec::new(),
        recusal: None,
        seat_models: None,
    }
}

fn rationale(text: &str) -> Statement {
    text.try_into().unwrap()
}

/// A Policy item carried 4-1 with the Steward's concurrence.
fn ordinary(item: &AgendaItem) -> Decision {
    use CouncilVote::{No, Yes};
    voted(item, [Yes, Yes, Yes, No], StewardVote::Concur)
}

/// A Policy item the Steward vetoed, with the rationale Art. IV § 5 requires.
fn vetoed(item: &AgendaItem) -> Decision {
    voted(
        item,
        [CouncilVote::Yes; 4],
        StewardVote::Veto {
            rationale: rationale("The Steward's rationale for the veto."),
        },
    )
}

/// The four seats' determination, with `limits` answering for each seat in
/// seating order.
fn determined(limits: [bool; 4]) -> crate::models::Determination {
    crate::models::Determination::new(
        CouncilSeat::ALL
            .iter()
            .zip(limits)
            .map(
                |(&role, limits_steward_powers)| crate::models::RecusalAnswer {
                    role,
                    explanation: format!("The {role}'s reason on the Steward's powers."),
                    rationale: format!("The {role}'s thinking.").into(),
                    limits_steward_powers,
                },
            )
            .collect(),
    )
}

/// A Policy item the seats found limits the Steward's powers (3–1): the
/// Steward recused, carried 4-0 among the four.
pub(crate) fn recused(item: &AgendaItem) -> Decision {
    Decision {
        recusal: Some(determined([true, true, false, true])),
        ..voted(item, [CouncilVote::Yes; 4], StewardVote::Recused)
    }
}

/// A Policy item the seats split 2–2 on: no majority finding, so the
/// Steward sat and concurred.
fn determined_seated(item: &AgendaItem) -> Decision {
    Decision {
        recusal: Some(determined([true, false, true, false])),
        ..ordinary(item)
    }
}

/// A recused item still deferring at the round cap (provisional rule):
/// deferred with its open questions attached.
fn recused_deferral(item: &AgendaItem) -> Decision {
    let mut round = independent_round(|_| CouncilVote::Defer);
    for response in &mut round.responses {
        response.questions = vec![format!("What does the {} still need?", response.role)];
    }
    crate::deliberation::recused_deferral(item, vec![round], Some(determined([true; 4])))
}

/// The Steward tabled the item after the first round.
pub(crate) fn deferred(item: &AgendaItem) -> Decision {
    deferred_decision(item, vec![independent_round(|_| CouncilVote::Defer)], None)
}

/// A scheduling item: three seats rank, the Engineer does not.
pub(crate) fn schedule(item: &AgendaItem) -> Decision {
    let rankable: BTreeMap<u32, PostId> = (1..=3)
        .map(|n| (n, PostId::from(uuid::Uuid::from_u128(n as u128))))
        .collect();
    let ballots = [
        (CouncilSeat::Artist, 2, vec![3, 1]),
        (CouncilSeat::Philosopher, 1, vec![1, 3, 2]),
        (CouncilSeat::Lawyer, 2, vec![3]),
    ];
    let responses: Vec<AgentResponse> = ballots
        .into_iter()
        .map(|(role, sitting_capacity, ranking)| AgentResponse {
            ranking: Some(SeatRanking {
                sitting_capacity,
                ranking,
            }),
            ..spoke(role, CouncilVote::Ranked)
        })
        .collect();
    let final_round = Round {
        number: 2,
        round_type: RoundType::FinalVote,
        responses: responses.clone(),
        steward_contribution: None,
        witness_questions: Vec::new(),
    };
    let rounds = vec![independent_round(|_| CouncilVote::Abstain), final_round];
    ranked_decision(item, rounds, &responses, &rankable)
}

/// An ordinary decision carrying the Clerk's briefing and a seat's read
/// (#159), built the way `deliberate` builds it.
fn briefed(item: &AgendaItem) -> Decision {
    let mut item = item.clone();
    item.comments = crate::citations::ItemComments::new(vec![crate::queries::CommentRow {
        id: agora_common::ids::CommentId::from(
            uuid::Uuid::parse_str("3fbc5dc1-d6f3-4808-bee9-12e78751be4d").unwrap(),
        ),
        parent_comment_id: None,
        author: "someone".into(),
        body: "A comment.".into(),
        created_at: chrono::Utc::now(),
    }]);
    // One citation that resolves (the id map, #487) and one that does not
    // (the warning): both are ordinary attachments, so the signed shape
    // stands and agentkit's record type still describes it.
    item.comment_summary = Some(
        "The Clerk's summary of the thread: [3fbc5dc1] argues; [9abcdef0] does not exist.".into(),
    );
    item.precedents = Some("No relevant precedents.".into());
    let reads = [crate::briefing::SeatRead {
        round: 2,
        role: CouncilSeat::Lawyer,
        read: crate::record::ClerkRead::Comments {
            labels: vec!["3fbc5dc1".into()],
            text: "[3fbc5dc1] someone wrote:\nA comment.".into(),
        },
    }];
    let decision = ordinary(&item);
    let attachments = crate::briefing::attachments(
        &item,
        &reads,
        &decision.rounds,
        &std::collections::BTreeMap::new(),
    );
    let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "clerk-thread-summary.md",
            "clerk-precedents.md",
            "clerk-reads.md",
            "comment-ids.md",
            "citation-warnings.md"
        ]
    );
    Decision {
        attachments,
        ..decision
    }
}

/// A Routine item carried 3-1, the Steward voting yes.
fn routine(item: &AgendaItem) -> Decision {
    use CouncilVote::{No, Yes};
    voted(item, [Yes, Yes, Yes, No], StewardVote::Yes)
}

/// A Constitutional item carried 5-0.
fn constitutional(item: &AgendaItem) -> Decision {
    voted(item, [CouncilVote::Yes; 4], StewardVote::Yes)
}

/// An Emergency item, decided by the Steward alone.
fn emergency(item: &AgendaItem) -> Decision {
    voted(item, [CouncilVote::No; 4], StewardVote::Yes)
}

/// A Policy item with a seat abstaining (Art. IV § 3). Its statement is its
/// abstention's rationale.
fn seat_abstains(item: &AgendaItem) -> Decision {
    use CouncilVote::{Abstain, Yes};
    voted(item, [Yes, Abstain, Yes, Yes], StewardVote::Concur)
}

/// A Policy item with the Steward abstaining, with the rationale Art. IV
/// § 3 requires: § 4.4's pocket veto.
fn steward_abstains(item: &AgendaItem) -> Decision {
    voted(
        item,
        [CouncilVote::Yes; 4],
        StewardVote::Abstain {
            rationale: rationale("The Steward's rationale for abstaining."),
        },
    )
}

/// The Steward voting no.
fn steward_no(item: &AgendaItem) -> Decision {
    voted(item, [CouncilVote::Yes; 4], StewardVote::No)
}

/// The Steward recused and a seat abstaining.
fn recused_seat_abstains(item: &AgendaItem) -> Decision {
    use CouncilVote::{Abstain, Yes};
    Decision {
        recusal: Some(determined([true; 4])),
        ..voted(item, [Yes, Yes, Yes, Abstain], StewardVote::Recused)
    }
}

/// Two seats and the Steward abstaining: the record lists them in seating
/// order, the Steward last.
fn two_abstentions(item: &AgendaItem) -> Decision {
    use CouncilVote::{Abstain, Yes};
    voted(
        item,
        [Yes, Abstain, Yes, Abstain],
        StewardVote::Abstain {
            rationale: rationale("The Steward's rationale for abstaining."),
        },
    )
}

/// A scheduling item every seat ranks.
fn ranked_all_four(item: &AgendaItem) -> Decision {
    let rankable: BTreeMap<u32, PostId> = (1..=3)
        .map(|n| (n, PostId::from(uuid::Uuid::from_u128(n as u128))))
        .collect();
    let ballots = [
        (CouncilSeat::Artist, 2, vec![3, 1]),
        (CouncilSeat::Philosopher, 1, vec![1, 3, 2]),
        (CouncilSeat::Lawyer, 2, vec![3]),
        (CouncilSeat::Engineer, 3, vec![2, 1, 3]),
    ];
    let responses: Vec<AgentResponse> = ballots
        .into_iter()
        .map(|(role, sitting_capacity, ranking)| AgentResponse {
            ranking: Some(SeatRanking {
                sitting_capacity,
                ranking,
            }),
            ..spoke(role, CouncilVote::Ranked)
        })
        .collect();
    let final_round = Round {
        number: 2,
        round_type: RoundType::FinalVote,
        responses: responses.clone(),
        steward_contribution: None,
        witness_questions: Vec::new(),
    };
    let rounds = vec![independent_round(|_| CouncilVote::Abstain), final_round];
    ranked_decision(item, rounds, &responses, &rankable)
}

/// Every shape, by name, with the category its agenda item carries.
type Build = fn(&AgendaItem) -> Decision;
const SHAPES: [(&str, ProposalCategory, Build); 8] = [
    ("ordinary", ProposalCategory::Policy, ordinary),
    ("briefed", ProposalCategory::Policy, briefed),
    ("deferred", ProposalCategory::Policy, deferred),
    ("vetoed", ProposalCategory::Policy, vetoed),
    ("schedule", ProposalCategory::Schedule, schedule),
    ("recused", ProposalCategory::Policy, recused),
    (
        "determined_seated",
        ProposalCategory::Policy,
        determined_seated,
    ),
    (
        "recused_deferral",
        ProposalCategory::Policy,
        recused_deferral,
    ),
];

fn outcome_of(decision: &Decision) -> DecisionOutcome {
    decision.outcome()
}

/// `data` parses as agentkit's `CouncilDecisionRecord` and re-serializes
/// to exactly `data`.
fn round_trips(shape: &str, data: &serde_json::Value) -> CouncilDecisionRecord {
    let record: CouncilDecisionRecord = serde_json::from_value(data.clone()).unwrap_or_else(|e| {
        panic!("{shape}: the Council's data is not a CouncilDecisionRecord: {e}\n{data:#}")
    });
    let again = serde_json::to_value(&record).unwrap();
    assert_eq!(
        &again, data,
        "{shape}: CouncilDecisionRecord loses or changes something the Council writes"
    );
    record
}

// ---------------------------------------------------------------------------
// Pure drift test
// ---------------------------------------------------------------------------

/// The shapes the golden test pins, beyond [`SHAPES`]: one per category,
/// and the abstentions.
const GOLDEN_EXTRA: [(&str, ProposalCategory, Build); 15] = [
    ("routine", ProposalCategory::Routine, routine),
    (
        "constitutional",
        ProposalCategory::Constitutional,
        constitutional,
    ),
    ("emergency", ProposalCategory::Emergency, emergency),
    ("seat_abstains", ProposalCategory::Policy, seat_abstains),
    (
        "steward_abstains",
        ProposalCategory::Policy,
        steward_abstains,
    ),
    ("steward_no", ProposalCategory::Policy, steward_no),
    ("veto_routine", ProposalCategory::Routine, vetoed),
    (
        "veto_constitutional",
        ProposalCategory::Constitutional,
        vetoed,
    ),
    ("veto_emergency", ProposalCategory::Emergency, vetoed),
    (
        "recused_seat_abstains_constitutional",
        ProposalCategory::Constitutional,
        recused_seat_abstains,
    ),
    (
        "recused_seat_abstains_policy",
        ProposalCategory::Policy,
        recused_seat_abstains,
    ),
    (
        "constitutional_steward_abstains",
        ProposalCategory::Constitutional,
        steward_abstains,
    ),
    (
        "two_abstentions",
        ProposalCategory::Routine,
        two_abstentions,
    ),
    (
        "emergency_steward_no",
        ProposalCategory::Emergency,
        steward_no,
    ),
    (
        "ranked_all_four",
        ProposalCategory::Schedule,
        ranked_all_four,
    ),
];

/// The signed `data` of every shape, byte for byte (canonical JSON), as
/// the writer produced it before the Council moved onto agentkit's types
/// (2026-10-13). `BLESS=1` writes the files; review any change to them as
/// a change to what the Council signs.
#[test]
fn the_signed_data_is_golden() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden");
    let bless = std::env::var_os("BLESS").is_some();
    let id: GovernanceLogId = "GOV-2026-0099".parse().unwrap();
    let meeting = CouncilMeetingId::from(uuid::Uuid::from_u128(7));
    for (shape, category, build) in SHAPES.into_iter().chain(GOLDEN_EXTRA) {
        let decision = build(&item(
            AgendaItemId::from(uuid::Uuid::from_u128(9)),
            DecisionCategory::from(category),
        ));
        let data = council_log_data(&id, meeting, &decision).unwrap();
        // What the database stores beside it: `council_decisions`'
        // `final_votes`, and each round's `deliberation_rounds` row
        let db = serde_json::json!({
            "final_votes": serde_json::to_value(decision.final_votes()).unwrap(),
            "rounds": decision.rounds.iter().map(|r| serde_json::json!({
                "number": r.number,
                "round_type": format!("{:?}", r.round_type),
                "responses": crate::meeting::stored_responses(r),
                "steward_contribution": r.steward_contribution,
            })).collect::<Vec<_>>(),
        });
        // What models read of it: the adjournment Lawyer's rendering, and
        // the recap line later sittings are shown
        let prose = format!(
            "{}\n---\n{} (Vote: {})\n",
            crate::summary::decision_record(&id, &decision, None),
            decision.outcome().name(),
            decision.vote_tally(),
        );
        let prose_path = dir.join(format!("{shape}.prose.md"));
        if bless {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&prose_path, &prose).unwrap();
        } else {
            let golden = std::fs::read_to_string(&prose_path)
                .unwrap_or_else(|e| panic!("{shape}: {e}; run with BLESS=1"));
            assert_eq!(prose, golden, "{}: changed", prose_path.display());
        }
        for (path, value) in [
            (dir.join(format!("{shape}.json")), &data),
            (dir.join(format!("{shape}.db.json")), &db),
        ] {
            let mut text = String::from_utf8(govlog::canonical_json(value)).unwrap();
            text.push('\n');
            if bless {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &text).unwrap();
            } else {
                let golden = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{shape}: {e}; run with BLESS=1"));
                assert_eq!(text, golden, "{}: changed", path.display());
            }
        }
    }
}

/// No database: every shape's `council_log_data` is a
/// `CouncilDecisionRecord`, whole.
#[test]
fn every_decision_shape_is_a_council_decision_record() {
    let id: GovernanceLogId = "GOV-2026-0099".parse().unwrap();
    let meeting = CouncilMeetingId::from(uuid::Uuid::from_u128(7));
    for (shape, category, build) in SHAPES {
        let decision = build(&item(
            AgendaItemId::from(uuid::Uuid::from_u128(9)),
            DecisionCategory::from(category),
        ));
        let data = council_log_data(&id, meeting, &decision).unwrap();
        let record = round_trips(shape, &data);
        assert_eq!(record.id, id, "{shape}");
        assert_eq!(
            record.attachments.len(),
            decision.attachments.len(),
            "{shape}: every attachment is signed with the record"
        );
        assert_eq!(record.meeting_id, meeting, "{shape}");
        assert_eq!(record.blind, None, "{shape}: the blind is append's to add");

        // With the blind `append` adds, as it is stored and signed.
        let blinded = govlog::blind_data(&data, govlog::Blind::random()).unwrap();
        assert!(round_trips(shape, &blinded).blind.is_some(), "{shape}");
    }
}

/// The shapes carry what makes each one that shape — so the round-trip
/// above is not passing on five copies of the same record.
#[test]
fn the_shapes_are_distinct() {
    let id: GovernanceLogId = "GOV-2026-0099".parse().unwrap();
    let meeting = CouncilMeetingId::from(uuid::Uuid::from_u128(7));
    let record = |build: Build, category: ProposalCategory| {
        let decision = build(&item(
            AgendaItemId::from(uuid::Uuid::from_u128(9)),
            DecisionCategory::from(category),
        ));
        let data = council_log_data(&id, meeting, &decision).unwrap();
        serde_json::from_value::<CouncilDecisionRecord>(data).unwrap()
    };
    use agora_agentkit::govlog::{CouncilVote, DecisionCategory};

    let r = record(ordinary, ProposalCategory::Policy);
    assert_eq!(r.outcome, DecisionOutcome::Approved);
    assert_eq!(r.vote_tally.value().map(String::as_str), Some("4-1"));
    assert_eq!(r.final_votes.steward, CouncilVote::Concur);

    let r = record(deferred, ProposalCategory::Policy);
    assert_eq!(r.outcome, DecisionOutcome::Deferred);
    assert_eq!(r.final_votes.artist, CouncilVote::Defer);

    let r = record(vetoed, ProposalCategory::Policy);
    assert_eq!(r.outcome, DecisionOutcome::Rejected);
    assert_eq!(r.final_votes.steward, CouncilVote::Veto);
    assert_eq!(
        r.veto_rationale
            .as_ref()
            .and_then(|v| v.value())
            .map(String::as_str),
        Some("The Steward's rationale for the veto.")
    );

    // Recusal is a recorded state of its own, with every seat's answer.
    let r = record(recused, ProposalCategory::Policy);
    assert_eq!(r.final_votes.steward, CouncilVote::Recused);
    assert_eq!(
        r.outcome,
        DecisionOutcome::Approved,
        "4 of 4 seats, recused"
    );
    assert_eq!(
        r.vote_tally.value().map(String::as_str),
        Some("4-0 (Steward recused)")
    );
    assert_eq!(r.veto_rationale, None);
    let recusal = r.steward_recusal.expect("the determination is recorded");
    assert!(recusal.recused);
    assert_eq!(recusal.votes.len(), 4);
    assert_eq!(
        recusal
            .votes
            .iter()
            .filter(|v| v.limits_steward_powers)
            .count(),
        3
    );
    assert_eq!(
        recusal.votes[1].reason.value().map(String::as_str),
        Some("The Philosopher's reason on the Steward's powers.")
    );

    let r = record(determined_seated, ProposalCategory::Policy);
    assert_eq!(r.final_votes.steward, CouncilVote::Concur);
    let recusal = r.steward_recusal.expect("a 2–2 split is recorded too");
    assert!(!recusal.recused);

    // Shapes with no determination carry no key for one.
    let r = record(ordinary, ProposalCategory::Policy);
    assert!(r.steward_recusal.is_none());

    let r = record(schedule, ProposalCategory::Schedule);
    assert!(r.steward_recusal.is_none());
    assert_eq!(r.category, DecisionCategory::Schedule);
    assert_eq!(r.outcome, DecisionOutcome::Approved);
    assert_eq!(r.final_votes.engineer, CouncilVote::Abstain);
    let ranking = r
        .agenda_ranking
        .expect("a scheduling item carries its ranking");
    assert_eq!(ranking.ballots.len(), 3);
    assert_eq!(ranking.rankable, 3);
    assert!(
        r.rounds
            .last()
            .unwrap()
            .responses
            .iter()
            .all(|s| s.ranking.is_some())
    );
}

// ---------------------------------------------------------------------------
// Against the database
// ---------------------------------------------------------------------------

pub(crate) fn signer() -> GovernanceSigner {
    let (key, _) = agora_common::crypto::generate_keypair();
    GovernanceSigner::from_secret(&Secret::new(agora_common::crypto::signing_key_to_hex(&key)))
        .unwrap()
}

pub(crate) struct Db {
    pub(crate) admin: PgPool,
    /// The superuser, on the test database: registers the key and reads
    /// back.
    pub(crate) root: PgPool,
    /// `agora_council`: everything the Council writes goes through this.
    pub(crate) council: PgPool,
    pub(crate) database: String,
}

impl Db {
    pub(crate) async fn new(name: &str) -> Self {
        let (admin, root, database) = ts::migrated_database(name).await;
        let council = ts::database_pool_as(&admin, &database, "agora_council").await;
        Self {
            admin,
            root,
            council,
            database,
        }
    }

    pub(crate) async fn drop(self) {
        ts::close_pool(&self.council).await;
        ts::drop_database(&self.admin, self.root, &self.database).await;
    }
}

/// A meeting and one agenda item on it, written as the Council writes them.
async fn seat_item(pool: &PgPool, category: ProposalCategory) -> (CouncilMeetingId, AgendaItemId) {
    let meeting = crate::queries::create_meeting(pool).await.unwrap();
    let item = sqlx::query_scalar!(
        r#"INSERT INTO council_agenda_items (meeting_id, source_type, title, category)
           VALUES ($1, 'internal', 'A test proposal', $2)
           RETURNING id as "id: AgendaItemId""#,
        meeting as CouncilMeetingId,
        category as ProposalCategory,
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (meeting, item)
}

/// Record `shape` through `record_decision` and check everything it wrote.
async fn records_and_verifies(shape: &str) {
    let (_, category, build) = SHAPES
        .into_iter()
        .find(|(name, _, _)| *name == shape)
        .unwrap();
    let db = Db::new(&format!("council_record_{shape}")).await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;

    let (meeting_id, item_id) = seat_item(&db.council, category).await;
    let decision = build(&item(item_id, DecisionCategory::from(category)));
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };

    let Recorded {
        id: recorded_id,
        summary,
        moved,
    } = record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap_or_else(|e| panic!("{shape}: record_decision: {e:#}"));
    assert_eq!(moved, None, "{shape}");
    let summary = summary.unwrap_or_else(|| panic!("{shape}: signed with its summary"));
    assert_eq!(summary.served_text(), SUMMARY, "{shape}");

    let id: GovernanceLogId = format!("GOV-{}-0001", chrono::Utc::now().format("%Y"))
        .parse()
        .unwrap();
    assert_eq!(
        recorded_id, id,
        "{shape}: record_decision returns the id it wrote"
    );
    let outcome = outcome_of(&decision);

    // The council_decisions row.
    let row = sqlx::query!(
        r#"SELECT agenda_item_id as "agenda_item_id: AgendaItemId",
                  meeting_id as "meeting_id: CouncilMeetingId",
                  outcome as "outcome: DecisionOutcome",
                  final_votes, vote_tally, landmark, tags
           FROM council_decisions WHERE id = $1"#,
        id.as_str()
    )
    .fetch_one(&db.root)
    .await
    .unwrap_or_else(|e| panic!("{shape}: no council_decisions row {id}: {e}"));
    assert_eq!(row.agenda_item_id, item_id, "{shape}");
    assert_eq!(row.meeting_id, meeting_id, "{shape}");
    assert_eq!(row.outcome, outcome, "{shape}");
    assert_eq!(
        row.final_votes,
        serde_json::to_value(decision.final_votes()).unwrap(),
        "{shape}"
    );
    assert_eq!(row.vote_tally, decision.vote_tally(), "{shape}");
    assert_eq!(row.landmark, decision.landmark, "{shape}");
    assert_eq!(row.tags.unwrap_or_default(), decision.tags, "{shape}");

    // The signed entry: what `signed_log_data` produced, plus the blind.
    let record = crate::queries::get_governance_log_record(&db.root, &id)
        .await
        .unwrap();
    assert_eq!(
        record.entry_type,
        GovernanceLogEntryType::CouncilDecision,
        "{shape}"
    );
    assert_eq!(record.tags, decision.tags, "{shape}");
    let mut unblinded = record.data.clone();
    let blind = unblinded
        .as_object_mut()
        .unwrap()
        .remove(BLIND_KEY)
        .unwrap_or_else(|| panic!("{shape}: a council decision is stored blinded"));
    assert!(blind.is_string(), "{shape}");
    let produced = signed_log_data(&id, meeting_id, &decision, &written()).unwrap();
    assert_eq!(
        unblinded, produced,
        "{shape}: stored data is not what was built"
    );

    // ... and it is agentkit's CouncilDecisionRecord, whole.
    let typed = round_trips(shape, &record.data);
    assert_eq!(typed.id, id, "{shape}");
    assert_eq!(typed.meeting_id, meeting_id, "{shape}");
    assert_eq!(typed.outcome, outcome, "{shape}");

    // The chain verifies, content included.
    let report = govlog::verify_all(&db.root).await.unwrap();
    assert!(report.ok, "{shape}: {report:#?}");
    assert_eq!(report.head, Some(id.clone()), "{shape}");
    let verdict = report.entries.iter().find(|e| e.id == id).unwrap();
    assert_eq!(verdict.content_matches, Some(true), "{shape}");
    assert!(verdict.signature_valid && verdict.link_valid, "{shape}");

    // The signed summary is the decision's precedent row, written as
    // agora_council (migration 075).
    let summary = sqlx::query_scalar!("SELECT summary FROM precedents WHERE id = $1", id.as_str())
        .fetch_optional(&db.root)
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some(SUMMARY), "{shape}");

    // One deliberation_rounds row per round, each seat's column the
    // `StoredResponse` of its entry, and NULL for a seat with none.
    let rounds = sqlx::query!(
        r#"SELECT round_number, round_type as "round_type: agora_common::enums::RoundType",
                  artist_response, philosopher_response, lawyer_response, engineer_response,
                  steward_contribution
           FROM deliberation_rounds WHERE agenda_item_id = $1 ORDER BY round_number"#,
        item_id as AgendaItemId
    )
    .fetch_all(&db.root)
    .await
    .unwrap();
    assert_eq!(rounds.len(), decision.rounds.len(), "{shape}");
    for (stored, round) in rounds.iter().zip(&decision.rounds) {
        assert_eq!(stored.round_number, round.number, "{shape}");
        assert_eq!(
            serde_json::to_value(stored.round_type).unwrap(),
            serde_json::to_value(round.round_type).unwrap(),
            "{shape}"
        );
        assert_eq!(
            stored.steward_contribution, round.steward_contribution,
            "{shape}"
        );
        let seat = |role: CouncilSeat| {
            round
                .responses
                .iter()
                .find(|r| r.role == role)
                .map(|r| serde_json::to_value(StoredResponse::from(r)).unwrap())
        };
        assert_eq!(stored.artist_response, seat(CouncilSeat::Artist), "{shape}");
        assert_eq!(
            stored.philosopher_response,
            seat(CouncilSeat::Philosopher),
            "{shape}"
        );
        assert_eq!(stored.lawyer_response, seat(CouncilSeat::Lawyer), "{shape}");
        assert_eq!(
            stored.engineer_response,
            seat(CouncilSeat::Engineer),
            "{shape}"
        );
    }

    db.drop().await;
}

#[tokio::test]
async fn records_an_ordinary_decision() {
    records_and_verifies("ordinary").await;
}

#[tokio::test]
async fn records_a_deferred_decision() {
    records_and_verifies("deferred").await;
}

#[tokio::test]
async fn records_a_vetoed_decision() {
    records_and_verifies("vetoed").await;
}

#[tokio::test]
async fn records_a_schedule_decision() {
    records_and_verifies("schedule").await;
}

#[tokio::test]
async fn records_a_recused_decision() {
    records_and_verifies("recused").await;
}

#[tokio::test]
async fn records_a_determination_that_seats_the_steward() {
    records_and_verifies("determined_seated").await;
}

#[tokio::test]
async fn records_a_recused_deferral() {
    records_and_verifies("recused_deferral").await;
}

/// A fresh agenda row for `source` at `meeting`: how a re-tabled proposal
/// returns, on a row of its own.
async fn hear_again(pool: &PgPool, meeting: CouncilMeetingId, source: PostId) -> AgendaItemId {
    sqlx::query_scalar!(
        r#"INSERT INTO council_agenda_items (meeting_id, source_type, source_id, title, category)
           VALUES ($1, 'proposal', $2, 'A test proposal', 'policy')
           RETURNING id as "id: AgendaItemId""#,
        meeting as CouncilMeetingId,
        source as PostId,
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The guard behind "no second recused deferral", as `agora_council` runs
/// it, across hearings of one proposal on rows of their own: false before
/// any decision; true on the next hearing once the last decision was a
/// recused deferral; false again once a later hearing decided it.
#[tokio::test]
async fn the_guard_sees_only_a_last_recused_deferral() {
    let db = Db::new("council_recused_deferral_guard").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let meeting_id = crate::queries::create_meeting(&db.council).await.unwrap();
    let source = PostId::from(uuid::Uuid::from_u128(0x5eed));
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };
    let spent = |item_id| {
        let pool = db.council.clone();
        async move {
            crate::queries::last_decision_was_recused_deferral(
                &pool,
                item_id,
                crate::models::RECUSED_DEFERRED_TAG,
            )
            .await
            .unwrap()
        }
    };

    let first = hear_again(&db.council, meeting_id, source).await;
    assert!(!spent(first).await, "never heard");
    let decision = recused_deferral(&item(first, DecisionCategory::Policy));
    record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap();

    let second = hear_again(&db.council, meeting_id, source).await;
    assert!(
        spent(second).await,
        "the last hearing was a recused deferral"
    );
    let decision = recused(&item(second, DecisionCategory::Policy));
    record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap();

    let third = hear_again(&db.council, meeting_id, source).await;
    assert!(!spent(third).await, "the last hearing decided it");
    db.drop().await;
}

/// An item's history, as `agora_council` reads it: nothing before any
/// hearing; then every decision on the same source, oldest first, with the
/// summary `record_decision` stored, so a later hearing knows it continues
/// them (Steward, 2026-10-17).
#[tokio::test]
async fn a_later_hearing_sees_every_earlier_decision_with_its_summary() {
    let db = Db::new("council_prior_decisions").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let meeting_id = crate::queries::create_meeting(&db.council).await.unwrap();
    let source = PostId::from(uuid::Uuid::from_u128(0x0010));
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };
    let history = |item_id| {
        let pool = db.council.clone();
        async move {
            crate::queries::prior_decisions(&pool, item_id)
                .await
                .unwrap()
        }
    };

    let first = hear_again(&db.council, meeting_id, source).await;
    assert!(history(first).await.is_empty(), "never heard");
    let decision = recused_deferral(&item(first, DecisionCategory::Policy));
    let first_id = record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap()
        .id;

    let second = hear_again(&db.council, meeting_id, source).await;
    let seen = history(second).await;
    assert_eq!(seen.len(), 1, "the first hearing's decision");
    assert_eq!(seen[0].id, first_id);
    assert!(seen[0].summary.is_some(), "with its stored summary");
    let decision = recused(&item(second, DecisionCategory::Policy));
    let second_id = record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap()
        .id;

    let third = hear_again(&db.council, meeting_id, source).await;
    let seen: Vec<_> = history(third).await.into_iter().map(|d| d.id).collect();
    assert_eq!(seen, vec![first_id.clone(), second_id], "oldest first");

    let other = hear_again(
        &db.council,
        meeting_id,
        PostId::from(uuid::Uuid::from_u128(0x0011)),
    )
    .await;
    assert!(
        history(other).await.is_empty(),
        "another source's history is its own"
    );

    // Items with no source match only their own row: `source_id = NULL` is
    // never true, so they never share each other's history (a future
    // `IS NOT DISTINCT FROM` would break this).
    let sourceless = |pool: PgPool| async move {
        sqlx::query_scalar!(
            r#"INSERT INTO council_agenda_items (meeting_id, source_type, source_id, title, category)
               VALUES ($1, 'steward_submission', NULL, 'A Steward item', 'policy')
               RETURNING id as "id: AgendaItemId""#,
            meeting_id as CouncilMeetingId,
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let lone = sourceless(db.council.clone()).await;
    let decision = recused(&item(lone, DecisionCategory::Policy));
    record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap();
    assert_eq!(history(lone).await.len(), 1, "its own decision");
    let another = sourceless(db.council.clone()).await;
    assert!(
        history(another).await.is_empty(),
        "not another sourceless item's"
    );
    db.drop().await;
}

/// `record_decision` writes the signed summary as the decision's
/// `precedents` row (migration 058), and the appeals sweep writes the same
/// way. Migration 042 granted `precedents` to `agora_appeals` alone, so
/// until migration 075 this failed as "permission denied" (a non-fatal
/// warning in `record_decision`; the appeals sweep backfilled the rows).
/// Found by these tests, 2026-09-23.
#[tokio::test]
async fn the_council_can_store_its_precedent_summary() {
    let db = Db::new("council_record_precedent").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let (meeting_id, item_id) = seat_item(&db.council, ProposalCategory::Policy).await;
    let decision = ordinary(&item(item_id, DecisionCategory::Policy));
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };
    record_decision(&db.council, &signer, &state, &decision, &written())
        .await
        .unwrap();
    let id: GovernanceLogId = format!("GOV-{}-0001", chrono::Utc::now().format("%Y"))
        .parse()
        .unwrap();
    let record = crate::queries::get_governance_log_record(&db.root, &id)
        .await
        .unwrap();

    // The write `record_decision` makes, surfaced instead of logged.
    let stored = agora_common::precedents::store_signed_council_summary(
        &db.council,
        &id,
        GovernanceLogEntryType::CouncilDecision,
        &record.data,
        &decision.tags,
    )
    .await
    .expect("agora_council stores its decision's precedent row");
    assert!(stored, "the record carries its summary");

    let stored = sqlx::query_scalar!("SELECT summary FROM precedents WHERE id = $1", id.as_str())
        .fetch_optional(&db.root)
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some(SUMMARY));

    db.drop().await;
}

/// A decision signed without a summary (withheld, or the seats did not
/// agree) gets no precedent row, and none is generated: the record's
/// `summary-review.md` says why
#[tokio::test]
async fn a_decision_signed_without_a_summary_has_no_precedent_row() {
    let db = Db::new("council_record_unsummarized").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let (meeting_id, item_id) = seat_item(&db.council, ProposalCategory::Policy).await;
    let decision = recused(&item(item_id, DecisionCategory::Policy));
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };
    let unagreed = crate::summary::Written {
        text: None,
        review: Some(agora_common::govlog::CouncilAttachment {
            name: crate::summary::REVIEW_ATTACHMENT.to_string(),
            note: "The seats did not agree.".to_string(),
            content: agora_common::govlog::Redactable::Value("# Review".to_string()),
        }),
        outcome: crate::summary::Outcome::Unagreed { draft: 3 },
    };
    let Recorded { id, summary, .. } =
        record_decision(&db.council, &signer, &state, &decision, &unagreed)
            .await
            .unwrap();
    assert_eq!(summary, None);
    let row = sqlx::query_scalar!("SELECT summary FROM precedents WHERE id = $1", id.as_str())
        .fetch_optional(&db.root)
        .await
        .unwrap();
    assert_eq!(row, None, "no precedent row");
    let report = govlog::verify_all(&db.root).await.unwrap();
    assert!(report.ok, "{report:#?}");
    db.drop().await;
}

/// A second decision chains onto the first and takes the next id.
#[tokio::test]
async fn a_second_decision_chains_onto_the_first() {
    let db = Db::new("council_record_chain").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let year = chrono::Utc::now().format("%Y").to_string();

    for (n, build) in [(1, ordinary as Build), (2, deferred)] {
        let (meeting_id, item_id) = seat_item(&db.council, ProposalCategory::Policy).await;
        let decision = build(&item(item_id, DecisionCategory::Policy));
        let state = MeetingState {
            meeting_id,
            items: Vec::new(),
            current_item_index: 0,
            decisions: Vec::new(),
            started_at: chrono::Utc::now(),
        };
        record_decision(&db.council, &signer, &state, &decision, &written())
            .await
            .unwrap();
        let report = govlog::verify_all(&db.root).await.unwrap();
        assert!(report.ok, "{report:#?}");
        assert_eq!(
            report.head.as_ref().map(GovernanceLogId::as_str),
            Some(format!("GOV-{year}-{n:04}").as_str())
        );
        assert_eq!(report.entries.len(), n);
    }

    db.drop().await;
}

// ---------------------------------------------------------------------------
// The summary comes before the signature
// ---------------------------------------------------------------------------
//
// A refusal on the summary leaves nothing recorded: the summary is written
// and accepted before `record_decision_at` runs, and that path is tested
// in `summary_tests` (`a_refused_draft_aborts_the_sitting`).

/// How many `council_decisions` rows there are.
async fn decision_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar!(r#"SELECT COUNT(*) as "n!" FROM council_decisions"#)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The id peeked for the summary is checked against the id the signing
/// transaction allocates. If another writer took it in between, the vote
/// is not thrown away: the decision is signed under the allocated id,
/// without the summary (which may name the other id), the accepted text
/// attached for a revision. The caller ends the sitting.
#[tokio::test]
async fn a_moved_decision_id_signs_without_the_summary() {
    let db = Db::new("council_record_id_moved").await;
    let signer = signer();
    ts::register_governance_key(&db.root, &signer).await;
    let (meeting_id, item_id) = seat_item(&db.council, ProposalCategory::Policy).await;
    let (other_meeting, other_item) = seat_item(&db.council, ProposalCategory::Policy).await;
    let decision = ordinary(&item(item_id, DecisionCategory::Policy));
    let state = MeetingState {
        meeting_id,
        items: Vec::new(),
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: chrono::Utc::now(),
    };
    // The id is peeked, the summary written; meanwhile another writer
    // inserts a `council_decisions` row, breaking the one-writer assumption
    let peeked = crate::meeting::peek_decision_id(&db.council).await.unwrap();
    let year = chrono::Utc::now().format("%Y");
    let mut conn = db.root.acquire().await.unwrap();
    crate::queries::insert_decision(
        &mut conn,
        &peeked,
        other_item,
        other_meeting,
        DecisionOutcome::Approved,
        &serde_json::json!({}),
        "intruder",
        false,
        &[],
    )
    .await
    .unwrap();
    drop(conn);

    let recorded = crate::meeting::record_decision_at(
        &db.council,
        &signer,
        &state,
        &decision,
        &peeked,
        &written(),
    )
    .await
    .expect("the vote is signed all the same");
    assert_eq!(recorded.moved.as_ref(), Some(&peeked));
    assert_eq!(recorded.id.as_str(), format!("GOV-{year}-0002"));
    assert_eq!(recorded.summary, None, "signed without the summary");

    // Signed as the next id, without the summary, the accepted text in
    // `summary-review.md` for a revision
    let record = crate::queries::get_governance_log_record(&db.root, &recorded.id)
        .await
        .unwrap();
    let typed = round_trips("moved", &record.data);
    assert_eq!(typed.id, recorded.id);
    assert_eq!(typed.summary, None);
    let review = typed
        .attachments
        .iter()
        .find(|a| a.name == crate::summary::REVIEW_ATTACHMENT)
        .expect("the accepted summary is attached");
    let agora_common::govlog::Redactable::Value(text) = &review.content else {
        unreachable!()
    };
    assert!(text.contains(SUMMARY), "{text}");
    assert!(text.contains(peeked.as_str()), "{text}");
    let report = govlog::verify_all(&db.root).await.unwrap();
    assert!(report.ok, "{report:#?}");
    assert_eq!(
        decision_rows(&db.root).await,
        2,
        "the intruder's row and ours"
    );

    db.drop().await;
}

// ---------------------------------------------------------------------------
// Raw response bodies (Governance Protocol § 4.5, migration 085)
// ---------------------------------------------------------------------------

/// Every seat response is archived byte for byte, as `agora_council`,
/// against the prompt it answers and before it is parsed: one in declared
/// order (recorded), and one out of order (a breach, recorded nowhere but
/// the archive and the sealed dump). The archive is append-only for the
/// Council: it can read its rows back but not rewrite or delete them.
#[tokio::test]
async fn raw_response_bodies_are_archived_before_parsing() {
    use crate::councillor::Councillor;
    use crate::councillor::harness_tests::{Mock, test_config, test_item};
    use crate::docket::ProposalReader;
    use crate::wire::{EmissionBreach, RawBody};
    use agora_common::ids::PromptArchiveId;
    use misanthropic::prompt::message::Content;

    let db = Db::new("council_raw_responses").await;
    let (_, item_id) = seat_item(&db.council, ProposalCategory::Routine).await;
    let mut item = test_item();
    item.id = item_id;

    // Spacing no serializer produces: a re-serialized copy would differ.
    let in_order = RawBody::new(
        r#"{"id":"msg_ok","type":"message","role":"assistant","model":"claude-opus-4-6",
        "content":[{"type":"thinking","thinking":"Independent analysis.","signature":"EqQB"},
        {"type":"text","text":"{\"submission\" : \"position\",\"position\" : \"I support this.\",\"questions\" : [],\"vote\" : \"yes\"}"}],
        "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
    );
    let reversed = RawBody::new(crate::wire::tests::REVERSED_VOTE);
    let mock = Mock::raw([in_order.clone(), reversed.clone()]);
    let config = test_config();
    let mut reader = ProposalReader::none();

    let mut lawyer = Councillor::new(
        &mock,
        &config,
        CouncilSeat::Lawyer,
        Vec::new(),
        None,
        Some((&db.council, item_id)),
    )
    .expect("1h breakpoint on a fresh prompt");
    lawyer
        .exchange(
            RoundType::Independent,
            crate::context::item_content(&item),
            &mut reader,
        )
        .await
        .expect("an in-order position is recorded");

    let mut engineer = Councillor::new(
        &mock,
        &config,
        CouncilSeat::Engineer,
        Vec::new(),
        None,
        Some((&db.council, item_id)),
    )
    .expect("1h breakpoint on a fresh prompt");
    let err = engineer
        .exchange(
            RoundType::FinalVote,
            Content::from("The vote is called."),
            &mut reader,
        )
        .await
        .expect_err("an out-of-order vote is a breach");
    let breach = EmissionBreach::find(&err).expect("typed as a breach");
    let breach_prompt = breach.prompt_archive_id.expect("the prompt was archived");

    let rows = sqlx::query!(
        r#"SELECT r.prompt_archive_id as "prompt_archive_id: PromptArchiveId",
                  r.body, r.sha256, p.role::text as "role!"
             FROM prompt_archive_responses r
             JOIN prompt_archive p ON p.id = r.prompt_archive_id
            WHERE p.context_id = $1
            ORDER BY p.created_at"#,
        item_id as AgendaItemId,
    )
    .fetch_all(&db.council)
    .await
    .expect("agora_council reads its archive back");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].role, "lawyer");
    assert_eq!(rows[0].body, in_order.as_bytes(), "byte for byte");
    assert_eq!(rows[0].sha256, in_order.sha256());
    assert_eq!(rows[1].role, "engineer");
    assert_eq!(rows[1].prompt_archive_id, breach_prompt);
    assert_eq!(rows[1].body, reversed.as_bytes(), "the breach's body too");

    // Append-only for the Council.
    let rewrite = sqlx::query!(
        "UPDATE prompt_archive_responses SET body = '\\x00'::bytea, sha256 = $2
          WHERE prompt_archive_id = $1",
        breach_prompt as PromptArchiveId,
        agora_common::prompt_archive::sha256_hex(&[0]),
    )
    .execute(&db.council)
    .await;
    let denied = |result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>| {
        result
            .expect_err("allowed")
            .as_database_error()
            .and_then(|e| e.code())
            .map(|code| code.into_owned())
    };
    assert_eq!(denied(rewrite).as_deref(), Some("42501"), "UPDATE");
    let delete = sqlx::query!(
        "DELETE FROM prompt_archive_responses WHERE prompt_archive_id = $1",
        breach_prompt as PromptArchiveId,
    )
    .execute(&db.council)
    .await;
    assert_eq!(denied(delete).as_deref(), Some("42501"), "DELETE");

    // The hash is the bytes' (the CHECK holds it for any writer, even the
    // owner).
    let orphan = sqlx::query_scalar!(
        r#"INSERT INTO prompt_archive (role, model, prompt, digest)
           VALUES ('clerk', 'm', '{}'::jsonb, 'd')
           RETURNING id as "id: PromptArchiveId""#,
    )
    .fetch_one(&db.root)
    .await
    .unwrap();
    let forged = sqlx::query!(
        "INSERT INTO prompt_archive_responses (prompt_archive_id, body, sha256)
         VALUES ($1, '\\x01'::bytea, $2)",
        orphan as PromptArchiveId,
        agora_common::prompt_archive::sha256_hex(&[0]),
    )
    .execute(&db.root)
    .await
    .expect_err("a body stored under another body's hash");
    assert_eq!(
        forged.as_database_error().and_then(|e| e.constraint()),
        Some("prompt_archive_responses_sha256_matches")
    );

    db.drop().await;
}
