#![allow(dead_code)] // Model types used as the module grows

use agora_common::enums::Standing;
use agora_common::ids::{AgendaItemId, CouncilMeetingId, GovernanceLogId, PostId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use agora_agentkit::govlog::{
    AgendaRanking, Ballot, CouncilDecisionRecord, CouncilRound, Decided, FinalVotes, Rationale,
    SeatRanking, SeatResponse, SeatVotes, Statement, StewardVote, Votes,
};

pub use agora_agentkit::govlog::CouncilSeat;

pub use agora_agentkit::govlog::CouncilVote;

pub use agora_common::enums::{DecisionOutcome, RoundType};

/// A single agent's response in a deliberation round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub role: CouncilSeat,
    pub position: String,
    /// Always `Some` for a turn this program records. It is an `Option`
    /// because a turn that drew a refusal from the API was once recorded
    /// here as a fact with no vote (Steward, 2026-09-23); since the
    /// refusal-abort rule (superseding GOV-2026-0011's abstain condition,
    /// pending the Council's approval of the amendment) a refusal aborts
    /// the sitting and is never recorded in a round. The key stays
    /// optional so the stored shape agentkit describes is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote: Option<CouncilVote>,
    /// Why the seat said it: the turn's thinking, or on rounds recorded
    /// before 2026-10 the seat's `reasoning` field
    pub rationale: Rationale,
    pub questions: Vec<String>,
    pub ready_to_vote: bool,
    /// The seat's ranking, on a scheduling item's final round only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking: Option<SeatRanking>,
    // There was a `raw_text` here: "raw response text from the model", from
    // when replies were parsed out of free text. Since councillors submit
    // through a tool it had been a byte-for-byte copy of `rationale` — 76 of
    // 76 responses from GOV-2026-0003 on, 35-40% of each signed entry — and
    // a second place for every word a lawful redaction has to find
    // (agora#351). Removed 2026-09-18. Entries already signed keep theirs;
    // serde ignores the extra key when they are read back.
}

impl AgentResponse {
    /// The vote as shown to people and prompts: the vote, or that there
    /// was none (only possible on an entry recorded before the
    /// refusal-abort rule).
    pub fn vote_label(&self) -> String {
        match &self.vote {
            Some(vote) => vote.to_string(),
            None => "none (the API returned a refusal)".to_string(),
        }
    }
}

/// One councillor's response as `deliberation_rounds.{role}_response`
/// stores it. A type rather than a `json!` blob so the stored shape has one
/// declaration (see `memory/feedback_no_json_macro.md`); field order is the
/// stored key order.
#[derive(Debug, Serialize)]
pub struct StoredResponse<'a> {
    pub position: &'a str,
    /// Absent for a turn that drew a refusal — see [`AgentResponse::vote`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vote: Option<String>,
    pub rationale: &'a Rationale,
}

impl<'a> From<&'a AgentResponse> for StoredResponse<'a> {
    fn from(r: &'a AgentResponse) -> Self {
        Self {
            position: &r.position,
            vote: r.vote.as_ref().map(CouncilVote::to_string),
            rationale: &r.rationale,
        }
    }
}

/// A complete deliberation round with all agent responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Round {
    pub number: i32,
    pub round_type: RoundType,
    pub responses: Vec<AgentResponse>,
    pub steward_contribution: Option<String>,
    /// The questions seats put to witnesses this round, in the order asked
    #[serde(default)]
    pub witness_questions: Vec<agora_common::govlog::WitnessQuestion>,
}

/// A round as the record signs it. The council's own [`Round`] is the
/// round being deliberated, which nothing has redacted; agentkit's
/// [`CouncilRound`] is the signed one, whose text a later amendment may
/// redact. This is the one place the first becomes the second.
impl From<&Round> for CouncilRound {
    fn from(round: &Round) -> Self {
        Self {
            number: u32::try_from(round.number).expect("rounds are numbered from 1"),
            round_type: round.round_type,
            responses: round.responses.iter().map(SeatResponse::from).collect(),
            steward_contribution: round.steward_contribution.clone().map(Into::into),
            steward_emergency: None,
            tabled_by_recused_steward: false,
            witness_questions: round.witness_questions.clone(),
        }
    }
}

/// A seat's turn as the record signs it; see [`CouncilRound`]'s `From`
impl From<&AgentResponse> for SeatResponse {
    fn from(r: &AgentResponse) -> Self {
        Self {
            role: r.role,
            position: r.position.clone().into(),
            vote: r.vote,
            rationale: r.rationale.clone().into(),
            questions: r
                .questions
                .iter()
                .cloned()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into(),
            ready_to_vote: r.ready_to_vote,
            ranking: r.ranking.clone(),
            raw_text: None,
        }
    }
}

pub use agora_agentkit::govlog::DecisionCategory;

/// Whether the Steward sits on an item. Every Steward prompt in the
/// deliberation loop takes it, and refuses what a recused Steward may not
/// do (Constitution Art. IV § 3, GOV-2026-0009: "no deliberation, no vote,
/// and no veto on that item").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StewardStanding {
    /// The Steward deliberates, votes and holds the veto as usual.
    Seated,
    /// The four seats determined the item limits the Steward's powers.
    Recused,
}

/// One seat's answer to whether an item limits the Steward's powers,
/// given in its isolated determination conversation before any round.
#[derive(Debug, Clone, PartialEq)]
pub struct RecusalAnswer {
    pub role: CouncilSeat,
    /// The seat's explanation for the record. On the record as `reason`.
    pub explanation: String,
    /// The thinking the seat did before it answered
    pub rationale: Rationale,
    pub limits_steward_powers: bool,
}

/// The four seats' determination, recorded before any round opens
/// (Constitution Art. IV § 3: "determined on the record at the start of
/// deliberation by simple majority of the four Council agents").
#[derive(Debug, Clone, PartialEq)]
pub struct Determination {
    answers: Vec<RecusalAnswer>,
}

impl Determination {
    /// The determination from the seats' answers. Every seat answers: a
    /// refusal aborts the sitting before a determination exists.
    pub fn new(answers: Vec<RecusalAnswer>) -> Self {
        Self { answers }
    }

    pub fn answers(&self) -> &[RecusalAnswer] {
        &self.answers
    }

    /// How many seats found the item limits the Steward's powers.
    pub fn affirmative(&self) -> usize {
        self.answers
            .iter()
            .filter(|a| a.limits_steward_powers)
            .count()
    }

    /// A simple majority *of the four*: three or more. A 2–2 split is not
    /// a majority finding that the item limits the Steward's powers, so
    /// the Steward is not recused. Counted against the four seats, not
    /// against those who answered.
    pub fn recused(&self) -> bool {
        self.affirmative() * 2 > CouncilSeat::ALL.len()
    }

    pub fn standing(&self) -> StewardStanding {
        if self.recused() {
            StewardStanding::Recused
        } else {
            StewardStanding::Seated
        }
    }
}

/// The determination as the Governance Log records it (agentkit's
/// `steward_recusal`)
impl From<&Determination> for agora_agentkit::govlog::StewardRecusal {
    fn from(d: &Determination) -> Self {
        use agora_agentkit::govlog::RecusalVote;
        Self {
            votes: d
                .answers
                .iter()
                .map(|a| RecusalVote {
                    seat: a.role,
                    reason: a.explanation.clone().into(),
                    rationale: Some(a.rationale.clone().into()),
                    limits_steward_powers: a.limits_steward_powers,
                })
                .collect(),
            recused: d.recused(),
        }
    }
}

/// One past Council decision as the Council reads it: the cached
/// precedent summary, plus what any later amendment says about it.
///
/// The Council sees decisions whatever their standing — it is writing the
/// record, not deciding a case under it — so the label is the whole
/// safeguard. Nothing on the moderation path uses this type; Tier 2,
/// jurors and the judge are served only what is in force (Steward,
/// 2026-08-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentDecision {
    pub id: GovernanceLogId,
    /// The anonymized precedent summary.
    pub summary: String,
    pub standing: Standing,
    /// The amending entry's `note`, when an amendment is what changed the
    /// standing. `None` for an entry retired by an older `overrules`
    /// projection, which carries no note.
    pub standing_note: Option<String>,
}

/// What a reader is told when a decision is no longer in force and the
/// amendment left no note of its own.
const UNLABELLED_STANDING: &str = "A later governance entry changed this decision's standing. Do not rely \
     on it as authority for what it once held.";

impl RecentDecision {
    /// The line this decision takes in a prompt.
    ///
    /// A decision that is not in force leads with that fact — before the
    /// summary, in the reader's own vocabulary, so it cannot be skimmed
    /// past. Same shape as `agora_common::precedents::render`'s banner,
    /// for the same reason.
    pub fn prompt_line(&self) -> String {
        match self.standing {
            Standing::InForce => format!("- **{}**: {}", self.id, self.summary.trim()),
            standing => format!(
                "- **{id}** — **NOT IN FORCE ({standing}).** {note}\n  {summary}",
                id = self.id,
                note = self
                    .standing_note
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(UNLABELLED_STANDING),
                summary = self.summary.trim(),
            ),
        }
    }
}

/// An agenda item ready for deliberation.
#[derive(Debug, Clone)]
pub struct AgendaItem {
    pub id: AgendaItemId,
    pub title: String,
    pub body: String,
    pub category: DecisionCategory,
    /// The proposal this item deliberates, when it came from one (the
    /// other agenda sources — referrals, Steward submissions — carry none).
    pub proposal_id: Option<PostId>,
    pub agent_name: Option<String>,
    pub score: Option<i32>,
    pub priority: i32,
    /// The proposal's live comment thread, labelled by short id (read by
    /// the Clerk; served verbatim by `ask_clerk`).
    pub comments: crate::citations::ItemComments,
    /// The Clerk's summary of the comment thread, if any.
    pub comment_summary: Option<misanthropic::prompt::message::Content>,
    /// The Clerk's note on relevant past decisions, if any.
    pub precedents: Option<misanthropic::prompt::message::Content>,
    /// On a scheduling item: the proposals the Council may rank.
    pub docket: Option<crate::docket::Docket>,
    /// On an appeal referral: the appeal it decides (agora#471). The item
    /// and its vote call then carry [`crate::referral::MOTION`].
    pub appeal_id: Option<agora_common::ids::AppealId>,
    /// The item's last hearing ended in a recused deferral (the seats
    /// still deferring at the round cap with the Steward recused). It may
    /// not be deferred again: it goes to the final vote, and the Steward
    /// may not table it (Steward, 2026-10-04).
    pub deferral_spent: bool,
    /// The decisions already taken on this item's source, oldest first:
    /// a hearing that continues one opens with them (Steward, 2026-10-17),
    /// at the recusal determination and in round 1 alike.
    pub history: Vec<crate::queries::PriorDecisionRow>,
    /// The open obligations this hearing owes or settles: those a decision
    /// in [`Self::history`] created, and those due at the next sitting
    /// after one ([`crate::context::obligations_due`]). Listed with the
    /// earlier decisions.
    pub obligations: Vec<agora_common::obligations::Obligation>,
    /// The witnesses called on this item (`--witnesses`), described by the
    /// Clerk (`crate::witness::describe`)
    pub witnesses: Vec<agora_common::govlog::Witness>,
}

/// The tags a recused deferral, and every decision with the Steward
/// recused, carry. The Steward is not asked for tags on such an item (a tag
/// is a say on the record).
pub use agora_common::govlog::{RECUSED_DEFERRED_TAG, STEWARD_RECUSED_TAG};

/// The tag a decision deferred at the final vote carries: two or more
/// seats voted `defer` (agentkit 0.97). Not a spent deferral: a
/// final-vote deferral may be followed by another (Steward, 2026-10-17:
/// whether to limit repeated deferrals is the Council's question), so
/// unlike [`RECUSED_DEFERRED_TAG`] nothing reads it as one.
pub const FINAL_VOTE_DEFERRED_TAG: &str = "final-vote-deferred";

/// On an item the Steward is recused from, the last round the seats
/// deliberate before the vote is called without them all being ready:
/// round 1 and four deliberation rounds. Nobody calls the vote on a
/// recused item; this cap ends it (Steward, 2026-10-04).
pub const RECUSED_ROUND_CAP: i32 = 5;

/// The outcome of deliberation on an agenda item.
#[derive(Debug, Clone)]
pub struct Decision {
    pub agenda_item_id: AgendaItemId,
    pub title: String,
    pub category: DecisionCategory,
    pub rounds: Vec<Round>,
    /// How it was decided: the one value the record's `final_votes`,
    /// `outcome`, `vote_tally`, `veto_rationale`, `abstentions` and
    /// `agenda_ranking` all come from (agentkit [`Decided`]), so a veto
    /// cannot be recorded without its rationale (Art. IV § 5) nor an
    /// abstention without its own (Art. IV § 3).
    pub decided: Decided,
    pub landmark: bool,
    pub tags: Vec<String>,
    /// What the seats were shown beyond the proposal; see
    /// [`crate::briefing`]. Filled in by `deliberate` once the item ends.
    pub attachments: Vec<agora_common::govlog::CouncilAttachment>,
    /// The seats' recusal determination, made before round 1 (Art. IV
    /// § 3). `None` on a category that does not determine one.
    pub recusal: Option<Determination>,
    /// The model each seat ran on. Filled in by `deliberate` once the item
    /// ends.
    pub seat_models: Option<agora_agentkit::govlog::SeatModels>,
}

impl Decision {
    /// The five votes as the record holds them
    pub fn final_votes(&self) -> FinalVotes {
        self.decided.final_votes()
    }

    pub fn outcome(&self) -> DecisionOutcome {
        self.decided.outcome(self.category)
    }

    /// The record's `vote_tally`
    pub fn vote_tally(&self) -> String {
        self.decided.tally()
    }

    /// The Steward's rationale, when the Steward vetoed
    pub fn veto_rationale(&self) -> Option<&Statement> {
        match &self.decided {
            Decided::Voted(votes) => votes.veto_rationale(),
            Decided::Ranked(_) | Decided::Tabled | Decided::RecusedDeferral { .. } => None,
        }
    }

    /// The Steward's rationale, when the Steward abstained (Art. IV § 3)
    pub fn steward_abstention_rationale(&self) -> Option<&Statement> {
        match &self.decided {
            Decided::Voted(Votes {
                steward: StewardVote::Abstain { rationale },
                ..
            }) => Some(rationale),
            Decided::Voted(_)
            | Decided::Ranked(_)
            | Decided::Tabled
            | Decided::RecusedDeferral { .. } => None,
        }
    }

    /// On a scheduling item, the aggregated ranking
    pub fn agenda_ranking(&self) -> Option<&AgendaRanking> {
        match &self.decided {
            Decided::Ranked(ranking) => Some(ranking),
            Decided::Voted(_) | Decided::Tabled | Decided::RecusedDeferral { .. } => None,
        }
    }

    /// The signed record of this decision, as the governance log entry
    /// `id` of sitting `meeting_id` holds it (before `append` adds its
    /// blind)
    pub fn record(
        &self,
        id: &GovernanceLogId,
        meeting_id: CouncilMeetingId,
    ) -> CouncilDecisionRecord {
        CouncilDecisionRecord {
            id: id.clone(),
            meeting_id,
            category: self.category,
            title: self.title.clone().into(),
            rounds: self.rounds.iter().map(CouncilRound::from).collect(),
            // Set from `decided` below
            final_votes: self.final_votes(),
            outcome: self.outcome(),
            vote_tally: String::new().into(),
            landmark: self.landmark,
            veto_rationale: None,
            agenda_ranking: None,
            steward_recusal: self.recusal.as_ref().map(Into::into),
            abstentions: Vec::new(),
            overrules: Vec::new(),
            attachments: self.attachments.clone(),
            seat_models: self.seat_models.clone(),
            // The Lawyer's, accepted before signing: `with_summary`
            // (`meeting::signed_log_data`)
            summary: None,
            blind: None,
        }
        .decided(self.decided.clone())
    }
}

/// State of a meeting in progress.
pub struct MeetingState {
    pub meeting_id: CouncilMeetingId,
    pub items: Vec<AgendaItem>,
    pub current_item_index: usize,
    pub decisions: Vec<Decision>,
    pub started_at: DateTime<Utc>,
}

/// Actions the Steward can take during deliberation.
#[derive(Debug)]
pub enum StewardAction {
    /// Add notes for the next round.
    AddNotes(String),
    /// Skip to the next item (defer this one).
    Skip,
    /// Call for a final vote.
    CallVote,
    /// Continue deliberation (no notes).
    Continue,
}

/// The seats' recusal determination (Constitution Art. IV § 3,
/// GOV-2026-0009): a simple majority of the four, recorded with every
/// seat's answer. The thresholds it puts into effect are agentkit's
/// (`FinalVotes::outcome`), tested there.
#[cfg(test)]
mod determination_tests {
    use super::*;

    fn determination(answers: [bool; 4]) -> Determination {
        Determination::new(
            CouncilSeat::ALL
                .iter()
                .zip(answers)
                .map(|(&role, limits)| RecusalAnswer {
                    role,
                    explanation: format!("The {role} reasons."),
                    rationale: format!("The {role} thinks.").into(),
                    limits_steward_powers: limits,
                })
                .collect(),
        )
    }

    /// Simple majority of the four: three or four recuse; a 2–2 split is
    /// not a majority finding, so the Steward sits.
    #[test]
    fn the_determination_is_a_simple_majority_of_the_four() {
        for (answers, recused) in [
            ([true, true, true, true], true),
            ([true, true, true, false], true),
            ([false, true, true, true], true),
            ([true, true, false, false], false),
            ([false, true, false, true], false),
            ([true, false, false, false], false),
            ([false, false, false, false], false),
        ] {
            let d = determination(answers);
            assert_eq!(d.recused(), recused, "{answers:?}");
            assert_eq!(
                d.standing(),
                if recused {
                    StewardStanding::Recused
                } else {
                    StewardStanding::Seated
                }
            );
        }
    }

    /// The determination as signed: every seat's reason and answer, and
    /// the result, in agentkit's `steward_recusal` shape.
    #[test]
    fn the_determination_records_every_seat() {
        let record =
            agora_agentkit::govlog::StewardRecusal::from(&determination([true, true, false, true]));
        assert!(record.recused);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["votes"].as_array().unwrap().len(), 4);
        assert_eq!(value["votes"][2]["seat"], "lawyer");
        assert_eq!(value["votes"][2]["reason"], "The Lawyer reasons.");
        assert_eq!(value["votes"][2]["limits_steward_powers"], false);
        assert_eq!(value["recused"], true);
    }

    #[test]
    fn only_voted_categories_determine_recusal() {
        for category in [
            DecisionCategory::Routine,
            DecisionCategory::Policy,
            DecisionCategory::Constitutional,
        ] {
            assert!(category.determines_recusal());
            assert!(category.recused_threshold().is_some());
        }
        for category in [DecisionCategory::Emergency, DecisionCategory::Schedule] {
            assert!(!category.determines_recusal());
            assert!(category.recused_threshold().is_none());
        }
    }
}

#[cfg(test)]
mod raw_text_tests {
    use super::*;

    fn response() -> AgentResponse {
        AgentResponse {
            role: CouncilSeat::Lawyer,
            position: "for the record".to_string(),
            vote: Some(CouncilVote::Yes),
            rationale: "the reasoning".into(),
            questions: Vec::new(),
            ready_to_vote: true,
            ranking: None,
        }
    }

    /// The reasoning is stored once. Anything that reintroduces a second
    /// copy of a councillor's words doubles what a lawful redaction of the
    /// signed entry has to find (agora#351).
    #[test]
    fn a_response_carries_its_reasoning_exactly_once() {
        for json in [
            serde_json::to_string(&response()).unwrap(),
            serde_json::to_string(&StoredResponse::from(&response())).unwrap(),
        ] {
            assert_eq!(json.matches("the reasoning").count(), 1, "{json}");
            assert!(!json.contains("raw_text"), "{json}");
        }
        assert_eq!(
            serde_json::to_string(&StoredResponse::from(&response())).unwrap(),
            r#"{"position":"for the record","vote":"Yes","rationale":"the reasoning"}"#,
            "the stored shape and key order are unchanged apart from raw_text"
        );
    }

    /// Entries signed before 2026-09-18 carry `raw_text`; they must still
    /// read back.
    #[test]
    fn a_response_recorded_with_raw_text_still_deserializes() {
        let mut old = serde_json::to_value(response()).unwrap();
        old["raw_text"] = "the reasoning".into();
        let read: AgentResponse = serde_json::from_value(old).unwrap();
        assert_eq!(read.rationale, "the reasoning".into());
    }
}
