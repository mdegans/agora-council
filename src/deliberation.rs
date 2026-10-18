//! The deliberation loop for a single agenda item.
//!
//! Four [`Councillor`]s are seated at the start of the item, each owning
//! one append-only conversation, and every round is one
//! [`exchange`](Councillor::exchange) per seat. The Steward's flow
//! (defer overrides, notes, calling the vote) is unchanged from the
//! previous design.
//!
//! Before round 1, and before the Steward is offered anything, the seats
//! make the recusal determination (Constitution Art. IV § 3,
//! GOV-2026-0009; [`determine_recusal`]). If it recuses the Steward, the
//! Steward's notes, tabling, vote and veto are refused on the item and the
//! recused thresholds apply ([`FinalVotes::outcome`]).
//!
//! A refusal from the API in any round aborts the sitting ([`crate::abort`]):
//! the seats' conversations are sealed and no decision is recorded. So does
//! a seat's call whose fields come back out of declared order
//! ([`crate::wire::EmissionBreach`]). This
//! supersedes GOV-2026-0011's abstain condition, pending the Council's
//! approval of the amendment.
//!
//! A Steward stop (Ctrl-C, [`crate::stop`]) ends the item the same way:
//! the round in flight is cancelled, the Steward gives a reason, and the
//! seats' conversations are sealed. Its record carries what a decided
//! item's would, as far as the item got ([`stop_attachments`]).

use misanthropic::prompt::message::Content;

use std::collections::BTreeMap;

use agora_common::ids::PostId;

use agora_common::govlog::RecordAttachment;

use crate::abort::{self, ClerkBriefing, Conversation, Holder, Moment, RoundKind, Stage};
use crate::briefing::{self, SeatRead};
use crate::config::Config;
use crate::context;
use crate::councillor::Councillor;
use crate::docket::ProposalReader;
use crate::models::{
    AgendaItem, AgentResponse, CouncilRound, CouncilSeat, CouncilVote, Decided, Decision,
    DecisionCategory, Determination, RECUSED_DEFERRED_TAG, RECUSED_ROUND_CAP, RecusalAnswer, Round,
    RoundType, STEWARD_RECUSED_TAG, SeatVotes, StewardAction, StewardStanding, Votes,
};
use crate::models::{AgendaRanking, Ballot};
use crate::steward::{self, NextStep, StewardInput};
use crate::wire::RawTransport;

/// A decided item, ready to sign: the decision, the id it will be signed
/// under, and its summary as written and accepted in the seats' own
/// conversations (`crate::summary`)
#[derive(Debug)]
pub struct Deliberated {
    pub decision: Decision,
    /// Peeked before the summary, which names it; the signing transaction
    /// checks it is still the next (`meeting::record_decision_at`)
    pub id: agora_common::ids::GovernanceLogId,
    pub written: crate::summary::Written,
    /// The seats' conversations as they stood, when the Steward stopped
    /// the sitting during the summary: the decision is signed first (a
    /// stop never discards a vote already cast), then the stop is sealed
    /// with these and recorded
    pub stopped: Option<Vec<(Holder, misanthropic::prompt::CachedPrompt)>>,
}

/// Run the full deliberation process for a single agenda item, then the
/// decision's summary while the seats are still seated: the Lawyer drafts,
/// and the Steward or (recused) the other three seats review, each in its
/// own conversation for the item, appended (Steward, 2026-10-17: always
/// append, never prune).
///
/// Returns what is to be signed, or None if the item was skipped.
#[allow(clippy::too_many_arguments)]
pub async fn deliberate<T: RawTransport>(
    pool: &sqlx::PgPool,
    transport: &T,
    config: &Config,
    item: &AgendaItem,
    recap: &Content,
    editor: &mut impl StewardInput,
) -> anyhow::Result<Option<Deliberated>> {
    // Seat the council: one append-only conversation per role, alive for
    // this item only. Each seat's `record` holds the comment thread so
    // `ask_clerk` can read from it.
    let rankable = item.docket.as_ref().map(|d| d.rankable());
    // The seats' agent tools read through the server's own endpoint
    // (`crate::seat_tools`). A client that cannot be built leaves the tools
    // offered and refusing, rather than stopping the sitting.
    let reads: Option<std::sync::Arc<dyn crate::seat_tools::AgoraReads>> = match config
        .read_url
        .clone()
        .map(agora_agentkit::client::Client::new)
    {
        Some(Ok(client)) => Some(std::sync::Arc::new(client)),
        Some(Err(e)) => {
            tracing::warn!(error = %e, "the seats' get_content client could not be built");
            None
        }
        None => None,
    };
    // The item's witnesses (OBL-2026-0028), one bench shared by every
    // seat's tools: one conversation per witness for the item.
    let bench = crate::witness::Bench::new(
        &item.witnesses,
        |witness| crate::witness::transport_for(witness, config.api_key.expose()),
        Some((pool.clone(), item.id)),
    )?;
    let mut councillors: Vec<Councillor<'_, T>> = CouncilSeat::ALL
        .iter()
        .map(|&role| {
            Councillor::new(
                transport,
                config,
                role,
                item.comments.clone(),
                rankable.clone(),
                Some((pool, item.id)),
            )
            .map(|seat| seat.with_reads(reads.clone()).with_witnesses(bench.clone()))
        })
        .collect::<Result<_, _>>()?;
    // One memo for the item: the seats run one after another, and four
    // seats reading the same proposal pay the Clerk once.
    let mut reader = ProposalReader::new(Some(pool), item.docket.as_ref())
        .for_item(item.id)
        .beside(&item.comments.labels);
    let mut reads = Vec::new();

    let decision = sit_item(
        config,
        item,
        recap,
        editor,
        &mut councillors,
        &mut reader,
        &mut reads,
        rankable.as_ref(),
    )
    .await?;
    let Some(mut decision) = decision else {
        return Ok(None);
    };
    // The briefing first, then anything the decision brought (a recused
    // deferral's open questions).
    let own = std::mem::take(&mut decision.attachments);
    decision.attachments = briefing::attachments(item, &reads, &decision.rounds, reader.readings());
    // Every called witness's testimony, asked or not
    if let Some(bench) = &bench {
        decision.attachments.extend(bench.testimonies());
    }
    decision.attachments.extend(own);
    decision.seat_models = Some(crate::councillor::seat_models(config));

    // The summary names the decision, so its id is peeked first (one
    // writer: see `meeting::record_decision_at`)
    let id = crate::meeting::peek_decision_id(pool).await?;
    let record = crate::summary::decision_record(&id, &decision, Some(item));
    let written = crate::summary::write_summary(
        config,
        item,
        &decision,
        &record,
        &mut councillors,
        &mut reader,
        editor,
    )
    .await?;
    // Copies, not the conversations: they are sealed after the signing,
    // once the seats are gone. Nothing is changed in them.
    let stopped = matches!(written.outcome, crate::summary::Outcome::Stopped { .. }).then(|| {
        live_conversations(&councillors)
            .into_iter()
            .map(|c| (c.holder, c.prompt.clone()))
            .collect()
    });
    Ok(Some(Deliberated {
        decision,
        id,
        written,
        stopped,
    }))
}

/// [`run_item`], sealing the moment if the API returns a refusal or a
/// seat's decision comes back out of declared order (Governance Protocol
/// § 4.5): every seat's conversation as it stands, the Clerk's briefing,
/// and the request (with, for a breach, the raw response body). The error
/// is then [`abort::SittingAborted`]; nothing is retried and no decision
/// is made.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sit_item<T: RawTransport>(
    config: &Config,
    item: &AgendaItem,
    recap: &Content,
    editor: &mut impl StewardInput,
    councillors: &mut [Councillor<'_, T>],
    reader: &mut ProposalReader<'_>,
    reads: &mut Vec<SeatRead>,
    rankable: Option<&BTreeMap<u32, PostId>>,
) -> anyhow::Result<Option<Decision>> {
    let stop = editor.stop_signal();
    let mut progress = Progress::default();
    // Raced against the Steward's Ctrl-C: a stop drops the item where it
    // stands, cancelling the call in flight.
    let result = crate::stop::race(
        stop.as_ref(),
        run_item(
            item,
            recap,
            editor,
            councillors,
            reader,
            reads,
            rankable,
            &mut progress,
        ),
    )
    .await;
    let stopped_at = chrono::Utc::now();
    let e = match result {
        Ok(decision) => return Ok(decision),
        Err(e) => e,
    };
    // The witnesses' conversations as they stand, sealed beside the seats'
    let witnesses: Vec<(Holder, misanthropic::prompt::CachedPrompt)> = progress
        .bench
        .as_ref()
        .map(|bench| {
            bench
                .conversations()
                .into_iter()
                .map(|(id, prompt)| (Holder::Witness(id), prompt))
                .collect()
        })
        .unwrap_or_default();
    if let Some(requested) = crate::stop::StewardStopRequested::find(&e) {
        // Questions put in the round the stop cut short: on that round's
        // record, as a completed round's are
        if let Some(bench) = &progress.bench {
            let heard = bench.take_heard();
            if let Some(round) = progress.rounds.last_mut() {
                round
                    .witness_questions
                    .extend(heard.iter().map(|e| e.question()));
            }
        }
        // The reads of a turn the stop cut short are still with its seat.
        let round = match progress.stage {
            Stage::Round { number, .. } => number,
            _ => 0,
        };
        for councillor in councillors.iter_mut() {
            collect_reads(reads, round, councillor);
        }
        let mut facts = crate::stop::take(editor, requested, stopped_at);
        // The Steward's standing on the item, as far as it was decided
        facts.steward_recused = if !item.category.determines_recusal() {
            Some(false)
        } else if progress.recusal_answers.len() == CouncilSeat::ALL.len() {
            Some(Determination::new(progress.recusal_answers.clone()).recused())
        } else {
            None
        };
        let stop = abort::StewardStop {
            facts,
            attachments: stop_attachments(item, reads, &progress, reader.readings()),
        };
        return Err(abort::seal(
            config,
            Moment::item(item, progress.stage),
            &stop,
            &sealed(councillors, &witnesses),
            Some(ClerkBriefing::of(item)),
        )
        .await);
    }
    match abort::Cause::find(&e) {
        Some(cause) => Err(abort::seal(
            config,
            Moment::item(item, progress.stage.clone()),
            cause,
            &sealed(councillors, &witnesses),
            Some(ClerkBriefing::of(item)),
        )
        .await),
        None => Err(e),
    }
}

/// [`live_conversations`], and the witnesses' conversations beside them
fn sealed<'c, T: RawTransport>(
    councillors: &'c [Councillor<'_, T>],
    witnesses: &'c [(Holder, misanthropic::prompt::CachedPrompt)],
) -> Vec<Conversation<'c>> {
    let mut all = live_conversations(councillors);
    all.extend(witnesses.iter().map(|(holder, prompt)| Conversation {
        holder: holder.clone(),
        prompt,
    }));
    all
}

/// Every seat's deliberation and, once it has run, its isolated
/// recusal-determination conversation, as they stand.
pub(crate) fn live_conversations<'c, T: RawTransport>(
    councillors: &'c [Councillor<'_, T>],
) -> Vec<Conversation<'c>> {
    councillors
        .iter()
        .flat_map(|c| {
            let recusal = c.determination_conversation().map(|prompt| Conversation {
                holder: Holder::SeatRecusal(c.role),
                prompt,
            });
            std::iter::once(Conversation {
                holder: Holder::Seat(c.role),
                prompt: c.conversation(),
            })
            .chain(recusal)
        })
        .collect()
}

/// How far an item has got, kept outside [`run_item`] so that what it
/// held survives the item being cut short: where it was (for a seal), and
/// what a stop's record carries (the recusal determination and the rounds
/// so far, every response as it was recorded).
#[derive(Debug, Clone)]
pub(crate) struct Progress {
    /// The turn in progress.
    pub stage: Stage,
    /// The recusal question as each seat was asked it (after its identity
    /// block), once the determination has opened.
    pub recusal_question: Option<Content>,
    /// The seats' recusal answers recorded so far.
    pub recusal_answers: Vec<RecusalAnswer>,
    /// The rounds so far; the last may be partial (the seats that spoke).
    pub rounds: Vec<Round>,
    /// The item's witnesses, whose testimony so far a stop's record carries
    pub bench: Option<crate::witness::Bench>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            stage: Stage::Briefing,
            recusal_question: None,
            recusal_answers: Vec::new(),
            rounds: Vec::new(),
            bench: None,
        }
    }
}

impl Progress {
    /// A seat spoke in round `number`.
    fn spoke(&mut self, number: i32, round_type: RoundType, response: &AgentResponse) {
        match self.rounds.last_mut() {
            Some(round) if round.number == number => round.responses.push(response.clone()),
            _ => self.rounds.push(Round {
                number,
                round_type,
                responses: vec![response.clone()],
                steward_contribution: None,
                witness_questions: Vec::new(),
            }),
        }
    }

    /// The rounds as completed, Steward notes included.
    fn completed(&mut self, rounds: &[Round]) {
        self.rounds = rounds.to_vec();
    }
}

/// What a Steward stop's record carries: everything a decided item's
/// record would, as far as the item got (Steward, 2026-10-17). The
/// briefing attachments come from [`briefing::attachments`], the one
/// renderer; the recusal question, the answers so far and the rounds so
/// far are in the shapes a decision's signed `data` holds them
/// (agentkit's `RecusalVote` and `CouncilRound`).
pub(crate) fn stop_attachments(
    item: &AgendaItem,
    reads: &[SeatRead],
    progress: &Progress,
    readings: &BTreeMap<u32, crate::docket::Reading>,
) -> Vec<RecordAttachment> {
    let mut attachments = briefing::attachments(item, reads, &progress.rounds, readings);
    if let Some(bench) = &progress.bench {
        attachments.extend(bench.testimonies());
    }
    record_attachments(
        attachments,
        progress.recusal_question.as_ref(),
        &progress.recusal_answers,
        &progress.rounds,
    )
}

/// What a Steward stop's record carries when the stop came after the
/// item was deliberated and voted, during its decision's summary (the
/// Lawyer's draft and its review): the decision's own attachments (built by
/// [`deliberate`] with [`briefing::attachments`]), its recusal
/// determination and its rounds. Nothing of it was recorded or signed.
pub(crate) fn decision_stop_attachments(
    item: &AgendaItem,
    decision: &Decision,
) -> Vec<RecordAttachment> {
    let question = item
        .category
        .determines_recusal()
        .then(|| context::recusal_question(item));
    record_attachments(
        decision.attachments.clone(),
        question.as_ref(),
        decision.recusal.as_ref().map_or(&[], |d| d.answers()),
        &decision.rounds,
    )
}

/// The stop record's attachments from their parts.
fn record_attachments(
    briefing: Vec<agora_common::govlog::CouncilAttachment>,
    recusal_question: Option<&Content>,
    recusal_answers: &[RecusalAnswer],
    rounds: &[Round],
) -> Vec<RecordAttachment> {
    let mut out: Vec<RecordAttachment> = briefing
        .into_iter()
        .filter_map(|a| {
            let content = a.content.value()?.clone();
            Some(RecordAttachment {
                name: a.name,
                note: a.note,
                content,
            })
        })
        .collect();
    if let Some(question) = recusal_question {
        out.push(RecordAttachment {
            name: "recusal-question.md".to_string(),
            note: "The recusal question (Constitution Art. IV § 3, GOV-2026-0009) as \
                   each seat was asked it, alone, after its identity block."
                .to_string(),
            content: question.to_string(),
        });
    }
    if !recusal_answers.is_empty() {
        let votes = agora_agentkit::govlog::StewardRecusal::from(&Determination::new(
            recusal_answers.to_vec(),
        ))
        .votes;
        out.push(RecordAttachment {
            name: "recusal-determinations.json".to_string(),
            note: format!(
                "The recusal answers recorded before the stop ({} of {}), in the \
                 shape a decision's `steward_recusal.votes` holds them: each seat's \
                 explanation for the record, its answer, and the thinking the API \
                 returned (Anthropic's summary). No determination was recorded \
                 unless all four answered.",
                recusal_answers.len(),
                CouncilSeat::ALL.len()
            ),
            content: serde_json::to_string_pretty(&votes).expect("recusal votes always serialize"),
        });
    }
    if !rounds.is_empty() {
        let rounds: Vec<CouncilRound> = rounds.iter().map(CouncilRound::from).collect();
        out.push(RecordAttachment {
            name: "rounds.json".to_string(),
            note: "The rounds before the stop, in the shape a decision's `rounds` holds \
                   them: every response recorded, and the Steward's notes. The last \
                   round holds only the seats that spoke before the stop."
                .to_string(),
            content: serde_json::to_string_pretty(&rounds).expect("rounds always serialize"),
        });
    }
    out
}

/// Above this, what a seat is shown is flagged as large (about 100K
/// tokens), so an oversized determination or opening is seen before it is
/// sent four times.
const LARGE_PROMPT_BYTES: usize = 400_000;

/// Print and log what every seat is about to be shown: its size, the
/// earlier decisions it opens with, and the attached threads it carries.
/// Every request is also archived and dumped whole (`AGORA_PROMPT_DUMP_DIR`,
/// `just council-prompts`).
fn shown(what: &str, item: &AgendaItem, content: &Content) {
    let bytes = content.to_string().len();
    let history: Vec<String> = item.history.iter().map(|d| d.id.to_string()).collect();
    let related: Vec<String> = item
        .comments
        .related()
        .map(|t| t.post_id.to_string())
        .collect();
    println!(
        "  Shown to every seat ({what}): {bytes} bytes; earlier decisions: {}; attached threads: {}{}",
        if history.is_empty() {
            "none".to_string()
        } else {
            history.join(", ")
        },
        if related.is_empty() {
            "none".to_string()
        } else {
            related.join(", ")
        },
        if bytes > LARGE_PROMPT_BYTES {
            "  ** LARGE **"
        } else {
            ""
        },
    );
    tracing::info!(
        agenda_item = %item.id,
        what,
        bytes,
        earlier_decisions = ?history,
        attached_threads = ?related,
        "shown to every seat"
    );
}

/// The "round" a read made during the recusal determination is filed
/// under: before round 1 (see `briefing::render_reads`).
pub(crate) const DETERMINATION_STAGE: i32 = 0;

/// Move `councillor`'s Clerk reads into `reads`, stamped with the round.
fn collect_reads<T: RawTransport>(
    reads: &mut Vec<SeatRead>,
    round: i32,
    councillor: &mut Councillor<'_, T>,
) {
    let role = councillor.role;
    reads.extend(
        councillor
            .take_reads()
            .into_iter()
            .map(|read| SeatRead { round, role, read }),
    );
}

/// A witness exchange, for the Steward's terminal
fn show_exchange(exchange: &crate::witness::Exchange) {
    use crate::witness::Outcome;
    println!(
        "\n  [witness {}, asked by the {} in round {}]\n  Q: {}",
        exchange.witness.id, exchange.seat, exchange.round, exchange.question
    );
    match &exchange.outcome {
        Outcome::Answered { answer, .. } => println!("  A: {}", answer.join("\n\n")),
        Outcome::Unheard(why) => println!("  (unanswered: {why:?})"),
    }
}

/// The rounds of [`deliberate`], with the Council seated. `stage` is kept
/// at the turn in progress, so a refusal can be sealed with where it came
/// back.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_item<T: RawTransport>(
    item: &AgendaItem,
    recap: &Content,
    editor: &mut impl StewardInput,
    councillors: &mut [Councillor<'_, T>],
    reader: &mut ProposalReader<'_>,
    reads: &mut Vec<SeatRead>,
    rankable: Option<&BTreeMap<u32, PostId>>,
    progress: &mut Progress,
) -> anyhow::Result<Option<Decision>> {
    // --- The recusal determination (Constitution Art. IV § 3) ---
    //
    // First, before the Steward is offered anything and before any round
    // opens: "determined on the record at the start of deliberation by
    // simple majority of the four Council agents".
    let bench = councillors.first().and_then(|c| c.bench().cloned());
    progress.bench = bench.clone();
    let recusal = determine_recusal(item, councillors, reader, progress).await?;
    // What each seat read during the determination, filed before round 1.
    for councillor in councillors.iter_mut() {
        collect_reads(reads, DETERMINATION_STAGE, councillor);
    }
    let standing = recusal
        .as_ref()
        .map(Determination::standing)
        .unwrap_or(StewardStanding::Seated);

    let mut rounds: Vec<Round> = Vec::new();
    let mut steward_notes: Option<String> = None;

    // --- Round 1: Independent Analysis ---
    println!("\n--- Round 1 (Independent Analysis) ---\n");

    // The opening content: the meeting recap (recent decisions, clearly
    // sourced from the governance log), then the item and the Clerk's
    // reports, and the determination's result.
    if let Some(bench) = &bench {
        bench.open_round(1);
    }
    let mut opening = recap.clone();
    opening.extend(context::item_content_with(item, recusal.as_ref()));
    shown("round 1 opening", item, &opening);
    let mut responses = Vec::new();
    for councillor in councillors.iter_mut() {
        println!("  [{}]:", councillor.role);
        progress.stage = Stage::Round {
            number: 1,
            kind: RoundKind::Opening,
            seat: councillor.role,
        };
        let response = councillor
            .exchange(RoundType::Independent, opening.clone(), reader)
            .await?;
        collect_reads(reads, 1, councillor);
        display_response(&response);
        progress.spoke(1, RoundType::Independent, &response);
        responses.push(response);
    }

    // Check for defer votes — give the Steward a chance to respond
    // with context before auto-tabling.
    let defer_count = responses
        .iter()
        .filter(|r| r.vote == Some(CouncilVote::Defer))
        .count();

    rounds.push(Round {
        number: 1,
        round_type: RoundType::Independent,
        responses,
        steward_contribution: None,
        witness_questions: Vec::new(),
    });
    progress.completed(&rounds);

    if defer_count >= 2 && standing == StewardStanding::Recused {
        // The defer override is the Steward's (notes or tabling), and a
        // recused Steward has neither. The seats' requests stand on the
        // record, and deliberation continues among them up to the round
        // cap (see the loop below).
        println!(
            "\n  Two or more agents voted Defer. You are recused from this item, \
             so there is no override: deliberation continues among the seats."
        );
    } else if defer_count >= 2 {
        println!("\n  Two or more agents voted Defer.");
        println!("  You can add Steward Notes to provide missing context,");
        println!("  then the agents will deliberate with your notes visible.\n");

        let action = steward::prompt_action(editor, NextStep::Round(2), standing)?;
        match action {
            StewardAction::Skip if item.deferral_spent => println!("{DEFERRAL_SPENT}"),
            StewardAction::Skip => {
                println!("\n  Steward confirmed tabling.\n");
                return Ok(Some(deferred_decision(item, rounds, recusal)));
            }
            StewardAction::AddNotes(notes) => {
                steward_notes = Some(notes);
            }
            StewardAction::CallVote => {
                // Fall through to deliberation → final vote
            }
            StewardAction::Continue => {
                // Continue to deliberation without notes
            }
        }
    }

    // --- Deliberation Rounds ---
    //
    // Notes the Steward types are never dropped (#450): they go to the next
    // round if there is one, and otherwise to the vote call, and they are
    // recorded on whichever round the seats heard them in.
    //
    // On a recused item nobody is asked anything: rounds advance on their
    // own until every seat is ready or the round cap is reached (Steward,
    // 2026-10-04: choosing when to cut deliberation off is influence).
    // What the seats heard from witnesses in the round before, read to
    // the others in the next round or with the vote call
    let mut testimony: Vec<crate::witness::Exchange> = Vec::new();
    let mut round_number = 2;
    loop {
        // Check if all agents are ready to vote. Asked before the Steward
        // prompt, so the menu says what actually comes next.
        let all_ready = rounds
            .last()
            .map(|r| r.responses.iter().all(|resp| resp.ready_to_vote))
            .unwrap_or(false);
        if all_ready {
            println!("\n  All agents indicated readiness to vote.");
        }

        match standing {
            StewardStanding::Recused if all_ready => {}
            StewardStanding::Recused if round_number > RECUSED_ROUND_CAP => {
                let deferring = still_deferring(&rounds);
                // PROVISIONAL (Steward, 2026-10-04): an interim rule, to be
                // revisited once observed in real sittings and open to
                // change by the Council. Every time it is consulted at the
                // cap, the event says what it found and what it did.
                let deferred = deferring.len() >= 2 && !item.deferral_spent;
                tracing::warn!(
                    event_type = "council_recused_round_cap",
                    agenda_item = %item.id,
                    category = %item.category,
                    round_cap = RECUSED_ROUND_CAP,
                    still_deferring = deferring.len(),
                    seats = ?deferring,
                    deferral_spent = item.deferral_spent,
                    deferred,
                    "recused item reached the round cap (provisional stuck-item rule)"
                );
                if deferred {
                    println!(
                        "\n  Round cap reached with {} seat(s) still deferring: the \
                         item is deferred to a later sitting.\n",
                        deferring.len()
                    );
                    return Ok(Some(recused_deferral(item, rounds, recusal)));
                }
                println!("\n  Round cap reached: the final vote is called.");
                break;
            }
            StewardStanding::Recused => {
                println!(
                    "\n  [Steward, recused] Round {round_number} runs automatically \
                     (cap: round {RECUSED_ROUND_CAP})."
                );
            }
            // Skip the Steward prompt on the first deliberation round if
            // notes were already provided via the Round 1 Defer override —
            // go straight to calling agents with that context.
            StewardStanding::Seated if steward_notes.is_some() => {}
            StewardStanding::Seated => {
                let next = if all_ready {
                    NextStep::FinalVote
                } else {
                    NextStep::Round(round_number)
                };
                let action = steward::prompt_action(editor, next, standing)?;

                match action {
                    StewardAction::Skip if item.deferral_spent => {
                        println!("{DEFERRAL_SPENT}");
                    }
                    StewardAction::Skip => {
                        println!("\n  Steward skipped this item. Deferring.\n");
                        return Ok(Some(deferred_decision(item, rounds, recusal)));
                    }
                    StewardAction::CallVote => {
                        break;
                    }
                    StewardAction::AddNotes(notes) => {
                        steward_notes = Some(notes);
                    }
                    StewardAction::Continue => {}
                }
            }
        }

        if all_ready {
            // Any notes pending here ride with the vote call below.
            break;
        }

        println!("\n--- Round {round_number} (Deliberation) ---\n");

        if let Some(bench) = &bench {
            bench.open_round(round_number as u32);
        }
        let previous = &rounds.last().expect("round 1 exists").responses;
        let mut responses = Vec::new();
        for councillor in councillors.iter_mut() {
            // Each councillor hears the *others* — its own words are
            // already in its conversation as assistant turns.
            let others: Vec<&AgentResponse> = previous
                .iter()
                .filter(|r| r.role != councillor.role)
                .collect();
            let mut content = context::deliberation_content(
                &item.category,
                round_number,
                &others,
                heard(standing, steward_notes.as_deref())?,
                crate::witness::heard_by(&testimony, councillor.role),
            );
            if let Some(reminder) = crate::witness::reminder(item) {
                content.push(reminder);
            }

            println!("  [{}]:", councillor.role);
            progress.stage = Stage::Round {
                number: round_number,
                kind: RoundKind::Deliberation,
                seat: councillor.role,
            };
            let response = councillor
                .exchange(RoundType::Deliberation, content, reader)
                .await?;
            collect_reads(reads, round_number, councillor);
            display_response(&response);
            progress.spoke(round_number, RoundType::Deliberation, &response);
            responses.push(response);
        }

        testimony = bench.as_ref().map(|b| b.take_heard()).unwrap_or_default();
        for exchange in &testimony {
            show_exchange(exchange);
        }
        rounds.push(Round {
            number: round_number,
            round_type: RoundType::Deliberation,
            responses,
            steward_contribution: steward_notes.take(),
            witness_questions: testimony.iter().map(|e| e.question()).collect(),
        });
        progress.completed(&rounds);

        round_number += 1;
    }

    // --- Final Vote ---
    println!("\n--- Final Vote ---\n");

    let kind = final_round_kind(&item.category);
    let mut responses = Vec::new();
    for councillor in councillors.iter_mut() {
        println!("  [{}]:", councillor.role);
        progress.stage = Stage::Round {
            number: round_number,
            kind,
            seat: councillor.role,
        };
        let response = councillor
            .exchange(
                RoundType::FinalVote,
                context::vote_call_for(
                    item,
                    standing,
                    heard(standing, steward_notes.as_deref())?,
                    crate::witness::heard_by(&testimony, councillor.role),
                ),
                reader,
            )
            .await?;
        collect_reads(reads, round_number, councillor);
        display_response(&response);
        progress.spoke(round_number, RoundType::FinalVote, &response);
        responses.push(response);
    }

    rounds.push(Round {
        number: round_number,
        round_type: RoundType::FinalVote,
        responses: responses.clone(),
        // Read to every seat with the vote call.
        steward_contribution: steward_notes,
        witness_questions: Vec::new(),
    });
    progress.completed(&rounds);

    // A scheduling item is ranked, not voted: aggregate, show the Steward,
    // and record. The Steward executes the order and does not vote on it.
    if let Some(rankable) = rankable {
        return Ok(Some(ranked_decision(item, rounds, &responses, rankable)));
    }

    // The seats' votes and statements, from the round they were cast in.
    // Every final response here was spoken (a refusal aborts the sitting
    // above), and `Record::submit` refused a blank statement and anything
    // but a vote, so this fails only on a program error, and then nothing
    // is signed.
    let final_round = CouncilRound::from(rounds.last().expect("the final round, pushed above"));
    let seats = SeatVotes::from_final_round(&final_round)
        .map_err(|e| anyhow::anyhow!("the final round cannot be recorded: {e}"))?;

    // Two or more seats deferring defer the item, unless the Steward
    // vetoes (agentkit `FinalVotes::outcome`): he is told before he votes
    let deferring = seats
        .iter()
        .filter(|(_, v)| v.choice == agora_agentkit::govlog::SeatChoice::Defer)
        .count();
    let defers = deferring >= agora_agentkit::govlog::DEFERRING_SEATS
        && matches!(
            item.category,
            DecisionCategory::Routine | DecisionCategory::Policy | DecisionCategory::Constitutional
        );
    if defers {
        println!(
            "\n  {deferring} seats voted Defer: the item is deferred to a later sitting{}.",
            match standing {
                StewardStanding::Seated => " unless you veto it",
                StewardStanding::Recused => "",
            }
        );
    }

    // The Steward's vote: `Recused`, unasked, when the seats so determined.
    let steward = steward::prompt_vote(editor, &item.category, &seats, standing)?;
    let votes = Votes { seats, steward };
    let decided = Decided::Voted(votes);
    let outcome = decided.outcome(item.category);
    let vote_tally = decided.tally();

    println!("\n  Outcome: {}  (Vote: {vote_tally})\n", outcome.name());

    // The landmark flag and the tags. Neither is asked of a Steward
    // recused from the item: the landmark flag weighs the decision as
    // precedent (Steward, 2026-09-28, on agora#468), and the tags are
    // signed into the entry and stored with its precedent summary, which
    // later readers and retrieval see, so a tag is a say on how the
    // decision is found and read (Steward, 2026-10-04). The program tags
    // it `steward-recused` instead.
    let (landmark, tags) = match standing {
        StewardStanding::Seated => (
            steward::prompt_landmark(editor)?,
            steward::prompt_tags(editor)?,
        ),
        StewardStanding::Recused => (false, vec![STEWARD_RECUSED_TAG.to_string()]),
    };
    // Deferred at the final vote: tagged so, by the program
    let mut tags = tags;
    if outcome == crate::models::DecisionOutcome::Deferred {
        tags.push(crate::models::FINAL_VOTE_DEFERRED_TAG.to_string());
    }

    Ok(Some(Decision {
        agenda_item_id: item.id,
        title: item.title.clone(),
        category: item.category,
        rounds,
        decided,
        landmark,
        tags,
        attachments: Vec::new(),
        recusal,
        seat_models: None,
    }))
}

/// What the Steward is told on trying to table an item whose deferral is
/// spent.
const DEFERRAL_SPENT: &str = "  Refused: this item was deferred at its last hearing with the Steward \
     recused, and may not be deferred again (Steward, 2026-10-04). It goes \
     to the final vote.";

/// The seats still deferring: those that voted `defer` in round 1 and have
/// not since said they are ready to vote. `defer` exists only in round 1
/// (the later rounds' vote has no `defer`), so a seat's standing request
/// for more information is its round-1 defer, until it declares itself
/// ready.
fn still_deferring(rounds: &[Round]) -> Vec<CouncilSeat> {
    let Some(first) = rounds.first() else {
        return Vec::new();
    };
    let latest = rounds.last().unwrap_or(first);
    first
        .responses
        .iter()
        .filter(|r| r.vote == Some(CouncilVote::Defer))
        .map(|r| r.role)
        .filter(|role| {
            rounds.len() == 1
                || latest
                    .responses
                    .iter()
                    .any(|r| r.role == *role && !r.ready_to_vote)
        })
        .collect()
}

/// A recused item deferred to a later sitting: two or more seats still
/// deferring at the round cap (Steward, 2026-10-04). Recorded as
/// `Deferred` with the existing wire states (seats `defer`, the Steward
/// `recused`), tagged so the next hearing cannot defer it again, and with
/// the open questions the deferring seats named attached, so the community
/// can answer them before it returns.
pub(crate) fn recused_deferral(
    item: &AgendaItem,
    rounds: Vec<Round>,
    recusal: Option<Determination>,
) -> Decision {
    let deferring = still_deferring(&rounds);
    let mut questions = String::new();
    if let Some(first) = rounds.first() {
        for response in first
            .responses
            .iter()
            .filter(|r| deferring.contains(&r.role))
        {
            let _ = std::fmt::Write::write_fmt(
                &mut questions,
                format_args!("## The {}\n\n", response.role),
            );
            if response.questions.is_empty() {
                questions.push_str("(No question named; see the seat's position.)\n\n");
            }
            for q in &response.questions {
                let _ = std::fmt::Write::write_fmt(&mut questions, format_args!("- {q}\n"));
            }
            questions.push('\n');
        }
    }
    Decision {
        agenda_item_id: item.id,
        title: item.title.clone(),
        category: item.category,
        rounds,
        decided: Decided::RecusedDeferral {
            deferring: u32::try_from(deferring.len()).expect("at most four seats"),
        },
        landmark: false,
        tags: vec![
            STEWARD_RECUSED_TAG.to_string(),
            RECUSED_DEFERRED_TAG.to_string(),
        ],
        attachments: vec![agora_common::govlog::CouncilAttachment {
            name: "open-questions.md".to_string(),
            note: "The questions the seats still deferring named in round 1: what the \
                   Council needs answered before this item returns."
                .to_string(),
            content: questions.trim_end().to_string().into(),
        }],
        recusal,
        seat_models: None,
    }
}

/// Steward notes as the seats may hear them: refused outright on an item
/// the Steward is recused from. The menu cannot produce notes for a
/// recused Steward ([`steward::prompt_action`]); this is the second lock,
/// at the point where notes would reach a seat.
fn heard(standing: StewardStanding, notes: Option<&str>) -> anyhow::Result<Option<&str>> {
    match (standing, notes) {
        (StewardStanding::Recused, Some(_)) => anyhow::bail!(
            "refusing to read Steward notes to the seats: the Steward is recused \
             from this item (Constitution Art. IV § 3)"
        ),
        (_, notes) => Ok(notes),
    }
}

/// The recusal determination (Constitution Art. IV § 3, GOV-2026-0009):
/// each seat answers in its own isolated conversation whether the item
/// limits the Steward's powers ([`Councillor::determine`]), and a simple
/// majority of the four decides. Recorded (printed, logged and kept for
/// the decision's signed record; every prompt and raw response is in the
/// prompt archive) before any round opens. `None` on a category that does
/// not determine recusal.
pub(crate) async fn determine_recusal<T: RawTransport>(
    item: &AgendaItem,
    councillors: &mut [Councillor<'_, T>],
    reader: &mut ProposalReader<'_>,
    progress: &mut Progress,
) -> anyhow::Result<Option<Determination>> {
    if !item.category.determines_recusal() {
        return Ok(None);
    }
    println!("\n--- Recusal determination (Constitution Art. IV § 3) ---\n");
    let question = context::recusal_question(item);
    // What every seat is about to be shown, so the Steward (and anyone
    // reading along) can see it is the right material before it is sent.
    shown("recusal question", item, &question);
    progress.recusal_question = Some(question.clone());
    let mut answers = Vec::new();
    for councillor in councillors.iter_mut() {
        println!("  [{}]:", councillor.role);
        progress.stage = Stage::RecusalDetermination {
            seat: councillor.role,
        };
        let answer = councillor.determine(question.clone(), reader).await?;
        progress.recusal_answers.push(answer.clone());
        tracing::info!(
            agenda_item = %item.id,
            seat = %answer.role,
            limits_steward_powers = answer.limits_steward_powers,
            explanation = %answer.explanation,
            "recusal determination: seat answered"
        );
        if !answer.rationale.is_empty() {
            println!("  Thinking: {}\n", answer.rationale);
        }
        println!("{}\n", answer.explanation);
        println!(
            "  Limits the Steward's powers: {}\n",
            if answer.limits_steward_powers {
                "Yes"
            } else {
                "No"
            }
        );
        answers.push(answer);
    }
    let determination = Determination::new(answers);
    tracing::info!(
        agenda_item = %item.id,
        affirmative = determination.affirmative(),
        seats = CouncilSeat::ALL.len(),
        recused = determination.recused(),
        "recusal determination recorded"
    );
    if determination.recused() {
        println!(
            "  Determined {}-{}: the item limits the Steward's powers. The Steward \
             is RECUSED: no deliberation, no vote, no veto. Threshold: {}",
            determination.affirmative(),
            CouncilSeat::ALL.len() - determination.affirmative(),
            item.category
                .recused_threshold()
                .unwrap_or_else(|| item.category.threshold())
        );
    } else {
        println!(
            "  Determined {}-{}: not a majority finding that the item limits the \
             Steward's powers. The Steward sits.",
            determination.affirmative(),
            CouncilSeat::ALL.len() - determination.affirmative(),
        );
    }
    Ok(Some(determination))
}

/// Aggregate the seats' rankings into the scheduling item's decision, and
/// print what the Steward is about to execute.
pub(crate) fn ranked_decision(
    item: &AgendaItem,
    rounds: Vec<Round>,
    responses: &[AgentResponse],
    rankable: &BTreeMap<u32, PostId>,
) -> Decision {
    let ballots: Vec<Ballot> = responses
        .iter()
        .filter_map(|r| {
            r.ranking.as_ref().map(|seat| Ballot {
                seat: r.role,
                sitting_capacity: seat.sitting_capacity,
                ranking: seat.ranking.clone(),
            })
        })
        .collect();
    let submitted = ballots.len();
    let ranking = AgendaRanking::aggregate(rankable, ballots);

    let docket = item.docket.as_ref();
    let title_of = |number: u32| {
        docket
            .and_then(|d| d.candidate(number))
            .map(|c| c.title.as_str())
            .unwrap_or("?")
    };
    println!(
        "\n  [Ranking] Borda count over {submitted} seat(s), scale {}",
        ranking.rankable
    );
    for (i, placed) in ranking.order.iter().enumerate() {
        let marker = if (i as u32) < ranking.cut { "*" } else { " " };
        println!(
            "   {marker}{:>2}. P{:<3} borda {:>4}  seats {}  pos-sum {:>3}{}  {}",
            i + 1,
            placed.number,
            placed.borda,
            placed.seats,
            placed.position_sum,
            if placed.tied_with_next {
                "  (TIED with next)"
            } else {
                ""
            },
            title_of(placed.number),
        );
    }
    println!(
        "  Cut (median capacity): {} — starred items are the docket.",
        ranking.cut
    );
    match ranking.condorcet_winner {
        Some(n) if ranking.order.first().map(|p| p.number) == Some(n) => {
            println!("  Pairwise check: P{n} beats every other head to head; Borda agrees.")
        }
        Some(n) => println!(
            "  !! Pairwise check: P{n} beats every other head to head, but Borda puts \
             it lower. Say so in the record before executing the order."
        ),
        None => println!(
            "  !! Pairwise check: no item beats every other head to head (a cycle \
             or a tie at the top). The Borda order is a tiebreak, not a consensus."
        ),
    }
    let ids: Vec<String> = ranking.docket().map(|p| p.post_id.to_string()).collect();
    if !ids.is_empty() {
        println!("\n  To execute the docket, one item per run:");
        println!("    just council-run {} --force", ids.join(","));
    }

    // The seats that ranked are `ranked`, any that did not and the
    // Steward `abstain`; the tally says in words that the Steward executes
    // the order and does not vote (agentkit `Decided::Ranked`)
    Decision {
        agenda_item_id: item.id,
        title: item.title.clone(),
        category: item.category,
        rounds,
        decided: Decided::Ranked(ranking),
        landmark: false,
        tags: vec!["schedule".to_string()],
        attachments: Vec::new(),
        recusal: None,
        seat_models: None,
    }
}

/// Print a recorded submission for the Steward: the seat's thinking
/// first, then the for-the-record statement and the vote.
fn display_response(response: &AgentResponse) {
    let rationale = response.rationale.to_string();
    if !rationale.trim().is_empty() {
        println!("  Thinking: {rationale}\n");
    }
    if !response.position.trim().is_empty() && response.position != rationale {
        println!("  For the record: {}", response.position);
    }
    for q in &response.questions {
        println!("  Question: {q}");
    }
    println!(
        "  Vote: {}  |  Ready: {}\n",
        response.vote_label(),
        if response.ready_to_vote { "Yes" } else { "No" }
    );
}

/// The all-defer decision used when the Steward tables an item. Only a
/// seated Steward tables, so `recusal` is a determination that did not
/// recuse, or none.
pub(crate) fn deferred_decision(
    item: &AgendaItem,
    rounds: Vec<Round>,
    recusal: Option<Determination>,
) -> Decision {
    Decision {
        agenda_item_id: item.id,
        title: item.title.clone(),
        category: item.category,
        rounds,
        decided: Decided::Tabled,
        landmark: false,
        tags: Vec::new(),
        attachments: Vec::new(),
        recusal,
        seat_models: None,
    }
}

/// What the final round asks of a seat.
fn final_round_kind(category: &DecisionCategory) -> RoundKind {
    match category {
        DecisionCategory::Schedule => RoundKind::Ranking,
        DecisionCategory::Routine
        | DecisionCategory::Policy
        | DecisionCategory::Constitutional
        | DecisionCategory::Emergency => RoundKind::FinalVote,
    }
}

// There were `final_turn` and `refusal_tabled_decision` here: a refusal on
// a final vote or ranking offered the Steward retry once / table / abort,
// and tabling recorded a Deferred decision tagged `api-refusal`. Removed
// with the refusal-abort rule (see `crate::abort`): a refusal anywhere
// aborts the sitting, is never retried, and writes no decision.

#[cfg(test)]
#[path = "deliberation_tests.rs"]
mod tests;
