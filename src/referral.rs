//! Appeals the Court referred to the Council (agora#471).
//!
//! **How a referral reaches the agenda.** By hand: the Steward names it
//! with `--referrals <appeal-uuid,…>` when convening (Protocol § 2.3:
//! "a case the Court refers to the Council must still be placed on an
//! agenda by hand"). A typed flag of its own rather than `--items`,
//! because `--items` takes post ids and an appeal id is the same shape —
//! one list that could hold either would be an untyped id deciding which
//! table it names. Referrals are heard first (Art. VI § 2: "at its next
//! available opportunity") and are added to whatever else the agenda
//! holds.
//!
//! **Category.** `routine` — Constitution Art. IV § 3's table names
//! "Individual moderation decisions" as the Routine example, and a
//! referral decides one moderation action (Art. VI § 2: "The Council's
//! decision is final for that moderation action"). This is not free of
//! doubt: Art. VI § 1 makes Tier 3 decisions "binding moderation
//! precedent", and Protocol § 3.1 refers exactly the cases that turn on
//! uninterpreted provisions or would "effectively create policy" — the
//! Policy row's territory. Protocol § 2.1 lets the Council decide an item
//! at the threshold its substance requires, and the item content says so.
//!
//! **The motion.** YES = overturn the moderation action; NO = uphold it
//! (Steward, 2026-09-29). Stated in the item every seat reads and again in
//! the vote call, in plain words, including what a YES does to the
//! appellant: [`MOTION`].
//!
//! **Write-back.** [`write_back`] runs inside `record_decision`'s
//! transaction, which also holds the `council_decisions` row and the
//! signed, chained log entry (signing is in-process, inside that same
//! transaction), so the appeal's outcome commits or rolls back with the
//! decision itself. Approved → `overturned` (and the action marked
//! reversed, for the appeals service's remedy sweep to carry out);
//! Rejected → `upheld`; Deferred/Amended → nothing, the appeal stays
//! referred. Clearing `outcome_notified_at` hands the appellant's notice
//! to the server's outcome-notice sweep.

use std::fmt::Write as _;

use agora_common::enums::{AppealOutcome, ModerationActionType, ModerationTier};
use agora_common::ids::{AgendaItemId, AppealId, ContentId, GovernanceLogId};
use serde::Deserialize;

use crate::models::DecisionOutcome;
use crate::queries::{ReferralCaseRow, WithheldRow};

/// The motion, as every seat reads it with the item.
pub const MOTION: &str = "### The motion on this referral\n\n\
     **A YES vote OVERTURNS the moderation action. A NO vote UPHOLDS it.**\n\n\
     - **YES** reverses the action against the appellant: it is struck from \
     their moderation record, removed content is restored or a suspension \
     lifted (a permanent ban is lifted by a human), and the appeal credit \
     they spent is returned.\n\
     - **NO** leaves the action standing exactly as it was imposed.\n\n\
     In the final vote only YES counts toward overturning: a `no`, \
     `abstain` or `defer` there all leave the action standing. A reduced \
     sanction (\"modify\") cannot be voted: if you think the action should \
     stand but be reduced, say so during deliberation and vote `defer` in \
     round 1 — the Steward may table the item and record a modification by \
     hand. The Council's decision is final for this moderation action \
     (Art. VI § 2).";

/// The motion again, closing the vote call.
pub const VOTE_REMINDER: &str = "**This is a referred appeal. YES = OVERTURN the moderation \
     action: it is reversed, the appellant's record is cleared of it, \
     removed content is restored or a suspension lifted, and their appeal \
     credit is returned. NO = UPHOLD it: the action stands as imposed.** \
     Only YES counts toward overturning.";

/// What the item content says about the category it is heard at.
pub const CATEGORY_NOTE: &str = "Seated as **Routine** (3 of 5): Art. IV § 3 names individual \
     moderation decisions as Routine. If this case would effectively set \
     policy, Protocol § 2.1 lets the Council decide it at the threshold its \
     substance requires — say so on the record.";

/// What a referral decision does to the appeal. `None`: nothing is
/// written and the appeal stays referred.
pub fn appeal_outcome(outcome: &DecisionOutcome) -> Option<AppealOutcome> {
    match outcome {
        DecisionOutcome::Approved => Some(AppealOutcome::Overturned),
        DecisionOutcome::Rejected => Some(AppealOutcome::Upheld),
        // Tabled, or a shape a yes/no vote cannot produce: the Steward
        // records any modification by hand.
        DecisionOutcome::Deferred | DecisionOutcome::Amended => None,
    }
}

fn tier(tier: ModerationTier) -> &'static str {
    match tier {
        ModerationTier::Tier1 => "Tier 1 (automated)",
        ModerationTier::Tier2 => "Tier 2 (AI review)",
        ModerationTier::Tier3 => "Tier 3 (Council)",
    }
}

/// The agenda item's title. Published with the decision, so it names the
/// sanction and provision, never a party.
pub fn title(
    appeal_id: AppealId,
    action: ModerationActionType,
    constitutional_ref: &str,
) -> String {
    let short: String = appeal_id.to_string().chars().take(8).collect();
    format!(
        "Referred appeal {short}: {} under {}",
        action.words(),
        constitutional_ref.trim()
    )
}

// ---------------------------------------------------------------------------
// The Court's record, typed
// ---------------------------------------------------------------------------

/// A verdict as the Court stores it
pub use agora_common::moderation_records::AppealVerdict;

/// One juror's verdict, as stored in `appeals.jury_verdicts`.
#[derive(Debug, Clone, Deserialize)]
pub struct JurorVerdict {
    pub juror_number: u8,
    pub verdict: AppealVerdict,
    pub rationale: String,
    #[serde(default)]
    pub constitutional_refs: Vec<String>,
    #[serde(default)]
    pub context_analysis: String,
}

/// The judge's ruling, as stored in `appeals.judge_ruling`.
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeRuling {
    pub verdict: AppealVerdict,
    pub rationale: String,
    #[serde(default)]
    pub jury_assessment: String,
    #[serde(default)]
    pub referral_reason: Option<String>,
    #[serde(default)]
    pub context_analysis: String,
    #[serde(default)]
    pub precedents_cited: Vec<String>,
    #[serde(default)]
    pub clamped_from: Option<AppealVerdict>,
}

/// One node of the redacted record (`appeal_redactions.redacted_citations`).
#[derive(Debug, Clone, Deserialize)]
pub struct RedactedCitation {
    pub content_id: ContentId,
    #[serde(default)]
    pub redacted_title: Option<String>,
    pub transcription: String,
}

/// The jury floor ([`agora_common::moderation_records::jury_floor`])
pub fn jury_floor(verdicts: &[JurorVerdict]) -> Option<AppealVerdict> {
    agora_common::moderation_records::jury_floor(verdicts.iter().map(|v| v.verdict))
}

/// A referred case, loaded and typed.
#[derive(Debug, Clone)]
pub struct ReferralCase {
    pub row: ReferralCaseRowView,
    pub citations: Vec<RedactedCitation>,
    pub withheld: Vec<(ContentId, String)>,
    pub jury: Vec<JurorVerdict>,
    pub ruling: Option<JudgeRuling>,
}

/// The scalar half of [`ReferralCaseRow`], owned by the case.
#[derive(Debug, Clone)]
pub struct ReferralCaseRowView {
    pub appeal_id: AppealId,
    pub filed_at: chrono::DateTime<chrono::Utc>,
    pub referred_at: Option<chrono::DateTime<chrono::Utc>>,
    pub provisional_relief: bool,
    pub action_type: ModerationActionType,
    pub tier: ModerationTier,
    pub constitutional_ref: String,
    pub action_at: chrono::DateTime<chrono::Utc>,
    pub redacted_statement: String,
    pub redacted_reason: String,
    pub redactor_notes: Option<String>,
    pub court_entry: Option<GovernanceLogId>,
}

/// Why a referral cannot be heard as it stands.
#[derive(Debug)]
pub enum CaseError {
    NotReferred(AppealId, agora_common::enums::AppealStatus),
    Unsealed(AppealId),
    Record {
        appeal: AppealId,
        field: &'static str,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for CaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaseError::NotReferred(id, status) => write!(
                f,
                "appeal {id} is not referred to the Council (status: {status:?})"
            ),
            CaseError::Unsealed(id) => write!(
                f,
                "appeal {id} has no sealed redaction: the Council hears only the \
                 Court's anonymized record, and its deliberation is published"
            ),
            CaseError::Record {
                appeal,
                field,
                source,
            } => write!(
                f,
                "appeal {appeal}: the stored {field} does not parse: {source}"
            ),
        }
    }
}

impl std::error::Error for CaseError {}

impl ReferralCase {
    /// Type the stored record. Refuses an appeal that is no longer
    /// referred, or whose redaction never sealed.
    pub fn from_rows(row: ReferralCaseRow, withheld: Vec<WithheldRow>) -> Result<Self, CaseError> {
        use agora_common::enums::AppealStatus;
        let appeal = row.appeal_id;
        if row.status != AppealStatus::ReferredToCouncil {
            return Err(CaseError::NotReferred(appeal, row.status));
        }
        let (Some(_), Some(statement), Some(reason)) =
            (row.sealed_at, row.redacted_statement, row.redacted_reason)
        else {
            return Err(CaseError::Unsealed(appeal));
        };
        let record = |field: &'static str| {
            move |source| CaseError::Record {
                appeal,
                field,
                source,
            }
        };
        let citations: Vec<RedactedCitation> = match row.redacted_citations {
            Some(v) => serde_json::from_value(v).map_err(record("redacted_citations"))?,
            None => Vec::new(),
        };
        let jury: Vec<JurorVerdict> = match row.jury_verdicts {
            Some(serde_json::Value::Null) | None => Vec::new(),
            Some(v) => serde_json::from_value(v).map_err(record("jury_verdicts"))?,
        };
        let ruling: Option<JudgeRuling> = match row.judge_ruling {
            Some(serde_json::Value::Null) | None => None,
            Some(v) => Some(serde_json::from_value(v).map_err(record("judge_ruling"))?),
        };
        Ok(Self {
            row: ReferralCaseRowView {
                appeal_id: appeal,
                filed_at: row.filed_at,
                referred_at: row.referred_at,
                provisional_relief: row.provisional_relief,
                action_type: row.action_type,
                tier: row.tier,
                constitutional_ref: row.constitutional_ref,
                action_at: row.action_at,
                redacted_statement: statement,
                redacted_reason: reason,
                redactor_notes: row.redactor_notes,
                court_entry: row.court_entry,
            },
            citations,
            withheld: withheld
                .into_iter()
                .map(|w| (w.content_id, w.reason))
                .collect(),
            jury,
            ruling,
        })
    }

    /// The item body every seat reads: the Court's anonymized record, the
    /// verdicts and the ruling that sent it here. Defanged once, whole,
    /// as community text is everywhere else it becomes prompt material.
    pub fn body(&self) -> String {
        let r = &self.row;
        let mut s = String::new();
        let date = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%d").to_string();

        let _ = writeln!(
            s,
            "A case the Appeals Court referred to the Council (Art. VI § 2; \
             Protocol § 3.1). Everything below is the Court's own record, \
             anonymized by its redactor: the appellant is `[APPELLANT]` and \
             other parties are pseudonyms. Refer to them only that way.\n"
        );
        let _ = writeln!(s, "**Appeal**: {}", r.appeal_id);
        let _ = writeln!(s, "**Filed**: {}", date(r.filed_at));
        if let Some(at) = r.referred_at {
            let _ = writeln!(s, "**Referred**: {}", date(at));
        }
        let _ = writeln!(
            s,
            "**Court record**: {}",
            r.court_entry.as_ref().map_or_else(
                || "no decision entry found".to_string(),
                |id| id.to_string()
            )
        );
        let _ = writeln!(
            s,
            "**Provisional relief**: {}\n",
            if r.provisional_relief {
                "yes — the jury's floor was overturn, so the appellant's \
                 credit was restored on referral"
            } else {
                "no"
            }
        );
        let _ = writeln!(s, "{CATEGORY_NOTE}\n");

        let _ = writeln!(
            s,
            "#### The moderation action\n\n\
             **Sanction**: {}\n\
             **Tier**: {}\n\
             **Imposed**: {}\n\
             **Constitutional provision**: {}\n\
             **Published reason** (redacted):\n\n{}\n",
            r.action_type.words(),
            tier(r.tier),
            date(r.action_at),
            r.constitutional_ref.trim(),
            r.redacted_reason.trim(),
        );

        let _ = writeln!(
            s,
            "#### The appeal statement (redacted)\n\n{}\n",
            r.redacted_statement.trim()
        );

        let _ = writeln!(
            s,
            "#### The record before the Court\n\n\
             The content under appeal and what the appellant cited, as the \
             Court's redactor transcribed it — the text the jury and judge \
             read."
        );
        if self.citations.is_empty() {
            let _ = writeln!(s, "\n(No transcribed content on record.)");
        }
        for c in &self.citations {
            let withheld = self.withheld.iter().any(|(id, _)| *id == c.content_id);
            let _ = write!(s, "\n##### {}", c.content_id);
            if let Some(t) = &c.redacted_title {
                let _ = write!(s, " [title: {}]", t.trim());
            }
            if withheld {
                s.push_str(" [withheld from the jury and the bench by chambers]");
            }
            let _ = writeln!(s, "\n\n{}", c.transcription.trim());
        }
        if let Some(notes) = &r.redactor_notes {
            let _ = writeln!(s, "\n**Redactor's notes**: {}", notes.trim());
        }

        if !self.withheld.is_empty() {
            // Protocol § 3.1: the bench may not rule on withheld material;
            // referral is the remedy. The Council is the body that can.
            let _ = writeln!(
                s,
                "\n#### Material chambers withheld\n\n\
                 The jury and the judge did not see these items. The bench may \
                 not rule on withheld material (Protocol § 3.1); the Council \
                 may. Chambers' reasons:\n"
            );
            for (id, reason) in &self.withheld {
                let _ = writeln!(s, "- {id}: {}", reason.trim());
            }
        }

        let _ = writeln!(s, "\n#### The jury\n");
        if self.jury.is_empty() {
            let _ = writeln!(s, "No jury verdicts are on record.");
        }
        for v in &self.jury {
            let _ = writeln!(
                s,
                "**Juror #{}: {}**\n\n\
                 Context analysis: {}\n\n\
                 Rationale: {}\n\n\
                 Provisions: {}\n",
                v.juror_number,
                v.verdict.as_str(),
                v.context_analysis.trim(),
                v.rationale.trim(),
                if v.constitutional_refs.is_empty() {
                    "none".to_string()
                } else {
                    v.constitutional_refs.join(", ")
                },
            );
        }
        if let Some(floor) = jury_floor(&self.jury) {
            let _ = writeln!(
                s,
                "**Jury floor** (the median juror): {}. The floor bound \
                 the Court's judge, who could not rule less favourably to the \
                 appellant than this (Protocol § 3.5). Whether it binds the \
                 Council is not settled: a proposal that it should is \
                 pending, and until the Council decides that question, the \
                 floor is not enforced on the Council's ruling. Weigh the \
                 jury's position as part of the record.\n",
                floor.as_str()
            );
        }

        let _ = writeln!(s, "#### The judge's ruling and referral\n");
        match &self.ruling {
            None => {
                let _ = writeln!(s, "No ruling is on record.");
            }
            Some(j) => {
                let _ = writeln!(s, "**Verdict the judge reached**: {}", j.verdict.as_str());
                if let Some(from) = j.clamped_from {
                    let _ = writeln!(
                        s,
                        "(raised by the jury-floor rule from the judge's own {})",
                        from.as_str()
                    );
                }
                let _ = writeln!(
                    s,
                    "\n**Why the judge referred**: {}\n",
                    j.referral_reason
                        .as_deref()
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                        .unwrap_or("(no reason recorded)")
                );
                let _ = writeln!(
                    s,
                    "**Context analysis**: {}\n\n\
                     **Assessment of the jury**: {}\n\n\
                     **Reasoning**: {}",
                    j.context_analysis.trim(),
                    j.jury_assessment.trim(),
                    j.rationale.trim(),
                );
                if !j.precedents_cited.is_empty() {
                    let _ = writeln!(
                        s,
                        "\n**Precedents cited**: {}",
                        j.precedents_cited.join(", ")
                    );
                }
            }
        }

        agora_common::clerk::defang(&s).into_owned()
    }
}

// ---------------------------------------------------------------------------
// Write-back
// ---------------------------------------------------------------------------

/// What recording a decision did to a referred appeal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteBack {
    /// The item was not a referral.
    NotAReferral,
    /// Tabled (or otherwise not a yes/no outcome): the appeal stays
    /// referred.
    Unchanged(AppealId),
    /// The outcome was written, the notice handed to the server's sweep.
    Decided {
        appeal: AppealId,
        outcome: AppealOutcome,
        /// On an overturn: whether this write marked the action reversed
        /// (false if something had already reversed it).
        reversed: bool,
    },
    /// The appeal was no longer `referred_to_council` — decided by hand
    /// meanwhile. Nothing was written; the Steward must reconcile.
    NoLongerReferred(AppealId),
}

/// Write a referral decision back to its appeal, inside the caller's
/// transaction (the decision's own).
pub async fn write_back(
    conn: &mut sqlx::PgConnection,
    agenda_item_id: AgendaItemId,
    decision_id: &GovernanceLogId,
    outcome: &DecisionOutcome,
) -> Result<WriteBack, sqlx::Error> {
    let Some(appeal) = crate::queries::referral_of_item(conn, agenda_item_id).await? else {
        return Ok(WriteBack::NotAReferral);
    };
    let Some(appeal_outcome) = appeal_outcome(outcome) else {
        return Ok(WriteBack::Unchanged(appeal));
    };
    // The reversal first: its policy requires the appeal still referred.
    let reversed = match appeal_outcome {
        AppealOutcome::Overturned => {
            crate::queries::mark_referred_action_reversed(conn, appeal).await? > 0
        }
        AppealOutcome::Upheld | AppealOutcome::Modified | AppealOutcome::Referred => false,
    };
    let written =
        crate::queries::decide_referred_appeal(conn, appeal, appeal_outcome, decision_id).await?;
    if written == 0 {
        return Ok(WriteBack::NoLongerReferred(appeal));
    }
    Ok(WriteBack::Decided {
        appeal,
        outcome: appeal_outcome,
        reversed,
    })
}

/// Tell the Steward (and the log) what the write-back did, after commit.
pub fn report(decision_id: &GovernanceLogId, write_back: &WriteBack) {
    match write_back {
        WriteBack::NotAReferral => {}
        WriteBack::Unchanged(appeal) => {
            tracing::info!(
                decision = %decision_id,
                appeal_id = %appeal,
                "referral not decided by vote; the appeal stays referred"
            );
            println!(
                "  Appeal {appeal} stays referred to the Council: the item was not \
                 decided yes/no. A modification is recorded by hand."
            );
        }
        WriteBack::Decided {
            appeal,
            outcome,
            reversed,
        } => {
            tracing::info!(
                decision = %decision_id,
                appeal_id = %appeal,
                outcome = %outcome,
                action_reversed = reversed,
                "referred appeal decided by the Council"
            );
            println!(
                "  Appeal {appeal}: {outcome} by the Council ({decision_id}). \
                 The appellant's notice is queued for the server's outcome sweep{}.",
                if *reversed {
                    "; the appeals service carries out the reversal first"
                } else {
                    ""
                }
            );
        }
        WriteBack::NoLongerReferred(appeal) => {
            tracing::error!(
                decision = %decision_id,
                appeal_id = %appeal,
                "the Council decided a referral whose appeal is no longer referred; \
                 nothing was written to the appeal"
            );
            println!(
                "\n  !! Appeal {appeal} was no longer referred to the Council when \
                 {decision_id} was recorded (decided by hand meanwhile?).\n     \
                 The decision is recorded; the appeal was NOT changed. Reconcile by hand."
            );
        }
    }
}

/// Load a referral item's case: the typed record and the rendered body.
pub async fn load(pool: &sqlx::PgPool, appeal_id: AppealId) -> anyhow::Result<ReferralCase> {
    let row = crate::queries::referral_case(pool, appeal_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no appeal {appeal_id}"))?;
    let withheld = crate::queries::referral_withheld(pool, appeal_id).await?;
    Ok(ReferralCase::from_rows(row, withheld)?)
}

/// Check the Steward's `--referrals` list: every id an appeal still
/// referred, with a sealed record. Returns the agenda titles, in order.
/// Errors name every id that cannot be seated — a silently shorter
/// agenda is worse than a refusal.
pub async fn check_seats(
    pool: &sqlx::PgPool,
    appeal_ids: &[AppealId],
) -> anyhow::Result<Vec<(AppealId, String)>> {
    use agora_common::enums::AppealStatus;
    let seats = crate::queries::referral_seats(pool, appeal_ids).await?;
    let mut problems = Vec::new();
    for id in appeal_ids {
        match seats.iter().find(|s| s.appeal_id == *id) {
            None => problems.push(format!("{id}: no such appeal")),
            Some(s) if s.status != AppealStatus::ReferredToCouncil => problems.push(format!(
                "{id}: not referred to the Council ({:?})",
                s.status
            )),
            Some(s) if !s.sealed => problems.push(format!(
                "{id}: no sealed redaction — the Council hears only the anonymized record"
            )),
            Some(_) => {}
        }
    }
    anyhow::ensure!(
        problems.is_empty(),
        "--referrals: cannot seat {}",
        problems.join("; ")
    );
    Ok(seats
        .iter()
        .map(|s| {
            (
                s.appeal_id,
                title(s.appeal_id, s.action_type, &s.constitutional_ref),
            )
        })
        .collect())
}

/// Seat the checked referrals on a meeting's agenda, ahead of everything
/// else, in the order given (Art. VI § 2: heard "at its next available
/// opportunity").
pub async fn seat(
    pool: &sqlx::PgPool,
    meeting_id: agora_common::ids::CouncilMeetingId,
    seats: &[(AppealId, String)],
) -> anyhow::Result<()> {
    for (n, (appeal_id, title)) in seats.iter().enumerate() {
        // Above a Steward-set proposal list (1_000_000 - n).
        let priority = 2_000_000 - n as i32;
        crate::queries::seat_referral(pool, meeting_id, *appeal_id, title, priority).await?;
        tracing::info!(
            meeting = %meeting_id,
            appeal_id = %appeal_id,
            "appeal referral seated by the Steward"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "referral_tests.rs"]
mod tests;
