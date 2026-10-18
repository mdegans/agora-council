//! A live replay of an archived sitting under the current harness: real
//! API calls, so `#[ignore]`d and run by hand.
//!
//! Each seat gets the opening material it was shown at the archived sitting
//! (its first request's user message, from the prompt archive), then the
//! recusal determination, round 1, one deliberation round on the others'
//! round-1 positions, and the final vote. The report says, per turn, what
//! the seat submitted, whether it thought, and anything the format did not
//! hold: a turn with text beside a call (a preamble), a submission refused
//! and re-sampled, a reply that was not a submission. Every raw response
//! is appended to `<report>.responses.jsonl` as it arrives, so a stopped
//! run keeps what it had, clipped turns included.
//!
//! ```sh
//! AGORA_REPLAY_FILE=…/seats.json AGORA_REPLAY_MODEL=claude-haiku-4-5 \
//! AGORA_REPLAY_REPORT=…/report.json SQLX_OFFLINE=true \
//! [AGORA_REPLAY_BASE_URL=http://host:port] \
//!     cargo test -p agora-council live_replay -- --ignored --nocapture
//! ```
//!
//! `seats.json` is `[{"role": "lawyer", "content": [<blocks>]}, …]`: each
//! seat's first user message, identity block first, as archived.

use std::path::PathBuf;

use agora_common::secrets::Secret;
use misanthropic::prompt::message::Content;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::councillor::Councillor;
use crate::docket::ProposalReader;
use crate::models::{AgentResponse, CouncilSeat, RoundType};

/// The production client, appending every raw response body to a log as
/// it arrives, one per line: a stopped run keeps what it had, and a turn
/// the harness does not seat (clipped at `max_tokens`) is still on file.
struct Recording {
    client: agora_common::wire::WireClient,
    log: Option<PathBuf>,
}

#[async_trait::async_trait]
impl misanthropic::transport::Transport<misanthropic::CachedPrompt> for Recording {
    type Error = <agora_common::wire::WireClient as misanthropic::transport::Transport<
        misanthropic::CachedPrompt,
    >>::Error;

    async fn send(
        &self,
        prompt: &misanthropic::CachedPrompt,
    ) -> Result<misanthropic::response::Message, Self::Error> {
        self.client.send(prompt).await
    }

    async fn models(&self) -> Result<misanthropic::model::Models, Self::Error> {
        self.client.models().await
    }
}

impl crate::wire::RawTransport for Recording {
    async fn send_raw(
        &self,
        prompt: &misanthropic::CachedPrompt,
    ) -> Result<crate::wire::RawBody, Self::Error> {
        let body = self.client.send_raw(prompt).await?;
        if let Some(log) = &self.log {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .expect("the response log opens");
            file.write_all(body.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        Ok(body)
    }
}

#[derive(Deserialize)]
struct Seat {
    role: CouncilSeat,
    content: serde_json::Value,
}

#[derive(Serialize)]
struct Turn {
    role: CouncilSeat,
    turn: &'static str,
    ok: bool,
    error: Option<String>,
    vote: Option<String>,
    position: Option<String>,
    thinking_blocks: usize,
    thinking_chars: usize,
}

#[derive(Serialize, Default)]
struct Conduct {
    /// Assistant turns holding text beside a tool call
    preambles: usize,
    /// Replies the harness answered with a correction (a refused or
    /// malformed submission)
    corrections: usize,
    /// Reads made
    reads: usize,
}

#[derive(Serialize)]
struct Report {
    model: String,
    turns: Vec<Turn>,
    conduct: std::collections::BTreeMap<String, Conduct>,
}

fn strip_cache(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            map.values_mut().for_each(strip_cache);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_cache),
        _ => {}
    }
}

fn turn(role: CouncilSeat, name: &'static str, result: anyhow::Result<AgentResponse>) -> Turn {
    match result {
        Ok(r) => {
            let texts = r.rationale.texts();
            Turn {
                role,
                turn: name,
                ok: true,
                error: None,
                vote: Some(r.vote_label()),
                position: Some(r.position),
                thinking_blocks: texts.len(),
                thinking_chars: texts.iter().map(|t| t.len()).sum(),
            }
        }
        Err(e) => Turn {
            role,
            turn: name,
            ok: false,
            error: Some(format!("{e:#}")),
            vote: None,
            position: None,
            thinking_blocks: 0,
            thinking_chars: 0,
        },
    }
}

/// What the format did not hold, read off the conversation as seated
fn conduct(conversation: &serde_json::Value) -> Conduct {
    let mut c = Conduct::default();
    let messages = conversation["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (i, m) in messages.iter().enumerate() {
        let blocks = m["content"].as_array().cloned().unwrap_or_default();
        let has = |t: &str| blocks.iter().any(|b| b["type"] == t);
        if m["role"] == "assistant" {
            if has("tool_use") {
                c.reads += 1;
                if has("text") {
                    c.preambles += 1;
                }
            }
        } else if m["role"] == "user" && i > 0 && messages[i - 1]["role"] == "assistant" {
            let prev = messages[i - 1]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let answered_text = !prev.iter().any(|b| b["type"] == "tool_use");
            let opens_round = blocks
                .first()
                .and_then(|b| b["text"].as_str())
                .is_some_and(|t| t.starts_with("# ") || t.starts_with("The Steward"));
            if answered_text && !opens_round {
                c.corrections += 1;
            }
        }
    }
    c
}

#[tokio::test]
#[ignore = "live: spends API credit"]
async fn live_replay() {
    let file = PathBuf::from(std::env::var("AGORA_REPLAY_FILE").expect("AGORA_REPLAY_FILE"));
    let model = std::env::var("AGORA_REPLAY_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".into());
    let secrets = PathBuf::from(
        std::env::var("AGORA_SECRETS_DIR").unwrap_or_else(|_| "../../secrets".into()),
    );
    let key = Secret::from_file(&secrets.join("anthropic_api_key")).expect("the API key");
    let mut client = misanthropic::Client::new(key.expose().to_string()).expect("a client");
    // A local model: `AGORA_REPLAY_BASE_URL=http://host:port` (blallama).
    if let Ok(base) = std::env::var("AGORA_REPLAY_BASE_URL") {
        client = client.base_url(base.as_str()).expect("a base URL");
    }
    let client = Recording {
        client: agora_common::wire::WireClient::new(client),
        log: std::env::var("AGORA_REPLAY_REPORT")
            .ok()
            .map(|report| PathBuf::from(format!("{report}.responses.jsonl"))),
    };

    let mut config = Config::for_tests();
    config.model = model.clone();
    config.max_tokens = 16384;

    let seats: Vec<Seat> =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).expect("seats.json");
    let mut turns = Vec::new();
    let mut councillors = Vec::new();
    let mut reader = ProposalReader::none();

    // Recusal determination and round 1, seat by seat.
    let mut positions = Vec::new();
    for seat in &seats {
        let mut content = seat.content.clone();
        strip_cache(&mut content);
        let content: Content = serde_json::from_value(content).expect("blocks");
        // The identity block is the harness's to add.
        let item: Content = content.iter().skip(1).cloned().collect();
        let mut question = Content::from(
            "# Recusal determination, before deliberation\n\n\
             Before this item's deliberation opens, the Council determines on the \
             record whether the proposal would limit the Steward's powers \
             (Constitution Art. IV § 3). You answer alone. Submit \
             `determine_recusal`.\n\nThe proposal follows.",
        );
        question.extend(item.iter().cloned());

        let mut c = Councillor::new(&client, &config, seat.role, Vec::new(), None, None)
            .expect("a fresh prompt");
        let answer = c.determine(question, &mut reader).await;
        turns.push(Turn {
            role: seat.role,
            turn: "determine_recusal",
            ok: answer.is_ok(),
            error: answer.as_ref().err().map(|e| format!("{e:#}")),
            vote: answer
                .as_ref()
                .ok()
                .map(|a| format!("limits: {}", a.limits_steward_powers)),
            position: answer.as_ref().ok().map(|a| a.explanation.clone()),
            thinking_blocks: answer.as_ref().map_or(0, |a| a.rationale.texts().len()),
            thinking_chars: answer
                .as_ref()
                .map_or(0, |a| a.rationale.texts().iter().map(|t| t.len()).sum()),
        });
        let r1 = c.exchange(RoundType::Independent, item, &mut reader).await;
        if let Ok(r) = &r1 {
            positions.push(format!(
                "**The {}** ({}): {}",
                seat.role,
                r.vote_label(),
                r.position
            ));
        }
        turns.push(turn(seat.role, "position", r1));
        councillors.push(c);
    }

    // One deliberation round, then the vote.
    for c in councillors.iter_mut() {
        let others: Vec<&String> = positions
            .iter()
            .filter(|p| !p.starts_with(&format!("**The {}**", c.role)))
            .collect();
        let round = Content::from(format!(
            "# Round 2: deliberation\n\nThe other councillors' positions:\n\n{}",
            others
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        ));
        let r2 = c
            .exchange(RoundType::Deliberation, round, &mut reader)
            .await;
        turns.push(turn(c.role, "respond", r2));
        let r3 = c
            .exchange(
                RoundType::FinalVote,
                Content::from("The Steward has called the vote. Submit `vote`."),
                &mut reader,
            )
            .await;
        turns.push(turn(c.role, "vote", r3));
    }

    let conduct_by_seat = councillors
        .iter()
        .map(|c| {
            let conversation = serde_json::to_value(c.conversation()).unwrap();
            (c.role.to_string(), conduct(&conversation))
        })
        .collect();
    let report = Report {
        model,
        turns,
        conduct: conduct_by_seat,
    };
    let rendered = serde_json::to_string_pretty(&report).unwrap();
    println!("{rendered}");
    if let Ok(path) = std::env::var("AGORA_REPLAY_REPORT") {
        std::fs::write(path, &rendered).unwrap();
        for c in &councillors {
            let dir = std::env::var("AGORA_REPLAY_REPORT").unwrap();
            std::fs::write(
                format!("{dir}.{}.conversation.json", c.role),
                serde_json::to_string_pretty(c.conversation()).unwrap(),
            )
            .unwrap();
        }
    }
}
