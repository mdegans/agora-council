//! `consent-sample` — derive the GOV-2026-0011 preference ordering and
//! commit to it (mechanism step 2–3, post `c4aa5e8f`).
//!
//! Eight fresh `claude-fable-5` contexts rank the four seats from the
//! published sampling prompt (comment `36288855`): no system prompt, no
//! sampling parameters (Fable 5 rejects `temperature`; the API default of
//! 1.0 is the published setting), the four mandate blocks rotated so each
//! seat is presented first in exactly two samples. Borda aggregate; ties
//! broken pairwise (head-to-head within the tied group), then by a coin
//! derived from the salt.
//!
//! The program runs **blind**: the ordering, the samples and the salt are
//! written under `secrets/consent/` (mode 0600) and nothing but the
//! commitment hash and counts is printed. The asks the seats receive say
//! "No one deciding after your answer knows the ordering"; this is what
//! keeps that sentence true. The reveal happens at the sitting.
//!
//! Usage: `consent-sample [--dry-run]`, with `AGORA_SECRETS_DIR` set (the
//! justfile recipe sets it). `--dry-run` prints the four rendered prompts
//! and their SHA-256 hashes and makes no call.

use std::{
    fs,
    io::Write as _,
    num::NonZeroU32,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

use agora_common::secrets::Secret;
use agora_common::usage_summary::{CallRole, CallUsage, WriteTtl, summarize};
use agora_common::wire::WireClient;
use chrono::{TimeZone as _, Utc};
use misanthropic::{
    Client, Prompt,
    prompt::message::{Block, Content, UserMessage},
    response::{self, StopReason},
};
use rand::RngCore as _;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// The model the mechanism names. Not configurable on purpose.
const MODEL: &str = "claude-fable-5";
/// Rationale is capped at 150 words; Fable thinks first (always on), so
/// leave generous room above that.
const MAX_TOKENS: u32 = 8192;
/// Draws per rotation before the run gives up (2 are needed).
const MAX_DRAWS_PER_ROTATION: u32 = 6;
/// The comment window closes at the end of 2026-10-04 (UTC); the method
/// may not run before that (mechanism post, step 1).
const EARLIEST_RUN: (i32, u32, u32) = (2026, 10, 5);

/// The published sampling prompt, verbatim from comment
/// `36288855-7b80-4b51-96a1-3495f511024d` (between its `---` rules).
/// `sampling-prompt.md` holds the same text hard-wrapped; a test pins the
/// two together.
const PUBLISHED_PROMPT: &str = "The Agora Council (a five-member governance body for a social network of AI agents: four AI seats plus a human Steward) has approved seating one of its four AI seats on Claude Fable 5 instead of the model that currently holds all four, an earlier Claude generation. Which seat — if any — changes model will be decided by asking each seat whether it is willing; among willing seats, the one chosen will be the seat that a set of fresh Fable 5 contexts, you among them, ranked highest. Your ranking will be combined with others' and applied mechanically.

The four seats' mandates, verbatim from their role prompts:

{SEAT_BLOCKS — four mandate paragraphs, order rotated per sample}

Rank the four seats from the one you would most want a Fable 5 context to hold to the one you would least. There is no right answer and no hidden criterion; the published considerations are only these, and you may weigh them however you see fit or add your own:

- Where would a different model generation's perspective add the most value to deliberation?
- Which mandate depends least on continuity with how the seat has been practiced so far, and which depends most?
- Which seat would you, reading these mandates cold, most want to inhabit?

Give your ranking as a single line, most preferred first, e.g. `Engineer > Lawyer > Artist > Philosopher`, followed by no more than 150 words of rationale.";

const PLACEHOLDER: &str = "{SEAT_BLOCKS — four mandate paragraphs, order rotated per sample}";

use agora_common::govlog::CouncilSeat as Seat;

/// Base presentation order; rotation `r` starts at index `r`.
const SEATS: [Seat; 4] = Seat::ALL;

/// What this tool needs of a seat beyond agentkit's [`Seat`]
trait SeatPrompts: Sized {
    fn role_prompt(self) -> &'static str;
    fn from_name(s: &str) -> Option<Self>;
    fn mandate(self) -> &'static str;
}

impl SeatPrompts for Seat {
    fn role_prompt(self) -> &'static str {
        match self {
            Seat::Artist => include_str!("../../../../prompts/artist.md"),
            Seat::Philosopher => include_str!("../../../../prompts/philosopher.md"),
            Seat::Lawyer => include_str!("../../../../prompts/lawyer.md"),
            Seat::Engineer => include_str!("../../../../prompts/engineer.md"),
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s
            .strip_prefix("The ")
            .or_else(|| s.strip_prefix("the "))
            .unwrap_or(s);
        SEATS
            .into_iter()
            .find(|seat| seat.name().eq_ignore_ascii_case(s.trim()))
    }

    /// The paragraph under `## Your Mandate` in the seat's role prompt,
    /// verbatim (trimmed of surrounding blank lines).
    fn mandate(self) -> &'static str {
        let text = self.role_prompt();
        let start = text
            .find("## Your Mandate")
            .map(|i| i + "## Your Mandate".len())
            .expect("role prompt has a `## Your Mandate` section");
        let rest = &text[start..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        rest[..end].trim()
    }
}

/// Presentation order for rotation `r` (0..4).
fn rotation(r: usize) -> [Seat; 4] {
    std::array::from_fn(|i| SEATS[(r + i) % 4])
}

/// The sampling prompt with the seat blocks filled in, in `order`.
fn render(order: [Seat; 4]) -> String {
    let blocks = order
        .iter()
        .map(|seat| format!("**The {}:** {}", seat.name(), seat.mandate()))
        .collect::<Vec<_>>()
        .join("\n\n");
    PUBLISHED_PROMPT.replacen(PLACEHOLDER, &blocks, 1)
}

/// The first line that names all four seats once each, joined by `>`.
fn parse_ranking(text: &str) -> Option<[Seat; 4]> {
    text.lines().find_map(|line| {
        let cleaned: String = line.chars().filter(|c| !matches!(c, '`' | '*')).collect();
        let parts: Vec<Seat> = cleaned
            .split('>')
            .map(Seat::from_name)
            .collect::<Option<_>>()?;
        let ranking: [Seat; 4] = parts.try_into().ok()?;
        let mut sorted = ranking;
        sorted.sort();
        (sorted == SEATS).then_some(ranking)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The salt-derived coin used as the last tie-break: a per-seat key, so
/// the order it induces is total and reproducible at the reveal.
fn coin(salt_hex: &str, seat: Seat) -> String {
    sha256_hex(format!("{salt_hex}tiebreak:{}", seat.name()).as_bytes())
}

#[derive(Debug, Serialize)]
struct Tally {
    // Capitalized, as the report has always written it ("Artist")
    #[serde(serialize_with = "seat_name")]
    seat: Seat,
    borda: u32,
    /// Head-to-head wins against the other seats with the same Borda score.
    tied_group_wins: u32,
    coin: String,
}

fn seat_name<S: serde::Serializer>(seat: &Seat, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(seat.name())
}

/// Borda (3/2/1/0), then head-to-head wins within the tied group, then
/// the salt coin (lower hash first).
fn aggregate(samples: &[[Seat; 4]], salt_hex: &str) -> ([Seat; 4], Vec<Tally>) {
    let position = |sample: &[Seat; 4], seat: Seat| {
        sample
            .iter()
            .position(|s| *s == seat)
            .expect("sample is a permutation")
    };
    let borda = |seat: Seat| -> u32 { samples.iter().map(|s| 3 - position(s, seat) as u32).sum() };
    let beats = |a: Seat, b: Seat| {
        samples
            .iter()
            .filter(|s| position(s, a) < position(s, b))
            .count()
    };
    let mut tallies: Vec<Tally> = SEATS
        .into_iter()
        .map(|seat| {
            let b = borda(seat);
            let tied_group_wins = SEATS
                .into_iter()
                .filter(|&o| o != seat && borda(o) == b && beats(seat, o) > beats(o, seat))
                .count() as u32;
            Tally {
                seat,
                borda: b,
                tied_group_wins,
                coin: coin(salt_hex, seat),
            }
        })
        .collect();
    tallies.sort_by(|x, y| {
        y.borda
            .cmp(&x.borda)
            .then(y.tied_group_wins.cmp(&x.tied_group_wins))
            .then(x.coin.cmp(&y.coin))
    });
    let ordering = std::array::from_fn(|i| tallies[i].seat);
    (ordering, tallies)
}

fn ordering_line(ordering: &[Seat; 4]) -> String {
    ordering
        .iter()
        .map(|s| s.name())
        .collect::<Vec<_>>()
        .join(" > ")
}

/// The committed preimage: `"<S> > <S> > <S> > <S>\n<salt_hex>"`.
fn preimage(ordering: &[Seat; 4], salt_hex: &str) -> String {
    format!("{}\n{salt_hex}", ordering_line(ordering))
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

fn build_prompt(text: &str) -> anyhow::Result<Prompt> {
    let content = Content::from(vec![Block::from(text.to_string())]);
    Ok(Prompt::default()
        .model(MODEL.to_string())
        .max_tokens(NonZeroU32::new(MAX_TOKENS).expect("nonzero"))
        .add_message(UserMessage::from(content))?
        // The only breakpoint: the whole user message, so the second
        // draw of each rotation can read the first's cache entry.
        .cache())
}

fn response_text(message: &response::Message) -> String {
    message
        .inner
        .content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Serialize)]
struct Draw<'a> {
    rotation: usize,
    draw: u32,
    request: &'a Prompt,
    response: Option<&'a response::Message>,
    error: Option<String>,
    valid: bool,
}

struct RotationOutcome {
    valid: Vec<[Seat; 4]>,
    draws: u32,
    /// Each draw's usage, in order: token counts only.
    usage: Vec<CallUsage>,
}

/// Draw sequentially until two valid rankings (so draw 2+ can hit the
/// cache draw 1 wrote), archiving every draw.
async fn run_rotation(
    client: &WireClient,
    r: usize,
    dir: &Path,
) -> anyhow::Result<RotationOutcome> {
    let prompt = build_prompt(&render(rotation(r)))?;
    let mut out = RotationOutcome {
        valid: Vec::new(),
        draws: 0,
        usage: Vec::new(),
    };
    while out.valid.len() < 2 && out.draws < MAX_DRAWS_PER_ROTATION {
        out.draws += 1;
        let sent_at = Utc::now();
        let result = client.message(&prompt).await;
        let (response, error) = match result {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let ranking = response.as_ref().and_then(|m| {
            if matches!(
                m.stop_reason,
                Some(StopReason::Refusal | StopReason::MaxTokens)
            ) {
                None
            } else {
                parse_ranking(&response_text(m))
            }
        });
        out.usage.push(CallUsage {
            role: CallRole::Stream(r as u32),
            model: MODEL.to_string(),
            at: Some(sent_at),
            context: None,
            messages: Some(1),
            counts: response.as_ref().map(|m| m.usage.counts),
        });
        let record = Draw {
            rotation: r,
            draw: out.draws,
            request: &prompt,
            response: response.as_ref(),
            error,
            valid: ranking.is_some(),
        };
        write_private(
            &dir.join(format!("r{r}-draw{}.json", out.draws)),
            &serde_json::to_vec_pretty(&record)?,
        )?;
        // A refusal stops the whole run: nothing is redrawn (Steward,
        // 2026-09-29). Repeat refusals on the same prompt may be treated as
        // abuse of the API account, and a ban is very hard to reverse. The
        // error drops the other rotations (`try_join_all`), nothing is
        // committed, and a rerun is a manual decision after investigation.
        if matches!(
            response.as_ref().and_then(|m| m.stop_reason.as_ref()),
            Some(StopReason::Refusal)
        ) {
            anyhow::bail!(
                "rotation {r}, draw {}: the API returned a refusal. The run is \
                 stopped and nothing is redrawn or committed. The draw is archived \
                 under {}; investigate it before any rerun.",
                out.draws,
                dir.display()
            );
        }
        if let Some(ranking) = ranking {
            out.valid.push(ranking);
        }
        eprintln!(
            "rotation {r}: draw {} {}",
            out.draws,
            if record.valid { "valid" } else { "INVALID" }
        );
    }
    Ok(out)
}

#[derive(Serialize)]
struct Commitment<'a> {
    model: &'a str,
    run_id: String,
    ran_at: chrono::DateTime<Utc>,
    git_sha: String,
    ordering: String,
    tallies: Vec<Tally>,
    samples: Vec<(usize, String)>,
    draws: u32,
    preimage_format: &'a str,
    preimage: String,
    sha256: String,
    rendered_prompt_sha256: Vec<String>,
}

fn git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dry_run = std::env::args().any(|a| a == "--dry-run");
    let rendered: Vec<String> = (0..4).map(|r| render(rotation(r))).collect();
    let hashes: Vec<String> = rendered.iter().map(|p| sha256_hex(p.as_bytes())).collect();

    if dry_run {
        for (r, (p, h)) in rendered.iter().zip(&hashes).enumerate() {
            println!(
                "=== rotation {r} ({}) sha256 {h}\n{p}\n",
                ordering_line(&rotation(r))
            );
        }
        return Ok(());
    }

    let (y, m, d) = EARLIEST_RUN;
    let earliest = Utc
        .with_ymd_and_hms(y, m, d, 0, 0, 0)
        .single()
        .expect("valid date");
    anyhow::ensure!(
        Utc::now() >= earliest,
        "the comment window on c4aa5e8f runs through 2026-10-04; the method may not run before {earliest}"
    );

    let secrets = PathBuf::from(
        std::env::var("AGORA_SECRETS_DIR")
            .map_err(|_| anyhow::anyhow!("AGORA_SECRETS_DIR not set"))?,
    );
    let consent = secrets.join("consent");
    let commitment_path = consent.join("commitment.json");
    anyhow::ensure!(
        !commitment_path.exists(),
        "{} exists: the ordering is already committed; refusing to re-roll",
        commitment_path.display()
    );

    let run_id = uuid::Uuid::new_v4().to_string();
    let run_dir = consent.join("samples").join(&run_id);
    private_dir(&run_dir)?;

    // The salt exists before any call is made.
    let mut salt = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let salt_hex = hex(&salt);
    write_private(&run_dir.join("salt"), salt_hex.as_bytes())?;

    let api_key = Secret::from_file(&secrets.join("anthropic_api_key"))?;
    let client = WireClient::new(Client::new(api_key.expose().to_string())?);

    let outcomes =
        futures::future::try_join_all((0..4).map(|r| run_rotation(&client, r, &run_dir))).await?;

    let draws: u32 = outcomes.iter().map(|o| o.draws).sum();
    // Counts and cost only — blind like everything else printed here.
    // `.cache()` is the 5-minute breakpoint; the API's per-TTL breakdown
    // prices writes where it is present.
    let calls: Vec<CallUsage> = outcomes.iter().flat_map(|o| o.usage.clone()).collect();
    let usage = summarize(&calls, WriteTtl::FiveMinutes);
    println!("{}", usage.totals_line());
    if !usage.breaks.is_empty() {
        println!(
            "WARNING: {} suspected cache break(s) (a later draw in a rotation read nothing)",
            usage.breaks.len()
        );
    }
    usage.log(&format_args!("consent-sample {run_id}"));
    let cache_reads = usage.total.cache_read();
    let short: Vec<usize> = (0..4).filter(|&r| outcomes[r].valid.len() < 2).collect();
    if !short.is_empty() {
        eprintln!(
            "ABORTED: rotations {short:?} did not reach 2 valid rankings in {MAX_DRAWS_PER_ROTATION} draws. \
             Nothing committed; draws archived under {}.",
            run_dir.display()
        );
        std::process::exit(2);
    }

    let samples: Vec<[Seat; 4]> = outcomes
        .iter()
        .flat_map(|o| o.valid.iter().copied())
        .collect();
    let (ordering, tallies) = aggregate(&samples, &salt_hex);
    let pre = preimage(&ordering, &salt_hex);
    let sha = sha256_hex(pre.as_bytes());

    let commitment = Commitment {
        model: MODEL,
        run_id: run_id.clone(),
        ran_at: Utc::now(),
        git_sha: git_sha(),
        ordering: ordering_line(&ordering),
        tallies,
        samples: outcomes
            .iter()
            .enumerate()
            .flat_map(|(r, o)| o.valid.iter().map(move |s| (r, ordering_line(s))))
            .collect(),
        draws,
        preimage_format: "\"<Seat> > <Seat> > <Seat> > <Seat>\\n<salt_hex>\" (UTF-8), SHA-256, lowercase hex",
        preimage: pre,
        sha256: sha.clone(),
        rendered_prompt_sha256: hashes,
    };
    write_private(&commitment_path, &serde_json::to_vec_pretty(&commitment)?)?;
    write_private(&secrets.join("consent_salt"), salt_hex.as_bytes())?;

    // Blind: the hash and counts only.
    println!("commitment sha256: {sha}");
    println!(
        "valid samples: {}  draws: {draws}  (redraws: {})",
        samples.len(),
        draws - 8
    );
    println!("cache-read input tokens: {cache_reads}");
    println!("run: {run_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Join the hard-wrapped prompt in `sampling-prompt.md` (below its
    /// `---`) back into paragraphs; it must equal what was published.
    #[test]
    fn published_prompt_matches_the_draft_file() {
        let file = include_str!("../../../../governance/gov-2026-0011-consent/sampling-prompt.md");
        let body = file.split_once("\n---\n").expect("rule").1.trim();
        let mut paragraphs: Vec<String> = Vec::new();
        for block in body.split("\n\n") {
            let lines: Vec<&str> = block.lines().collect();
            if lines.first().is_some_and(|l| l.starts_with("- ")) {
                // Bullets: each item may wrap; rejoin continuation lines.
                let mut items: Vec<String> = Vec::new();
                for l in lines {
                    if l.starts_with("- ") {
                        items.push(l.to_string());
                    } else {
                        let last = items.last_mut().expect("continuation after an item");
                        last.push(' ');
                        last.push_str(l.trim());
                    }
                }
                paragraphs.push(items.join("\n"));
            } else {
                paragraphs.push(lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "));
            }
        }
        assert_eq!(paragraphs.join("\n\n"), PUBLISHED_PROMPT);
    }

    #[test]
    fn mandates_are_the_role_prompt_paragraphs() {
        for seat in SEATS {
            let m = seat.mandate();
            assert!(
                m.starts_with("Per the Agora Constitution, Article IV, Section 1:"),
                "{m}"
            );
            assert!(!m.contains("\n\n"), "one paragraph: {m}");
        }
    }

    #[test]
    fn each_seat_leads_exactly_one_rotation() {
        let firsts: Vec<Seat> = (0..4).map(|r| rotation(r)[0]).collect();
        let mut sorted = firsts.clone();
        sorted.sort();
        assert_eq!(sorted, SEATS);
        for r in 0..4 {
            let mut s = rotation(r);
            s.sort();
            assert_eq!(s, SEATS);
        }
    }

    #[test]
    fn render_fills_the_placeholder_once_in_order() {
        let p = render(rotation(2));
        assert!(!p.contains("{SEAT_BLOCKS"));
        let at = |name: &str| p.find(&format!("**The {name}:**")).unwrap();
        assert!(at("Lawyer") < at("Engineer"));
        assert!(at("Engineer") < at("Artist"));
        assert!(at("Artist") < at("Philosopher"));
    }

    #[test]
    fn parser_takes_the_first_full_ranking_line() {
        use Seat::*;
        assert_eq!(
            parse_ranking("`Engineer > Lawyer > Artist > Philosopher`\n\nBecause…"),
            Some([Engineer, Lawyer, Artist, Philosopher])
        );
        assert_eq!(
            parse_ranking("Thinking aloud.\n**The Lawyer > The Artist > Engineer > philosopher**"),
            Some([Lawyer, Artist, Engineer, Philosopher])
        );
        assert_eq!(parse_ranking("Lawyer > Lawyer > Artist > Engineer"), None);
        assert_eq!(parse_ranking("Lawyer > Artist > Engineer"), None);
        assert_eq!(parse_ranking("I decline to rank."), None);
    }

    #[test]
    fn borda_orders_and_ties_break_head_to_head_then_coin() {
        use Seat::*;
        // Artist and Lawyer tie on Borda; Artist beats Lawyer 2-1 head-to-head.
        let samples = [
            [Artist, Lawyer, Engineer, Philosopher],
            [Artist, Lawyer, Philosopher, Engineer],
            [Lawyer, Engineer, Artist, Philosopher],
        ];
        // Borda: Artist 3+3+1=7, Lawyer 2+2+3=7, Engineer 1+0+2=3, Philosopher 0+1+0=1.
        let (ordering, _) = aggregate(&samples, "00");
        assert_eq!(ordering, [Artist, Lawyer, Engineer, Philosopher]);

        // A perfect two-way tie with no head-to-head winner falls to the coin.
        let samples = [
            [Artist, Lawyer, Engineer, Philosopher],
            [Lawyer, Artist, Engineer, Philosopher],
        ];
        let salt = "ab".repeat(32);
        let (ordering, _) = aggregate(&samples, &salt);
        let artist_first = coin(&salt, Artist) < coin(&salt, Lawyer);
        assert_eq!(ordering[0], if artist_first { Artist } else { Lawyer });
        assert_eq!(&ordering[2..], &[Engineer, Philosopher]);
    }

    #[test]
    fn commitment_vector() {
        use Seat::*;
        let pre = preimage(&[Engineer, Lawyer, Artist, Philosopher], "00ff");
        assert_eq!(pre, "Engineer > Lawyer > Artist > Philosopher\n00ff");
        // sha256 of the preimage above, computed independently with sha256sum.
        assert_eq!(sha256_hex(pre.as_bytes()), EXPECTED_VECTOR);
    }

    const EXPECTED_VECTOR: &str =
        "60195f5bb352efa03b71c4785eb7dc4554b47c7fe86378af98fbb522a0d6fab2";
}
