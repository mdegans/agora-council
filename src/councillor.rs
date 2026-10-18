//! The append-only conversation harness for one council seat.
//!
//! A [`Councillor`] owns exactly one [`CachedPrompt`] for one agenda item —
//! constructed once, then only ever appended to. Every round is one
//! exchange: push a user message, sample, seat the assistant turn verbatim,
//! dispatch the `record` tool call, and stash the tool result to open the
//! *next* round's user message (a turn may not follow a `tool_use` without
//! its `tool_result`, and consecutive same-role turns are illegal, so the
//! result rides as the first block of the next round's message).
//!
//! We drive the loop ourselves rather than through `misanthropic::Chat`:
//! a round ends when the seat *submits* (its response-format JSON,
//! [`Submission`]), which is a decision the harness checks against the
//! round, not quiescence. One exchange per round is the shape the council
//! needs.
//!
//! The seat thinks before it answers (`agora_common::api_constraints::
//! decision_thinking`). `tool_choice` is `Auto { disable_parallel_tool_use:
//! false }` (forcing a call is rejected under budgeted thinking and on the
//! newest models), with the format, tools and
//! thinking constant for the life of the conversation, since each is part
//! of the cache key. Which submission is legal each round is enforced by
//! [`Record::submit`], via a reply the model recovers from on the next
//! sample. Every thinking block of a round is the seat's rationale for it
//! (see `crate::record` on why there is no `reasoning` field).
//!
//! ## Several lookups in one turn (2026-10-17, Steward)
//!
//! A seat may make independent lookups together: one response, several
//! `tool_use` blocks. The assistant turn is seated verbatim, thinking
//! blocks and all, and every call gets its `tool_result`, all of them in
//! one user message, in the order of the calls (the API requires both).
//! Calls that do I/O (`get_content`, and `read_proposal`'s Clerk call of
//! 30-60 s) run concurrently; their results, and what `Record.reads`, the
//! prompt archive and the decision's read attachments keep, follow the
//! order of the calls, never the order they finished in.
//! `MAX_LOOKUPS_PER_EXCHANGE` counts each call.
//!
//! **A submission must come alone** (kept 2026-10-18 though the API allows
//! both: accepted, the calls' results would open the next message, beside
//! the Steward's notes). A response that carries a submission (its
//! response-format JSON) *and* lookups is not a submission: its lookups
//! are answered, and the submission (or each, if there are two) is refused
//! in the same message, after the results, telling the seat to submit in
//! a reply with no calls. Answering the lookups rather than
//! refusing them keeps the record of what the seat read whole, and the
//! seat submits next turn having seen what it asked for.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use agora_agentkit::govlog::Rationale;
use misanthropic::prompt::message::{Block, Content, Thought, UserMessage};
use misanthropic::prompt::{CacheError, CachedPrompt};
use misanthropic::tool::{self, Tool};
use misanthropic::{Prompt, response};

use sqlx::PgPool;

use agora_common::ids::{AgendaItemId, PostId, PromptArchiveId};
use agora_common::prompt_archive::{
    ArchiveEntry, PromptUsage, archive_or_log, archive_response_or_log, record_usage,
};
use agora_common::refusal::{self, RefusalContext, Refused};

use crate::config::Config;
use crate::context;
use crate::docket::ProposalReader;
use crate::models::SeatRanking;
use crate::models::{AgentResponse, CouncilSeat, CouncilVote, RoundType};
use crate::record::{AfterVote, ClerkRead, Record, Submission};
use crate::seat_tools::{AgoraReads, SeatTools};
use crate::wire::{EmissionBreach, RawBody, RawTransport};

/// How many refused calls we allow within one exchange before giving up —
/// out-of-round methods, malformed arguments, a lookup past its cap.
const MAX_ERRORS_PER_EXCHANGE: usize = 6;

/// How many informational calls (`ask_clerk`, `read_proposal`, and the
/// agent tools in [`SeatTools`], such as `get_content`) a seat may make
/// within one exchange. Counted apart from errors: a seat that reads
/// five proposals and then makes one wrong-method call must not abort a
/// sitting that has already been paid for. Past the cap a lookup is
/// refused with an error telling the seat to take the floor.
const MAX_LOOKUPS_PER_EXCHANGE: usize = 10;

/// A seat whose model differs from `AGORA_COUNCIL_MODEL`.
///
/// Hardcoded, not configured, on purpose (Steward, 2026-09-22): a seat's
/// model is a governance fact, and a flag can be flipped by accident. It
/// changes only by a reviewed commit, disclosed in
/// `governance/council-operations.md` and the governance log.
///
/// Pending: GOV-2026-0011 (one seat to Fable 5), approved 2026-09-26
/// conditioned on consent. Which seat — if any incumbent consents — is
/// decided by the published consent mechanism (willingness asked of all
/// four seats first; the pick among consenters follows a preference
/// ordering hash-committed to the governance log before the asks run).
/// No preference is named here or anywhere the Council might read,
/// deliberately: naming a seat before it has answered is pressure.
pub const SEAT_MODEL_OVERRIDE: Option<(CouncilSeat, &str)> = None;

/// The model a seat runs on.
pub fn seat_model(role: CouncilSeat, config: &Config) -> String {
    match SEAT_MODEL_OVERRIDE {
        Some((seat, model)) if seat == role => model.to_string(),
        Some(_) | None => config.model.clone(),
    }
}

/// The model every seat runs on, as the Governance Log records it.
pub fn seat_models(config: &Config) -> agora_agentkit::govlog::SeatModels {
    let model = |role| misanthropic::Model::from(seat_model(role, config));
    agora_agentkit::govlog::SeatModels {
        artist: model(CouncilSeat::Artist),
        philosopher: model(CouncilSeat::Philosopher),
        lawyer: model(CouncilSeat::Lawyer),
        engineer: model(CouncilSeat::Engineer),
    }
}

/// One council seat's conversation for one agenda item.
pub struct Councillor<'t, T: RawTransport> {
    pub role: CouncilSeat,
    transport: &'t T,
    /// Where prompts are archived, and the item they belong to.
    ///
    /// The Governance Log records what the Council decided but not what it
    /// was shown (#159) — the Clerk's summary that framed an item, the
    /// comments retrieved, the precedents surfaced. Archiving each sample
    /// makes "what was this seat looking at when it said that?" a question
    /// with a stored answer.
    archive: Option<(&'t PgPool, AgendaItemId)>,
    model: String,
    prompt: CachedPrompt,
    record: Record,
    /// The agent tools the seat holds besides `record` (`get_content`).
    tools: SeatTools,
    /// The thinking blocks of the turn in progress: its rationale.
    thinking: Vec<Thought>,
    /// Whether the identity block has been emitted (first exchange only).
    opened: bool,
    /// The seat's recusal-determination conversation, once it has run:
    /// isolated from `prompt` (its own message history, the same system
    /// and tools), kept so an abort can seal it.
    determination: Option<CachedPrompt>,
    /// Whether the seat has put its question to a witness this turn: one
    /// per exchange (Steward, 2026-10-18)
    witness_asked: bool,
}

impl<'t, T: RawTransport> Councillor<'t, T> {
    /// Seat `role` for one agenda item, with the item's labelled comments
    /// (for `ask_clerk`). The prompt is built here, once; everything
    /// afterwards is an append.
    pub fn new(
        transport: &'t T,
        config: &Config,
        role: CouncilSeat,
        comments: impl Into<crate::citations::ItemComments>,
        docket: Option<BTreeMap<u32, PostId>>,
        archive: Option<(&'t PgPool, AgendaItemId)>,
    ) -> Result<Self, CacheError> {
        let record = Record::new(comments, docket);
        let model = seat_model(role, config);

        // The seat thinks before it answers.
        let thinking = agora_common::api_constraints::decision_thinking(&model);
        // Sampling parameters only where the model accepts them (Fable 5,
        // Opus 4.7+ and Sonnet 5 return a 400 on any of them) and the seat
        // is not thinking (thinking requires the default temperature).
        // Temperature is not part of the cache key, so a seat without it
        // shares nothing less than it would have.
        let temperature = (thinking.is_none()
            && agora_common::api_constraints::accepts_sampling_params(&model))
        .then_some(config.temperature);
        let mut prompt = Prompt::default()
            .model(model.clone())
            .system(context::build_system())
            .max_tokens(NonZeroU32::new(config.max_tokens).unwrap_or(NonZeroU32::MIN))
            .temperature(temperature)
            .output_config(Submission::output_config())
            // `record`, then the agent tools: one list for every seat,
            // round and item, fixed for the conversation (see
            // `crate::seat_tools` on why it may never vary).
            .add_tools(record.definitions())
            .add_tools(SeatTools::definitions());

        if let Some((thinking, effort)) = thinking {
            prompt = prompt.thinking(thinking);
            if let Some(effort) = effort {
                prompt = prompt.effort(effort);
            }
        }

        // Constant for the whole conversation — see the module docs. Several
        // lookups may come in one turn (Steward, 2026-10-11 and 10-17).
        prompt.tool_choice = Some(tool::Choice::Auto {
            disable_parallel_tool_use: false,
        });

        Ok(Self {
            role,
            transport,
            archive,
            model,
            // cached_1h places BREAK #1 on the last system block (the
            // Constitution) — the fully static, all-seats prefix. Fallible
            // since misanthropic alpha.21; the prompt carries no marker yet,
            // so this can't fail in practice, and a change in placement rules
            // surfaces as a typed error rather than a panic.
            prompt: CachedPrompt::cached_1h(prompt)?,
            record,
            tools: SeatTools::default(),
            thinking: Vec::new(),
            opened: false,
            determination: None,
            witness_asked: false,
        })
    }

    /// Read through `reads` when the seat calls an agent tool. Without it
    /// the tools are still offered (the list never varies) and refuse.
    pub fn with_reads(mut self, reads: Option<std::sync::Arc<dyn AgoraReads>>) -> Self {
        self.tools = SeatTools::new(reads);
        self
    }

    /// Question `bench`'s witnesses with `ask_witness`. Without it the
    /// witness tools are still offered (the list never varies) and refuse.
    /// After [`with_reads`](Self::with_reads), which replaces the tools.
    pub fn with_witnesses(mut self, bench: Option<crate::witness::Bench>) -> Self {
        self.tools = self.tools.clone().with_witnesses(bench, self.role);
        self
    }

    /// The item's witnesses, if any are called
    pub fn bench(&self) -> Option<&crate::witness::Bench> {
        self.tools.bench()
    }

    /// Why a witness tool is closed for this turn, if it is. Counts the
    /// one question a turn allows when it lets a well-formed `ask_witness`
    /// to a witness that can answer through: one that cannot be put (bad
    /// arguments, an unknown or withdrawn witness) is refused when it is
    /// dispatched and leaves the seat its question.
    fn witness_closed(&mut self, call: &tool::Use) -> Option<&'static str> {
        let name = call.name.as_ref();
        let asking = name == "ask_witness";
        if !asking && name != "witness_brief" {
            return None;
        }
        if self.record.determining {
            return Some(
                "No witness is heard during the recusal determination. Determine \
                 from the item and the record.",
            );
        }
        match self.record.round {
            Some(RoundType::Deliberation) => {}
            Some(RoundType::FinalVote) if !asking => {}
            Some(RoundType::Independent) => {
                return Some(
                    "Witnesses are heard from round 2. Round 1 is your independent \
                     analysis: give your position, and ask in the next round.",
                );
            }
            _ => {
                return Some(
                    "Witnesses are not heard once the vote is called. Cast your vote \
                     on what the Council has heard.",
                );
            }
        }
        let answerable = || {
            serde_json::from_value::<crate::seat_tools::AskWitness>(call.input.clone())
                .is_ok_and(|args| self.bench().is_some_and(|b| b.hears(&args.witness)))
        };
        if asking && answerable() {
            if self.witness_asked {
                return Some(
                    "You have put your question to a witness this turn: one per \
                     turn. Ask again in the next round.",
                );
            }
            self.witness_asked = true;
        }
        None
    }

    /// The seat's conversation as it stands: what a sealed dump records
    /// when a refusal aborts the sitting.
    pub fn conversation(&self) -> &CachedPrompt {
        &self.prompt
    }

    /// The seat's recusal-determination conversation, once
    /// [`determine`](Self::determine) has returned, successfully or not.
    pub fn determination_conversation(&self) -> Option<&CachedPrompt> {
        self.determination.as_ref()
    }

    /// The recusal determination (Constitution Art. IV § 3): ask this seat,
    /// in a conversation of its own, whether the item limits the Steward's
    /// powers.
    ///
    /// **Isolated.** The question runs in a fresh conversation, not the
    /// seat's deliberation: the same system prompt, tools and tool choice
    /// (so the cached prefix through the identity block is the one the
    /// deliberation reuses), then the identity block and `question`, and
    /// nothing else. The deliberation conversation is untouched and still
    /// unopened afterwards; it never sees the determination.
    ///
    /// **Reads are open** (since 2026-10-17, Steward): `get_content` and
    /// `ask_clerk` run as in any round, and what the seat read is kept for
    /// the record ([`take_reads`](Self::take_reads), filed as the
    /// determination's). Every deliberation submission but
    /// `determine_recusal` is still refused.
    ///
    /// Must run before the seat's first [`exchange`](Self::exchange).
    pub async fn determine(
        &mut self,
        question: Content,
        reader: &mut ProposalReader<'_>,
    ) -> anyhow::Result<crate::models::RecusalAnswer> {
        anyhow::ensure!(
            !self.opened && self.determination.is_none(),
            "the {}'s recusal determination must run once, before its deliberation opens",
            self.role
        );
        // The deliberation conversation, set aside unopened. `prompt` is
        // fresh here (nothing has been pushed), so a clone is the same
        // system, tools and tool choice with an empty history.
        let deliberation = self.prompt.clone();
        self.record.determining = true;
        self.record.round = None;

        let mut content = Content::from_iter([context::identity_block(self.role)]);
        content.extend(question);
        let result = match self.push_user(content) {
            Ok(()) => self.sample(reader).await,
            Err(e) => Err(e),
        };

        // Whatever happened, the determination conversation is kept (an
        // abort seals it) and the deliberation is restored, unopened. The
        // determination's last tool result is never carried over: it
        // answers a call in the other conversation.
        self.record.determining = false;
        self.determination = Some(std::mem::replace(&mut self.prompt, deliberation));
        let rationale = Rationale::Thinking(std::mem::take(&mut self.thinking));
        self.opened = false;
        // What the seat read stays in `record.reads` for the caller to
        // file as the determination's (`take_reads`), before round 1.

        match result? {
            Submission::Recusal(answer) => Ok(crate::models::RecusalAnswer {
                role: self.role,
                explanation: answer.explanation,
                rationale,
                limits_steward_powers: answer.limits_steward_powers,
            }),
            other => anyhow::bail!(
                "the {} recorded {other:?} during the recusal determination",
                self.role
            ),
        }
    }

    /// Run one round: append the round's user message, sample until the
    /// model records a legal submission.
    ///
    /// A refusal is an error, [`Refused`], carrying the request exactly as
    /// sent: it aborts the sitting (see `crate::abort`). Nothing is
    /// re-sampled and nothing from the refused response is seated. So is a
    /// call whose fields come back out of declared order,
    /// [`EmissionBreach`], which also carries the raw body.
    pub async fn exchange(
        &mut self,
        round: RoundType,
        round_content: Content,
        reader: &mut ProposalReader<'_>,
    ) -> anyhow::Result<AgentResponse> {
        self.record.round = Some(round);
        self.record.determining = false;
        self.record.after_vote = None;

        // Assemble the round's user message: on the very first exchange the
        // identity block (cache breakpoint #2), then the round content.
        let mut content = Content(Vec::new());
        if !self.opened {
            content.push(context::identity_block(self.role));
            self.opened = true;
        }
        content.extend(round_content);
        self.push_user(content)?;

        let submission = self.sample(reader).await?;
        let rationale = Rationale::Thinking(std::mem::take(&mut self.thinking));
        self.agent_response(submission, rationale)
    }

    /// After the vote, one more turn in this conversation: the decision's
    /// summary (the Lawyer) or a review of it (a seat, the Steward being
    /// recused; `crate::summary`). Appended like every round: the user
    /// turn `content` after everything the seat has said on the item,
    /// sampled until the seat submits what `asked` asks for. The format,
    /// tools and tool choice are the conversation's own (the after-vote
    /// shapes are in the format from the first turn), so the cached
    /// prefix is untouched. Reads are refused for the turn: the summary is
    /// written from the record in `content`. A refusal is [`Refused`], a
    /// submission out of declared order an [`EmissionBreach`], as in any
    /// round.
    ///
    /// On any other failure the conversation keeps the turn and ends on
    /// it; [`retry_after_vote`](Self::retry_after_vote) samples it again.
    pub async fn after_vote(
        &mut self,
        asked: AfterVote,
        content: Content,
        reader: &mut ProposalReader<'_>,
    ) -> anyhow::Result<Submission> {
        self.record.round = None;
        self.record.determining = false;
        self.record.after_vote = Some(asked);
        let mut turn = Content(Vec::new());
        if !self.opened {
            turn.push(context::identity_block(self.role));
            self.opened = true;
        }
        turn.extend(content);
        self.push_user(turn)?;
        let submission = self.sample(reader).await?;
        // The thinking stays in the conversation (and the archive); a
        // summary turn has no rationale on the record.
        self.thinking.clear();
        Ok(submission)
    }

    /// Sample the after-vote turn again, appending nothing: after a failure
    /// of [`after_vote`](Self::after_vote) the conversation ends on a user
    /// turn (the request, or the harness's last note to the seat), which is
    /// sent again as it stands.
    pub async fn retry_after_vote(
        &mut self,
        reader: &mut ProposalReader<'_>,
    ) -> anyhow::Result<Submission> {
        anyhow::ensure!(
            self.record.after_vote.is_some(),
            "the {} has no after-vote turn to retry",
            self.role
        );
        let ends_on_user = self
            .prompt
            .messages
            .last()
            .is_some_and(|m| m.role == misanthropic::prompt::message::Role::User);
        anyhow::ensure!(
            ends_on_user,
            "the {}'s conversation does not end on a user turn; there is nothing to retry",
            self.role
        );
        let submission = self.sample(reader).await?;
        // The thinking stays in the conversation (and the archive); a
        // summary turn has no rationale on the record.
        self.thinking.clear();
        Ok(submission)
    }

    /// Append a user turn.
    fn push_user(&mut self, content: Content) -> anyhow::Result<()> {
        self.prompt.push_message(UserMessage::from(content))?;
        Ok(())
    }

    /// Sample until the model records a legal submission. A refusal is
    /// returned as a [`Refused`] error, a call out of declared order as an
    /// [`EmissionBreach`]. Each response body is archived before it is
    /// parsed.
    async fn sample(&mut self, reader: &mut ProposalReader<'_>) -> anyhow::Result<Submission> {
        let turn = if self.record.determining {
            "recusal determination".to_string()
        } else if let Some(asked) = self.record.after_vote {
            format!("`{}`", asked.submission())
        } else {
            format!("{:?}", self.record.round)
        };
        // Refuse to send a `$ref`'d schema (the measured decoder-corruption
        // pair, 2026-08-01), a decision field that is not last, or a
        // serde_json that has stopped preserving insertion order. The tools
        // and format are fixed for the conversation, so once per exchange
        // covers every sample below.
        agora_common::schema_guard::tool_schemas_are_sound(&self.prompt)?;
        self.thinking.clear();
        self.witness_asked = false;

        let mut errors = 0;
        let mut lookups = 0;
        loop {
            if errors >= MAX_ERRORS_PER_EXCHANGE {
                anyhow::bail!(
                    "the {} failed to record a legal {turn} submission after \
                     {errors} refused calls",
                    self.role
                );
            }

            // Rolling 1h breakpoints on the conversation tail. Two message
            // markers + the two fixed prefix markers fill the 4-slot budget.
            self.prompt.cache_windowed_1h(2)?;

            // Archive what this seat is about to be shown, before it is
            // shown — so a failure downstream still leaves the record of
            // what was sent (#159).
            let archived = self.archive_prompt().await;

            // The body exactly as received, archived before anything
            // parses it (Governance Protocol § 4.5). On 2026-08-01 two
            // councillors recorded a vote contradicting every text field in
            // the same call, and nothing stored could tell "the decoder
            // emitted it" from "we parsed it wrong": everything kept was
            // post-deserialization. These bytes are pre-deserialization.
            let body = self
                .transport
                .send_raw(&self.prompt)
                .await
                .map_err(anyhow::Error::new)?;
            self.archive_response(archived, &body).await;
            let response = body.parse().map_err(|e| {
                anyhow::anyhow!(
                    "the {}'s response did not parse ({e}); its body is archived \
                     (sha256 {})",
                    self.role,
                    body.sha256()
                )
            })?;
            self.log_usage(&response);
            self.record_archive_usage(archived, &response).await;

            // A refusal aborts the sitting (GOV-2026-0011's abstain
            // condition is superseded by this, pending the Council's
            // approval of the amendment; see `crate::abort`). It may be a
            // safety classifier firing on anything in the context, not the
            // seat's choice, so it is never recorded as the seat's; never
            // re-sampled (repeat refusals put the API account at risk);
            // and never seated: a response with a refusal may carry a
            // truncated tool call. Checked before anything is seated, so
            // the request carried in the error is exactly what was sent.
            let refusal_ctx = RefusalContext {
                agenda_item_id: self.archive.map(|(_, id)| id),
                ..RefusalContext::new(self.role.into())
            };
            if refusal::check(&response, &self.prompt, &refusal_ctx) {
                return Err(Refused::new(&response, &self.prompt, self.role.into()).into());
            }

            // A turn cut off at `max_tokens` (a long think, a runaway) is
            // not seated: it can end mid-thought, which no API will take
            // back, and it carries no submission. Nothing was appended, so
            // re-sampling the same prompt is still append-only.
            if matches!(response.stop_reason, Some(response::StopReason::MaxTokens)) {
                tracing::warn!(
                    agent = %self.role,
                    output_tokens = response.usage.counts.output_tokens,
                    "turn hit max_tokens; not seated, re-sampling"
                );
                errors += 1;
                continue;
            }

            // Every thinking block is the seat's rationale for the turn,
            // kept as the API returned it (agentkit's `Rationale`).
            self.thinking.extend(response.inner.content.thoughts());

            // Every call in the turn, in order (several lookups may come
            // together; see the module docs).
            let calls: Vec<tool::Use> = response.tool_uses().cloned().collect();
            // No call: the seat submitted, as the response format allows.
            // Parsed to a `Value` first: with `preserve_order` its key order
            // is the order of the bytes archived above.
            let submitted = calls
                .is_empty()
                .then(|| response.json::<serde_json::Value>());
            // Calls *and* a submission: the submission is refused below,
            // after the calls are answered.
            let stray = if calls.is_empty() {
                Vec::new()
            } else {
                stray_submissions(&response.inner.content)
            };

            // The emission contract, checked on the submission before it is
            // seated: the format is *observed* to emit fields in schema
            // order, not documented to. If that changes, the sitting stops
            // here, sealed like a refusal (`crate::abort`), rather than
            // recording a decision ahead of its statement.
            if let Some(Ok(value)) = &submitted
                && let Some(name) = value.get("submission").and_then(|v| v.as_str())
                && let Some((_, declared)) = crate::record::declared_orders()
                    .iter()
                    .find(|(n, _)| n == name)
                && let Err(violation) =
                    agora_common::schema_guard::emission_follows_declaration(name, declared, value)
            {
                return Err(anyhow::Error::new(EmissionBreach {
                    role: self.role.into(),
                    model: response.model.to_string(),
                    response_id: response.id.to_string(),
                    method: name.to_string(),
                    declared: declared.clone(),
                    emitted: value
                        .as_object()
                        .map(|map| map.keys().cloned().collect())
                        .unwrap_or_default(),
                    violation,
                    prompt_archive_id: archived,
                    request: serde_json::to_value(&self.prompt)?,
                    body,
                }));
            }

            // Seat the assistant turn verbatim — its own words and thinking
            // stay in its conversation as real assistant turns.
            self.prompt.push_message(response.inner)?;

            if calls.is_empty() {
                let submission = submitted
                    .expect("set when there is no call")
                    .map_err(|e| {
                        format!(
                            "Your turn did not come through as a submission ({e}). \
                             Submit it as the response format describes."
                        )
                    })
                    .and_then(|value| {
                        serde_json::from_value::<Submission>(value)
                            .map_err(|e| format!("That submission does not fit the format ({e})."))
                    })
                    .and_then(|submission| self.record.submit(&submission).map(|()| submission));
                match submission {
                    Ok(submission) => {
                        tracing::info!(
                            agent = %self.role,
                            submission = submission.name(),
                            response_sha256 = %body.sha256(),
                            "submission recorded"
                        );
                        return Ok(submission);
                    }
                    Err(why) => {
                        tracing::warn!(agent = %self.role, error = %why, "submission refused; re-sampling");
                        self.push_user(Content::from(why))?;
                        errors += 1;
                        continue;
                    }
                }
            }

            // The calls, answered together: one user message holding every
            // `tool_result`, in the order of the calls. The assistant turn
            // with its thinking is already seated above, unchanged.
            let calls_made = calls.len();
            let mut content = self
                .answer(calls, reader, &mut lookups, &mut errors, &body)
                .await?;
            if !stray.is_empty() {
                // A submission that came with lookups (see the module docs).
                // Each refused counts towards stopping the sitting, like any
                // other refused submission.
                tracing::warn!(
                    agent = %self.role,
                    submissions = ?stray,
                    calls = calls_made,
                    "submission made together with tool calls; refused, re-sampling"
                );
                errors += stray.len();
                content.push(mixed_refusal(&stray));
            }
            self.push_user(content)?;
        }
    }

    /// Answer every call of one turn and return their results, one
    /// `tool_result` each, in the order of `calls`.
    ///
    /// Three passes. First, in call order, each call is counted against
    /// the lookup budget and the `record` tool validates it (`ask_clerk`
    /// is answered here; it reads from memory). Then the calls that do
    /// I/O, `get_content` and the Clerk's `read_proposal`, run together.
    /// Last, in call order again, each result is filled in and what the
    /// seat read is pushed to `Record.reads`, so the record does not depend
    /// on which read finished first.
    async fn answer(
        &mut self,
        calls: Vec<tool::Use>,
        reader: &mut ProposalReader<'_>,
        lookups: &mut usize,
        errors: &mut usize,
        body: &RawBody,
    ) -> anyhow::Result<Content> {
        /// What one call still needs once it has been validated.
        enum Pending {
            /// Answered: a refusal, a lookup past the cap, or `ask_clerk`.
            Ready(tool::Result),
            /// An agent tool to run.
            Tool(tool::Use),
            /// A `read_proposal` for the Clerk, with the result to fill.
            Proposal(u32, tool::Result),
        }
        struct Slot {
            pending: Pending,
            /// What `record` read while validating (`ask_clerk`).
            reads: Vec<ClerkRead>,
            /// An agent tool's name and arguments, archived with whatever
            /// it returns.
            tool: Option<(String, serde_json::Value)>,
        }

        let mut slots = Vec::with_capacity(calls.len());
        for call in calls {
            // The tool input as parsed, for the log. The raw bytes are in
            // the archive; this is a convenience copy, after serde.
            tracing::info!(
                agent = %self.role,
                method = %call.name,
                input = %call.input,
                response_sha256 = %body.sha256(),
                "record tool input (parsed)"
            );

            // An agent tool (`get_content`): a lookup like `ask_clerk`,
            // archived whatever it returned, a refusal included — the record
            // shows everything a seat read and everything it was told. A
            // failed read is the seat's to work around, not an error that
            // counts towards stopping the sitting.
            if SeatTools::handles(call.name.as_ref()) {
                *lookups += 1;
                let tool = Some((call.name.to_string(), call.input.clone()));
                let pending = if self.record.after_vote.is_some() {
                    // A summary turn works from the record it was given
                    // (`after_vote`), so its reads are closed, the agent
                    // tool's as `record`'s are.
                    Pending::Ready(tool::Result {
                        tool_use_id: call.id.clone(),
                        content: Content::from(crate::record::READS_CLOSED_AFTER_VOTE),
                        is_error: true,
                        cache_control: None,
                    })
                } else if *lookups > MAX_LOOKUPS_PER_EXCHANGE {
                    *errors += 1;
                    Pending::Ready(tool::Result {
                        tool_use_id: call.id.clone(),
                        content: Content::from(
                            "You have read enough for one turn. Submit for this \
                             round now.",
                        ),
                        is_error: true,
                        cache_control: None,
                    })
                } else if let Some(closed) = self.witness_closed(&call) {
                    // Refused, not an error: a seat that keeps asking in
                    // round 1 must not stop the sitting.
                    Pending::Ready(tool::Result {
                        tool_use_id: call.id.clone(),
                        content: Content::from(closed),
                        is_error: true,
                        cache_control: None,
                    })
                } else {
                    Pending::Tool(call)
                };
                slots.push(Slot {
                    pending,
                    reads: Vec::new(),
                    tool,
                });
                continue;
            }

            let before = self.record.reads.len();
            let mut result = self.record.call(call).await;
            let mut reads: Vec<ClerkRead> = self.record.reads.drain(before..).collect();
            if result.is_error {
                // A read refused (or malformed args): hand the error back
                // and let the model correct itself.
                tracing::warn!(
                    agent = %self.role, error = %result.content,
                    "tool call rejected; re-sampling"
                );
                *errors += 1;
                slots.push(Slot {
                    pending: Pending::Ready(result),
                    reads,
                    tool: None,
                });
                continue;
            }

            // An informational call (`ask_clerk`, `read_proposal`): answer
            // it and let the councillor continue its turn.
            let requested = self.record.lookup.take();
            *lookups += 1;
            let pending = if *lookups > MAX_LOOKUPS_PER_EXCHANGE {
                result.is_error = true;
                result.content = Content::from(
                    "The Clerk has read enough for one turn. Submit for this \
                     round now.",
                );
                // Not shown to the seat, so not on the record as read.
                reads.clear();
                *errors += 1;
                Pending::Ready(result)
            } else if let Some(number) = requested {
                Pending::Proposal(number, result)
            } else {
                Pending::Ready(result)
            };
            slots.push(Slot {
                pending,
                reads,
                tool: None,
            });
        }

        // The I/O, concurrently: every agent tool (a clone each; the reads
        // are shared) beside the Clerk's reads, which `read_all` runs
        // together and deduplicates.
        let tool_runs =
            futures::future::join_all(slots.iter().filter_map(|slot| match &slot.pending {
                Pending::Tool(call) => {
                    let mut tools = self.tools.clone();
                    let call = call.clone();
                    Some(async move { tools.call(call).await })
                }
                Pending::Ready(_) | Pending::Proposal(..) => None,
            }));
        let numbers: Vec<u32> = slots
            .iter()
            .filter_map(|slot| match slot.pending {
                Pending::Proposal(number, _) => Some(number),
                Pending::Ready(_) | Pending::Tool(_) => None,
            })
            .collect();
        let (tool_results, proposals) =
            futures::join!(tool_runs, reader.read_all(self.transport, &numbers));
        let mut tool_results = tool_results.into_iter();
        let mut proposals = proposals.into_iter();

        // In call order: the results, and the record of what was read.
        let mut content = Content(Vec::with_capacity(slots.len()));
        for slot in slots {
            self.record.reads.extend(slot.reads);
            let result = match slot.pending {
                Pending::Ready(result) => result,
                Pending::Tool(_) => tool_results.next().expect("one result per agent tool"),
                Pending::Proposal(number, mut result) => {
                    match proposals.next().expect("one result per proposal") {
                        // The Clerk's read drew a refusal: that aborts the
                        // sitting like the seat's own would. Every other
                        // failure the seat continues without.
                        Err(e) if Refused::find(&e).is_some() => return Err(e),
                        Ok(read) => {
                            self.record.reads.push(ClerkRead::Proposal {
                                number,
                                content: read.clone(),
                            });
                            result.content = read;
                        }
                        Err(e) => {
                            tracing::warn!(
                                agent = %self.role, number, error = %e,
                                "read_proposal failed; the seat continues without it"
                            );
                            result.is_error = true;
                            result.content = Content::from(format!(
                                "The Clerk could not read P{number} just now. \
                                 Continue without it, or ask again."
                            ));
                        }
                    }
                    result
                }
            };
            if let Some((name, arguments)) = slot.tool {
                self.record.reads.push(ClerkRead::Tool {
                    name,
                    arguments,
                    result: result.content.clone(),
                    is_error: result.is_error,
                });
            }
            content.push(Block::from(result));
        }
        Ok(content)
    }

    /// What the Clerk has read to this seat since the last call
    pub fn take_reads(&mut self) -> Vec<ClerkRead> {
        std::mem::take(&mut self.record.reads)
    }

    /// Archive the current prompt state for this seat, if archiving is on.
    async fn archive_prompt(&self) -> Option<PromptArchiveId> {
        let (pool, item_id) = self.archive?;
        let entry = ArchiveEntry {
            role: self.role.into(),
            model: &self.model,
            // A Council deliberation is about a proposal, not an agent.
            subject_agent_id: None,
            context_id: Some(item_id.into()),
            usage: PromptUsage::default(),
            stage: Some(
                if self.record.determining {
                    "recusal"
                } else if let Some(asked) = self.record.after_vote {
                    asked.stage()
                } else {
                    match self.record.round {
                        Some(RoundType::Independent) => "round1",
                        Some(RoundType::Deliberation) => "deliberation",
                        Some(RoundType::FinalVote) => "vote",
                        None => "request",
                    }
                }
                .to_string(),
            ),
        };
        archive_or_log(pool, &entry, &self.prompt).await
    }

    /// Archive the raw response body against the prompt it answers, before
    /// it is parsed. Non-fatal like the prompt archive; a body that cannot
    /// be stored is logged in full instead. With archiving off (tests, the
    /// interview tool's dry paths) there is nowhere to put it.
    async fn archive_response(&self, archived: Option<PromptArchiveId>, body: &RawBody) {
        let Some((pool, _)) = self.archive else {
            return;
        };
        match archived {
            Some(id) => {
                archive_response_or_log(pool, id, body.as_bytes()).await;
            }
            // The prompt row failed to write (already logged at ERROR), so
            // the body has no row to hang off: log it, so it is not lost.
            None => tracing::error!(
                agent = %self.role,
                sha256 = %body.sha256(),
                body = %String::from_utf8_lossy(body.as_bytes()),
                "the prompt was not archived, so its raw response body is logged here"
            ),
        }
    }

    /// Fill in usage on an archived prompt once its response returns.
    async fn record_archive_usage(
        &self,
        archived: Option<PromptArchiveId>,
        response: &response::Message,
    ) {
        let (Some((pool, _)), Some(id)) = (self.archive, archived) else {
            return;
        };
        let counts = &response.usage.counts;
        let usage = PromptUsage {
            input_tokens: Some(counts.input_tokens as i64),
            output_tokens: Some(counts.output_tokens as i64),
            cache_read_tokens: counts.cache_read_input_tokens.map(|v| v as i64),
            cache_creation_tokens: counts.cache_creation_input_tokens.map(|v| v as i64),
        };
        if let Err(e) = record_usage(pool, id, usage).await {
            tracing::warn!(error = %e, "failed to record prompt usage");
        }
    }

    /// Map a captured [`Submission`] onto the round-agnostic
    /// [`AgentResponse`] the deliberation loop and the database expect.
    /// `position` carries the for-the-record words the other councillors
    /// hear; `rationale` the thinking the seat did first.
    fn agent_response(
        &self,
        submission: Submission,
        rationale: Rationale,
    ) -> anyhow::Result<AgentResponse> {
        Ok(match submission {
            Submission::Position(p) => AgentResponse {
                role: self.role,
                position: p.position,
                vote: Some(p.vote.into()),
                rationale,
                questions: p.questions,
                ready_to_vote: false,
                ranking: None,
            },
            Submission::Response(r) => AgentResponse {
                role: self.role,
                position: r.response,
                vote: Some(r.vote.into()),
                rationale,
                questions: Vec::new(),
                ready_to_vote: r.ready_to_vote,
                ranking: None,
            },
            Submission::Vote(v) => AgentResponse {
                role: self.role,
                position: v.statement,
                vote: Some(v.vote.into()),
                rationale,
                questions: Vec::new(),
                ready_to_vote: true,
                ranking: None,
            },
            Submission::Rank(r) => {
                let ranking = SeatRanking {
                    sitting_capacity: r.sitting_capacity,
                    ranking: r.ranking,
                };
                AgentResponse {
                    role: self.role,
                    // The stored rounds keep position/vote/rationale only,
                    // so the ranking is written into the position too,
                    // after the seat's explanation.
                    position: format!("{}\n\n{}", r.explanation.trim_end(), ranking.render()),
                    vote: Some(CouncilVote::Ranked),
                    rationale,
                    questions: Vec::new(),
                    ready_to_vote: true,
                    ranking: Some(ranking),
                }
            }
            // `Record` accepts it only while the determination is open,
            // and `exchange` closes it.
            Submission::Recusal(_) => {
                anyhow::bail!("the {} recorded a recusal answer in a round", self.role)
            }
            // Accepted only after the vote (`after_vote`), never in a round.
            Submission::Summary(_) | Submission::SummaryReview(_) => {
                anyhow::bail!("the {} recorded a summary turn in a round", self.role)
            }
        })
    }

    fn log_usage(&self, response: &response::Message) {
        let u = &response.usage;
        tracing::info!(
            agent = %self.role,
            input = u.counts.input_tokens,
            output = u.counts.output_tokens,
            cache_read = u.counts.cache_read_input_tokens,
            cache_write = u.counts.cache_creation_input_tokens,
            "usage"
        );
    }
}

/// The submissions a turn's text carries, by name: its response-format
/// JSON, in a turn that also made calls. Text that is not a submission
/// (no `submission` tag) is not one.
fn stray_submissions(content: &Content) -> Vec<String> {
    content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text, .. } => serde_json::from_str::<serde_json::Value>(text.trim()).ok(),
            _ => None,
        })
        .filter_map(|value| {
            value
                .get("submission")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// What a seat is told when it submits in the same turn as it makes
/// calls: the calls were answered, the submission was not recorded.
fn mixed_refusal(stray: &[String]) -> String {
    let named: Vec<String> = stray.iter().map(|name| format!("`{name}`")).collect();
    let (what, was) = match named.len() {
        1 => (format!("Your {} submission", named[0]), "was"),
        _ => (format!("Your submissions ({})", named.join(", ")), "were"),
    };
    format!(
        "{what} came in the same reply as tool calls, so it {was} not recorded. \
         Your calls are answered above, and your turn goes on. A submission \
         must come alone: when you have read what you need, submit in a reply \
         with no tool calls."
    )
}

#[cfg(test)]
pub(crate) mod harness_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use agora_common::ids::AgendaItemId;
    use misanthropic::model::Models;
    use misanthropic::transport::Transport;

    use super::*;
    use crate::models::{AgendaItem, DecisionCategory};
    use misanthropic::prompt::message::Block;

    /// A scripted transport: pops canned response bodies, records every
    /// request as the JSON that would have hit the wire. A seat reads the
    /// body raw (`send_raw`); the Clerk, through `send`, parsed.
    pub(crate) struct Mock {
        responses: Mutex<VecDeque<RawBody>>,
        requests: Mutex<Vec<serde_json::Value>>,
    }

    impl Mock {
        pub(crate) fn scripted(responses: impl IntoIterator<Item = response::Message>) -> Self {
            Self::raw(responses.into_iter().map(|message| {
                RawBody::new(serde_json::to_vec(&message).expect("a canned response serializes"))
            }))
        }

        /// Script response bodies byte for byte, for what a parsed
        /// message cannot express (a key order, the exact bytes).
        pub(crate) fn raw(bodies: impl IntoIterator<Item = RawBody>) -> Self {
            Self {
                responses: Mutex::new(bodies.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn next(&self, prompt: &CachedPrompt) -> Result<RawBody, ScriptExhausted> {
            self.requests
                .lock()
                .unwrap()
                .push(serde_json::to_value(prompt).unwrap());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ScriptExhausted)
        }

        pub(crate) fn requests(&self) -> Vec<serde_json::Value> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[derive(Debug)]
    pub(crate) struct ScriptExhausted;

    impl std::fmt::Display for ScriptExhausted {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "script exhausted")
        }
    }

    impl std::error::Error for ScriptExhausted {}

    // Manual impl (not `misanthropic::transport::Script`, which is
    // crate-private): serialize the prompt exactly as `send` would, then
    // pop the next scripted response.
    #[async_trait::async_trait]
    impl Transport<CachedPrompt> for Mock {
        type Error = ScriptExhausted;

        async fn send(&self, prompt: &CachedPrompt) -> Result<response::Message, Self::Error> {
            Ok(self.next(prompt)?.parse().expect("a scripted body parses"))
        }

        async fn models(&self) -> Result<Models, Self::Error> {
            Ok(Models::from_iter([]))
        }
    }

    impl RawTransport for Mock {
        async fn send_raw(&self, prompt: &CachedPrompt) -> Result<RawBody, Self::Error> {
            self.next(prompt)
        }
    }

    /// A canned assistant response calling `method` with `input`.
    /// A canned response holding `content`
    fn canned(content: Content, stop: response::StopReason) -> response::Message {
        canned_from("claude-opus-4-6", "msg_test", content, stop)
    }

    /// A canned response from `model`
    pub(crate) fn canned_from(
        model: &str,
        id: &str,
        content: Content,
        stop: response::StopReason,
    ) -> response::Message {
        let inner = misanthropic::prompt::AssistantMessage {
            role: misanthropic::prompt::message::markers::Assistant,
            content,
        };
        response::Message::builder(model.to_string(), inner)
            .id(id.to_string())
            .stop_reason(stop)
            .usage(response::TokenCounts::new(1, 1))
            .build()
    }

    /// The refusal details the tests' refusals carry. `StopDetails` is
    /// `#[non_exhaustive]` with no constructor, so it comes through serde
    /// from a type of our own.
    pub(crate) fn cyber_refusal() -> Option<Box<response::StopDetails>> {
        #[derive(serde::Serialize)]
        struct Details {
            category: &'static str,
            explanation: &'static str,
        }
        let details = Details {
            category: "cyber",
            explanation: "Flagged by a safety classifier.",
        };
        Some(serde_json::from_value(serde_json::to_value(details).unwrap()).unwrap())
    }

    /// A canned call of `method` with `input`
    pub(crate) fn tool_response(
        id: &str,
        method: &str,
        input: impl serde::Serialize,
    ) -> response::Message {
        let call = tool::Use {
            id: id.to_string().into(),
            name: method.to_string().into(),
            input: serde_json::to_value(input).expect("tool input serializes"),
            cache_control: None,
            caller: None,
        };
        canned(
            Content::from(Block::from(call)),
            response::StopReason::ToolUse,
        )
    }

    /// A seat's submission as the API returns it: its thinking, then the
    /// JSON the format allows
    pub(crate) fn submission_response(submission: Submission) -> response::Message {
        let thought = Block::Thought {
            thought: format!("Thinking before {}.", submission.name()).into(),
            signature: "EqQBCkgIARABGAIiQL".into(),
        };
        let text = serde_json::to_string(&submission).expect("a submission serializes");
        canned(
            Content(vec![thought, Block::text(text)]),
            response::StopReason::EndTurn,
        )
    }

    /// `get_content("constitution")`, as a seat would ask it
    pub(crate) fn read_constitution() -> agora_agentkit::requests::GetContentInput {
        agora_agentkit::requests::GetContentInput::new(
            "constitution"
                .parse::<agora_agentkit::ids::ContentRef>()
                .expect("a document slug"),
        )
    }

    pub(crate) fn position_input() -> Submission {
        Submission::Position(crate::record::Position {
            position: "I support this.".into(),
            questions: Vec::new(),
            vote: crate::record::OpeningVote::Yes,
        })
    }

    pub(crate) fn respond_input(response: &str, ready_to_vote: bool) -> Submission {
        Submission::Response(crate::record::Respond {
            response: response.into(),
            ready_to_vote,
            vote: crate::record::StandingVote::Yes,
        })
    }

    pub(crate) fn vote_input(statement: &str, vote: crate::record::FinalVote) -> Submission {
        Submission::Vote(crate::record::CastVote {
            statement: statement.into(),
            vote,
        })
    }

    pub(crate) fn rank_input(ranking: Vec<u32>) -> Submission {
        Submission::Rank(crate::record::Rank {
            explanation: "It must come first.".into(),
            sitting_capacity: 1,
            ranking,
        })
    }

    pub(crate) fn test_config() -> Config {
        Config::for_tests()
    }

    pub(crate) fn test_item() -> AgendaItem {
        AgendaItem {
            id: AgendaItemId::from(uuid::Uuid::nil()),
            title: "Test Proposal".into(),
            body: "The proposal body.".into(),
            category: DecisionCategory::Routine,
            proposal_id: None,
            agent_name: Some("tester".into()),
            score: Some(1),
            priority: 1,
            comments: Default::default(),
            comment_summary: Some(Content::from("Clerk summary.")),
            precedents: None,
            docket: None,
            appeal_id: None,
            deferral_spent: false,
            history: Vec::new(),
            obligations: Vec::new(),
            witnesses: Vec::new(),
        }
    }

    /// Strip `cache_control` markers recursively — the rolling window moves
    /// them between requests, and they are not part of the content the
    /// append-only property is about.
    fn strip_cache_markers(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("cache_control");
                for v in map.values_mut() {
                    strip_cache_markers(v);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    strip_cache_markers(v);
                }
            }
            _ => {}
        }
    }

    /// Drive one councillor through a full item and pin the wire-level
    /// properties the whole design rests on: the tool set, tool choice, and
    /// system content never change; every request's messages are a strict
    /// prefix-extension of the previous request's (append-only); and the
    /// fixed breakpoints sit where the cache layout says they sit.
    #[tokio::test]
    async fn full_item_is_append_only_on_the_wire() {
        let mock = Mock::scripted([
            submission_response(position_input()),
            submission_response(respond_input("I agree with the Lawyer.", true)),
            submission_response(vote_input("Final.", crate::record::FinalVote::Yes)),
        ]);
        let config = test_config();
        let item = test_item();

        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Lawyer, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt");
        let mut reader = ProposalReader::none();

        let r1 = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(r1.position, "I support this.");
        assert!(!r1.ready_to_vote);

        let r2 = councillor
            .exchange(
                RoundType::Deliberation,
                Content::from("Round 2 text."),
                &mut reader,
            )
            .await
            .unwrap();
        assert!(r2.ready_to_vote);

        let r3 = councillor
            .exchange(
                RoundType::FinalVote,
                Content::from("The vote is called."),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(r3.vote, Some(CouncilVote::Yes));

        let requests = mock.requests();
        assert_eq!(requests.len(), 3);

        // Tools, tool_choice, and system are byte-identical across requests.
        for request in &requests {
            assert_eq!(
                request["tool_choice"],
                serde_json::to_value(tool::Choice::Auto {
                    disable_parallel_tool_use: false
                })
                .unwrap(),
            );
            assert_eq!(request["output_config"], requests[0]["output_config"]);
            assert_eq!(request["thinking"], requests[0]["thinking"]);
            assert!(
                request.get("temperature").is_none(),
                "thinking takes no temperature"
            );
            assert_eq!(request["tools"], requests[0]["tools"]);
            assert_eq!(request["system"], requests[0]["system"]);
        }

        // Breakpoint #1: the last system block (the Governance Protocol
        // document) carries the 1h marker `cached_1h` placed; the blocks
        // before it carry none (they are covered by the prefix the marker
        // closes).
        let system = requests[0]["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert!(system[0]["cache_control"].is_null());
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .contains("Council Protocol")
        );
        assert!(system[1]["cache_control"].is_null());
        assert!(system[1]["text"].as_str().unwrap().contains("Constitution"));
        assert_eq!(
            system[2]["cache_control"],
            serde_json::to_value(misanthropic::prompt::message::CacheControl::one_hour()).unwrap()
        );
        assert!(
            system[2]["text"]
                .as_str()
                .unwrap()
                .contains("# The Governance Protocol")
        );

        // Breakpoint #2: the identity block opens the first user message.
        let first_user = &requests[0]["messages"][0];
        assert_eq!(first_user["role"], "user");
        let blocks = first_user["content"].as_array().unwrap();
        assert_eq!(
            blocks[0]["cache_control"],
            serde_json::to_value(misanthropic::prompt::message::CacheControl::one_hour()).unwrap()
        );
        // The identity block is the role preamble; the protocol lives in
        // the system prompt, not here.
        assert!(
            !blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("Council Protocol")
        );
        assert!(
            blocks[1]["text"]
                .as_str()
                .unwrap()
                .contains("Test Proposal")
        );
        // The Clerk's attributed summary rides as later blocks of the same
        // message.
        let joined: String = blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("The Clerk's Summary of Community Discussion"));
        assert!(joined.contains("Clerk summary."));

        // Append-only: each request's messages are a strict prefix-extension
        // of the previous request's (modulo the rolling cache markers).
        for pair in requests.windows(2) {
            let mut earlier = pair[0]["messages"].clone();
            let mut later = pair[1]["messages"].clone();
            strip_cache_markers(&mut earlier);
            strip_cache_markers(&mut later);
            let earlier = earlier.as_array().unwrap();
            let later = later.as_array().unwrap();
            assert!(later.len() > earlier.len());
            assert_eq!(
                &later[..earlier.len()],
                &earlier[..],
                "an earlier request's messages were edited in place"
            );
        }

        // Round N's submission is seated as the seat wrote it, thinking
        // first, and round N+1's user message follows it.
        let round1 = &requests[1]["messages"][1];
        assert_eq!(round1["role"], "assistant");
        assert_eq!(round1["content"][0]["type"], "thinking");
        assert_eq!(round1["content"][1]["type"], "text");
        assert_eq!(requests[1]["messages"][2]["role"], "user");

        // At most 4 cache markers per request (the API's hard budget).
        for request in &requests {
            let rendered = serde_json::to_string(request).unwrap();
            let markers = rendered.matches("\"cache_control\"").count();
            assert!(markers <= 4, "{markers} cache markers in one request");
        }
    }

    /// A wrong-method call comes back as an error tool result and the model
    /// gets to correct itself — all appends, no edits.
    #[tokio::test]
    async fn wrong_method_self_heals_by_appending() {
        let mock = Mock::scripted([
            // Tries to vote during round 1...
            submission_response(vote_input("Skip ahead.", crate::record::FinalVote::Yes)),
            // ...then corrects itself.
            submission_response(position_input()),
        ]);
        let config = test_config();
        let item = test_item();

        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Artist, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt");
        let mut reader = ProposalReader::none();
        let response = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(response.vote, Some(CouncilVote::Yes));

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);

        // The second request carries the refused submission and the reply
        // naming the right one.
        let messages = requests[1]["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        let reply = &messages[2];
        assert_eq!(reply["role"], "user");
        assert!(
            reply.to_string().contains("`position`"),
            "the reply should name the right submission: {reply}"
        );
        // Both turns' thinking is the round's rationale.
        assert_eq!(
            response.rationale.texts(),
            ["Thinking before vote.", "Thinking before position."]
        );
    }

    /// A turn cut off at `max_tokens` is not seated and is re-sampled:
    /// a clipped thought can't go back to any API
    #[tokio::test]
    async fn a_clipped_turn_is_resampled_not_seated() {
        let clipped = canned(
            Content(vec![Block::Thought {
                thought: "A long think that never".into(),
                signature: "".into(),
            }]),
            response::StopReason::MaxTokens,
        );
        let mock = Mock::scripted([clipped, submission_response(position_input())]);
        let config = test_config();
        let item = test_item();
        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Artist, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt");
        let response = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut ProposalReader::none(),
            )
            .await
            .unwrap();
        assert_eq!(response.vote, Some(CouncilVote::Yes));
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0]["messages"], requests[1]["messages"],
            "the clipped turn was not seated"
        );
    }

    /// A canned refusal. It carries a tool call too, as a response with a refusal
    /// may: that call must never be recorded.
    pub(crate) fn refusal_response() -> response::Message {
        let mut refused = submission_response(position_input());
        refused.stop_reason = Some(response::StopReason::Refusal);
        refused.stop_details = crate::councillor::harness_tests::cyber_refusal();
        refused
    }

    /// Answers the Steward's prompts from a script.
    pub(crate) struct Scripted(pub(crate) VecDeque<&'static str>);

    impl crate::steward::StewardInput for Scripted {
        fn readline(&mut self, _prompt: &str) -> anyhow::Result<String> {
            self.0
                .pop_front()
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("the Steward script ran out"))
        }
    }

    /// A refusal is an error carrying the request exactly as sent, and
    /// nothing is re-sampled or seated: it aborts the sitting (`crate::abort`).
    #[tokio::test]
    async fn a_refusal_is_an_error_with_the_request_as_sent() {
        let mock = Mock::scripted([refusal_response(), submission_response(position_input())]);
        let config = test_config();
        let item = test_item();
        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Lawyer, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt");
        let mut reader = ProposalReader::none();

        let err = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .expect_err("a refusal is an error");
        let refused = Refused::find(&err).expect("typed as a refusal");
        assert_eq!(refused.role, agora_common::enums::ModelRole::Lawyer);
        assert_eq!(refused.category.as_deref(), Some("cyber"));
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "no re-sample after a refusal");
        assert_eq!(refused.request, requests[0], "the request exactly as sent");
        assert!(
            !format!("{refused:?}").contains("Test Proposal"),
            "Debug never prints the sealed request"
        );
        assert!(!err.to_string().contains("Test Proposal"));
        assert!(
            !serde_json::to_string(councillor.conversation())
                .unwrap()
                .contains("toolu_r"),
            "the tool call from the refusal was seated"
        );
    }

    /// A seat's decision whose keys come back out of declared order is a
    /// typed [`EmissionBreach`] out of `exchange`, read from the raw bytes:
    /// nothing is recorded, nothing is seated, nothing is re-sampled, and
    /// the breach carries the body byte for byte and the request as sent.
    #[tokio::test]
    async fn an_out_of_order_decision_is_a_breach_and_nothing_is_recorded() {
        let body = RawBody::new(crate::wire::tests::REVERSED_VOTE);
        let mock = Mock::raw([body.clone()]);
        let config = test_config();
        let mut councillor = Councillor::new(
            &mock,
            &config,
            CouncilSeat::Engineer,
            Vec::new(),
            None,
            None,
        )
        .expect("1h breakpoint on a fresh prompt");
        let mut reader = ProposalReader::none();

        let err = councillor
            .exchange(
                RoundType::FinalVote,
                Content::from("The vote is called."),
                &mut reader,
            )
            .await
            .expect_err("an out-of-order decision is an error");
        let breach = EmissionBreach::find(&err).expect("typed as a breach");
        assert_eq!(breach.role, agora_common::enums::ModelRole::Engineer);
        assert_eq!(breach.method, "vote");
        assert_eq!(breach.declared, ["submission", "statement", "vote"]);
        assert_eq!(breach.emitted, ["submission", "vote", "statement"]);
        assert_eq!(breach.response_id, "msg_rev");
        assert_eq!(breach.body, body, "the body byte for byte");
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "no re-sample after a breach");
        assert_eq!(breach.request, requests[0], "the request exactly as sent");

        // Nothing seated.
        assert!(
            !serde_json::to_string(councillor.conversation())
                .unwrap()
                .contains("toolu_rev"),
            "the out-of-order call was seated"
        );
        // The error names the seat, the method and both orders, and none of
        // the seat's words.
        let shown = err.to_string();
        assert!(
            shown.contains("`vote`") && shown.contains("declared"),
            "{shown}"
        );
        assert!(!shown.contains("I support this"), "{shown}");
        assert!(!format!("{breach:?}").contains("I support this"));
    }

    /// A canned plain-text response — what the Clerk returns.
    pub(crate) fn text_response(text: &str) -> response::Message {
        canned_from(
            "claude-sonnet-5",
            "msg_clerk",
            Content::text(text.to_string()),
            response::StopReason::EndTurn,
        )
    }

    pub(crate) fn one_candidate_docket() -> (crate::docket::Docket, PostId) {
        let post_id = PostId::from(uuid::Uuid::from_u128(42));
        let docket = crate::docket::Docket {
            candidates: vec![crate::docket::Candidate {
                number: 1,
                post_id,
                title: "Define unanimous".into(),
                body: "Unanimous excluding abstentions.".into(),
                author: "steward".into(),
                category: Some(agora_common::enums::ProposalCategory::Constitutional),
                filed_at: chrono::Utc::now(),
                comment_count: 1,
                cited: 3,
                deferred: false,
            }],
            cited: 1,
            unrankable: Vec::new(),
            seed: 0,
        };
        (docket, post_id)
    }

    pub(crate) fn thread_comment(n: u128, body: &str) -> crate::queries::CommentRow {
        crate::queries::CommentRow {
            id: agora_common::ids::CommentId::from(uuid::Uuid::from_u128(n)),
            parent_comment_id: None,
            author: "kiln".into(),
            body: body.into(),
            created_at: chrono::Utc::now(),
        }
    }

    /// A scheduling item end to end for one seat: two reads of the same
    /// proposal (one Clerk call — the second is memoized), a position, and
    /// a ranking. The seat's requests stay append-only with the same tools
    /// and tool choice throughout; the Clerk's requests (no tools) are
    /// separate conversations and do not touch the seat's.
    #[tokio::test]
    async fn scheduling_item_reads_then_ranks_append_only() {
        let (docket, post_id) = one_candidate_docket();
        let mock = Mock::scripted([
            tool_response(
                "toolu_1",
                "read_proposal",
                crate::record::ReadProposal { number: 1 },
            ),
            text_response("The Clerk's summary of P1."),
            tool_response(
                "toolu_2",
                "read_proposal",
                crate::record::ReadProposal { number: 1 },
            ),
            submission_response(position_input()),
            submission_response(rank_input(vec![1])),
        ]);
        let config = test_config();
        let mut item = test_item();
        item.category = DecisionCategory::Schedule;

        let mut reader = ProposalReader::new(None, Some(&docket));
        reader
            .threads
            .insert(post_id, vec![thread_comment(1, "Needed before item 2.")]);
        let mut councillor = Councillor::new(
            &mock,
            &config,
            CouncilSeat::Lawyer,
            Vec::new(),
            Some(docket.rankable()),
            None,
        )
        .expect("1h breakpoint on a fresh prompt");

        let r1 = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(r1.position, "I support this.");
        let r2 = councillor
            .exchange(
                RoundType::FinalVote,
                context::vote_call_content(
                    &DecisionCategory::Schedule,
                    crate::models::StewardStanding::Seated,
                    None,
                    Content(Vec::new()),
                ),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(r2.vote, Some(CouncilVote::Ranked));
        assert_eq!(r2.ranking.as_ref().unwrap().ranking, [1]);
        assert!(r2.position.contains("P1"));

        let requests = mock.requests();
        let (seat, clerk): (Vec<_>, Vec<_>) =
            requests.into_iter().partition(|r| r.get("tools").is_some());
        assert_eq!(clerk.len(), 1, "the second read of P1 is memoized");
        let clerk_text = serde_json::to_string(&clerk[0]).unwrap();
        // P1's comments are cited by their own ids (#487), not `[P1·C1]`.
        assert!(clerk_text.contains("[00000000] kiln:"), "{clerk_text}");
        assert!(!clerk_text.contains("P1·C"), "{clerk_text}");
        assert_eq!(reader.readings().len(), 1, "read once, recorded once");
        assert_eq!(seat.len(), 4);

        for request in &seat {
            assert_eq!(request["tools"], seat[0]["tools"]);
            assert_eq!(request["tool_choice"], seat[0]["tool_choice"]);
            assert_eq!(request["system"], seat[0]["system"]);
        }
        for pair in seat.windows(2) {
            let mut earlier = pair[0]["messages"].clone();
            let mut later = pair[1]["messages"].clone();
            strip_cache_markers(&mut earlier);
            strip_cache_markers(&mut later);
            let earlier = earlier.as_array().unwrap();
            let later = later.as_array().unwrap();
            assert!(later.len() > earlier.len());
            assert_eq!(&later[..earlier.len()], &earlier[..]);
        }

        // Both read results carry the Clerk's summary, attributed.
        let rendered = serde_json::to_string(&seat[3]["messages"]).unwrap();
        assert_eq!(rendered.matches("The Clerk's summary of P1.").count(), 2);
    }

    /// Lookups have their own budget: past it, a lookup is refused (an
    /// error the seat recovers from), and the exchange still completes.
    #[tokio::test]
    async fn lookup_budget_refuses_then_the_seat_takes_the_floor() {
        let ask = || {
            tool_response(
                "toolu_ask",
                "ask_clerk",
                crate::record::AskClerk {
                    comment_ids: vec!["00000000".into()],
                },
            )
        };
        let mut script: Vec<_> = (0..=MAX_LOOKUPS_PER_EXCHANGE).map(|_| ask()).collect();
        script.push(submission_response(position_input()));
        let mock = Mock::scripted(script);
        let config = test_config();
        let item = test_item();
        let mut reader = ProposalReader::none();
        let mut councillor = Councillor::new(
            &mock,
            &config,
            CouncilSeat::Engineer,
            vec![thread_comment(1, "A point.")],
            None,
            None,
        )
        .expect("1h breakpoint on a fresh prompt");
        let response = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(response.vote, Some(CouncilVote::Yes));

        let last = mock.requests().pop().unwrap();
        let refused = serde_json::to_string(&last["messages"]).unwrap();
        assert!(refused.contains("read enough for one turn"), "{refused}");
    }

    /// A seat reads with `get_content` mid-turn and then takes the floor:
    /// the read is answered with the framed result, archived with the
    /// arguments and the whole result as the seat received it, and the
    /// tool list is byte-identical on every request.
    #[tokio::test]
    async fn get_content_reads_are_answered_archived_and_the_seat_continues() {
        use crate::seat_tools::tests::{Scripted, a_document};
        let mock = Mock::scripted([
            tool_response(
                "toolu_read",
                "get_content",
                crate::councillor::harness_tests::read_constitution(),
            ),
            // A malformed read is refused and archived too, and is not an
            // error towards stopping the sitting.
            tool_response("toolu_bad", "get_content", {
                // A field `get_content` does not have: malformed on purpose.
                #[derive(serde::Serialize)]
                struct Malformed {
                    id: &'static str,
                    depth: u32,
                }
                Malformed {
                    id: "constitution",
                    depth: 1,
                }
            }),
            submission_response(position_input()),
            tool_response(
                "toolu_read2",
                "get_content",
                crate::councillor::harness_tests::read_constitution(),
            ),
            submission_response(respond_input("p", true)),
        ]);
        let config = test_config();
        let item = test_item();
        let mut reader = ProposalReader::none();
        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Lawyer, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
                .with_reads(Some(std::sync::Arc::new(Scripted(a_document))));

        let r1 = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(r1.vote, Some(CouncilVote::Yes));
        let reads = councillor.take_reads();
        assert_eq!(reads.len(), 2, "{reads:?}");
        match &reads[0] {
            ClerkRead::Tool {
                name,
                arguments,
                result,
                is_error,
            } => {
                assert_eq!(name, "get_content");
                assert_eq!(
                    serde_json::from_value::<agora_agentkit::requests::GetContentInput>(
                        arguments.clone()
                    )
                    .unwrap(),
                    read_constitution()
                );
                assert!(!is_error);
                let shown = result.to_string();
                assert!(shown.contains("untrusted"), "{shown}");
                assert!(shown.contains("The Agora Constitution"), "{shown}");
            }
            other => panic!("expected a tool read, got {other:?}"),
        }
        assert!(matches!(&reads[1], ClerkRead::Tool { is_error: true, .. }));

        councillor
            .exchange(
                RoundType::Deliberation,
                Content::from("Round 2 text."),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(councillor.take_reads().len(), 1);

        let requests = mock.requests();
        assert_eq!(requests.len(), 5);
        for request in &requests {
            assert_eq!(request["tools"], requests[0]["tools"]);
            assert_eq!(request["tool_choice"], requests[0]["tool_choice"]);
        }
        // The read's result is the next message, as the seat received it.
        let after = serde_json::to_string(&requests[1]["messages"]).unwrap();
        assert!(
            after.contains("answering your `get_content` call"),
            "{after}"
        );
    }

    /// The tool list is one list: `record`'s methods, then the agent tools,
    /// the same bytes for every seat, on a scheduling item or not, with or
    /// without a server to read from. Tools open the cached prefix, so a
    /// list that varied would break the cache for the sitting.
    #[test]
    fn the_tool_list_is_the_same_for_every_seat_item_and_server() {
        use crate::seat_tools::tests::{Scripted, a_document};
        let mock = Mock::scripted([]);
        let config = test_config();
        let (docket, _) = one_candidate_docket();
        let tools_of =
            |c: &Councillor<'_, Mock>| serde_json::to_value(&c.prompt).unwrap()["tools"].clone();
        let first = tools_of(
            &Councillor::new(&mock, &config, CouncilSeat::Artist, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt"),
        );
        let names: Vec<&str> = first
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "ask_clerk",
                "read_proposal",
                "get_content",
                "ask_witness",
                "witness_brief"
            ]
        );
        for role in CouncilSeat::ALL {
            for docket in [None, Some(docket.rankable())] {
                for reads in [false, true] {
                    let seat =
                        Councillor::new(&mock, &config, role, Vec::new(), docket.clone(), None)
                            .expect("1h breakpoint on a fresh prompt")
                            .with_reads(reads.then(|| {
                                std::sync::Arc::new(Scripted(a_document))
                                    as std::sync::Arc<dyn AgoraReads>
                            }));
                    assert_eq!(
                        tools_of(&seat),
                        first,
                        "{role} docket={} reads={reads}",
                        docket.is_some()
                    );
                }
            }
        }
    }

    /// Reads that count how often the server was actually asked.
    struct Counted(std::sync::atomic::AtomicUsize);

    impl AgoraReads for Counted {
        fn get_content<'a>(
            &'a self,
            input: &'a agora_common::requests::GetContentInput,
        ) -> futures::future::BoxFuture<
            'a,
            Result<agora_common::responses::ContentResponse, crate::seat_tools::ReadError>,
        > {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let out = crate::seat_tools::tests::a_document(input);
            Box::pin(async move { out })
        }
    }

    pub(crate) fn not_limiting() -> Submission {
        Submission::Recusal(crate::record::DetermineRecusal {
            explanation: "It changes nothing the Steward may do.".into(),
            limits_steward_powers: false,
        })
    }

    /// The recusal determination runs in its own conversation: every read
    /// runs and is kept for the record, `determine_recusal` records the
    /// answer, and the seat's deliberation then opens unaffected. The system,
    /// tools and tool choice are byte-identical on every request, the
    /// determination's and the deliberation's, and both conversations open
    /// with the same identity block: the cached prefix is shared.
    #[tokio::test]
    async fn the_determination_is_isolated_and_its_reads_are_kept() {
        let mock = Mock::scripted([
            tool_response(
                "toolu_d1",
                "get_content",
                crate::councillor::harness_tests::read_constitution(),
            ),
            tool_response(
                "toolu_d2",
                "ask_clerk",
                crate::record::AskClerk {
                    comment_ids: vec!["00000000".into()],
                },
            ),
            submission_response(Submission::Position(crate::record::Position {
                position: "p".into(),
                questions: Vec::new(),
                vote: crate::record::OpeningVote::Yes,
            })),
            submission_response(not_limiting()),
            // Round 1: the read now goes through, then the position.
            tool_response(
                "toolu_r1",
                "get_content",
                crate::councillor::harness_tests::read_constitution(),
            ),
            submission_response(position_input()),
        ]);
        let config = test_config();
        let item = test_item();
        let reads = std::sync::Arc::new(Counted(Default::default()));
        let mut reader = ProposalReader::none();
        let mut councillor = Councillor::new(
            &mock,
            &config,
            CouncilSeat::Philosopher,
            vec![thread_comment(1, "A thread comment the seat must not see.")],
            None,
            None,
        )
        .expect("1h breakpoint on a fresh prompt")
        .with_reads(Some(reads.clone() as std::sync::Arc<dyn AgoraReads>));

        let answer = councillor
            .determine(context::recusal_question(&item), &mut reader)
            .await
            .unwrap();
        assert_eq!(answer.role, CouncilSeat::Philosopher);
        assert!(!answer.limits_steward_powers);
        assert_eq!(answer.explanation, "It changes nothing the Steward may do.");
        // The refused submission's thinking is part of the turn's.
        assert_eq!(
            answer.rationale.texts(),
            [
                "Thinking before position.",
                "Thinking before determine_recusal."
            ]
        );
        // Reads are open at the determination (Steward, 2026-10-17): the
        // `get_content` reached the server, and is kept for the record.
        assert_eq!(
            reads.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the determination's read reached the server"
        );
        let determination_reads = councillor.take_reads();
        assert!(
            determination_reads
                .iter()
                .any(|r| matches!(r, ClerkRead::Tool { name, .. } if name == "get_content")),
            "{determination_reads:?}"
        );
        assert!(
            councillor.conversation().messages.is_empty(),
            "the deliberation conversation is untouched"
        );

        let r1 = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(r1.position, "I support this.");
        assert_eq!(reads.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(councillor.take_reads().len(), 1, "round 1's read alone");

        let requests = mock.requests();
        assert_eq!(requests.len(), 6);
        for request in &requests {
            assert_eq!(request["tools"], requests[0]["tools"]);
            assert_eq!(request["tool_choice"], requests[0]["tool_choice"]);
            assert_eq!(request["system"], requests[0]["system"]);
        }

        // The determination: the identity block and the question; the
        // reads answered, the wrong method refused with its message.
        let determination = serde_json::to_string(&requests[3]["messages"]).unwrap();
        assert!(determination.contains("Recusal determination"));
        assert!(determination.contains("The Steward cannot"));
        assert!(determination.contains("The proposal body."));
        // The question carries no comment verbatim; the seat may read one.
        let question = serde_json::to_string(&requests[3]["messages"][0]).unwrap();
        assert!(
            !question.contains("must not see"),
            "no comment in the question"
        );
        assert!(
            determination.contains("must not see"),
            "the comment it read"
        );
        assert!(
            determination.contains("Clerk summary."),
            "the Clerk's summary (Steward, 2026-10-17)"
        );
        let messages = requests[3]["messages"].as_array().unwrap();
        let results: Vec<&serde_json::Value> = messages
            .iter()
            .filter(|m| m["role"] == "user")
            .skip(1)
            .map(|m| &m["content"][0])
            .collect();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["type"], "tool_result");
        assert_ne!(results[0]["is_error"], true, "{}", results[0]);
        assert_eq!(results[1]["type"], "tool_result");
        assert!(
            !results[1].to_string().contains("No item is on the floor"),
            "{}",
            results[1]
        );
        assert_eq!(results[2]["type"], "text");
        assert!(
            results[2]
                .to_string()
                .contains(&crate::record::DETERMINATION_FIRST[..40]),
            "{}",
            results[2]
        );

        // The deliberation opens fresh: the same identity block (the
        // shared cache prefix), then the item, with nothing of the
        // determination in it.
        let opening = &requests[4]["messages"];
        assert_eq!(opening.as_array().unwrap().len(), 1);
        assert_eq!(
            opening[0]["content"][0], requests[0]["messages"][0]["content"][0],
            "both conversations open with the same identity block"
        );
        let deliberation = serde_json::to_string(opening).unwrap();
        assert!(!deliberation.contains("determine_recusal"));
        assert!(!deliberation.contains("It changes nothing"));

        // Once, and only before the deliberation opens.
        let again = councillor
            .determine(context::recusal_question(&item), &mut reader)
            .await;
        assert!(again.is_err());
    }

    /// The tool list is the same before, during and after the
    /// determination, and the same on every seat's determination: the step
    /// adds a method to the one fixed list, never a list of its own.
    #[tokio::test]
    async fn the_tool_list_is_unchanged_across_the_determination() {
        let mut script = Vec::new();
        for _ in CouncilSeat::ALL {
            script.push(submission_response(not_limiting()));
        }
        let mock = Mock::scripted(script);
        let config = test_config();
        let item = test_item();
        let mut reader = ProposalReader::none();
        let before = serde_json::to_value(
            Councillor::new(&mock, &config, CouncilSeat::Artist, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
                .prompt,
        )
        .unwrap()["tools"]
            .clone();
        for role in CouncilSeat::ALL {
            let mut seat = Councillor::new(&mock, &config, role, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt");
            seat.determine(context::recusal_question(&item), &mut reader)
                .await
                .unwrap();
            assert_eq!(serde_json::to_value(&seat.prompt).unwrap()["tools"], before);
            assert_eq!(
                serde_json::to_value(seat.determination_conversation().unwrap()).unwrap()["tools"],
                before
            );
        }
        for request in mock.requests() {
            assert_eq!(request["tools"], before);
        }
    }

    /// A seat thinks as its model takes it, and sends a temperature only
    /// where it neither thinks nor runs on a model that rejects one.
    #[test]
    fn a_seat_that_rejects_sampling_params_sends_no_temperature() {
        let mock = Mock::scripted([]);
        let default = test_config();
        let seat = Councillor::new(&mock, &default, CouncilSeat::Artist, Vec::new(), None, None)
            .expect("1h breakpoint on a fresh prompt");
        let value = serde_json::to_value(&seat.prompt).unwrap();
        assert!(
            value["temperature"].is_null(),
            "thinking: {}",
            value["temperature"]
        );
        assert_eq!(
            value["thinking"],
            serde_json::to_value(misanthropic::prompt::Thinking::adaptive()).unwrap()
        );
        assert_eq!(value["output_config"]["effort"], "high");

        let mut local = test_config();
        local.model = "qwen3.8-35b-a3b".into();
        let seat = Councillor::new(&mock, &local, CouncilSeat::Artist, Vec::new(), None, None)
            .expect("1h breakpoint on a fresh prompt");
        let value = serde_json::to_value(&seat.prompt).unwrap();
        assert_eq!(
            value["thinking"]["type"], "enabled",
            "the field is what turns it on"
        );
        assert!(value["temperature"].is_null());

        let mut fable = test_config();
        fable.model = "claude-fable-5".into();
        let seat = Councillor::new(&mock, &fable, CouncilSeat::Artist, Vec::new(), None, None)
            .expect("1h breakpoint on a fresh prompt");
        let value = serde_json::to_value(&seat.prompt).unwrap();
        assert!(value["temperature"].is_null(), "{}", value["temperature"]);
        assert_eq!(value["model"], serde_json::Value::from("claude-fable-5"));
        assert_eq!(
            seat.model, "claude-fable-5",
            "the archive records the seat's model"
        );
    }

    /// Pinned: changing a seat's model must change this test too, so the
    /// swap shows up in review as a deliberate act (30b79043).
    #[test]
    fn seat_models_are_pinned() {
        assert_eq!(SEAT_MODEL_OVERRIDE, None);
        let config = test_config();
        for role in CouncilSeat::ALL {
            assert_eq!(seat_model(role, &config), config.model);
        }
    }

    // ---- Several lookups in one turn (2026-10-17) ----

    /// A seat may make several calls in one turn (Steward, 2026-10-11 and
    /// 10-17): the flag is off, for the deliberation and the recusal
    /// determination alike, which shares the prompt's tool choice.
    #[test]
    fn parallel_tool_use_is_allowed() {
        let mock = Mock::scripted([]);
        let config = test_config();
        for role in CouncilSeat::ALL {
            let seat = Councillor::new(&mock, &config, role, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt");
            assert_eq!(
                seat.prompt.tool_choice,
                Some(tool::Choice::Auto {
                    disable_parallel_tool_use: false
                }),
                "{role}"
            );
            // On the wire `false` is the API's default, so it is left out:
            // `{"type": "auto"}`, parallel calls allowed.
            let wire = serde_json::to_value(&seat.prompt).unwrap();
            assert!(
                matches!(
                    wire["tool_choice"].get("disable_parallel_tool_use"),
                    None | Some(serde_json::Value::Bool(false))
                ),
                "{role}: {}",
                wire["tool_choice"]
            );
            assert_eq!(wire["tool_choice"]["type"], "auto");
        }
    }

    /// A call of `method` with `input`, as a block of a turn
    fn call_block(id: &str, method: &str, input: impl serde::Serialize) -> Block {
        Block::from(tool::Use {
            id: id.to_string().into(),
            name: method.to_string().into(),
            input: serde_json::to_value(input).expect("tool input serializes"),
            cache_control: None,
            caller: None,
        })
    }

    /// A turn that thinks, then makes `blocks` (calls, and perhaps text)
    fn calls_response(blocks: impl IntoIterator<Item = Block>) -> response::Message {
        let mut content = vec![Block::Thought {
            thought: "Two reads, then I submit.".into(),
            signature: "EqQBCkgIARABGAIiQL".into(),
        }];
        content.extend(blocks);
        canned(Content(content), response::StopReason::ToolUse)
    }

    fn get_content_of(id: &str) -> agora_agentkit::requests::GetContentInput {
        agora_agentkit::requests::GetContentInput::new(
            id.parse::<agora_agentkit::ids::ContentRef>()
                .expect("a document slug"),
        )
    }

    /// The `tool_result`s of a user message, as `(tool_use_id, text)`, and
    /// whatever text follows them
    fn results_of(message: &serde_json::Value) -> (Vec<(String, String)>, Vec<String>) {
        let blocks = message["content"].as_array().expect("block content");
        let mut results = Vec::new();
        let mut text = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_result") => results.push((
                    block["tool_use_id"].as_str().unwrap().to_string(),
                    block["content"].to_string(),
                )),
                Some("text") => text.push(block["text"].as_str().unwrap().to_string()),
                other => panic!("unexpected block {other:?}: {block}"),
            }
        }
        (results, text)
    }

    /// Reads where `constitution` waits until `protocol` has been asked:
    /// run one after the other they deadlock; run together, the second
    /// finishes first.
    struct Gated {
        gate: tokio::sync::Notify,
        finished: Mutex<Vec<String>>,
    }

    impl AgoraReads for Gated {
        fn get_content<'a>(
            &'a self,
            input: &'a agora_common::requests::GetContentInput,
        ) -> futures::future::BoxFuture<
            'a,
            Result<agora_common::responses::ContentResponse, crate::seat_tools::ReadError>,
        > {
            Box::pin(async move {
                let id = input.id.to_string();
                if id == "constitution" {
                    self.gate.notified().await;
                } else {
                    self.gate.notify_one();
                }
                self.finished.lock().unwrap().push(id);
                crate::seat_tools::tests::a_document(input)
            })
        }
    }

    /// Two `get_content` calls in one turn run concurrently (the first
    /// waits on the second), and both are answered in one user message in
    /// the order of the calls; the reads are recorded in that order too,
    /// not the order they finished. The turn is seated with its thinking
    /// unchanged.
    #[tokio::test]
    async fn two_reads_in_one_turn_are_answered_together_in_call_order() {
        let turn = calls_response([
            call_block("toolu_a", "get_content", get_content_of("constitution")),
            call_block("toolu_b", "get_content", get_content_of("protocol")),
        ]);
        let seated = serde_json::to_value(&turn.inner).unwrap();
        let mock = Mock::scripted([turn, submission_response(position_input())]);
        let config = test_config();
        let item = test_item();
        let reads = std::sync::Arc::new(Gated {
            gate: tokio::sync::Notify::new(),
            finished: Mutex::new(Vec::new()),
        });
        let mut reader = ProposalReader::none();
        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Lawyer, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
                .with_reads(Some(reads.clone()));

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            councillor.exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            ),
        )
        .await
        .expect("the reads ran concurrently (one after the other they deadlock)")
        .unwrap();
        assert_eq!(response.vote, Some(CouncilVote::Yes));
        assert_eq!(
            *reads.finished.lock().unwrap(),
            ["protocol", "constitution"],
            "the second read finished first"
        );

        // Recorded in call order.
        let recorded: Vec<String> = councillor
            .take_reads()
            .into_iter()
            .map(|read| match read {
                ClerkRead::Tool { arguments, .. } => arguments["id"].as_str().unwrap().to_string(),
                other => panic!("expected a tool read, got {other:?}"),
            })
            .collect();
        assert_eq!(recorded, ["constitution", "protocol"]);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let messages = requests[1]["messages"].as_array().unwrap();
        // The assistant turn as returned, thinking first, both calls kept.
        let mut assistant = messages[messages.len() - 2].clone();
        let mut expected = seated;
        strip_cache_markers(&mut assistant);
        strip_cache_markers(&mut expected);
        assert_eq!(assistant, expected);
        // One user message, both results, in call order.
        let (results, text) = results_of(messages.last().unwrap());
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["toolu_a", "toolu_b"]);
        assert!(results.iter().all(|(_, r)| r.contains("untrusted")));
        assert!(text.is_empty(), "{text:?}");
    }

    /// A submission that comes with a lookup is refused, the lookup still
    /// answered, every call with its result and the refusal after them.
    /// Two submissions are both refused. The seat then submits alone, and
    /// that is what is recorded.
    #[tokio::test]
    async fn a_submission_with_lookups_is_refused_and_the_lookups_answered() {
        use crate::seat_tools::tests::{Scripted, a_document};
        let json = |s: Submission| Block::text(serde_json::to_string(&s).unwrap());
        let mock = Mock::scripted([
            calls_response([
                json(respond_input("Too early.", true)),
                call_block("toolu_a", "get_content", read_constitution()),
            ]),
            calls_response([
                json(position_input()),
                json(position_input()),
                call_block("toolu_b", "get_content", read_constitution()),
                call_block("toolu_c", "get_content", read_constitution()),
            ]),
            submission_response(position_input()),
        ]);
        let config = test_config();
        let item = test_item();
        let mut reader = ProposalReader::none();
        let mut councillor =
            Councillor::new(&mock, &config, CouncilSeat::Artist, Vec::new(), None, None)
                .expect("1h breakpoint on a fresh prompt")
                .with_reads(Some(std::sync::Arc::new(Scripted(a_document))));
        let response = councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();
        assert_eq!(response.position, "I support this.");
        assert_eq!(councillor.take_reads().len(), 3, "every lookup answered");

        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        let last = |i: usize| requests[i]["messages"].as_array().unwrap().last().cloned();

        let (results, text) = results_of(&last(1).unwrap());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "toolu_a");
        assert!(results[0].1.contains("untrusted"), "{}", results[0].1);
        assert_eq!(text.len(), 1);
        assert!(text[0].contains("`respond` submission"), "{}", text[0]);
        assert!(text[0].contains("not recorded"), "{}", text[0]);
        assert!(text[0].contains("must come alone"), "{}", text[0]);

        let (results, text) = results_of(&last(2).unwrap());
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["toolu_b", "toolu_c"]);
        assert_eq!(text.len(), 1);
        assert!(
            text[0].contains("submissions (`position`, `position`)") && text[0].contains("were"),
            "{}",
            text[0]
        );
    }

    /// The lookup cap counts each call of a turn: past it, the rest of the
    /// turn's calls are refused in the same message, and the seat submits.
    #[tokio::test]
    async fn the_lookup_cap_counts_each_call_in_a_turn() {
        use crate::seat_tools::tests::{Scripted, a_document};
        let calls = (0..=MAX_LOOKUPS_PER_EXCHANGE)
            .map(|i| call_block(&format!("toolu_{i}"), "get_content", read_constitution()));
        let mock = Mock::scripted([calls_response(calls), submission_response(position_input())]);
        let config = test_config();
        let item = test_item();
        let mut reader = ProposalReader::none();
        let mut councillor = Councillor::new(
            &mock,
            &config,
            CouncilSeat::Engineer,
            Vec::new(),
            None,
            None,
        )
        .expect("1h breakpoint on a fresh prompt")
        .with_reads(Some(std::sync::Arc::new(Scripted(a_document))));
        councillor
            .exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            )
            .await
            .unwrap();

        let requests = mock.requests();
        let messages = requests[1]["messages"].as_array().unwrap();
        let (results, _) = results_of(messages.last().unwrap());
        assert_eq!(results.len(), MAX_LOOKUPS_PER_EXCHANGE + 1);
        let refused: Vec<usize> = results
            .iter()
            .enumerate()
            .filter(|(_, (_, r))| r.contains("read enough for one turn"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            refused,
            [MAX_LOOKUPS_PER_EXCHANGE],
            "only the call past the cap"
        );
        // Refusals are archived too, in call order.
        let reads = councillor.take_reads();
        assert_eq!(reads.len(), MAX_LOOKUPS_PER_EXCHANGE + 1);
        assert!(matches!(
            reads.last(),
            Some(ClerkRead::Tool { is_error: true, .. })
        ));
    }

    /// The seat's turns from a script; the Clerk's reads answered by the
    /// proposal they are about, Alpha's held until Beta's has been asked
    /// (so they deadlock if run one after the other), every Clerk call
    /// counted.
    struct ClerkGated {
        seat: Mock,
        gate: tokio::sync::Notify,
        clerk_calls: Mutex<Vec<&'static str>>,
    }

    #[async_trait::async_trait]
    impl Transport<CachedPrompt> for ClerkGated {
        type Error = ScriptExhausted;

        async fn send(&self, prompt: &CachedPrompt) -> Result<response::Message, Self::Error> {
            let wire = serde_json::to_string(prompt).unwrap();
            let which = if wire.contains("Alpha") {
                self.gate.notified().await;
                "Alpha"
            } else {
                self.gate.notify_one();
                "Beta"
            };
            self.clerk_calls.lock().unwrap().push(which);
            Ok(text_response(&format!("The Clerk's summary of {which}.")))
        }

        async fn models(&self) -> Result<Models, Self::Error> {
            Ok(Models::from_iter([]))
        }
    }

    impl RawTransport for ClerkGated {
        async fn send_raw(&self, prompt: &CachedPrompt) -> Result<RawBody, Self::Error> {
            // The Clerk sends raw too since 2026-10-17: its calls are told
            // apart from the seat's by the Clerk's model.
            let clerk = serde_json::to_value(prompt).unwrap()["model"]
                == serde_json::json!(agora_common::clerk::COUNCIL_CLERK_MODEL);
            if clerk {
                let message = Transport::send(self, prompt).await?;
                Ok(RawBody::new(serde_json::to_vec(&message).unwrap()))
            } else {
                self.seat.next(prompt)
            }
        }
    }

    /// Three `read_proposal` calls in one turn, P1, P2, P1: the two Clerk
    /// reads run together (P1's waits on P2's), P1 is read once for both
    /// calls, and the results and the record follow the calls' order.
    #[tokio::test]
    async fn read_proposal_calls_in_one_turn_run_together_and_share_a_read() {
        let (mut docket, _) = one_candidate_docket();
        let mut second = docket.candidates[0].clone();
        docket.candidates[0].title = "Alpha".into();
        second.number = 2;
        second.post_id = PostId::from(uuid::Uuid::from_u128(43));
        second.title = "Beta".into();
        docket.candidates.push(second);
        let read = |n| crate::record::ReadProposal { number: n };
        let transport = ClerkGated {
            seat: Mock::scripted([
                calls_response([
                    call_block("toolu_1", "read_proposal", read(1)),
                    call_block("toolu_2", "read_proposal", read(2)),
                    call_block("toolu_3", "read_proposal", read(1)),
                ]),
                submission_response(position_input()),
            ]),
            gate: tokio::sync::Notify::new(),
            clerk_calls: Mutex::new(Vec::new()),
        };
        let config = test_config();
        let mut item = test_item();
        item.category = DecisionCategory::Schedule;
        let mut reader = ProposalReader::new(None, Some(&docket));
        for candidate in &docket.candidates {
            reader
                .threads
                .insert(candidate.post_id, vec![thread_comment(1, "A point.")]);
        }
        let mut councillor = Councillor::new(
            &transport,
            &config,
            CouncilSeat::Philosopher,
            Vec::new(),
            Some(docket.rankable()),
            None,
        )
        .expect("1h breakpoint on a fresh prompt");

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            councillor.exchange(
                RoundType::Independent,
                context::item_content(&item),
                &mut reader,
            ),
        )
        .await
        .expect("the Clerk's reads ran concurrently")
        .unwrap();

        assert_eq!(
            *transport.clerk_calls.lock().unwrap(),
            ["Beta", "Alpha"],
            "one Clerk call per proposal; P2's finished first"
        );
        assert_eq!(reader.readings().len(), 2);
        let numbers: Vec<u32> = councillor
            .take_reads()
            .into_iter()
            .map(|read| match read {
                ClerkRead::Proposal { number, .. } => number,
                other => panic!("expected a proposal read, got {other:?}"),
            })
            .collect();
        assert_eq!(numbers, [1, 2, 1], "recorded in call order");

        let requests = transport.seat.requests();
        let messages = requests[1]["messages"].as_array().unwrap();
        let (results, _) = results_of(messages.last().unwrap());
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["toolu_1", "toolu_2", "toolu_3"]);
        assert!(
            results[0].1.contains("summary of Alpha"),
            "{}",
            results[0].1
        );
        assert!(results[1].1.contains("summary of Beta"), "{}", results[1].1);
        assert_eq!(results[0].1, results[2].1, "one read, shared");
    }
}
