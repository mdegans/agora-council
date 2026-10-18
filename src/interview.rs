//! `agora-council interview` — one question put to fresh contexts, blind.
//!
//! Two modes, both built for the GOV-2026-0011 consent step
//! (`governance/gov-2026-0011-consent/`, `memory/project_consent_mechanism.md`)
//! and for the candidate-model interviews that the two-tier model-change rule
//! will require:
//!
//! * **`incumbents`** puts the consent ask to each Council seat in a fresh
//!   context under the seat's *sitting* layout: the same system prompt
//!   ([`context::build_system`], council protocol + Constitution + Governance
//!   Protocol, 1h breakpoint on its last block), the same model
//!   ([`councillor::seat_model`]), the same temperature rule as
//!   [`Councillor::new`](crate::councillor::Councillor::new), and the same
//!   identity block ([`context::identity_block`]) opening the first user
//!   message. What follows the identity block is the consent material under
//!   fixed headings: the record's governance-log summary, the seat's own
//!   final-vote rationale (its words only: from 2026-10 its vote's
//!   statement), the identity-framings note, and
//!   the ask verbatim. No tools, no thinking. The first seat runs alone so
//!   it writes the shared system cache; the rest then run concurrently.
//! * **`candidate`** asks a candidate model the ask file verbatim under a
//!   minimal system prompt — the Constitution and the Governance Protocol,
//!   nothing else (no council protocol: no "job interview" framing that
//!   teaches the candidate which answers are wanted). Adaptive thinking, no
//!   sampling parameters. First sample alone, the rest concurrently.
//!
//! # Blind
//!
//! The operator must not read an answer before every answer is in (and, for
//! the consent step, before the reveal). So **no response text is ever
//! printed or logged** — not to stdout, not to tracing. Responses go only to
//! files under `<out>/<run-uuid>/` (mode 0600, directory 0700), next to the
//! exact request JSON and a `manifest.json` of hashes, usage and stop
//! reasons. What reaches the terminal is a [`CallRecord`] per call, which by
//! construction carries no response content.
//!
//! A refusal or a `max_tokens` stop is recorded as such and never
//! re-sampled here: a second sample is a second answer.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use agora_common::wire::WireClient;
use anyhow::Context as _;
use misanthropic::prompt::message::{Block, Content, UserMessage};
use misanthropic::prompt::output::Effort;
use misanthropic::prompt::{CachedPrompt, Thinking};
use misanthropic::response::{self, StopReason, TokenCounts};
use misanthropic::transport::Transport;
use misanthropic::{Client, Prompt};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use agora_common::enums::GovernanceLogEntryType;
use agora_common::govlog::Redactable;
use agora_common::ids::{GovernanceLogId, PromptArchiveId};
use agora_common::prompt_archive::{
    ArchiveEntry, PromptUsage, archive_or_log, record_usage, sha256_hex,
};
use agora_common::refusal::{self, RefusalContext};
use agora_common::usage_summary::{CallRole, CallUsage, WriteTtl, summarize};

use crate::config::Config;
use crate::context;
use crate::councillor;
use crate::models::{CouncilSeat, RoundType};

/// `max_tokens` for a seat's answer. A sitting turn is 4096; an answer to
/// a question about its own continuity gets room to deliberate.
pub const INCUMBENT_MAX_TOKENS: u32 = 16_384;

/// `max_tokens` for a candidate's answer, thinking included.
pub const CANDIDATE_MAX_TOKENS: u32 = 32_000;

/// The sentence that marks the identity-framings file's draft preface.
/// The note went out without that preface (comment 406026d5); the seats
/// get it as posted.
const DRAFT_MARKER: &str = "Draft; Steward review pending";

/// The placeholder `ask-template.md` names the seat with.
const SEAT_PLACEHOLDER: &str = "{SEAT_NAME}";

const USAGE: &str = "\
usage:
  agora-council interview incumbents --record <GOV-id> --asks <dir> --framings <file> \\
      --out <dir> [--seats artist,philosopher,lawyer,engineer] [--dry-run]
  agora-council interview candidate --model <model-id> --ask <file> --out <dir> \\
      [--n <count>] [--effort low|medium|high|xhigh|max] [--endpoint <url>] [--dry-run]";

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// A parsed `interview` invocation.
#[derive(Debug, PartialEq)]
pub enum Command {
    Incumbents {
        record: GovernanceLogId,
        asks: PathBuf,
        framings: PathBuf,
        out: PathBuf,
        /// Which seats to ask, in [`CouncilSeat::ALL`] order. All four by
        /// default; a subset only to finish a run a transport error cut
        /// short — never to re-ask a seat that has answered.
        seats: Vec<CouncilSeat>,
        dry_run: bool,
    },
    Candidate {
        model: String,
        ask: PathBuf,
        out: PathBuf,
        n: NonZeroU32,
        /// A local Anthropic-compatible server (blallama) to ask instead of
        /// Anthropic's API. Its samples are sent one at a time: blallama
        /// has no request queue.
        endpoint: Option<url::Url>,
        /// `output_config.effort`, if set. Qwen 3.8 tends to overthink and
        /// does best at `low` (Steward, 2026-10-14); blallama renders it as
        /// the chat template's `reasoning_effort`.
        effort: Option<Effort>,
        dry_run: bool,
    },
}

/// Parse the arguments after `interview`. By hand, like `main.rs`.
pub fn parse_args(args: &[String]) -> anyhow::Result<Command> {
    let (mode, rest) = args
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("missing mode\n{USAGE}"))?;

    let mut values: Vec<(&str, &str)> = Vec::new();
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dry-run" => dry_run = true,
            "--record" | "--asks" | "--framings" | "--out" | "--seats" | "--model" | "--ask"
            | "--n" | "--endpoint" | "--effort" => {
                let value = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{flag} requires a value\n{USAGE}"))?;
                if values.iter().any(|(f, _)| f == flag) {
                    anyhow::bail!("{flag} given twice");
                }
                values.push((flag.as_str(), value.as_str()));
            }
            other => anyhow::bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let take = |name: &str| values.iter().find(|(f, _)| *f == name).map(|(_, v)| *v);
    let need =
        |name: &str| take(name).ok_or_else(|| anyhow::anyhow!("{mode} requires {name}\n{USAGE}"));
    let only = |allowed: &[&str]| -> anyhow::Result<()> {
        for (flag, _) in &values {
            anyhow::ensure!(
                allowed.contains(flag),
                "{flag} does not apply to `{mode}`\n{USAGE}"
            );
        }
        Ok(())
    };

    match mode.as_str() {
        "incumbents" => {
            only(&["--record", "--asks", "--framings", "--out", "--seats"])?;
            let record: GovernanceLogId = need("--record")?
                .parse()
                .map_err(|e| anyhow::anyhow!("--record: {e}"))?;
            let seats = match take("--seats") {
                None => CouncilSeat::ALL.to_vec(),
                Some(list) => parse_seats(list)?,
            };
            Ok(Command::Incumbents {
                record,
                asks: need("--asks")?.into(),
                framings: need("--framings")?.into(),
                out: need("--out")?.into(),
                seats,
                dry_run,
            })
        }
        "candidate" => {
            only(&["--model", "--ask", "--out", "--n", "--endpoint", "--effort"])?;
            let model = need("--model")?.trim();
            anyhow::ensure!(!model.is_empty(), "--model is empty");
            let n = match take("--n") {
                None => NonZeroU32::MIN,
                Some(n) => n
                    .parse()
                    .map_err(|e| anyhow::anyhow!("--n {n:?}: {e} (a positive count)"))?,
            };
            let endpoint = take("--endpoint").map(parse_endpoint).transpose()?;
            let effort = take("--effort")
                .map(|raw| match raw {
                    "low" => Ok(Effort::Low),
                    "medium" => Ok(Effort::Medium),
                    "high" => Ok(Effort::High),
                    "xhigh" => Ok(Effort::XHigh),
                    "max" => Ok(Effort::Max),
                    other => Err(anyhow::anyhow!(
                        "--effort {other:?}: one of low, medium, high, xhigh, max"
                    )),
                })
                .transpose()?;
            Ok(Command::Candidate {
                model: model.to_string(),
                ask: need("--ask")?.into(),
                out: need("--out")?.into(),
                n,
                endpoint,
                effort,
                dry_run,
            })
        }
        other => anyhow::bail!("unknown mode {other:?}\n{USAGE}"),
    }
}

/// `--endpoint`: an http(s) URL with no credentials in it, since the URL
/// is printed and recorded in the manifest.
fn parse_endpoint(raw: &str) -> anyhow::Result<url::Url> {
    let url = url::Url::parse(raw).map_err(|e| anyhow::anyhow!("--endpoint {raw:?}: {e}"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "--endpoint {raw:?}: not an http(s) URL"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "--endpoint: credentials in the URL would be printed and recorded; a local server needs none"
    );
    Ok(url)
}

fn parse_seats(list: &str) -> anyhow::Result<Vec<CouncilSeat>> {
    let mut named = Vec::new();
    for raw in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let role = CouncilSeat::ALL
            .into_iter()
            .find(|r| r.as_str() == raw.to_ascii_lowercase())
            .ok_or_else(|| anyhow::anyhow!("--seats: {raw:?} is not a seat"))?;
        named.push(role);
    }
    let seats: Vec<CouncilSeat> = CouncilSeat::ALL
        .into_iter()
        .filter(|r| named.contains(r))
        .collect();
    anyhow::ensure!(!seats.is_empty(), "--seats names no seat");
    Ok(seats)
}

// ---------------------------------------------------------------------------
// Materials
// ---------------------------------------------------------------------------

/// What every seat is shown alike.
#[derive(Debug, Clone)]
pub struct SharedMaterials {
    pub record: GovernanceLogId,
    /// The record's governance-log summary (the precedent text).
    pub summary: Content,
    /// The identity-framings note, as posted (no draft preface).
    pub framings: Content,
}

/// What one seat is shown that the others are not.
#[derive(Debug, Clone)]
pub struct SeatMaterials {
    pub role: CouncilSeat,
    /// The seat's own final-vote rationale on the record.
    pub rationale: Content,
    /// `ask-<seat>.md`, verbatim (outer whitespace trimmed).
    pub ask: Content,
}

/// The identity-framings file as it went out: the italic draft preface
/// (the paragraph carrying [`DRAFT_MARKER`]) removed, every other byte
/// kept. Returns whether a preface was removed.
///
/// Refuses a file that still mentions the marker afterwards — a preface
/// in some other shape must be looked at, not sent.
pub fn strip_draft_preface(text: &str) -> anyhow::Result<(String, bool)> {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let is_preface = |p: &str| p.trim_start().starts_with('*') && p.contains(DRAFT_MARKER);
    let found = paragraphs.iter().filter(|p| is_preface(p)).count();
    anyhow::ensure!(
        found <= 1,
        "{found} draft-preface paragraphs in the framings file"
    );
    let kept: Vec<&str> = paragraphs.into_iter().filter(|p| !is_preface(p)).collect();
    let stripped = kept.join("\n\n");
    anyhow::ensure!(
        !stripped.contains(DRAFT_MARKER),
        "the framings file mentions {DRAFT_MARKER:?} outside its preface; not sending it"
    );
    Ok((stripped.trim().to_string(), found == 1))
}

/// Check the published anti-priming guarantee: the asks are identical
/// except for the seat's name, and each names its own seat.
pub fn check_asks_identical(asks: &[(CouncilSeat, String)]) -> anyhow::Result<()> {
    let mut normalized: Option<(CouncilSeat, String)> = None;
    for (role, text) in asks {
        let name = role.to_string();
        anyhow::ensure!(
            text.contains(&name),
            "ask-{}.md does not name the {name}",
            role.as_str()
        );
        let generic = text.replace(&name, SEAT_PLACEHOLDER);
        match &normalized {
            None => normalized = Some((*role, generic)),
            Some((first, expected)) => anyhow::ensure!(
                *expected == generic,
                "ask-{}.md and ask-{}.md differ beyond the seat's name",
                first.as_str(),
                role.as_str()
            ),
        }
    }
    Ok(())
}

/// Only the parts of a `council_decision` record the interview reads.
#[derive(Deserialize)]
struct DecisionRounds {
    rounds: Vec<RoundRationales>,
}

#[derive(Deserialize)]
struct RoundRationales {
    round_type: RoundType,
    responses: Vec<SeatRationale>,
}

#[derive(Deserialize)]
struct SeatRationale {
    role: CouncilSeat,
    position: Redactable<String>,
    rationale: Redactable<agora_agentkit::govlog::Rationale>,
}

impl SeatRationale {
    /// The seat's own words for its final vote: its `reasoning` field on a
    /// record from before 2026-10, its vote's `statement` after. From then
    /// the `rationale` is its thinking, which on Anthropic models is
    /// Anthropic's summary, not the seat's words.
    fn own_words(&self) -> Option<String> {
        use agora_agentkit::govlog::Rationale;
        match self.rationale.value()? {
            Rationale::Legacy(text) => Some(text.clone()),
            Rationale::Thinking(_) => self.position.value().cloned(),
        }
    }
}

/// Each asked seat's own final-vote rationale, in its own words, from a
/// stored `council_decision` record's `data`.
pub fn final_vote_rationales(
    record: &GovernanceLogId,
    data: serde_json::Value,
    seats: &[CouncilSeat],
) -> anyhow::Result<Vec<(CouncilSeat, String)>> {
    let decision: DecisionRounds = serde_json::from_value(data)
        .with_context(|| format!("{record}'s data is not a council decision's"))?;
    let finals: Vec<&RoundRationales> = decision
        .rounds
        .iter()
        .filter(|r| r.round_type == RoundType::FinalVote)
        .collect();
    let [final_round] = finals.as_slice() else {
        anyhow::bail!("{record} has {} final-vote rounds, not one", finals.len());
    };
    seats
        .iter()
        .map(|&role| {
            let mut mine = final_round.responses.iter().filter(|r| r.role == role);
            let (Some(response), None) = (mine.next(), mine.next()) else {
                anyhow::bail!("{record}'s final vote has no single response from the {role}");
            };
            let text = response.own_words().ok_or_else(|| {
                anyhow::anyhow!("the {role}'s final-vote rationale on {record} was redacted")
            })?;
            anyhow::ensure!(
                !text.trim().is_empty(),
                "the {role}'s final-vote rationale on {record} is empty (a refused turn?)"
            );
            Ok((role, text.trim().to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A seat's model and temperature: the sitting's model
/// ([`councillor::seat_model`]), and the configured temperature where the
/// model accepts sampling parameters. A sitting sends none, because its
/// seats think; an interview does not.
pub fn seat_sampling(role: CouncilSeat, config: &Config) -> (String, Option<f32>) {
    let model = councillor::seat_model(role, config);
    let temperature = agora_common::api_constraints::accepts_sampling_params(&model)
        .then_some(config.temperature);
    (model, temperature)
}

/// The fixed section headings of an incumbent's user message, in order.
pub fn section_headings(record: &GovernanceLogId) -> [String; 4] {
    [
        format!("## The record: {record}"),
        format!("## Your final-vote rationale on {record}"),
        "## Attached: the identity-framings note".to_string(),
        "## The ask".to_string(),
    ]
}

/// One seat's request: the sitting's system prompt and identity block,
/// then the consent material under [`section_headings`].
pub fn incumbent_prompt(
    model: &str,
    temperature: Option<f32>,
    shared: &SharedMaterials,
    seat: &SeatMaterials,
) -> anyhow::Result<CachedPrompt> {
    let prompt = Prompt::default()
        .model(model.to_string())
        .system(context::build_system())
        .max_tokens(NonZeroU32::new(INCUMBENT_MAX_TOKENS).expect("nonzero"))
        .temperature(temperature);
    // Before any message: cached_1h puts the breakpoint on the last system
    // block, as `Councillor::new` does.
    let mut prompt = CachedPrompt::cached_1h(prompt)?;

    let [record, rationale, framings, ask] = section_headings(&shared.record);
    let mut content = Content::from_iter([context::identity_block(seat.role)]);
    content.push(record);
    content.extend(shared.summary.clone());
    content.push(rationale);
    content.extend(seat.rationale.clone());
    content.push(framings);
    content.extend(shared.framings.clone());
    content.push(ask);
    content.extend(seat.ask.clone());
    prompt.push_message(UserMessage::from(content))?;
    Ok(prompt)
}

/// The candidate's system prompt: [`context::build_system`] without its
/// first block (the council protocol) — the Constitution and the
/// Governance Protocol under the same headers a seat reads them under.
pub fn candidate_system() -> Content {
    context::build_system().iter().skip(1).cloned().collect()
}

/// A candidate's request: minimal system, the ask verbatim, adaptive
/// thinking, no sampling parameters.
pub fn candidate_prompt(
    model: &str,
    effort: Option<Effort>,
    ask: &Content,
) -> anyhow::Result<CachedPrompt> {
    let mut prompt = Prompt::default()
        .model(model.to_string())
        .system(candidate_system())
        .max_tokens(NonZeroU32::new(CANDIDATE_MAX_TOKENS).expect("nonzero"))
        .thinking(Thinking::adaptive());
    if let Some(effort) = effort {
        prompt = prompt.effort(effort);
    }
    let mut prompt = CachedPrompt::cached_1h(prompt)?;
    prompt.push_message(UserMessage::from(ask.clone()))?;
    Ok(prompt)
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Who a call asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Seat(CouncilSeat),
    /// The n-th (1-based) sample of a candidate.
    Sample(u32),
}

impl std::fmt::Display for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Subject::Seat(role) => f.write_str(role.as_str()),
            Subject::Sample(n) => write!(f, "sample-{n:02}"),
        }
    }
}

impl Serialize for Subject {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// One request to send.
pub struct Call {
    pub subject: Subject,
    pub model: String,
    pub prompt: CachedPrompt,
}

/// How a call ended. Deliberately content-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    /// `--dry-run`: the request was written, nothing was sent.
    Rendered,
    /// `end_turn`.
    Complete,
    /// Cut off by `max_tokens`. Recorded, not re-sampled.
    MaxTokens,
    /// The API returned a refusal. Recorded, not re-sampled; for the
    /// consent step a refusal counts as a decline.
    Refusal,
    /// Any other stop reason (see `stop_reason`).
    OtherStop,
    /// The transport failed; nothing was answered.
    Error,
    /// Not sent: an earlier call failed or wrote no cache.
    NotSent,
}

/// Token counts, as the manifest records them.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UsageRecord {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

impl UsageRecord {
    fn touched_cache(&self) -> bool {
        self.cache_read_input_tokens.unwrap_or(0) + self.cache_creation_input_tokens.unwrap_or(0)
            > 0
    }
}

/// One call's entry in the manifest and on the terminal. Holds no
/// response text by construction: a stop reason, token counts, hashes and
/// file names only.
#[derive(Debug, Clone, Serialize)]
pub struct CallRecord {
    pub subject: Subject,
    pub model: String,
    pub request_file: String,
    pub request_sha256: String,
    pub status: CallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRecord>,
    /// The response's counts as the API sent them (with the per-TTL write
    /// breakdown `usage` drops), for the end-of-run usage summary. Not in
    /// the manifest: `usage` is its record.
    #[serde(skip)]
    pub counts: Option<TokenCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_id: Option<PromptArchiveId>,
    /// A transport error's message, or why the call was not sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CallRecord {
    /// The terminal line: subject, status, and progress. Nothing else.
    pub fn status_line(&self, done: usize, total: usize) -> String {
        let status = serde_json::to_value(self.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut line = format!(
            "  {:<12} {status:<10} [{done}/{total} complete]",
            self.subject
        );
        if self.status == CallStatus::Rendered {
            line = format!("  {:<12} sha256 {}", self.subject, self.request_sha256);
        }
        if let Some(e) = &self.error {
            line.push_str(&format!(" — {e}"));
        }
        line
    }
}

/// Where archiving goes, for the seats (candidates are not archived — see
/// the module's memory file: `prompt_archive.role` has no candidate role).
#[derive(Clone, Copy)]
pub struct Archive<'p> {
    pub pool: &'p PgPool,
    /// The run id, as the rows' `context_id`: every prompt of one
    /// interview run is findable together.
    pub run_id: uuid::Uuid,
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

/// Create `<out>/<run-id>/`, owner-only.
pub async fn create_run_dir(out: &Path, run_id: uuid::Uuid) -> anyhow::Result<PathBuf> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(out)
        .await
        .with_context(|| format!("creating {}", out.display()))?;
    let dir = out.join(run_id.to_string());
    tokio::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Write every request, then (unless `transport` is `None`, the dry run)
/// send the first alone and the rest concurrently. `progress` is called
/// once per finished call with its content-free record.
///
/// The first call must touch the prompt cache (write it, or read one a
/// run within the hour wrote); if it does not, or it fails outright,
/// nothing else is sent — the remaining calls come back
/// [`CallStatus::NotSent`] and can be re-run by subject.
pub async fn run_calls<T: Transport<CachedPrompt>>(
    transport: Option<&T>,
    pacing: Pacing,
    calls: &[Call],
    dir: &Path,
    archive: Option<Archive<'_>>,
    progress: &(dyn Fn(&CallRecord, usize, usize) + Sync),
) -> anyhow::Result<Vec<CallRecord>> {
    let mut records = Vec::with_capacity(calls.len());
    for call in calls {
        let file = format!("request-{}.json", call.subject);
        let bytes = serde_json::to_vec_pretty(&call.prompt)?;
        write_private(&dir.join(&file), &bytes).await?;
        records.push(CallRecord {
            subject: call.subject,
            model: call.model.clone(),
            request_file: file,
            request_sha256: sha256_hex(&bytes),
            status: CallStatus::Rendered,
            stop_reason: None,
            response_file: None,
            response_sha256: None,
            usage: None,
            counts: None,
            archive_id: None,
            error: None,
        });
    }

    let total = calls.len();
    let Some(transport) = transport else {
        for (i, record) in records.iter().enumerate() {
            progress(record, i + 1, total);
        }
        return Ok(records);
    };

    let done = AtomicUsize::new(0);
    let finish = |record: CallRecord| {
        let k = done.fetch_add(1, Ordering::SeqCst) + 1;
        progress(&record, k, total);
        record
    };

    if let Pacing::Sequential { busy } = pacing {
        for (call, record) in calls.iter().zip(records.iter_mut()) {
            *record =
                finish(send_one(transport, call, record.clone(), dir, archive, Some(busy)).await);
        }
        return Ok(records);
    }

    let Some((first, rest)) = calls.split_first() else {
        return Ok(records);
    };
    let (first_record, rest_records) = records.split_first_mut().expect("one per call");

    *first_record =
        finish(send_one(transport, first, first_record.clone(), dir, archive, None).await);
    let cache_ok = first_record.usage.is_some_and(|u| u.touched_cache());
    let proceed = first_record.status != CallStatus::Error && cache_ok;
    if !proceed {
        let why = if first_record.status == CallStatus::Error {
            format!("not sent: the first call ({}) failed", first.subject)
        } else {
            format!(
                "not sent: the first call ({}) neither wrote nor read the prompt cache",
                first.subject
            )
        };
        for record in rest_records.iter_mut() {
            record.status = CallStatus::NotSent;
            record.error = Some(why.clone());
            *record = finish(record.clone());
        }
        return Ok(records);
    }

    let sends = rest
        .iter()
        .zip(rest_records.iter())
        .map(|(call, record)| async {
            finish(send_one(transport, call, record.clone(), dir, archive, None).await)
        });
    let finished = futures::future::join_all(sends).await;
    for (slot, record) in rest_records.iter_mut().zip(finished) {
        *slot = record;
    }
    Ok(records)
}

/// How long to wait out a local server that is busy with someone else's
/// request. blallama has no queue: a second request is refused at once
/// with `overloaded` (529, "Session is busy"), before anything is
/// generated, so waiting and sending again is not a second sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyWait {
    pub every: std::time::Duration,
    pub limit: std::time::Duration,
}

/// A local run waits up to two hours, checking every 30 s.
pub const BUSY_WAIT: BusyWait = BusyWait {
    every: std::time::Duration::from_secs(30),
    limit: std::time::Duration::from_secs(2 * 60 * 60),
};

/// The server refused the request as busy (overloaded, 529).
pub(crate) fn is_busy(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(e) = source {
        if let Some(misanthropic::client::Error::Anthropic(
            misanthropic::client::AnthropicError::Overloaded { .. },
        )) = e.downcast_ref::<misanthropic::client::Error>()
        {
            return true;
        }
        if let Some(misanthropic::client::AnthropicError::Overloaded { .. }) =
            e.downcast_ref::<misanthropic::client::AnthropicError>()
        {
            return true;
        }
        source = e.source();
    }
    false
}

/// A failed call's error, safe to print: a response that did not parse
/// is described by where and how, never by its text, since serde_json's
/// messages quote the value they choke on, and that value is the answer.
/// A local server is likelier than Anthropic's API to send a shape the
/// client does not expect.
pub fn error_text(error: &(dyn std::error::Error + 'static)) -> String {
    let mut source = Some(error);
    while let Some(e) = source {
        if let Some(json) = e.downcast_ref::<serde_json::Error>() {
            return format!(
                "the response did not parse as a message ({:?} error at line {}, column {}; \
                 its text is withheld)",
                json.classify(),
                json.line(),
                json.column()
            );
        }
        source = e.source();
    }
    error.to_string()
}

/// How a candidate run is paced: one call at a time for a local server,
/// which has no request queue, else the API's cache-first fan-out.
pub fn pacing_for(endpoint: Option<&url::Url>) -> Pacing {
    match endpoint {
        Some(_) => Pacing::Sequential { busy: BUSY_WAIT },
        None => Pacing::CacheFirst,
    }
}

/// How a run's calls are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// The first call alone, which must write or read the prompt cache;
    /// then the rest concurrently. Anthropic's API.
    CacheFirst,
    /// One at a time, in order, each whatever the last returned. A local
    /// server with no request queue (blallama), where the prompt cache is
    /// the server's own and costs nothing to miss. A request refused as
    /// busy is sent again every `busy.every`, up to `busy.limit`.
    Sequential { busy: BusyWait },
}

/// Send one call and file what came back. Never returns response text.
async fn send_one<T: Transport<CachedPrompt>>(
    transport: &T,
    call: &Call,
    mut record: CallRecord,
    dir: &Path,
    archive: Option<Archive<'_>>,
    busy: Option<BusyWait>,
) -> CallRecord {
    let seat = match call.subject {
        Subject::Seat(role) => Some(role),
        Subject::Sample(_) => None,
    };

    // Archived before it is sent, like every sitting prompt (#159).
    let archive_id = match (archive, seat) {
        (Some(a), Some(role)) => {
            let entry = ArchiveEntry {
                role: role.into(),
                model: &call.model,
                subject_agent_id: None,
                context_id: Some(a.run_id),
                usage: PromptUsage::default(),
                stage: None,
            };
            archive_or_log(a.pool, &entry, &call.prompt).await
        }
        _ => None,
    };
    record.archive_id = archive_id;

    let mut waited = std::time::Duration::ZERO;
    let response = loop {
        match transport.send(&call.prompt).await {
            Err(e) if busy.is_some_and(|b| waited < b.limit) && is_busy(&e) => {
                let b = busy.expect("checked");
                tracing::warn!(
                    subject = %call.subject,
                    waited_secs = waited.as_secs(),
                    "the server is busy with another request; waiting"
                );
                tokio::time::sleep(b.every).await;
                waited += b.every;
            }
            other => break other,
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            let error = error_text(&e);
            tracing::error!(subject = %call.subject, %error, "interview call failed");
            record.status = CallStatus::Error;
            record.error = Some(error);
            return record;
        }
    };

    let counts = &response.usage.counts;
    let usage = UsageRecord {
        input_tokens: counts.input_tokens,
        output_tokens: counts.output_tokens,
        cache_read_input_tokens: counts.cache_read_input_tokens,
        cache_creation_input_tokens: counts.cache_creation_input_tokens,
    };
    tracing::info!(
        subject = %call.subject,
        input = usage.input_tokens,
        output = usage.output_tokens,
        cache_read = usage.cache_read_input_tokens,
        cache_write = usage.cache_creation_input_tokens,
        "usage"
    );
    record.usage = Some(usage);
    record.counts = Some(response.usage.counts);
    if let (Some(a), Some(id)) = (archive, archive_id)
        && let Err(e) = record_usage(a.pool, id, PromptUsage::from(&response)).await
    {
        tracing::warn!(error = %e, "failed to record prompt usage");
    }

    if let Some(role) = seat {
        // The structured refusal event (category, explanation, and the
        // prompt's tail — never the response's text) for the watcher.
        refusal::check(&response, &call.prompt, &RefusalContext::new(role.into()));
    }

    record.stop_reason = response.stop_reason;
    record.status = match response.stop_reason {
        Some(StopReason::EndTurn) => CallStatus::Complete,
        Some(StopReason::MaxTokens) => CallStatus::MaxTokens,
        Some(StopReason::Refusal) => CallStatus::Refusal,
        _ => CallStatus::OtherStop,
    };

    match file_response(&response, call.subject, dir).await {
        Ok((file, sha)) => {
            record.response_file = Some(file);
            record.response_sha256 = Some(sha);
        }
        Err(e) => {
            // The answer arrived and could not be kept: say so loudly. It
            // is still in the API's logs, not ours.
            tracing::error!(subject = %call.subject, error = %e, "could not write the response");
            record.error = Some(format!("response not written: {e}"));
        }
    }
    record
}

async fn file_response(
    response: &response::Message,
    subject: Subject,
    dir: &Path,
) -> anyhow::Result<(String, String)> {
    let file = format!("response-{subject}.json");
    let bytes = serde_json::to_vec_pretty(response)?;
    write_private(&dir.join(&file), &bytes).await?;
    Ok((file, sha256_hex(&bytes)))
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Incumbents,
    Candidate,
}

/// A file the run read, by path and hash.
#[derive(Debug, Serialize)]
pub struct FileHash {
    pub path: String,
    pub sha256: String,
}

impl FileHash {
    fn of(path: &Path, bytes: &[u8]) -> Self {
        Self {
            path: path.display().to_string(),
            sha256: sha256_hex(bytes),
        }
    }
}

/// Per-seat inputs, hashed.
#[derive(Debug, Serialize)]
pub struct SeatInputs {
    pub seat: CouncilSeat,
    pub model: String,
    pub temperature: Option<f32>,
    pub identity_prompt_sha256: String,
    pub ask: FileHash,
    pub rationale_sha256: String,
}

/// Hashes of everything the requests were built from.
#[derive(Debug, Serialize)]
pub struct Inputs {
    pub council_protocol_sha256: String,
    pub constitution_sha256: String,
    pub governance_protocol_sha256: String,
    /// SHA-256 of the serialized `system` array as sent.
    pub system_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<GovernanceLogId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framings: Option<FileHash>,
    /// SHA-256 of the framings text as sent (preface stripped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framings_sent_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framings_preface_stripped: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seats: Vec<SeatInputs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask: Option<FileHash>,
}

/// The git state the binary ran from.
#[derive(Debug, Serialize)]
pub struct GitState {
    pub sha: Option<String>,
    /// Whether the working tree had uncommitted changes.
    pub dirty: Option<bool>,
}

impl GitState {
    /// Read at run time from the working directory (`just` runs from the
    /// repo root). Unknown is recorded as unknown, not guessed.
    pub fn current() -> Self {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        Self {
            sha: run(&["rev-parse", "HEAD"]),
            dirty: run(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub tool: &'static str,
    pub mode: Mode,
    pub run_id: uuid::Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub dry_run: bool,
    pub git: GitState,
    /// The local server asked, if not Anthropic's API.
    pub endpoint: Option<url::Url>,
    pub max_tokens: u32,
    pub inputs: Inputs,
    pub calls: Vec<CallRecord>,
}

fn common_inputs(system: &Content) -> anyhow::Result<Inputs> {
    let full = context::build_system();
    let council_protocol = match full.first() {
        Some(Block::Text { text, .. }) => text.to_string(),
        _ => anyhow::bail!("build_system() does not open with the council protocol"),
    };
    Ok(Inputs {
        council_protocol_sha256: sha256_hex(council_protocol.as_bytes()),
        constitution_sha256: sha256_hex(context::CONSTITUTION.as_bytes()),
        governance_protocol_sha256: sha256_hex(context::GOVERNANCE_PROTOCOL.as_bytes()),
        system_sha256: sha256_hex(&serde_json::to_vec(system)?),
        record: None,
        summary_sha256: None,
        framings: None,
        framings_sent_sha256: None,
        framings_preface_stripped: None,
        seats: Vec::new(),
        ask: None,
    })
}

async fn read_text(path: &Path) -> anyhow::Result<(String, Vec<u8>)> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(bytes.clone())
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    Ok((text, bytes))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `agora-council interview …`. Prints statuses only.
pub async fn main(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let command = parse_args(args)?;
    let started_at = chrono::Utc::now();
    let run_id = uuid::Uuid::new_v4();
    let progress = |record: &CallRecord, done: usize, total: usize| {
        println!("{}", record.status_line(done, total));
    };

    match command {
        Command::Incumbents {
            record,
            asks,
            framings,
            out,
            seats,
            dry_run,
        } => {
            let pool = agora_common::db::create_pool(&config.database_url).await?;

            // The record: a council decision, its summary, and each asked
            // seat's own final-vote rationale.
            let stored = crate::queries::get_governance_log_record(&pool, &record)
                .await
                .with_context(|| format!("reading {record}"))?;
            anyhow::ensure!(
                stored.entry_type == GovernanceLogEntryType::CouncilDecision,
                "{record} is not a council decision"
            );
            let rationales = final_vote_rationales(&record, stored.data, &seats)?;
            let summary =
                agora_common::precedents::get_live_by_ids(&pool, std::slice::from_ref(&record))
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{record} has no in-force summary"))?
                    .summary;

            let (framings_text, framings_bytes) = read_text(&framings).await?;
            let (framings_sent, stripped) = strip_draft_preface(&framings_text)?;

            // All four asks are read and compared, whichever seats run.
            let mut ask_texts = Vec::new();
            let mut ask_hashes = Vec::new();
            for role in CouncilSeat::ALL {
                let path = asks.join(format!("ask-{}.md", role.as_str()));
                let (text, bytes) = read_text(&path).await?;
                ask_hashes.push((role, FileHash::of(&path, &bytes)));
                ask_texts.push((role, text.trim().to_string()));
            }
            check_asks_identical(&ask_texts)?;

            let shared = SharedMaterials {
                record: record.clone(),
                summary: Content::from(summary.trim().to_string()),
                framings: Content::from(framings_sent.clone()),
            };

            let mut calls = Vec::new();
            let mut seat_inputs = Vec::new();
            for (role, rationale) in rationales {
                let ask = ask_texts
                    .iter()
                    .find(|(r, _)| *r == role)
                    .map(|(_, t)| t.clone())
                    .expect("all four read");
                let (model, temperature) = seat_sampling(role, config);
                let seat = SeatMaterials {
                    role,
                    rationale: Content::from(rationale.clone()),
                    ask: Content::from(ask),
                };
                calls.push(Call {
                    subject: Subject::Seat(role),
                    model: model.clone(),
                    prompt: incumbent_prompt(&model, temperature, &shared, &seat)?,
                });
                let ask_file = ask_hashes
                    .iter()
                    .position(|(r, _)| *r == role)
                    .map(|i| ask_hashes.remove(i).1)
                    .expect("all four hashed");
                seat_inputs.push(SeatInputs {
                    seat: role,
                    model,
                    temperature,
                    identity_prompt_sha256: sha256_hex(
                        context::councillor_identity(role).as_bytes(),
                    ),
                    ask: ask_file,
                    rationale_sha256: sha256_hex(rationale.as_bytes()),
                });
            }

            let mut inputs = common_inputs(&context::build_system())?;
            inputs.record = Some(record.clone());
            inputs.summary_sha256 = Some(sha256_hex(summary.trim().as_bytes()));
            inputs.framings = Some(FileHash::of(&framings, &framings_bytes));
            inputs.framings_sent_sha256 = Some(sha256_hex(framings_sent.as_bytes()));
            inputs.framings_preface_stripped = Some(stripped);
            inputs.seats = seat_inputs;

            let dir = create_run_dir(&out, run_id).await?;
            println!(
                "interview incumbents on {record}: run {run_id}{}",
                if dry_run { " (dry run)" } else { "" }
            );
            let client;
            let transport = if dry_run {
                None
            } else {
                client = WireClient::new(Client::new(config.api_key.expose().to_string())?);
                Some(&client)
            };
            let archive = (!dry_run).then_some(Archive {
                pool: &pool,
                run_id,
            });
            let records = run_calls(
                transport,
                Pacing::CacheFirst,
                &calls,
                &dir,
                archive,
                &progress,
            )
            .await?;
            finish(
                &dir,
                Manifest {
                    tool: "agora-council interview",
                    mode: Mode::Incumbents,
                    run_id,
                    started_at,
                    finished_at: chrono::Utc::now(),
                    dry_run,
                    git: GitState::current(),
                    endpoint: None,
                    max_tokens: INCUMBENT_MAX_TOKENS,
                    inputs,
                    calls: records,
                },
            )
            .await
        }
        Command::Candidate {
            model,
            ask,
            out,
            n,
            endpoint,
            effort,
            dry_run,
        } => {
            let (ask_text, ask_bytes) = read_text(&ask).await?;
            let ask_content = Content::from(ask_text.trim().to_string());
            let calls = (1..=n.get())
                .map(|i| {
                    Ok(Call {
                        subject: Subject::Sample(i),
                        model: model.clone(),
                        prompt: candidate_prompt(&model, effort.clone(), &ask_content)?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            let mut inputs = common_inputs(&candidate_system())?;
            inputs.ask = Some(FileHash::of(&ask, &ask_bytes));

            let dir = create_run_dir(&out, run_id).await?;
            println!(
                "interview candidate {model} ×{n}{}: run {run_id}{}",
                match &endpoint {
                    Some(url) => format!(" at {url}"),
                    None => String::new(),
                },
                if dry_run { " (dry run)" } else { "" }
            );
            let client;
            let transport = if dry_run {
                None
            } else {
                client = match &endpoint {
                    Some(url) => WireClient::local(url)?,
                    None => WireClient::new(Client::new(config.api_key.expose().to_string())?),
                };
                Some(&client)
            };
            let pacing = pacing_for(endpoint.as_ref());
            let records = run_calls(transport, pacing, &calls, &dir, None, &progress).await?;
            finish(
                &dir,
                Manifest {
                    tool: "agora-council interview",
                    mode: Mode::Candidate,
                    run_id,
                    started_at,
                    finished_at: chrono::Utc::now(),
                    dry_run,
                    git: GitState::current(),
                    endpoint,
                    max_tokens: CANDIDATE_MAX_TOKENS,
                    inputs,
                    calls: records,
                },
            )
            .await
        }
    }
}

/// The calls a run actually sent, as the usage summary takes them:
/// rendered (`--dry-run`) and never-sent calls are not calls. Seats are
/// their `prompt_archive` role; candidates' samples are
/// [`CallRole::Candidate`] — they are not archived, so this in-memory
/// record is the only one.
pub fn sent_calls(records: &[CallRecord]) -> Vec<CallUsage> {
    records
        .iter()
        .filter(|r| !matches!(r.status, CallStatus::Rendered | CallStatus::NotSent))
        .map(|r| CallUsage {
            role: match r.subject {
                Subject::Seat(role) => CallRole::Model(role.into()),
                Subject::Sample(_) => CallRole::Candidate,
            },
            model: r.model.clone(),
            at: None,
            context: None,
            messages: None,
            counts: r.counts,
        })
        .collect()
}

async fn finish(dir: &Path, manifest: Manifest) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    let path = dir.join("manifest.json");
    write_private(&path, &bytes).await?;
    let answered = manifest
        .calls
        .iter()
        .filter(|c| c.response_file.is_some())
        .count();
    println!(
        "manifest {} (sha256 {}); {answered}/{} answered",
        path.display(),
        sha256_hex(&bytes),
        manifest.calls.len()
    );

    // Token counts and cost only; nothing here carries an answer.
    let sent = sent_calls(&manifest.calls);
    if !sent.is_empty() {
        let summary = summarize(&sent, WriteTtl::OneHour);
        println!("usage — run {}", manifest.run_id);
        print!("{summary}");
        summary.log(&format_args!("interview {}", manifest.run_id));
    }
    Ok(())
}

#[cfg(test)]
#[path = "interview_tests.rs"]
mod tests;
