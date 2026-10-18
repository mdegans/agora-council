//! Each decision's summary: written by the Lawyer at the end of the item,
//! from the decision's record, and accepted before the record is signed
//! with it (Steward, 2026-10-17; agentkit 0.96 `DecisionSummary`).
//!
//! **There is one summary per decision, and it is the Lawyer's.** Until
//! 2026-10-17 a decision had the Clerk's unsigned summary (Sonnet 5,
//! `clerk::summarize_decision`) and a meeting had the Lawyer's, reviewed by
//! the Steward at adjournment. Now the Lawyer's review rule runs per
//! decision, and the meeting's summary is composed from its decisions'
//! (`crate::adjournment`). The rule keeps its two halves:
//!
//! - **Nothing the Lawyer writes is signed without its reviewers'
//!   acceptance.** On an item the Steward sat on, he reviews each draft
//!   ([`review`]: accept, input, dissent or withhold). On an item he was
//!   recused from, he may neither shape the account nor suppress it
//!   (Constitution Art. IV § 3; Steward, 2026-10-13), so the Artist, the
//!   Philosopher and the Engineer review it instead ([`SeatReview`]), and
//!   all three must accept the same text. They have [`REVIEW_ROUNDS`]
//!   rounds to agree.
//! - **No reviewer can put words into the Lawyer's text.** Input goes to
//!   the Lawyer, who revises and may decline, saying why. The signed text
//!   is the Lawyer's latest draft, verbatim.
//!
//! **Always append, never prune** (Steward, 2026-10-17). Every model call
//! here continues that seat's *own* conversation for the item, the
//! [`Councillor`] that deliberated it: the system prompt, its identity, the
//! item, every round and its thinking stay where they are, and the summary
//! is one more user turn after its vote, then its answer, then any input
//! and revision after that ([`Councillor::after_vote`]). Nothing is
//! removed or rewritten, and the cached prefix is the deliberation's. The
//! after-vote shapes (`summary`, `summary_review`) are in every seat's
//! response format from its first turn, because the format is part of the
//! request a `CachedPrompt` fixes, and they are refused until asked for.
//! So this runs inside `deliberation::deliberate`, while the seats are
//! still seated, before the decision is recorded.
//!
//! What can come of it ([`Outcome`]): the Steward accepts, or lets the
//! text be signed unchanged with his dissent attached to the record
//! (`summary-review.md`, under his name); the seats accept; or there is no
//! summary: the Steward withholds it, or the seats do not agree within
//! [`REVIEW_ROUNDS`]. Then the decision is **signed without a summary**
//! (agentkit's rule admits `None`), said loudly, with `summary-review.md`
//! recording what happened (the seats' comments, round by round, and the
//! last draft none of them all accepted). A summary can be added later
//! only by a signed revision (`--set-summary`).
//!
//! **Every draft passes the corpus gate before anyone sees it**
//! ([`agora_common::precedents::council_summary_gate`]): an empty draft
//! goes back to the Lawyer with the reason, up to
//! [`GATE_REVISIONS`] times, because the signed text is what the precedent
//! corpus will serve.
//!
//! **A failed call is the Steward's to retry.** If a turn fails (an error
//! after the automatic retries), the Steward is told why and chooses: ask
//! again (the same conversation, sampled again with nothing added), or
//! (seated only) withhold the summary. Recused, he is offered only the
//! retry. Drafts that keep failing the corpus gate go to him the same way;
//! recused, they end the summary.
//!
//! **Every way out after the vote signs the vote or seals it** (review,
//! 2026-10-17):
//! - A **refusal**, or a submission out of declared order, aborts the
//!   sitting ([`crate::abort`]) at [`Stage::DecisionSummary`], as in any
//!   round: the record that would be signed may carry what drew it, so
//!   nothing is signed, the item is undecided, and every seat's
//!   conversation is sealed as it stands.
//! - A **Steward stop** (Ctrl-C) never discards a vote already cast,
//!   whether he is recused (where discarding it would be a veto he does
//!   not have) or seated: the summary ends ([`Outcome::Stopped`]), the
//!   decision is signed without one, `summary-review.md` giving the stop
//!   and his reason, and then the stop is sealed and recorded as any stop
//!   is, at `Stage::SummaryStopped` (`meeting::seal_summary_stop`).
//! - **Anything else** (the gate exhausted with the Steward recused, the
//!   terminal gone): the decision is signed without a summary, the reason
//!   attached ([`Outcome::Failed`]).
//!
//! Every draft, input, review and result is logged as a structured event
//! (`event_type = "decision_summary_*"`), and every prompt is archived
//! with the seat's other calls on the item (stage `summary` or
//! `summary_review`).

use std::fmt::Write as _;

use misanthropic::prompt::message::{Block, Content};

use agora_common::govlog::{CouncilAttachment, RECUSED_ACCEPTORS, Redactable};
use agora_common::ids::GovernanceLogId;
use agora_common::refusal::Refused;

use crate::abort::{self, ClerkBriefing, Moment, Stage};
use crate::councillor::Councillor;
use crate::docket::{Docket, ProposalReader};
use crate::models::{AgendaItem, CouncilSeat, CouncilVote, Decision};
use crate::steward::{StewardInput, read_multiline};
use crate::wire::RawTransport;

use crate::record::Submission;
pub use crate::record::{AfterVote, SeatReview, SummaryDraft};

/// The Lawyer's charge for this task. Version-controlled policy, like
/// every governance prompt (CLAUDE.md).
const SUMMARY_CHARGE: &str = include_str!("../../../prompts/lawyer_decision_summary.md");

/// A reviewing seat's charge, when the Steward is recused.
const REVIEW_CHARGE: &str = include_str!("../../../prompts/seat_summary_review.md");

/// How many rounds of review the three seats have to accept the same
/// text, the Steward being recused. At the cap the decision is signed
/// without a summary.
pub const REVIEW_ROUNDS: u32 = 3;

/// How many times a draft that fails the corpus gate goes back to the
/// Lawyer before it counts as a failed draft.
pub const GATE_REVISIONS: u32 = 2;

/// The attachment that records a summary's review when there is more to
/// say than who accepted it.
pub const REVIEW_ATTACHMENT: &str = "summary-review.md";

/// The Steward's answer at the checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    /// Sign the draft as written.
    Accept,
    /// Send this to the Lawyer, who revises.
    Input(String),
    /// Sign the draft unchanged, with this dissent attached under his name.
    Dissent(String),
    /// Sign the decision without a summary.
    Withhold,
}

/// How a decision's summary came out
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The Steward accepted draft `draft` as written
    Accepted { draft: u32 },
    /// The Steward let draft `draft` be signed unchanged, with his dissent
    /// in `summary-review.md`
    Dissented { draft: u32, dissent: String },
    /// The Artist, Philosopher and Engineer accepted draft `draft`, in
    /// review round `round`, the Steward being recused
    AcceptedBySeats { draft: u32, round: u32 },
    /// The Steward withheld the summary at draft `draft` (0: the first
    /// draft failed)
    Withheld { draft: u32 },
    /// The seats did not all accept one draft within [`REVIEW_ROUNDS`]
    Unagreed { draft: u32 },
    /// The Steward stopped the sitting during the summary (Ctrl-C). A stop
    /// never discards a vote already cast: the decision is signed without a
    /// summary, then the stop is sealed and recorded
    /// (`Stage::SummaryStopped`)
    Stopped { facts: crate::stop::StopFacts },
    /// The summary could not be completed for another reason (the Lawyer's
    /// drafts kept failing the corpus gate with the Steward recused, the
    /// Steward's terminal failed, …): the decision is signed without a
    /// summary, the reason attached
    Failed { reason: String },
}

/// What is signed with the decision: the Lawyer's accepted text, if any,
/// and the review's attachment, if there is one
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The Lawyer's text, verbatim; `None` when the decision is signed
    /// without a summary
    pub text: Option<String>,
    /// `summary-review.md`, when the review left more to record than its
    /// acceptance
    pub review: Option<CouncilAttachment>,
    pub outcome: Outcome,
}

/// Print a draft: its heading, the Lawyer's note and the summary
fn show(heading: &str, note: &str, summary: &str) {
    println!("\n================================================");
    println!("  {heading}");
    println!("================================================");
    if !note.trim().is_empty() {
        println!("\n  -- The Lawyer's note --\n\n{}", note.trim());
    }
    println!("\n  -- Summary --\n\n{}\n", summary.trim());
}

/// Show a draft to a seated Steward and read his answer. A Steward recused
/// from the item is never asked: the seats review it.
pub fn review(
    input: &mut impl StewardInput,
    heading: &str,
    note: &str,
    summary: &str,
) -> anyhow::Result<Review> {
    show(heading, note, summary);
    println!("  [Steward] Nothing is signed until you choose:");
    println!("    (a) Accept: sign this summary with the decision, as written");
    println!(
        "    (i) Input: send the Lawyer your corrections; it revises, and may decline and say why"
    );
    println!(
        "    (d) Dissent: sign it unchanged, with your note attached to the record under your name"
    );
    println!("    (w) Withhold: sign the decision without a summary");
    let options = "a/i/d/w";

    loop {
        let answer = input.readline("  > ")?;
        match answer.trim().to_lowercase().as_str() {
            "a" | "accept" => return Ok(Review::Accept),
            "i" | "input" => {
                let text =
                    read_multiline(input, "  Your input to the Lawyer (blank line to finish):")?;
                if text.trim().is_empty() {
                    println!("  No input entered. Choose {options}.");
                    continue;
                }
                return Ok(Review::Input(text));
            }
            "d" | "dissent" => {
                let text = read_multiline(
                    input,
                    "  Your dissent, attached under your name (blank line to finish):",
                )?;
                if text.trim().is_empty() {
                    println!("  A dissent needs text. Choose {options}.");
                    continue;
                }
                return Ok(Review::Dissent(text));
            }
            "w" | "withhold" => return Ok(Review::Withhold),
            _ => println!("  Unrecognized. Enter {options}."),
        }
    }
}

/// One round of the seats' review: the draft they read, and what each said
struct ReviewRound {
    draft: u32,
    reviews: Vec<(CouncilSeat, SeatReview)>,
}

impl ReviewRound {
    fn all_accept(&self) -> bool {
        self.reviews.len() == RECUSED_ACCEPTORS.len() && self.reviews.iter().all(|(_, r)| r.accept)
    }

    fn render(&self, s: &mut String) {
        for (seat, review) in &self.reviews {
            let _ = write!(
                s,
                "**The {seat}** ({}): {}\n\n",
                if review.accept {
                    "accepts"
                } else {
                    "asks for a correction"
                },
                review.comment.trim()
            );
        }
    }
}

/// A draft as a reviewer reads it
fn draft_text(number: u32, draft: &SummaryDraft) -> String {
    let mut s = format!("# The Lawyer's Draft (draft {number})\n\n");
    if !draft.note_to_reviewers.trim().is_empty() {
        let _ = write!(
            s,
            "**The Lawyer's note:** {}\n\n",
            draft.note_to_reviewers.trim()
        );
    }
    let _ = write!(s, "**The summary:**\n\n{}", draft.summary.trim());
    s
}

/// The seats, and where the summary's turns are: the Lawyer's and the
/// reviewers' own conversations, and how many drafts the Lawyer has
/// submitted
struct Session<'s, 't, 'r, T: RawTransport> {
    councillors: &'s mut [Councillor<'t, T>],
    reader: &'s mut ProposalReader<'r>,
    /// How many drafts the Lawyer has submitted
    drafts: u32,
}

impl<T: RawTransport> Session<'_, '_, '_, T> {
    /// One after-vote turn for `seat`, retried on the Steward's word. A
    /// refusal, an emission breach or a stop is returned as is (the caller
    /// seals); any other failure is shown to the Steward, who asks again
    /// (sampling the same conversation again, appending nothing) or, seated,
    /// withholds the summary (`Ok(None)`).
    async fn ask(
        &mut self,
        seat: CouncilSeat,
        asked: AfterVote,
        content: Content,
        what: &str,
        recused: bool,
        input: &mut impl StewardInput,
    ) -> anyhow::Result<Option<Submission>> {
        let reader = &mut *self.reader;
        let councillor = self
            .councillors
            .iter_mut()
            .find(|c| c.role == seat)
            .ok_or_else(|| anyhow::anyhow!("the {seat} is not seated"))?;
        let mut attempt = councillor.after_vote(asked, content, reader).await;
        let mut wrong_shapes = 0;
        loop {
            match attempt {
                // `Record` admits only the shape asked for; were another to
                // come through, it goes back to the seat as one more turn
                Ok(submission) if submission.name() != asked.submission() => {
                    wrong_shapes += 1;
                    anyhow::ensure!(
                        wrong_shapes <= 2,
                        "the {seat} kept submitting `{}` when asked for `{}`",
                        submission.name(),
                        asked.submission()
                    );
                    attempt = councillor
                        .after_vote(
                            asked,
                            Content::from(format!(
                                "That was `{}`. Submit `{}`.",
                                submission.name(),
                                asked.submission()
                            )),
                            reader,
                        )
                        .await;
                }
                Ok(submission) => return Ok(Some(submission)),
                Err(e) if fatal(&e) => return Err(e),
                Err(e) => {
                    let reason = format!("{e:#}");
                    tracing::error!(
                        event_type = "decision_summary_turn_failed",
                        seat = %seat,
                        asked = asked.submission(),
                        error = %reason,
                        "a summary turn failed; the Steward chooses"
                    );
                    match retry_or_withhold(input, what, &reason, recused)? {
                        Retry::Ask => {
                            println!("\n  Asking the {seat} again...");
                            attempt = councillor.retry_after_vote(reader).await;
                        }
                        Retry::Withhold => return Ok(None),
                    }
                }
            }
        }
    }

    /// A Lawyer's turn that must be a draft: sent back while it fails the
    /// corpus gate. `None`: the Steward withheld the summary.
    async fn draft(
        &mut self,
        content: Content,
        what: &str,
        recused: bool,
        input: &mut impl StewardInput,
    ) -> anyhow::Result<Option<SummaryDraft>> {
        let mut content = content;
        let mut sent_back = 0;
        loop {
            let Some(submission) = self
                .ask(
                    CouncilSeat::Lawyer,
                    AfterVote::Summary,
                    content,
                    what,
                    recused,
                    input,
                )
                .await?
            else {
                return Ok(None);
            };
            let Submission::Summary(draft) = submission else {
                unreachable!("`ask` returns only the shape asked for");
            };
            self.drafts += 1;
            let failed = if draft.summary.trim().is_empty() {
                Some("it is empty".to_string())
            } else {
                agora_common::precedents::council_summary_gate(&draft.summary)
                    .err()
                    .map(|e| e.to_string())
            };
            let Some(why) = failed else {
                return Ok(Some(draft));
            };
            tracing::warn!(
                event_type = "decision_summary_gate_failed",
                draft = self.drafts,
                error = %why,
                "the Lawyer's draft fails the corpus gate; it goes back to the Lawyer"
            );
            if sent_back == GATE_REVISIONS {
                // Recused, the Steward may not suppress the account: the
                // decision is signed without a summary (`write_summary`'s
                // last arm). Seated, he chooses: send it back again, or
                // withhold.
                let what = format!(
                    "The Lawyer's draft {} still fails the corpus gate",
                    self.drafts
                );
                if recused {
                    anyhow::bail!("{what} ({why})");
                }
                match retry_or_withhold(input, &what, &why, false)? {
                    Retry::Ask => sent_back = 0,
                    Retry::Withhold => return Ok(None),
                }
            }
            sent_back += 1;
            println!("\n  The Lawyer's draft fails the corpus gate ({why}); it is sent back.");
            content = Content::from(format!(
                "Your draft cannot be signed: {why}. Nothing was shown to the reviewers. \
                 Revise it, and submit the whole summary again as `summary`."
            ));
        }
    }
}

/// Whether a failure ends the sitting rather than going to the Steward: a
/// refusal, an emission breach (`abort::Cause`) or a Steward stop
fn fatal(e: &anyhow::Error) -> bool {
    Refused::find(e).is_some()
        || abort::Cause::find(e).is_some()
        || crate::stop::StewardStopRequested::find(e).is_some()
}

/// Write the decision's summary in the seats' own conversations: the
/// Lawyer drafts from `record` (the decision's record as
/// [`decision_record`] renders it), and the Steward, or with him recused
/// the three seats, review until it is accepted or there is to be none. A
/// refusal, an emission breach or a Steward stop returns
/// [`abort::SittingAborted`], every seat's conversation sealed; nothing has
/// been signed.
pub async fn write_summary<T: RawTransport>(
    config: &crate::config::Config,
    item: &AgendaItem,
    decision: &Decision,
    record: &str,
    councillors: &mut [Councillor<'_, T>],
    reader: &mut ProposalReader<'_>,
    input: &mut impl StewardInput,
) -> anyhow::Result<Written> {
    let recused = decision.final_votes().steward == CouncilVote::Recused;
    let stop = input.stop_signal();
    let result = {
        let mut session = Session {
            councillors: &mut *councillors,
            reader: &mut *reader,
            drafts: 0,
        };
        crate::stop::race(
            stop.as_ref(),
            summarize(&mut session, item, record, recused, input),
        )
        .await
    };
    let e = match result {
        Ok(written) => return Ok(written),
        Err(e) => e,
    };
    // A stop never discards a vote already cast, recused or seated: the
    // decision is signed without a summary (the caller signs, then seals
    // and records the stop, `Stage::SummaryStopped`)
    if let Some(requested) = crate::stop::StewardStopRequested::find(&e) {
        let mut facts = crate::stop::take(input, requested, chrono::Utc::now());
        facts.steward_recused = Some(recused);
        return Ok(stopped(item, facts));
    }
    // A refusal or an emission breach: the record that would be signed may
    // carry what drew it, so nothing is signed (the refusal-abort rule)
    if let Some(cause) = abort::Cause::find(&e) {
        let conversations = crate::deliberation::live_conversations(councillors);
        return Err(abort::seal(
            config,
            Moment::item(item, Stage::DecisionSummary),
            cause,
            &conversations,
            Some(ClerkBriefing::of(item)),
        )
        .await);
    }
    // Anything else: the vote stands, signed without a summary
    Ok(failed(item, &e))
}

/// The Steward stopped the sitting during the summary: the decision is
/// signed without one, `summary-review.md` saying so with his reason
fn stopped(item: &AgendaItem, facts: crate::stop::StopFacts) -> Written {
    tracing::warn!(
        event_type = "decision_summary_stopped",
        item = %item.id,
        steward_recused = ?facts.steward_recused,
        reason = facts.reason.as_deref().unwrap_or("(none could be read)"),
        "the Steward stopped the sitting during the summary; the decision is signed \
         without one, and the stop is sealed and recorded"
    );
    println!(
        "\n  The vote stands: the decision is signed WITHOUT a summary, and the stop is \
         recorded."
    );
    let standing = match facts.steward_recused {
        Some(true) => "The Steward was recused from this item (Constitution Art. IV § 3). ",
        Some(false) => "",
        None => "",
    };
    let reason = match &facts.reason {
        Some(reason) => format!("> {reason}"),
        None => "None could be read: the terminal's input closed or failed.".to_string(),
    };
    let content = format!(
        "# No summary: the Steward stopped the sitting\n\n\
         {standing}The Steward stopped the sitting (Ctrl-C) during this decision's summary, \
         after the vote, at {at} (UTC). A stop never discards a vote already cast, so the \
         decision was signed without a summary. The stop is recorded in the Governance Log \
         as a Steward's record.\n\n\
         **The Steward's reason, verbatim:**\n\n{reason}\n",
        at = facts.stopped_at.format("%Y-%m-%d %H:%M:%S"),
    );
    Written {
        text: None,
        review: Some(attachment(
            "Why this decision was signed without a summary: the Steward stopped the \
             sitting during it.",
            content,
        )),
        outcome: Outcome::Stopped { facts },
    }
}

/// The summary could not be completed: the decision is signed without one,
/// the reason attached, loudly
fn failed(item: &AgendaItem, e: &anyhow::Error) -> Written {
    let reason = format!("{e:#}");
    tracing::error!(
        event_type = "decision_summary_failed",
        item = %item.id,
        error = %reason,
        "the decision's summary could not be completed; the decision is signed WITHOUT \
         a summary"
    );
    println!(
        "\n  !! The summary could not be completed: {reason}\n  !! The vote stands: the \
         decision is signed WITHOUT a summary. Add one by a signed revision \
         (--set-summary)."
    );
    Written {
        text: None,
        review: Some(attachment(
            "Why this decision was signed without a summary.",
            format!(
                "# No summary\n\nThe decision's summary could not be completed, so the \
                 decision was signed without one:\n\n> {}\n",
                reason.replace('\n', "\n> ")
            ),
        )),
        outcome: Outcome::Failed { reason },
    }
}

/// [`write_summary`]'s drafts and reviews.
async fn summarize<T: RawTransport>(
    session: &mut Session<'_, '_, '_, T>,
    item: &AgendaItem,
    record: &str,
    recused: bool,
    input: &mut impl StewardInput,
) -> anyhow::Result<Written> {
    println!("\n  The Lawyer: writing the decision's summary...");
    let mut content = Content::from_iter(std::iter::empty::<Block>());
    content.push(SUMMARY_CHARGE);
    content.push(format!(
        "# The Decision's Record\n\n\
         The Council has voted. The record of its decision follows, as it will be \
         signed. Its reviewers: {}. Submit your draft as `summary`.",
        if recused {
            "the Artist, the Philosopher and the Engineer (the Steward was recused \
             from this item)"
        } else {
            "the Steward"
        }
    ));
    content.push(record.to_string());
    let Some(current) = session
        .draft(
            content,
            "The Lawyer could not write the summary",
            recused,
            input,
        )
        .await?
    else {
        // Withheld at the first draft: it failed, or kept failing the gate
        let draft = session.drafts;
        return Ok(withheld(
            item,
            draft,
            (draft == 0).then_some("the Lawyer's first draft failed"),
        ));
    };
    logged_draft(item, session.drafts, &current);
    if recused {
        seats_review(session, item, record, current, input).await
    } else {
        steward_review(session, item, current, input).await
    }
}

fn logged_draft(item: &AgendaItem, draft: u32, current: &SummaryDraft) {
    tracing::info!(
        event_type = "decision_summary_draft",
        item = %item.id,
        author = "lawyer",
        draft,
        note_to_reviewers = %current.note_to_reviewers,
        summary = %current.summary,
        "the Lawyer submitted a decision summary draft"
    );
}

/// The seated Steward's checkpoint, until he accepts, dissents or
/// withholds
async fn steward_review<T: RawTransport>(
    session: &mut Session<'_, '_, '_, T>,
    item: &AgendaItem,
    mut current: SummaryDraft,
    input: &mut impl StewardInput,
) -> anyhow::Result<Written> {
    loop {
        let draft = session.drafts;
        let heading = format!("THE LAWYER'S SUMMARY OF \"{}\": draft {draft}", item.title);
        match review(
            input,
            &heading,
            &current.note_to_reviewers,
            &current.summary,
        )? {
            Review::Accept => {
                tracing::info!(
                    event_type = "decision_summary_accepted",
                    item = %item.id,
                    accepted_by = "steward",
                    draft,
                    "the Steward accepted the decision's summary"
                );
                return Ok(Written {
                    text: Some(current.summary),
                    review: None,
                    outcome: Outcome::Accepted { draft },
                });
            }
            Review::Dissent(dissent) => {
                tracing::info!(
                    event_type = "decision_summary_dissented",
                    item = %item.id,
                    draft,
                    steward_dissent = %dissent,
                    "the Steward let the summary be signed with his dissent attached"
                );
                let content = format!(
                    "# The Steward's dissent from the summary\n\n\
                     The Steward reviewed the Lawyer's draft {draft} of this decision's \
                     summary and let it be signed unchanged, but did not agree with all of \
                     it. His dissent, in his own words:\n\n{}\n",
                    dissent.trim()
                );
                return Ok(Written {
                    text: Some(current.summary),
                    review: Some(attachment(
                        "The Steward's dissent from the decision's summary, attached when \
                         he let the Lawyer's text be signed unchanged.",
                        content,
                    )),
                    outcome: Outcome::Dissented { draft, dissent },
                });
            }
            Review::Withhold => return Ok(withheld(item, draft, None)),
            Review::Input(text) => {
                tracing::info!(
                    event_type = "decision_summary_input",
                    item = %item.id,
                    draft,
                    steward_input = %text,
                    "the Steward gave the Lawyer input on its draft"
                );
                println!("\n  The Lawyer: revising...");
                let asked = Content::from(format!(
                    "The Steward has read your draft and gives this input:\n\n\
                     {text}\n\n\
                     Revise as you judge right, and answer each point in \
                     `note_to_reviewers`. You may decline any of it; if you do, say \
                     which part and why. Submit the whole summary again as `summary`."
                ));
                match session
                    .draft(asked, "The Lawyer could not revise", false, input)
                    .await?
                {
                    Some(revised) => {
                        current = revised;
                        logged_draft(item, session.drafts, &current);
                    }
                    None => return Ok(withheld(item, draft, None)),
                }
            }
        }
    }
}

/// With the Steward recused: the Artist, Philosopher and Engineer review
/// each draft in their own conversations, and the Lawyer revises on their
/// corrections, until all three accept one text or [`REVIEW_ROUNDS`] pass
async fn seats_review<T: RawTransport>(
    session: &mut Session<'_, '_, '_, T>,
    item: &AgendaItem,
    record: &str,
    mut current: SummaryDraft,
    input: &mut impl StewardInput,
) -> anyhow::Result<Written> {
    let mut rounds: Vec<ReviewRound> = Vec::new();
    for round in 1..=REVIEW_ROUNDS {
        let draft = session.drafts;
        show(
            &format!(
                "THE LAWYER'S SUMMARY OF \"{}\": draft {draft}, for the seats' review",
                item.title
            ),
            &current.note_to_reviewers,
            &current.summary,
        );
        println!(
            "  [Steward] You were recused from this item (Art. IV § 3): the Artist, the \
             Philosopher and the Engineer review the summary (round {round} of {REVIEW_ROUNDS})."
        );
        let drafted = draft_text(draft, &current);
        let mut reviews = Vec::new();
        for seat in RECUSED_ACCEPTORS {
            let content = match rounds.last() {
                None => {
                    let mut content = Content::from_iter(std::iter::empty::<Block>());
                    content.push(REVIEW_CHARGE);
                    content.push(format!(
                        "# The Decision's Record\n\n\
                         The Council has voted. The record of its decision follows, as it \
                         will be signed.\n\n{record}"
                    ));
                    content.push(format!(
                        "{drafted}\n\nSubmit your review as `summary_review`."
                    ));
                    content
                }
                Some(previous) => {
                    let mut s = String::from(
                        "The three reviews of the Lawyer's last draft were these:\n\n",
                    );
                    previous.render(&mut s);
                    s.push_str("The Lawyer has revised its draft, having read them.\n\n");
                    s.push_str(&drafted);
                    s.push_str("\n\nSubmit your review as `summary_review`.");
                    Content::from(s)
                }
            };
            let what = format!("The {seat}'s review failed");
            let Some(submission) = session
                .ask(seat, AfterVote::Review, content, &what, true, input)
                .await?
            else {
                unreachable!("a recused Steward is offered only the retry");
            };
            let Submission::SummaryReview(review) = submission else {
                unreachable!("`ask` returns only the shape asked for");
            };
            tracing::info!(
                event_type = "decision_summary_review",
                item = %item.id,
                seat = %seat,
                round,
                draft,
                accept = review.accept,
                comment = %review.comment,
                "a seat reviewed the Lawyer's summary"
            );
            println!(
                "  The {seat} {}: {}",
                if review.accept {
                    "accepts"
                } else {
                    "asks for a correction"
                },
                review.comment.trim()
            );
            reviews.push((seat, review));
        }
        let this = ReviewRound { draft, reviews };
        let agreed = this.all_accept();
        rounds.push(this);
        if agreed {
            tracing::info!(
                event_type = "decision_summary_accepted",
                item = %item.id,
                accepted_by = "artist, philosopher, engineer",
                draft,
                round,
                "the three seats accepted the decision's summary, the Steward being recused"
            );
            return Ok(Written {
                text: Some(current.summary),
                review: Some(seat_review_attachment(&rounds, None)),
                outcome: Outcome::AcceptedBySeats { draft, round },
            });
        }
        if round == REVIEW_ROUNDS {
            break;
        }
        // The corrections go to the Lawyer, who revises or declines
        let mut asked = String::from(
            "The Artist, the Philosopher and the Engineer have reviewed your draft. All \
             three must accept the same text. Their reviews:\n\n",
        );
        rounds.last().expect("just pushed").render(&mut asked);
        asked.push_str(
            "Revise as you judge right, and answer each correction in \
             `note_to_reviewers`. You may decline any of it; if you do, say which part \
             and why. Submit the whole summary again as `summary`.",
        );
        println!("\n  The Lawyer: revising on the seats' corrections...");
        let Some(revised) = session
            .draft(
                Content::from(asked),
                "The Lawyer could not revise",
                true,
                input,
            )
            .await?
        else {
            unreachable!("a recused Steward is offered only the retry");
        };
        current = revised;
        logged_draft(item, session.drafts, &current);
    }

    let draft = session.drafts;
    tracing::error!(
        event_type = "decision_summary_unagreed",
        item = %item.id,
        draft,
        rounds = REVIEW_ROUNDS,
        "the seats did not all accept one draft of the summary; the decision is signed \
         WITHOUT a summary, the review attached"
    );
    println!(
        "\n  !! The Artist, the Philosopher and the Engineer did not all accept one draft \
         in {REVIEW_ROUNDS} rounds.\n  !! The decision is signed WITHOUT a summary; the \
         review is attached to the record as {REVIEW_ATTACHMENT}.\n  !! A summary can be \
         added later only by a signed revision (--set-summary)."
    );
    Ok(Written {
        text: None,
        review: Some(seat_review_attachment(&rounds, Some(&current))),
        outcome: Outcome::Unagreed { draft },
    })
}

/// `summary-review.md` for a review by the seats: every round, and, when
/// they did not agree, the last draft
fn seat_review_attachment(
    rounds: &[ReviewRound],
    unagreed: Option<&SummaryDraft>,
) -> CouncilAttachment {
    let mut s = String::from("# The seats' review of the decision's summary\n\n");
    s.push_str(
        "The Steward was recused from this item (Constitution Art. IV § 3), so the \
         Artist, the Philosopher and the Engineer reviewed the Lawyer's summary in his \
         place, each in its own conversation on the item; all three had to accept the \
         same text.\n\n",
    );
    for (i, round) in rounds.iter().enumerate() {
        let _ = write!(s, "## Round {}: draft {}\n\n", i + 1, round.draft);
        round.render(&mut s);
    }
    let note = match unagreed {
        None => {
            s.push_str("All three accepted the last draft, which is the signed summary.\n");
            "The Artist's, Philosopher's and Engineer's reviews of the Lawyer's summary, \
             the Steward being recused, round by round."
        }
        Some(last) => {
            let _ = write!(
                s,
                "They did not all accept one draft within {REVIEW_ROUNDS} rounds, so the \
                 decision was signed **without a summary**. The Lawyer's last draft, which \
                 not all of them accepted, follows for the record; it is not the \
                 decision's summary.\n\n{}\n",
                draft_text(rounds.last().map_or(0, |r| r.draft), last)
            );
            "The seats' review of the Lawyer's summary, the Steward being recused: they \
             did not agree, so the decision was signed without one."
        }
    };
    attachment(note, s)
}

fn attachment(note: &str, content: String) -> CouncilAttachment {
    CouncilAttachment {
        name: REVIEW_ATTACHMENT.to_string(),
        note: note.to_string(),
        content: Redactable::Value(content),
    }
}

/// The Steward withheld the summary: the decision is signed without one,
/// and the record says so. `draft` is the draft he withheld, 0 when the
/// first failed; `reason`, why it failed then.
fn withheld(item: &AgendaItem, draft: u32, reason: Option<&str>) -> Written {
    tracing::warn!(
        event_type = "decision_summary_withheld",
        item = %item.id,
        draft,
        reason,
        "the Steward withheld the decision's summary; the decision is signed without one"
    );
    println!(
        "\n  !! No summary: the decision is signed WITHOUT one.\n  !! A summary can be \
         added later only by a signed revision (--set-summary)."
    );
    let content = match reason {
        Some(reason) => format!(
            "# No summary\n\nThe Lawyer's draft of this decision's summary failed \
             ({reason}), and the Steward chose to sign the decision without a summary.\n"
        ),
        None => format!(
            "# No summary\n\nThe Steward withheld the Lawyer's draft {draft} of this \
             decision's summary, so the decision was signed without one.\n"
        ),
    };
    Written {
        text: None,
        review: Some(attachment(
            "Why this decision was signed without a summary.",
            content,
        )),
        outcome: Outcome::Withheld { draft },
    }
}

/// The Steward's answer when a call failed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Ask again
    Ask,
    /// Sign the decision without a summary
    Withhold,
}

/// Tell the Steward why a call failed, and ask whether to ask again or
/// withhold the summary. Nothing is retried without him. Recused from the
/// item, he may not suppress its account, so he is offered only the retry.
fn retry_or_withhold(
    input: &mut impl StewardInput,
    what: &str,
    reason: &str,
    recused: bool,
) -> anyhow::Result<Retry> {
    println!("\n  !! {what}: {reason}");
    if recused {
        println!(
            "  [Steward] You were recused from this item (Art. IV § 3), so the summary may \
             not be withheld:"
        );
    } else {
        println!("  [Steward] There is no other summary. Choose:");
    }
    println!("    (r) Retry: ask again");
    if !recused {
        println!("    (w) Withhold: sign the decision without a summary");
    }
    loop {
        let answer = input.readline("  > ")?;
        match answer.trim().to_lowercase().as_str() {
            "r" | "retry" => return Ok(Retry::Ask),
            "w" | "withhold" if !recused => return Ok(Retry::Withhold),
            _ if recused => println!("  You were recused from this item. Enter r."),
            _ => println!("  Unrecognized. Enter r/w."),
        }
    }
}

/// One decision's record for the Lawyer and its reviewers: what the signed
/// entry holds, rendered as prose headings. `docket` is the item's docket
/// on a scheduling item, to put titles on the P-numbers.
pub fn decision_record(
    id: &GovernanceLogId,
    decision: &Decision,
    item: Option<&AgendaItem>,
) -> String {
    let docket: Option<&Docket> = item.and_then(|i| i.docket.as_ref());
    let recused = decision.final_votes().steward == CouncilVote::Recused;
    // The threshold the outcome was found under: a recused item's own
    let threshold = match recused {
        true => decision
            .category
            .recused_threshold()
            .unwrap_or_else(|| decision.category.threshold()),
        false => decision.category.threshold(),
    };
    let mut s = format!(
        "## {id}: {}\n\n\
         **Category**: {} (threshold: {threshold})\n\
         **Outcome**: {} ({})\n",
        decision.title,
        decision.category,
        decision.outcome().name(),
        decision.vote_tally(),
    );
    let v = &decision.final_votes();
    let _ = writeln!(
        s,
        "**Final votes**: Artist {}, Philosopher {}, Lawyer {}, Engineer {}, Steward {}",
        v.artist, v.philosopher, v.lawyer, v.engineer, v.steward
    );
    if let Some(rationale) = decision.veto_rationale() {
        let _ = writeln!(s, "**The Steward's veto rationale**: {rationale}");
    }
    if let Some(rationale) = decision.steward_abstention_rationale() {
        let _ = writeln!(s, "**The Steward's abstention rationale**: {rationale}");
    }
    if decision.landmark {
        let _ = writeln!(s, "**Landmark**: yes");
    }
    if !decision.tags.is_empty() {
        let _ = writeln!(s, "**Tags**: {}", decision.tags.join(", "));
    }

    // The recusal determination (Art. IV § 3), before any round
    if let Some(determination) = &decision.recusal {
        let _ = write!(
            s,
            "\n### The recusal determination\n\n\
             Result: {}.\n\n",
            if determination.recused() {
                "the item limits the Steward's powers, and the Steward is recused"
            } else {
                "no majority finds that the item limits the Steward's powers; the Steward sits"
            }
        );
        for answer in determination.answers() {
            let _ = write!(
                s,
                "**The {}**: {}. {}\n\n",
                answer.role,
                if answer.limits_steward_powers {
                    "limits the Steward's powers"
                } else {
                    "does not limit them"
                },
                answer.explanation.trim()
            );
        }
    }

    for round in &decision.rounds {
        let _ = write!(
            s,
            "\n### Round {}: {}\n\n",
            round.number,
            round.round_type.name()
        );
        if let Some(notes) = &round.steward_contribution {
            let _ = write!(s, "**The Steward's notes**: {notes}\n\n");
        }
        for r in &round.responses {
            let _ = write!(s, "**The {}** (vote: {})\n\n", r.role, r.vote_label());
            if !r.rationale.is_empty() {
                let label = match r.rationale {
                    crate::models::Rationale::Legacy(_) => "Reasoning",
                    crate::models::Rationale::Thinking(_) => {
                        "Reasoning summary (by Anthropic, from the seat's thinking)"
                    }
                };
                let _ = write!(s, "{label}: {}\n\n", r.rationale);
            }
            if !r.position.is_empty() {
                let _ = write!(s, "Statement: {}\n\n", r.position);
            }
            if !r.questions.is_empty() {
                let _ = write!(s, "Questions: {}\n\n", r.questions.join(" / "));
            }
            if let Some(ranking) = &r.ranking {
                let _ = write!(s, "{}\n\n", ranking.render());
            }
        }
        // The questions put to witnesses this round, read back from their
        // testimony attachments, as the other seats were shown them
        for exchange in crate::witness::recorded(&decision.attachments, &round.witness_questions) {
            let _ = write!(s, "{exchange}\n\n");
        }
    }

    // What was adopted: the proposal as filed is in this conversation
    // (the item, above), and the seats' final statements say what each
    // voted for, a named draft or amendments included
    // (A scheduling item adopts an agenda, below, not a text.)
    if let Some(last) = decision
        .rounds
        .iter()
        .rev()
        .find(|r| r.round_type == crate::models::RoundType::FinalVote)
        .filter(|_| decision.agenda_ranking().is_none())
    {
        let _ = write!(
            s,
            "\n### What each seat voted for\n\n\
             The proposal as filed is the item in this conversation, above. What the \
             Council adopted is that text as the seats' final statements name it: a draft \
             they name, or amendments they state.\n\n"
        );
        for r in &last.responses {
            if !r.position.trim().is_empty() {
                let _ = write!(
                    s,
                    "**The {}** ({}): {}\n\n",
                    r.role,
                    r.vote_label(),
                    r.position.trim()
                );
            }
        }
    }

    if let Some(ranking) = decision.agenda_ranking() {
        let title = |n: u32| {
            docket
                .and_then(|d| d.candidates.iter().find(|c| c.number == n))
                .map(|c| c.title.as_str())
                .unwrap_or("(title not available)")
        };
        let _ = write!(
            s,
            "\n### The agenda the Council chose\n\n\
             Borda count over {} rankable proposals; the cut (median capacity) \
             is {}.\n\n",
            ranking.rankable, ranking.cut
        );
        for (i, placed) in ranking.order.iter().enumerate() {
            let _ = writeln!(
                s,
                "{}. P{} \"{}\" (post {}): Borda {}, ranked by {} seat(s){}{}",
                i + 1,
                placed.number,
                title(placed.number),
                // Its short id, never the UUID: the Lawyer copies what it
                // is shown, and the prompt asks it to cite by short id
                &placed.post_id.to_string()[..8],
                placed.borda,
                placed.seats,
                if (i as u32) < ranking.cut {
                    ", chosen"
                } else {
                    ", below the cut"
                },
                if placed.tied_with_next {
                    ", tied with the next"
                } else {
                    ""
                },
            );
        }
        let _ = writeln!(
            s,
            "\nHead to head: {}",
            match ranking.condorcet_winner {
                Some(n) => format!("P{n} beats every other proposal."),
                None => "no proposal beats every other.".to_string(),
            }
        );
    }

    // The obligations this hearing owed or settles, with their ids
    if let Some(item) = item
        && !item.obligations.is_empty()
    {
        s.push_str(
            "\n### Obligations before this hearing\n\n\
             Open on the record when the item was heard: each was created by an earlier \
             decision on it, or fell due at this hearing. Say which this decision settles \
             and which it leaves open.\n\n",
        );
        for obligation in &item.obligations {
            let _ = writeln!(s, "{}", crate::context::obligation_line(obligation));
        }
    }

    // What is signed with the record besides
    if !decision.attachments.is_empty() {
        s.push_str("\n### Attachments signed with the record\n\n");
        for attachment in &decision.attachments {
            let _ = writeln!(s, "- `{}`: {}", attachment.name, attachment.note.trim());
        }
    }
    s
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
