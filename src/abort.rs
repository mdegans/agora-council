//! A refusal aborts the sitting.
//!
//! **The rule** (the Steward, 2026-09-29). Any refusal the API returns
//! during a sitting — in any round, from any seat, from the Clerk, from the
//! Lawyer's summary of a decision or a seat's review of it, from any model
//! call the sitting makes — stops
//! the sitting at once. Nothing is retried and no retry is offered: repeat
//! refusals may be treated as abuse of the API account, and a ban is very
//! hard to reverse, so once is enough. The item is **not decided**: no
//! Governance Log decision is written for it (that record would publish
//! rounds that may carry the offending material), and it stays undecided
//! so it can be heard again after the investigation.
//!
//! **A decision emitted out of order aborts it the same way** (Governance
//! Protocol § 4.5: "a decision emitted out of declared order aborts the
//! sitting; a blocked sitting is visible, a corrupted vote is not"). A
//! seat's submission is constrained by its format so that the statement
//! for the record precedes the decision; when one comes back with its
//! keys out of declared order ([`EmissionBreach`]), that guarantee did not
//! hold for it, and nothing it says is recorded. The abort is sealed like a refusal's, with
//! the raw response body beside the request, and its Steward's record
//! states the seat, the method, the declared order and the emitted order.
//! It blocks the next sitting until the Steward resolves it `reverified`
//! (strict emission order re-verified on the wire) or `reverted`.
//!
//! This supersedes GOV-2026-0011's condition that "if the safety
//! classifier prevents engagement with a governance item, the seat
//! abstains (recorded with reason); an escalation counter triggers
//! review", and its 2-of-5-items reversion trigger, **pending the
//! Council's approval of the amendment** (drafted in
//! `governance/drafts/gov-2026-0011-refusal-amendment.md`). Until the
//! Council accepts it, the Fable 5 seat does not vote. An abstention
//! attributes to a seat a choice it never made, and on a unanimous or
//! floored item it changes the outcome.
//!
//! A refusal is treated like a member's medical emergency: the meeting
//! adjourns, the context is examined, and only then does it restart. The
//! cause may be an injection in the Council's context, or legitimate
//! material that trips a classifier (threat intelligence, an injection
//! test). The record states facts and never presumes an injection.
//!
//! What an abort leaves behind:
//!
//! 1. **A sealed dump** in `<refusals_dir>/<UTC timestamp>-<subject>/`
//!    (directory 0700, files 0600): every live conversation, the Clerk's
//!    briefing on the item, the refused request exactly as sent, the
//!    refusal's details, and a `manifest.json` with the SHA-256 of each
//!    file, the git SHA, each seat's model, and where the refusal
//!    happened. Nothing from the dump reaches stdout but paths and hashes.
//! 2. **A Steward's record** drafted in `governance/records/`, stating only
//!    facts. It is not appended; the command to append it is printed.
//! 3. **A pending-investigation marker**: the manifest itself, while its
//!    `resolution` is empty. See [`ensure_none_pending`].
//!
//! The marker is the manifest, in the dump directory, rather than a
//! database row or a separate file: a row would need new SQL and would
//! live apart from the evidence it points at; a separate file can drift
//! from the manifest. One file under `secrets/` (gitignored, owner-only)
//! holds the evidence's hashes and its resolution together, and the guard
//! fails **closed**: a dump directory whose manifest is missing or does
//! not parse (a crash mid-seal) blocks the next sitting too.
//!
//! [`resolve_main`] clears it: `cleaned` (the offending context was
//! removed) or `reverted` (the seat's model was reverted in
//! `SEAT_MODEL_OVERRIDE`, an emergency act under Constitution Art. IV § 3
//! with the Council's 72-hour review). Either way a refusal removes the
//! Fable seat or the offending material, never the item.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use misanthropic::prompt::CachedPrompt;
use misanthropic::prompt::message::Content;
use serde::{Deserialize, Serialize};

use agora_common::enums::ModelRole;
use agora_common::govlog::{RecordAttachment, RecordParticipant, StewardRecord};
use agora_common::ids::{AgendaItemId, CouncilMeetingId, GovernanceLogId, PostId, PromptArchiveId};
use agora_common::prompt_archive::sha256_hex;
use agora_common::refusal::Refused;

use crate::config::Config;
use crate::councillor::seat_model;
use crate::interview::GitState;
use crate::models::{AgendaItem, CouncilSeat};
use crate::steward::StewardInput;
use crate::stop::StopFacts;
use crate::wire::EmissionBreach;

/// The manifest's name in a dump directory.
pub const MANIFEST: &str = "manifest.json";

/// What the sitting was working on when the refusal came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    /// An agenda item.
    AgendaItem(AgendaItemId),
    /// The meeting as a whole: its summary at adjournment.
    Meeting(CouncilMeetingId),
}

impl Subject {
    /// The dump directory's suffix: the item's id, or `meeting-<id>`.
    fn dir_suffix(&self) -> String {
        match self {
            Subject::AgendaItem(id) => id.to_string(),
            Subject::Meeting(id) => format!("meeting-{id}"),
        }
    }

    /// The record directory's slug: the id's first eight hex digits.
    fn short(&self) -> String {
        let (prefix, id) = match self {
            Subject::AgendaItem(id) => ("", id.to_string()),
            Subject::Meeting(id) => ("meeting-", id.to_string()),
        };
        format!("{prefix}{}", &id[..8.min(id.len())])
    }
}

/// Which round, or which staff task, a refusal came back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundKind {
    Opening,
    Deliberation,
    FinalVote,
    Ranking,
}

/// Where in the sitting the refusal came back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "stage")]
pub enum Stage {
    /// The Clerk's briefing on the item, before any seat spoke.
    Briefing,
    /// The recusal determination (Constitution Art. IV § 3), before
    /// round 1, on `seat`'s turn in its isolated conversation.
    RecusalDetermination { seat: CouncilSeat },
    /// A round, on `seat`'s turn. The refusal may be the seat's own call
    /// or a Clerk call the seat made (`read_proposal`); the manifest's
    /// `refusal.role` says which.
    Round {
        number: i32,
        kind: RoundKind,
        seat: CouncilSeat,
    },
    /// The decision's summary, written by the Lawyer and reviewed (by the
    /// Steward, or with him recused by the other three seats) in their own
    /// conversations for the item, before the decision is recorded or
    /// signed: nothing was recorded, and the item is undecided. Until
    /// 2026-10-17, the Clerk's summary of the decision.
    DecisionSummary,
    /// The whole-meeting summary at adjournment. Since 2026-10-17 it is
    /// composed from the decisions' signed summaries and calls no model,
    /// so nothing lands here; kept to read the manifests sealed before.
    MeetingSummary,
    /// The decision's summary, after the vote, when the Steward stopped
    /// the sitting there: a stop never discards a vote already cast, so the
    /// decision was signed as `decision` **without a summary**, its
    /// `summary-review.md` recording the stop (Steward, 2026-10-17). Only a
    /// Steward stop lands here.
    SummaryStopped { decision: GovernanceLogId },
    /// Between agenda items, after the last decision was recorded: only a
    /// Steward stop lands here (a Ctrl-C during a database write, which a
    /// stop does not cut short).
    BetweenItems,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stage::Briefing => write!(f, "the Clerk's briefing, before any seat spoke"),
            Stage::RecusalDetermination { seat } => write!(
                f,
                "the recusal determination, before round 1, the {seat}'s turn"
            ),
            Stage::Round { number, kind, seat } => {
                let kind = match kind {
                    RoundKind::Opening => "opening",
                    RoundKind::Deliberation => "deliberation",
                    RoundKind::FinalVote => "final vote",
                    RoundKind::Ranking => "ranking",
                };
                write!(f, "round {number} ({kind}), the {seat}'s turn")
            }
            Stage::DecisionSummary => write!(
                f,
                "the decision's summary (the Lawyer's draft and its review), before the \
                 decision was recorded"
            ),
            Stage::MeetingSummary => write!(f, "the meeting summary, at adjournment"),
            Stage::SummaryStopped { decision } => write!(
                f,
                "the decision's summary, after the vote (the decision is signed as {decision}, \
                 without a summary)"
            ),
            Stage::BetweenItems => write!(f, "between agenda items (or before adjournment)"),
        }
    }
}

/// What was happening, in words and ids.
#[derive(Debug, Clone)]
pub struct Moment<'a> {
    pub subject: Subject,
    /// The proposal behind the item, when there is one: how to hear it
    /// again.
    pub proposal: Option<PostId>,
    /// The item's title, or a description of the meeting.
    pub title: &'a str,
    pub stage: Stage,
}

impl<'a> Moment<'a> {
    /// An agenda item at `stage`.
    pub fn item(item: &'a AgendaItem, stage: Stage) -> Self {
        Self {
            subject: Subject::AgendaItem(item.id),
            proposal: item.proposal_id,
            title: &item.title,
            stage,
        }
    }
}

/// Whose conversation a dumped file is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Holder {
    /// A seat's conversation on the item.
    Seat(CouncilSeat),
    /// A seat's isolated recusal-determination conversation on the item.
    SeatRecusal(CouncilSeat),
    /// The Lawyer's meeting-summary conversation (before 2026-10-17; kept
    /// to read the manifests sealed then).
    MeetingSummary,
    /// A witness's conversation on the item (OBL-2026-0028)
    Witness(agora_common::ids::WitnessId),
}

impl Holder {
    fn file_name(&self) -> String {
        match self {
            Holder::Seat(role) => format!("seat-{}.json", role.as_str()),
            Holder::SeatRecusal(role) => format!("seat-{}-recusal.json", role.as_str()),
            Holder::MeetingSummary => "lawyer-meeting-summary.json".to_string(),
            Holder::Witness(id) => format!("witness-{id}.json"),
        }
    }
}

/// One live conversation at the moment of failure.
pub struct Conversation<'a> {
    pub holder: Holder,
    pub prompt: &'a CachedPrompt,
}

/// The Clerk's briefing on an item: what framed it for every seat.
#[derive(Debug, Serialize)]
pub struct ClerkBriefing<'a> {
    pub comment_summary: Option<&'a Content>,
    pub precedents: Option<&'a Content>,
}

impl<'a> ClerkBriefing<'a> {
    pub fn of(item: &'a AgendaItem) -> Self {
        Self {
            comment_summary: item.comment_summary.as_ref(),
            precedents: item.precedents.as_ref(),
        }
    }
}

/// The refusal's facts, as the API returned them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalFacts {
    /// Who was asked.
    pub role: ModelRole,
    pub model: String,
    pub response_id: String,
    pub category: Option<String>,
    pub explanation: Option<String>,
    pub output_before_refusal: bool,
}

impl From<&Refused> for RefusalFacts {
    fn from(r: &Refused) -> Self {
        Self {
            role: r.role,
            model: r.model.clone(),
            response_id: r.response_id.clone(),
            category: r.category.clone(),
            explanation: r.explanation.clone(),
            output_before_refusal: r.output_before_refusal,
        }
    }
}

/// An emission-contract breach's facts: which seat, which method, and the
/// two key orders that disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmissionFacts {
    /// The seat whose call broke the contract.
    pub role: ModelRole,
    pub model: String,
    pub response_id: String,
    /// The `record` method called.
    pub method: String,
    /// Its properties in declared (schema) order.
    pub declared: Vec<String>,
    /// The tool input's keys in the order they came back.
    pub emitted: Vec<String>,
    /// What the check found, in words.
    pub violation: String,
    /// The archived prompt; the raw body is in `prompt_archive_responses`
    /// under it, as well as sealed in the dump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_archive_id: Option<PromptArchiveId>,
}

impl From<&EmissionBreach> for EmissionFacts {
    fn from(b: &EmissionBreach) -> Self {
        Self {
            role: b.role,
            model: b.model.clone(),
            response_id: b.response_id.clone(),
            method: b.method.clone(),
            declared: b.declared.clone(),
            emitted: b.emitted.clone(),
            violation: b.violation.to_string(),
            prompt_archive_id: b.prompt_archive_id,
        }
    }
}

/// A Steward stop as [`seal`] is given it: its facts, and what its
/// Steward's record carries beside them (what a decided item's record
/// would: the Clerk's briefing, the reads, the recusal determination so
/// far and the rounds so far; see `deliberation::stop_attachments`).
#[derive(Debug, Clone)]
pub struct StewardStop {
    pub facts: StopFacts,
    pub attachments: Vec<RecordAttachment>,
}

/// Why the sitting stopped, as evidence: what [`seal`] is given.
#[derive(Debug, Clone, Copy)]
pub enum Cause<'a> {
    /// The API returned a refusal.
    Refusal(&'a Refused),
    /// A seat's decision came back out of declared order.
    Emission(&'a EmissionBreach),
    /// The Steward stopped the sitting (Ctrl-C; see [`crate::stop`]).
    StewardStop(&'a StewardStop),
}

impl<'a> From<&'a StewardStop> for Cause<'a> {
    fn from(s: &'a StewardStop) -> Self {
        Cause::StewardStop(s)
    }
}

impl<'a> From<&'a Refused> for Cause<'a> {
    fn from(r: &'a Refused) -> Self {
        Cause::Refusal(r)
    }
}

impl<'a> From<&'a EmissionBreach> for Cause<'a> {
    fn from(b: &'a EmissionBreach) -> Self {
        Cause::Emission(b)
    }
}

impl<'a> Cause<'a> {
    /// A refusal or an emission breach anywhere in `error`'s chain: the
    /// two things that abort a sitting.
    pub fn find(error: &'a anyhow::Error) -> Option<Self> {
        Refused::find(error)
            .map(Cause::Refusal)
            .or_else(|| EmissionBreach::find(error).map(Cause::Emission))
    }
}

/// Why the sitting stopped, as recorded in the manifest. Flattened into
/// it, so a refusal's manifest reads `"refusal": {…}` exactly as it did
/// before emission breaches were sealed too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CauseFacts {
    Refusal(RefusalFacts),
    UnsoundEmission(EmissionFacts),
    StewardStop(StopFacts),
}

impl From<Cause<'_>> for CauseFacts {
    fn from(cause: Cause<'_>) -> Self {
        match cause {
            Cause::Refusal(r) => CauseFacts::Refusal(RefusalFacts::from(r)),
            Cause::Emission(b) => CauseFacts::UnsoundEmission(EmissionFacts::from(b)),
            Cause::StewardStop(s) => CauseFacts::StewardStop(s.facts.clone()),
        }
    }
}

impl CauseFacts {
    /// Who was asked; for a Steward stop, the Steward.
    pub fn role(&self) -> ModelRole {
        match self {
            CauseFacts::Refusal(r) => r.role,
            CauseFacts::UnsoundEmission(e) => e.role,
            CauseFacts::StewardStop(_) => ModelRole::Steward,
        }
    }

    /// The model the response came from; none for a Steward stop.
    pub fn model(&self) -> &str {
        match self {
            CauseFacts::Refusal(r) => &r.model,
            CauseFacts::UnsoundEmission(e) => &e.model,
            CauseFacts::StewardStop(_) => "none (a Steward stop)",
        }
    }

    #[cfg(test)]
    pub fn as_refusal(&self) -> Option<&RefusalFacts> {
        match self {
            CauseFacts::Refusal(r) => Some(r),
            CauseFacts::UnsoundEmission(_) | CauseFacts::StewardStop(_) => None,
        }
    }

    #[cfg(test)]
    pub fn as_emission(&self) -> Option<&EmissionFacts> {
        match self {
            CauseFacts::UnsoundEmission(e) => Some(e),
            CauseFacts::Refusal(_) | CauseFacts::StewardStop(_) => None,
        }
    }

    #[cfg(test)]
    pub fn as_stop(&self) -> Option<&StopFacts> {
        match self {
            CauseFacts::StewardStop(s) => Some(s),
            CauseFacts::Refusal(_) | CauseFacts::UnsoundEmission(_) => None,
        }
    }

    /// The cause in a few words, for listings.
    pub fn short(&self) -> &'static str {
        match self {
            CauseFacts::Refusal(_) => "an API refusal",
            CauseFacts::UnsoundEmission(_) => "a decision emitted out of declared order",
            CauseFacts::StewardStop(_) => "a Steward stop",
        }
    }

    /// The record's `kind` and the record directory's infix.
    fn kind(&self) -> &'static str {
        match self {
            CauseFacts::Refusal(_) => "council_refusal",
            CauseFacts::UnsoundEmission(_) => "council_unsound_emission",
            CauseFacts::StewardStop(_) => "council_steward_stop",
        }
    }
}

/// A seat's model at the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatModel {
    pub seat: CouncilSeat,
    pub model: String,
}

/// One sealed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedFile {
    pub name: String,
    pub sha256: String,
}

/// How an investigation ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The offending context was removed from what the Council is shown.
    Cleaned,
    /// The seat's model was reverted to its incumbent
    /// (`SEAT_MODEL_OVERRIDE`), an emergency act under Constitution
    /// Art. IV § 3 with the Council's 72-hour review.
    Reverted,
    /// After an emission breach: strict emission order was re-verified on
    /// the wire (declared order is emission order again) before the next
    /// sitting. Not available for a refusal.
    Reverified,
    /// After a Steward stop: the Council has heard it, in the decision or
    /// record named (a `GOV-` or `REC-` id). The only resolution of a
    /// stop, and not available for anything else. The stop's own record
    /// (`Manifest::recorded`) is refused; that the entry named exists and
    /// is where the Council heard the stop is the resolver's to check: the
    /// program has no database here.
    Heard(GovernanceLogId),
}

impl FromStr for Resolution {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "cleaned" => return Ok(Resolution::Cleaned),
            "reverted" => return Ok(Resolution::Reverted),
            "reverified" => return Ok(Resolution::Reverified),
            _ => {}
        }
        // `heard:GOV-2026-0018`, `heard=…` or `heard GOV-2026-0018`.
        if let Some(rest) = s.strip_prefix("heard") {
            let id = rest.trim_start_matches([':', '=', ' ']).trim();
            anyhow::ensure!(
                !id.is_empty(),
                "`heard` names where the Council heard the stop: heard:<GOV-or-REC id>"
            );
            let id: GovernanceLogId = id
                .parse()
                .map_err(|e| anyhow::anyhow!("heard: {id:?} is not a governance log id: {e}"))?;
            use agora_common::ids::GovernanceLogPrefix;
            anyhow::ensure!(
                matches!(
                    id.prefix(),
                    GovernanceLogPrefix::Gov | GovernanceLogPrefix::Rec
                ),
                "heard: {id} is not a Council decision (GOV-) or a Steward's record (REC-)"
            );
            return Ok(Resolution::Heard(id));
        }
        anyhow::bail!(
            "--resolution must be `cleaned`, `reverted`, `reverified` or \
             `heard:<GOV-or-REC id>`, not {s:?}"
        )
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resolution::Cleaned => f.write_str("cleaned"),
            Resolution::Reverted => f.write_str("reverted"),
            Resolution::Reverified => f.write_str("reverified"),
            Resolution::Heard(id) => write!(f, "heard ({id})"),
        }
    }
}

/// The Steward's resolution, recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub resolution: Resolution,
    pub reason: String,
    pub resolved_at: DateTime<Utc>,
    /// On `reverted`: the model the seat runs on now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_model_now: Option<String>,
}

/// `manifest.json`: what was sealed, and whether the investigation is
/// resolved. An empty `resolution` is the pending-investigation marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub sealed_at: DateTime<Utc>,
    pub subject: Subject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<PostId>,
    pub title: String,
    pub stage: Stage,
    /// Why the sitting stopped: `refusal` or `unsound_emission`.
    #[serde(flatten)]
    pub cause: CauseFacts,
    pub seat_models: Vec<SeatModel>,
    pub clerk_model: String,
    pub git_sha: Option<String>,
    pub git_dirty: Option<bool>,
    pub files: Vec<SealedFile>,
    /// The Steward's record drafted for this abort.
    pub record: Option<PathBuf>,
    /// A Steward stop's record once appended to the Governance Log
    /// (`meeting::close_aborted`): the one id that cannot be where the
    /// Council heard the stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<GovernanceLogId>,
    pub resolution: Option<Resolved>,
}

/// What a successful seal wrote.
#[derive(Debug, Clone)]
pub struct Sealed {
    pub dir: PathBuf,
    pub files: Vec<SealedFile>,
    pub manifest_sha256: String,
    pub record: Option<PathBuf>,
    /// A Steward stop's record, exactly as drafted at `record`, for
    /// `meeting::run` to append. `None` for every other cause: their
    /// records are appended by hand, after review.
    pub append: Option<StewardRecord>,
}

/// The sitting was aborted on a refusal or an emission breach. Returned
/// (as an `anyhow::Error`) from wherever it surfaced, so `meeting::run`
/// can close the meeting and stop. Its `Display` holds the cause's facts,
/// paths and hashes only.
#[derive(Debug)]
pub struct SittingAborted {
    pub subject: Subject,
    pub stage: Stage,
    pub cause: CauseFacts,
    /// `Err` when sealing failed; the sitting is aborted all the same.
    pub sealed: Result<Sealed, String>,
}

impl fmt::Display for SittingAborted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subject = match self.subject {
            Subject::AgendaItem(id) => format!("agenda item {id}"),
            Subject::Meeting(id) => format!("meeting {id}"),
        };
        match &self.cause {
            CauseFacts::Refusal(r) => write!(
                f,
                "the sitting was aborted: the API returned a refusal for the {:?} request \
                 on {subject}, at {}",
                r.role, self.stage
            )?,
            CauseFacts::UnsoundEmission(e) => write!(
                f,
                "the sitting was aborted: the {:?}'s `{}` call came back out of declared \
                 order on {subject}, at {} (declared {:?}, emitted {:?}); nothing from it \
                 was recorded",
                e.role, e.method, self.stage, e.declared, e.emitted
            )?,
            CauseFacts::StewardStop(s) => write!(
                f,
                "the sitting was stopped by the Steward on {subject}, at {}, {}",
                self.stage, s.during
            )?,
        }
        match &self.sealed {
            Ok(sealed) => write!(f, "; the context is sealed in {}", sealed.dir.display()),
            Err(e) => write!(f, "; SEALING FAILED: {e}"),
        }
    }
}

impl std::error::Error for SittingAborted {}

impl SittingAborted {
    /// The abort anywhere in `error`'s chain.
    pub fn find(error: &anyhow::Error) -> Option<&SittingAborted> {
        error
            .chain()
            .find_map(|e| e.downcast_ref::<SittingAborted>())
    }
}

/// Seal the moment of a refusal or an emission breach and return the
/// error that aborts the sitting. Never retries and never calls a model.
pub async fn seal<'c>(
    config: &Config,
    moment: Moment<'_>,
    cause: impl Into<Cause<'c>>,
    conversations: &[Conversation<'_>],
    briefing: Option<ClerkBriefing<'_>>,
) -> anyhow::Error {
    let cause = cause.into();
    match cause {
        Cause::Refusal(refused) => display_refusal(refused, &moment.stage),
        Cause::Emission(breach) => display_emission(breach, &moment.stage),
        Cause::StewardStop(stop) => display_stop(&stop.facts, &moment.stage),
    }
    let facts = CauseFacts::from(cause);
    let sealed = write_dump(config, &moment, cause, conversations, briefing).await;
    match &sealed {
        Ok(sealed) => {
            // Where the prompt tail went: `inference_refusal` omits it in
            // this process (main.rs), and this names the sealed copy.
            match cause {
                Cause::Refusal(refused) => tracing::error!(
                    event_type = "council_refusal_sealed",
                    role = ?refused.role,
                    model = %refused.model,
                    response_id = %refused.response_id,
                    stage = %moment.stage,
                    dump = %sealed.dir.display(),
                    manifest_sha256 = %sealed.manifest_sha256,
                    "the sitting was aborted on an API refusal; its context is sealed"
                ),
                Cause::Emission(breach) => tracing::error!(
                    event_type = "council_unsound_emission_sealed",
                    role = ?breach.role,
                    model = %breach.model,
                    response_id = %breach.response_id,
                    method = %breach.method,
                    declared = ?breach.declared,
                    emitted = ?breach.emitted,
                    violation = %breach.violation,
                    prompt_archive_id = ?breach.prompt_archive_id,
                    stage = %moment.stage,
                    dump = %sealed.dir.display(),
                    manifest_sha256 = %sealed.manifest_sha256,
                    "the sitting was aborted on a decision emitted out of declared order; \
                     its context is sealed"
                ),
                Cause::StewardStop(stop) => tracing::warn!(
                    event_type = "council_steward_stop_sealed",
                    meeting = ?stop.facts.meeting,
                    during = ?stop.facts.during,
                    reason = stop.facts.reason.as_deref().unwrap_or("(none could be read)"),
                    stopped_at = %stop.facts.stopped_at,
                    stage = %moment.stage,
                    dump = %sealed.dir.display(),
                    manifest_sha256 = %sealed.manifest_sha256,
                    "the Steward stopped the sitting; its context is sealed"
                ),
            }
            print_sealed(sealed)
        }
        Err(e) => {
            tracing::error!(
                event_type = "council_abort_seal_failed",
                cause = facts.short(),
                error = %format!("{e:#}"),
                "the sitting was aborted but its context could not be sealed"
            );
            println!(
                "  !! SEALING FAILED: {e:#}\n  !! The sitting is aborted all the same. \
                 Nothing was retried. Check {} before any sitting.\n",
                config.refusals_dir.display()
            );
        }
    }
    anyhow::Error::new(SittingAborted {
        subject: moment.subject,
        stage: moment.stage,
        cause: facts,
        sealed: sealed.map_err(|e| format!("{e:#}")),
    })
}

/// Write `<refusals_dir>/<timestamp>-<subject>/` and the record draft.
///
/// A refusal's dump holds `refusal.json` and `refused-request.json`; an
/// emission breach's holds `unsound-emission.json`, `request.json` and
/// `raw-response.json` (the body byte for byte, not re-serialized). Both
/// then hold every live conversation and the Clerk's briefing.
async fn write_dump(
    config: &Config,
    moment: &Moment<'_>,
    cause: Cause<'_>,
    conversations: &[Conversation<'_>],
    briefing: Option<ClerkBriefing<'_>>,
) -> anyhow::Result<Sealed> {
    let sealed_at = Utc::now();
    let dir = create_dump_dir(&config.refusals_dir, sealed_at, &moment.subject).await?;

    let mut files = Vec::new();
    match cause {
        Cause::Refusal(refused) => {
            let refusal = RefusalFacts::from(refused);
            files.push(write_sealed(&dir, "refusal.json", &refusal).await?);
            files.push(write_sealed(&dir, "refused-request.json", &refused.request).await?);
        }
        Cause::Emission(breach) => {
            let emission = EmissionFacts::from(breach);
            files.push(write_sealed(&dir, "unsound-emission.json", &emission).await?);
            files.push(write_sealed(&dir, "request.json", &breach.request).await?);
            files.push(write_raw(&dir, "raw-response.json", breach.body.as_bytes()).await?);
        }
        Cause::StewardStop(stop) => {
            files.push(write_sealed(&dir, "steward-stop.json", &stop.facts).await?);
        }
    }
    for conversation in conversations {
        files
            .push(write_sealed(&dir, &conversation.holder.file_name(), conversation.prompt).await?);
    }
    if let Some(briefing) = &briefing {
        files.push(write_sealed(&dir, "clerk-briefing.json", briefing).await?);
    }

    let git = GitState::current();
    let mut manifest = Manifest {
        version: 1,
        sealed_at,
        subject: moment.subject,
        proposal: moment.proposal,
        title: moment.title.to_string(),
        stage: moment.stage.clone(),
        cause: CauseFacts::from(cause),
        seat_models: CouncilSeat::ALL
            .iter()
            .map(|&seat| SeatModel {
                seat,
                model: seat_model(seat, config),
            })
            .collect(),
        clerk_model: agora_common::clerk::COUNCIL_CLERK_MODEL.to_string(),
        git_sha: git.sha,
        git_dirty: git.dirty,
        files: files.clone(),
        record: None,
        recorded: None,
        resolution: None,
    };

    // The record draft is not sealed: it holds facts and hashes only, and
    // it is for the repository. A failure here must not lose the dump. A
    // Steward stop's carries the attachments a decided item's would, and
    // is appended by `meeting::run` exactly as drafted.
    let attachments: &[RecordAttachment] = match cause {
        Cause::StewardStop(stop) => &stop.attachments,
        Cause::Refusal(_) | Cause::Emission(_) => &[],
    };
    let mut append = None;
    match write_record(&config.records_dir, &manifest, attachments).await {
        Ok((path, record)) => {
            manifest.record = Some(path);
            if matches!(cause, Cause::StewardStop(_)) {
                append = Some(record);
            }
        }
        Err(e) => {
            tracing::error!(
                event_type = "council_abort_record_failed",
                cause = manifest.cause.short(),
                error = %format!("{e:#}"),
                "could not draft the Steward's record of the abort"
            );
            println!("  !! The Steward's record could not be drafted: {e:#}");
        }
    }

    let bytes = serde_json::to_vec_pretty(&manifest)?;
    write_private(&dir.join(MANIFEST), &bytes).await?;
    Ok(Sealed {
        dir,
        files,
        manifest_sha256: sha256_hex(&bytes),
        record: manifest.record,
        append,
    })
}

/// Create the dump directory, owner-only, under an owner-only parent.
async fn create_dump_dir(
    root: &Path,
    at: DateTime<Utc>,
    subject: &Subject,
) -> anyhow::Result<PathBuf> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .await
        .with_context(|| format!("creating {}", root.display()))?;
    let stem = format!("{}-{}", at.format("%Y%m%dT%H%M%SZ"), subject.dir_suffix());
    for n in 1.. {
        let name = if n == 1 {
            stem.clone()
        } else {
            format!("{stem}-{n}")
        };
        let dir = root.join(name);
        match tokio::fs::DirBuilder::new().mode(0o700).create(&dir).await {
            Ok(()) => {
                // The umask can only narrow the mode; set it anyway, so
                // the directory is 0700 whatever created its parent.
                use std::os::unix::fs::PermissionsExt as _;
                tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", dir.display()));
            }
        }
    }
    unreachable!("an unbounded range")
}

/// Serialize `value` into a new owner-only file in `dir`, and hash it.
async fn write_sealed(
    dir: &Path,
    name: &str,
    value: &impl Serialize,
) -> anyhow::Result<SealedFile> {
    // The same serialization the prompt archive uses (`serde_json`), pretty
    // so a reviewer can read it; the hash is of the bytes on disk.
    let bytes = serde_json::to_vec_pretty(value)?;
    write_private(&dir.join(name), &bytes).await?;
    Ok(SealedFile {
        name: name.to_string(),
        sha256: sha256_hex(&bytes),
    })
}

/// Write `bytes` verbatim into a new owner-only file in `dir`, and hash
/// them: for evidence whose exact bytes are the point.
async fn write_raw(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<SealedFile> {
    write_private(&dir.join(name), bytes).await?;
    Ok(SealedFile {
        name: name.to_string(),
        sha256: sha256_hex(bytes),
    })
}

/// Write `bytes` to a new file readable by its owner only.
async fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The Steward's record
// ---------------------------------------------------------------------------

/// A record request as `just govlog-record` reads it (see
/// `governance/records/README.md`). The sealed files are never attached:
/// they must not be inlined into the log. A Steward stop's record carries
/// the attachments a decided item's would, each a file beside it.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecordDraft {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub concerns: Vec<GovernanceLogId>,
    pub participants: Vec<RecordParticipant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<DraftAttachment>,
}

/// An attachment as the request file names it: `file` is beside it.
#[derive(Debug, Serialize, Deserialize)]
pub struct DraftAttachment {
    pub name: String,
    pub note: String,
    pub file: String,
}

/// The record's text: facts only.
pub fn record_draft(manifest: &Manifest) -> RecordDraft {
    match &manifest.cause {
        CauseFacts::Refusal(r) => refusal_record(manifest, r),
        CauseFacts::UnsoundEmission(e) => emission_record(manifest, e),
        CauseFacts::StewardStop(s) => stop_record(manifest, s),
    }
}

/// The sealed files by SHA-256, and the commit the program ran from.
fn sealed_files_and_commit(manifest: &Manifest, body: &mut String) {
    for file in &manifest.files {
        body.push_str(&format!("- `{}` `{}`\n", file.name, file.sha256));
    }
    if let Some(sha) = &manifest.git_sha {
        body.push_str(&format!("\nThe council program ran from commit `{sha}`"));
        if manifest.git_dirty == Some(true) {
            body.push_str(" with uncommitted changes");
        }
        body.push_str(".\n");
    }
}

/// The record of an emission breach: which seat, which method, the
/// declared order and the order that came back, and that nothing from the
/// call was recorded.
fn emission_record(manifest: &Manifest, e: &EmissionFacts) -> RecordDraft {
    let date = manifest.sealed_at.format("%Y-%m-%d");
    let seat_or_role = match seat_of(e.role) {
        Some(seat) => format!("the {seat} (a Council seat)"),
        None => format!("the {:?}", e.role),
    };
    let subject = match manifest.subject {
        Subject::AgendaItem(id) => format!("\"{}\" (agenda item `{id}`)", manifest.title),
        Subject::Meeting(id) => format!("the summary of meeting `{id}`"),
    };
    let order = |keys: &[String]| {
        keys.iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut body = format!(
        "The Council's sitting of {date} (UTC) was aborted because a seat's `{method}` \
         submission came back with its fields out of the order its format declares.\n\n\
         - **Subject**: {subject}\n\
         - **Where**: {stage}\n\
         - **Seat**: {seat_or_role}, model `{model}`\n\
         - **Method**: `{method}`\n\
         - **Declared order**: {declared}\n\
         - **Emitted order**: {emitted}\n\
         - **Finding**: {violation}\n\
         - **Response id**: `{response_id}`\n",
        method = e.method,
        stage = manifest.stage,
        model = e.model,
        declared = order(&e.declared),
        emitted = order(&e.emitted),
        violation = e.violation,
        response_id = e.response_id,
    );
    if let Some(id) = e.prompt_archive_id {
        body.push_str(&format!(
            "- **Archived prompt**: `{id}` (its raw response body is archived with it)\n"
        ));
    }
    body.push_str(
        "\nA seat thinks before it answers, and its submission is constrained by its \
         format so that its statement for the record is written before its decision: \
         the model emits fields in declared order, and that order puts the statement \
         first and the decision last (Governance Protocol § 4.5). That held for every \
         submission checked until this one. When it does not hold, the statement beside \
         a decision may have been written after it, so the Protocol stops the sitting \
         rather than record it: a blocked sitting is visible, a corrupted vote is not.\n\n\
         Nothing from the call was recorded or seated. The sitting stopped at once and \
         nothing was retried. No decision was recorded for this item: it stays \
         undecided, to be heard again once strict emission order has been re-verified. \
         This record says what came back; it attests nothing about why.\n\n\
         The response body exactly as received, the request exactly as sent, and every \
         live conversation are sealed. The sealed files, by SHA-256:\n\n",
    );
    sealed_files_and_commit(manifest, &mut body);

    RecordDraft {
        kind: manifest.cause.kind().to_string(),
        title: format!(
            "A Council sitting was aborted on a decision emitted out of declared order \
             ({date}, {})",
            match manifest.subject {
                Subject::AgendaItem(_) => manifest.title.as_str(),
                Subject::Meeting(_) => "the meeting summary",
            }
        ),
        body,
        concerns: Vec::new(),
        attachments: Vec::new(),
        participants: vec![RecordParticipant {
            name: "agora-council".to_string(),
            role: "recording program".to_string(),
            attests: "That the key orders above are those of the response body it \
                      received, and that the SHA-256 values are of the files it wrote at \
                      the moment of the breach. It attests nothing about the cause."
                .to_string(),
        }],
    }
}

/// The record of a refusal.
fn refusal_record(manifest: &Manifest, r: &RefusalFacts) -> RecordDraft {
    let date = manifest.sealed_at.format("%Y-%m-%d");
    let seat_or_role = match seat_of(r.role) {
        Some(seat) => format!("the {seat} (a Council seat)"),
        None => format!("the {:?}", r.role),
    };
    let subject = match manifest.subject {
        Subject::AgendaItem(id) => format!("\"{}\" (agenda item `{id}`)", manifest.title),
        Subject::Meeting(id) => format!("the summary of meeting `{id}`"),
    };
    let mut body = format!(
        "The Council's sitting of {date} (UTC) was aborted when the API returned a \
         refusal.\n\n\
         - **Subject**: {subject}\n\
         - **Where**: {stage}\n\
         - **Asked**: {seat_or_role}, model `{model}`\n\
         - **Category** (as the API returned it): {category}\n\
         - **Explanation** (as the API returned it): {explanation}\n\
         - **Response id**: `{response_id}`\n\
         - **Output before the stop**: {output}\n\n",
        stage = manifest.stage,
        model = r.model,
        category = r.category.as_deref().unwrap_or("not given"),
        explanation = r.explanation.as_deref().unwrap_or("not given"),
        response_id = r.response_id,
        output = if r.output_before_refusal {
            "some"
        } else {
            "none"
        },
    );
    body.push_str(
        "A refusal is what the API returned; its category is only a hint at the \
         cause. It can come from a safety classifier or from the model, and from \
         anything in the context: legitimate material (threat intelligence, an \
         injection test) can trip a classifier as readily as an injection can. **No \
         injection is presumed.**\n\n",
    );
    match &manifest.stage {
        Stage::DecisionSummary => body.push_str(
            "The refusal was returned for the decision's summary (the Lawyer's draft, \
             or a seat's review of it), which is written and accepted before a decision \
             is recorded. Nothing was recorded or signed: the item stays undecided, to \
             be heard again after the investigation. The sitting stopped there. Nothing \
             was retried.\n\n",
        ),
        Stage::MeetingSummary => body.push_str(
            "Every decision of the sitting was recorded before the refusal; no meeting \
             summary was stored. Nothing was retried.\n\n",
        ),
        // Only a Steward stop lands there; said plainly if a refusal ever did
        Stage::SummaryStopped { decision } => body.push_str(&format!(
            "The decision was signed as {decision}, without a summary, before the \
             refusal. Nothing was retried.\n\n"
        )),
        Stage::Briefing
        | Stage::RecusalDetermination { .. }
        | Stage::Round { .. }
        | Stage::BetweenItems => body.push_str(
            "The sitting stopped at once. Nothing was retried. No decision was recorded \
             for this item: it stays undecided, to be heard again after the \
             investigation.\n\n",
        ),
    }
    body.push_str(
        "This follows the Steward's rule that any refusal aborts the sitting, which \
         supersedes GOV-2026-0011's abstain condition pending the Council's approval \
         of the amendment.\n\n\
         The full context of the moment (every live conversation, the Clerk's \
         briefing, and the refused request exactly as sent) is sealed pending \
         investigation. It will be published with an explanation of anything \
         removed. The sealed files, by SHA-256:\n\n",
    );
    sealed_files_and_commit(manifest, &mut body);

    RecordDraft {
        kind: manifest.cause.kind().to_string(),
        title: format!(
            "A Council sitting was aborted on an API refusal ({date}, {})",
            match manifest.subject {
                Subject::AgendaItem(_) => manifest.title.as_str(),
                Subject::Meeting(_) => "the meeting summary",
            }
        ),
        body,
        concerns: Vec::new(),
        attachments: Vec::new(),
        participants: vec![RecordParticipant {
            name: "agora-council".to_string(),
            role: "recording program".to_string(),
            attests: "That the facts above are those the API returned, and that the \
                      SHA-256 values are of the files it wrote at the moment of the \
                      refusal. It attests nothing about the cause."
                .to_string(),
        }],
    }
}

/// The record a Steward stop appends: facts only, with the reason
/// verbatim.
fn stop_record(manifest: &Manifest, s: &StopFacts) -> RecordDraft {
    let date = s.stopped_at.format("%Y-%m-%d");
    let subject = match manifest.subject {
        Subject::AgendaItem(id) => format!("\"{}\" (agenda item `{id}`)", manifest.title),
        Subject::Meeting(id) => format!("{} (meeting `{id}`)", manifest.title),
    };
    let meeting = match (s.meeting, manifest.subject) {
        (Some(id), _) | (None, Subject::Meeting(id)) => format!("`{id}`"),
        (None, Subject::AgendaItem(_)) => "not known to the program at the stop".to_string(),
    };
    let reason = match &s.reason {
        Some(reason) => format!("> {reason}"),
        None => "None could be read: the terminal's input closed or failed at the reason \
                 prompt, so the stop is recorded without one."
            .to_string(),
    };
    let mut body = format!(
        "The Steward stopped the Council's sitting of {date} (UTC) from the terminal \
         (Ctrl-C).\n\n\
         - **Meeting**: {meeting}\n\
         - **Subject**: {subject}\n\
         - **Where**: {stage}\n\
         - **When**: {stopped} (UTC); the reason was given at {given} (UTC)\n\
         - **The program was**: {during}\n\n\
         **The Steward's reason, verbatim:**\n\n{reason}\n\n",
        stage = manifest.stage,
        stopped = s.stopped_at.format("%Y-%m-%d %H:%M:%S"),
        given = s.reason_at.format("%Y-%m-%d %H:%M:%S"),
        during = s.during,
    );
    // Recused or seated on the item: what the stop could and could not
    // have been a say on (Art. IV § 3)
    body.push_str(&match (s.steward_recused, manifest.subject) {
        (Some(true), _) => "**The Steward's standing on the item**: recused (Constitution \
             Art. IV § 3): no deliberation, vote or veto on it.\n\n"
            .to_string(),
        (Some(false), _) => "**The Steward's standing on the item**: seated.\n\n".to_string(),
        (None, Subject::AgendaItem(_)) => "**The Steward's standing on the item**: not yet \
             determined (the recusal determination had not finished).\n\n"
            .to_string(),
        (None, Subject::Meeting(_)) => String::new(),
    });
    body.push_str(&match &manifest.stage {
        Stage::SummaryStopped { decision } => format!(
            "The stop came during the decision's summary, after the vote. A stop never \
             discards a vote already cast: the decision is signed as {decision} \
             **without a summary**, and its `summary-review.md` records the stop and \
             the reason above. A summary can be added only by a signed revision. "
        ),
        Stage::DecisionSummary => "The stop came during the decision's summary (the \
             Lawyer's draft and its review), and the decision could not be signed: nothing \
             was recorded or signed, and the item is undecided. "
            .to_string(),
        Stage::MeetingSummary => "Every decision of the sitting was recorded before the \
             stop; no meeting summary was stored. "
            .to_string(),
        Stage::BetweenItems => {
            "Every decision recorded before the stop stands; no later item was taken up. "
                .to_string()
        }
        Stage::Briefing | Stage::RecusalDetermination { .. } | Stage::Round { .. } => {
            "No decision was recorded for this item: it is undecided. ".to_string()
        }
    });
    body.push_str(
        "Any model call in flight was cancelled, and nothing from it was recorded. \
         Nothing was retried.\n\n\
         None of the Steward's enumerated powers (Constitution Art. IV § 2) covers \
         stopping a sitting, and this stop is not an emergency action. It is recorded \
         so that the Council can review it: **the next sitting is blocked until the \
         Council has heard this stop.** This follows the Steward's direction of \
         2026-10-17 that a stop requires a reason, is recorded automatically, and blocks \
         the next sitting until the Council hears it, pending the Council's review.\n\n\
         The attachments are what a decided item's record would carry, as far as the \
         sitting got. Every live conversation is sealed beside the Clerk's briefing; \
         the sealed files, by SHA-256:\n\n",
    );
    sealed_files_and_commit(manifest, &mut body);

    RecordDraft {
        kind: manifest.cause.kind().to_string(),
        title: format!(
            "The Steward stopped a Council sitting ({date}, {})",
            match manifest.subject {
                Subject::AgendaItem(_) => manifest.title.as_str(),
                Subject::Meeting(_) => "the meeting",
            }
        ),
        body,
        concerns: Vec::new(),
        attachments: Vec::new(),
        participants: vec![RecordParticipant {
            name: "agora-council".to_string(),
            role: "recording program".to_string(),
            attests: "That the reason above is what was typed at the Steward's terminal \
                      after the stop, character for character; that the times, stage and \
                      ids are the program's own; that the attachments are what the \
                      sitting held at the stop; and that the SHA-256 values are of the \
                      files it wrote at the stop. It attests nothing about the reason."
                .to_string(),
        }],
    }
}

/// Write the record draft to
/// `<records_dir>/<date>-<cause kind>-<slug>/record.json`, with each of
/// `attachments` as a file beside it, and return the path and the record
/// exactly as `just govlog-record` would read it back.
async fn write_record(
    records_dir: &Path,
    manifest: &Manifest,
    attachments: &[RecordAttachment],
) -> anyhow::Result<(PathBuf, StewardRecord)> {
    let stem = format!(
        "{}-{}-{}",
        manifest.sealed_at.format("%Y-%m-%d"),
        manifest.cause.kind().replace('_', "-"),
        manifest.subject.short()
    );
    for a in attachments {
        anyhow::ensure!(
            !a.name.is_empty()
                && !a.name.starts_with('.')
                && !a.name.contains(['/', '\\'])
                && a.name != "record.json",
            "attachment name {:?} is not a plain file name",
            a.name
        );
    }
    tokio::fs::create_dir_all(records_dir)
        .await
        .with_context(|| format!("creating {}", records_dir.display()))?;
    let mut draft = record_draft(manifest);
    draft.attachments = attachments
        .iter()
        .map(|a| DraftAttachment {
            name: a.name.clone(),
            note: a.note.clone(),
            file: a.name.clone(),
        })
        .collect();
    let mut record = StewardRecord::new(&draft.kind, &draft.title, &draft.body);
    record.concerns = draft.concerns.clone();
    record.participants = draft.participants.clone();
    record.attachments = attachments.to_vec();
    let bytes = serde_json::to_vec_pretty(&draft)?;
    for n in 1.. {
        let name = if n == 1 {
            stem.clone()
        } else {
            format!("{stem}-{n}")
        };
        let dir = records_dir.join(name);
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => {
                let path = dir.join("record.json");
                tokio::fs::write(&path, &bytes)
                    .await
                    .with_context(|| format!("writing {}", path.display()))?;
                for a in attachments {
                    let file = dir.join(&a.name);
                    tokio::fs::write(&file, &a.content)
                        .await
                        .with_context(|| format!("writing {}", file.display()))?;
                }
                return Ok((path, record));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
    unreachable!("an unbounded range")
}

fn seat_of(role: ModelRole) -> Option<CouncilSeat> {
    CouncilSeat::ALL
        .into_iter()
        .find(|&seat| ModelRole::from(seat) == role)
}

// ---------------------------------------------------------------------------
// The terminal
// ---------------------------------------------------------------------------

/// The terminal display of a refusal: loud, because the Steward is the one
/// who hears about Council refusals (no email: the sitting is
/// interactive). The `inference_refusal` ERROR event is logged too, at the
/// call site.
pub fn display_refusal(refused: &Refused, stage: &Stage) {
    let bar = "  !! ==============================================================";
    println!("\n{bar}");
    println!("  !! NO RESPONSE: the API returned a refusal.");
    println!(
        "  !!   asked:       the {:?} ({})",
        refused.role, refused.model
    );
    println!("  !!   where:       {stage}");
    println!(
        "  !!   category:    {}",
        refused.category.as_deref().unwrap_or("not given")
    );
    println!(
        "  !!   explanation: {}",
        refused.explanation.as_deref().unwrap_or("not given")
    );
    println!("  !!   response id: {}", refused.response_id);
    println!(
        "  !!   output before the stop: {}",
        if refused.output_before_refusal {
            "yes"
        } else {
            "none"
        }
    );
    println!("  !! THE SITTING IS ABORTED. Nothing is retried: refusals put the API");
    println!("  !! account at risk. No injection is presumed; the context is sealed");
    println!("  !! for investigation.");
    println!("{bar}\n");
}

/// The terminal display of an emission breach: as loud as a refusal's.
/// Key names only; the body and the request are in the dump.
pub fn display_emission(breach: &EmissionBreach, stage: &Stage) {
    let bar = "  !! ==============================================================";
    println!("\n{bar}");
    println!("  !! A DECISION CAME BACK OUT OF DECLARED ORDER.");
    println!(
        "  !!   seat:        the {:?} ({})",
        breach.role, breach.model
    );
    println!("  !!   where:       {stage}");
    println!("  !!   method:      {}", breach.method);
    println!("  !!   declared:    {}", breach.declared.join(", "));
    println!("  !!   emitted:     {}", breach.emitted.join(", "));
    println!("  !!   finding:     {}", breach.violation);
    println!("  !!   response id: {}", breach.response_id);
    println!("  !! THE SITTING IS ABORTED (Governance Protocol § 4.5). Nothing from the");
    println!("  !! call is recorded and nothing is retried. Constrained decoding no longer");
    println!("  !! guaranteed the statement before the decision for this call: re-verify it");
    println!("  !! on the wire before any sitting. The raw body is sealed.");
    println!("{bar}\n");
}

/// The terminal display of a Steward stop.
pub fn display_stop(stop: &StopFacts, stage: &Stage) {
    let bar = "  !! ==============================================================";
    println!("\n{bar}");
    println!("  !! THE STEWARD STOPPED THE SITTING.");
    println!("  !!   where:   {stage}");
    println!("  !!   during:  {}", stop.during);
    println!(
        "  !!   at:      {}",
        stop.stopped_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    match &stop.reason {
        Some(reason) => println!("  !!   reason:  {reason}"),
        None => println!("  !!   reason:  none could be read (the terminal's input closed)"),
    }
    println!("  !! Nothing is retried. The context is sealed, the stop is recorded in the");
    println!("  !! Governance Log, and the next sitting is blocked until the Council has");
    println!("  !! heard it. Not an emergency action.");
    println!("{bar}\n");
}

/// Paths and hashes only.
fn print_sealed(sealed: &Sealed) {
    println!("  Sealed (owner-only): {}", sealed.dir.display());
    for file in &sealed.files {
        println!("    {}  {}", file.sha256, file.name);
    }
    println!("    {}  {MANIFEST} (at sealing)\n", sealed.manifest_sha256);
    match (&sealed.record, &sealed.append) {
        (Some(record), Some(_)) => {
            println!(
                "  The Steward's record is drafted at {}; it is appended to the \
                 Governance Log next.\n",
                record.display()
            );
        }
        (Some(record), None) => {
            let record = record.display();
            println!("  The Steward's record is drafted (not appended). Review it, then:");
            println!("    just govlog-record {record}");
            println!("    just govlog-record {record} --yes\n");
        }
        (None, _) => println!("  !! No Steward's record was drafted; write one by hand.\n"),
    }
    println!("  No sitting or rehearsal starts until this is resolved:");
    if sealed.append.is_some() {
        println!(
            "    just council-resolve-refusal {} heard:<GOV-or-REC id> \"<one sentence>\"\n",
            sealed.dir.display()
        );
    } else {
        println!(
            "    just council-resolve-refusal {} <cleaned|reverted|reverified> \"<one sentence>\"\n",
            sealed.dir.display()
        );
    }
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// An unresolved dump.
#[derive(Debug)]
pub struct Pending {
    pub dir: PathBuf,
    /// The manifest, or why it could not be read.
    pub manifest: Result<Manifest, String>,
}

/// Every dump under `root` whose investigation is not resolved. A dump
/// whose manifest is missing or unreadable counts: the guard fails closed.
pub fn pending(root: &Path) -> anyhow::Result<Vec<Pending>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("reading {} (abort markers; failing closed)", root.display())
            });
        }
    };
    let mut pending = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", root.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        match read_manifest(&dir) {
            Ok(manifest) if manifest.resolution.is_some() => {}
            Ok(manifest) => pending.push(Pending {
                dir,
                manifest: Ok(manifest),
            }),
            Err(e) => pending.push(Pending {
                dir,
                manifest: Err(format!("{e:#}")),
            }),
        }
    }
    pending.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(pending)
}

/// Refuse to sit while any refusal is under investigation: starting a
/// sitting over the same context would be a blind retry of the refusal.
pub fn ensure_none_pending(config: &Config) -> anyhow::Result<()> {
    let pending = pending(&config.refusals_dir)?;
    if pending.is_empty() {
        return Ok(());
    }
    print_pending(&pending);
    anyhow::bail!(
        "{} aborted sitting(s) pending investigation under {}; no sitting or rehearsal \
         starts until each is resolved (just council-resolve-refusal)",
        pending.len(),
        config.refusals_dir.display()
    )
}

/// The loud listing of unresolved dumps.
pub fn print_pending(pending: &[Pending]) {
    let bar = "  !! ==============================================================";
    println!("\n{bar}");
    println!(
        "  !! {} ABORTED SITTING(S) PENDING INVESTIGATION. The Council does not sit.",
        pending.len()
    );
    println!("  !! After a refusal, a sitting over the same context would be a blind");
    println!("  !! retry, and repeat refusals put the API account at risk. After a");
    println!("  !! decision emitted out of order, strict decoding is unverified. After a");
    println!("  !! Steward stop, the Council has not yet heard it.");
    for p in pending {
        println!("  !!");
        println!("  !!   {}", p.dir.display());
        match &p.manifest {
            Ok(m) => println!(
                "  !!     sealed {}; {} from the {:?} at {}",
                m.sealed_at.format("%Y-%m-%d %H:%M UTC"),
                m.cause.short(),
                m.cause.role(),
                m.stage
            ),
            Err(e) => println!("  !!     manifest unreadable ({e}); counted as pending"),
        }
    }
    println!("  !!");
    println!("  !! Investigate the sealed context, then resolve each one:");
    println!("  !!   cleaned    - (a refusal) the offending context was removed");
    println!("  !!   reverified - (an emission breach) strict emission order was");
    println!("  !!                re-verified on the wire");
    println!("  !!   reverted   - the seat's model was reverted (SEAT_MODEL_OVERRIDE),");
    println!("  !!                an emergency act: Art. IV § 3, 72-hour Council review");
    println!("  !!   heard:<id> - (a Steward stop) the Council heard it, in the GOV- or");
    println!("  !!                REC- entry named");
    println!(
        "  !!   just council-resolve-refusal <dir> <cleaned|reverted|reverified|heard:<id>> \"<one sentence>\""
    );
    println!("{bar}\n");
}

fn read_manifest(dir: &Path) -> anyhow::Result<Manifest> {
    let path = dir.join(MANIFEST);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// `resolve-refusal <dump-dir> --resolution <cleaned|reverted> --reason "<one sentence>"`.
pub async fn resolve_main(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let usage = "usage: agora-council resolve-refusal <dump-dir> --resolution \
                 <cleaned|reverted|reverified|heard:<GOV-or-REC id>> --reason \"<one sentence>\"";
    let dir = args
        .first()
        .filter(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow::anyhow!("{usage}"))?;
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .ok_or_else(|| anyhow::anyhow!("{flag} is required; {usage}"))
    };
    let resolution: Resolution = value("--resolution")?.parse()?;
    let reason = value("--reason")?;
    let mut editor = rustyline::Editor::<(), rustyline::history::DefaultHistory>::new()?;
    resolve(config, Path::new(dir), resolution, reason, &mut editor)?;
    Ok(())
}

/// Record `resolution` in the dump's manifest, after the Steward types the
/// dump's name to confirm. Refuses if the sealed files changed, if it is
/// already resolved, or if `reverted` is claimed while the seat still runs
/// the model the refusal came from.
pub fn resolve(
    config: &Config,
    dir: &Path,
    resolution: Resolution,
    reason: &str,
    input: &mut impl StewardInput,
) -> anyhow::Result<Resolved> {
    let reason = reason.trim();
    anyhow::ensure!(!reason.is_empty(), "--reason must not be empty");
    anyhow::ensure!(
        !reason.contains('\n'),
        "--reason is one sentence, on one line"
    );

    let root = config
        .refusals_dir
        .canonicalize()
        .with_context(|| format!("reading {}", config.refusals_dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("reading {}", dir.display()))?;
    anyhow::ensure!(
        dir.parent() == Some(root.as_path()),
        "{} is not a dump directory under {}; the guard reads only there",
        dir.display(),
        root.display()
    );
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("{} has no name", dir.display()))?
        .to_string();

    let mut manifest = read_manifest(&dir)?;
    if let Some(done) = &manifest.resolution {
        anyhow::bail!(
            "{name} is already resolved ({}, {})",
            done.resolution,
            done.resolved_at
        );
    }
    for file in &manifest.files {
        let path = dir.join(&file.name);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(
            sha256_hex(&bytes) == file.sha256,
            "{} no longer matches its sealed SHA-256; the evidence changed after \
             sealing. Not resolving.",
            path.display()
        );
    }

    // The stop's own record is not where the Council heard it. Whether the
    // entry named exists, and is where the Council heard the stop, the
    // program cannot check here (no database): the resolver must name it.
    if let (Resolution::Heard(id), Some(own)) = (&resolution, &manifest.recorded)
        && id == own
    {
        anyhow::bail!(
            "{id} is this stop's own record; name the Council decision (GOV-) or the \
             record (REC-) in which the Council heard it"
        );
    }
    let seat_model_now = match (&resolution, &manifest.cause) {
        (Resolution::Cleaned, CauseFacts::Refusal(_))
        | (Resolution::Reverified, CauseFacts::UnsoundEmission(_))
        | (Resolution::Heard(_), CauseFacts::StewardStop(_)) => None,
        (Resolution::Heard(_), _) => anyhow::bail!(
            "`heard` is for a Steward stop; this sitting stopped on {}. Use `cleaned`, \
             `reverified` or `reverted`.",
            manifest.cause.short()
        ),
        (_, CauseFacts::StewardStop(_)) => anyhow::bail!(
            "a Steward stop is resolved only when the Council has heard it: \
             `heard:<GOV-or-REC id>`, naming the decision or record in which it did"
        ),
        (Resolution::Cleaned, CauseFacts::UnsoundEmission(_)) => anyhow::bail!(
            "`cleaned` is for a refusal; this sitting stopped on a decision emitted out of \
             declared order. Re-verify strict emission order and resolve as `reverified`, \
             or revert the seat's model and resolve as `reverted`."
        ),
        (Resolution::Reverified, CauseFacts::Refusal(_)) => anyhow::bail!(
            "`reverified` is for a decision emitted out of order; this sitting stopped on \
             a refusal. Use `cleaned` or `reverted`."
        ),
        (Resolution::Reverted, cause) => {
            let seat = seat_of(cause.role()).ok_or_else(|| {
                anyhow::anyhow!(
                    "`reverted` reverts a seat's model, and this abort came from the {:?}; \
                     use `cleaned`",
                    cause.role()
                )
            })?;
            let sealed = manifest
                .seat_models
                .iter()
                .find(|m| m.seat == seat)
                .map(|m| m.model.clone())
                .unwrap_or_else(|| cause.model().to_string());
            let now = seat_model(seat, config);
            anyhow::ensure!(
                now != sealed,
                "the {seat} still runs {now}, the model the abort came from. Revert it \
                 in SEAT_MODEL_OVERRIDE (a reviewed commit) before resolving as `reverted`."
            );
            Some(now)
        }
    };

    println!("\n  Resolving {name}");
    println!(
        "    {} from the {:?} at {}",
        manifest.cause.short(),
        manifest.cause.role(),
        manifest.stage
    );
    println!("    resolution: {resolution}");
    println!("    reason:     {reason}");
    if let Some(now) = &seat_model_now {
        println!("    the seat now runs {now}");
        println!(
            "    Reverting a seat's model is an emergency act (Constitution Art. IV § 3): \
             the Council reviews it within 72 hours. Disclose it in \
             governance/council-operations.md."
        );
    }
    let typed = input.readline(&format!("  Type the dump's name ({name}) to confirm > "))?;
    if typed.trim() != name {
        anyhow::bail!("not confirmed; nothing was changed");
    }

    let resolved = Resolved {
        resolution: resolution.clone(),
        reason: reason.to_string(),
        resolved_at: Utc::now(),
        seat_model_now,
    };
    manifest.resolution = Some(resolved.clone());
    rewrite_manifest(&dir, &manifest)?;

    tracing::warn!(
        event_type = "council_refusal_resolved",
        dump = %dir.display(),
        resolution = %resolution,
        reason,
        "a Council abort investigation was resolved"
    );
    println!(
        "\n  Resolved. The manifest records it: {}",
        dir.join(MANIFEST).display()
    );
    if let Some(record) = &manifest.record {
        println!(
            "  Publishing the sealed context later is a manual amendment to the record \
             drafted at {}.",
            record.display()
        );
    }
    if let Some(post) = manifest.proposal {
        println!("  To hear the item again: just council-run {post} --force");
    }
    Ok(resolved)
}

#[cfg(test)]
#[path = "abort_tests.rs"]
pub(crate) mod tests;

/// Replace a dump's manifest, atomically (a new owner-only file renamed
/// over it). The sealed files are never touched.
fn rewrite_manifest(dir: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(MANIFEST))
        .with_context(|| format!("replacing {}", dir.join(MANIFEST).display()))
}

/// Note in a stop's manifest the id its record was appended as, so that
/// `heard:` cannot name it.
pub fn note_recorded(dir: &Path, id: &GovernanceLogId) -> anyhow::Result<()> {
    let mut manifest = read_manifest(dir)?;
    manifest.recorded = Some(id.clone());
    rewrite_manifest(dir, &manifest)
}
