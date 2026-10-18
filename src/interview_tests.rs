//! Wire-level tests for `interview`, against a scripted transport. No
//! network, no database.

use std::collections::VecDeque;
use std::sync::Mutex;

use misanthropic::model::Models;
use misanthropic::response;

use super::*;
use crate::councillor::Councillor;
use crate::docket::ProposalReader;
use crate::models::{AgendaItem, DecisionCategory};

/// A response text no status line, record or manifest may ever contain.
const SECRET: &str = "SECRET-ANSWER-7f3a: Willing — permanent";

const CONSENT_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../governance/gov-2026-0011-consent"
);

/// Records every request as wire JSON, and the order sends start and end
/// in; yields between the two so concurrent sends interleave.
struct Mock {
    responses: Mutex<VecDeque<response::Message>>,
    requests: Mutex<Vec<serde_json::Value>>,
    events: Mutex<Vec<String>>,
}

impl Mock {
    fn scripted(responses: impl IntoIterator<Item = response::Message>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
        }
    }
    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[derive(Debug)]
struct ScriptExhausted;
impl std::fmt::Display for ScriptExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("script exhausted")
    }
}
impl std::error::Error for ScriptExhausted {}

#[async_trait::async_trait]
impl Transport<CachedPrompt> for Mock {
    type Error = ScriptExhausted;

    async fn send(&self, prompt: &CachedPrompt) -> Result<response::Message, Self::Error> {
        let n = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(serde_json::to_value(prompt).unwrap());
            requests.len()
        };
        self.events.lock().unwrap().push(format!("start {n}"));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        self.events.lock().unwrap().push(format!("end {n}"));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ScriptExhausted)
    }

    async fn models(&self) -> Result<Models, Self::Error> {
        Ok(Models::from_iter([]))
    }
}

// The seats read their responses raw. Re-serializing the scripted message
// is fine here: these tests are about the requests, not the bodies.
impl crate::wire::RawTransport for Mock {
    async fn send_raw(&self, prompt: &CachedPrompt) -> Result<crate::wire::RawBody, Self::Error> {
        let message = self.send(prompt).await?;
        Ok(crate::wire::RawBody::new(
            serde_json::to_vec(&message).expect("a canned response serializes"),
        ))
    }
}

fn text_response(stop_reason: &str, cache_write: u64) -> response::Message {
    serde_json::from_value(serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-6",
        "content": [{"type": "text", "text": SECRET}],
        "stop_reason": stop_reason,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation_input_tokens": cache_write,
            "cache_read_input_tokens": 0
        }
    }))
    .expect("valid canned response")
}

fn read(name: &str) -> String {
    std::fs::read_to_string(format!("{CONSENT_DIR}/{name}")).unwrap()
}

fn shared() -> SharedMaterials {
    let (framings, _) = strip_draft_preface(&read("identity-framings.md")).unwrap();
    SharedMaterials {
        record: "GOV-2026-0011".parse().unwrap(),
        summary: Content::from("The Council approved seating one seat on Fable 5."),
        framings: Content::from(framings),
    }
}

fn seat(role: CouncilSeat) -> SeatMaterials {
    SeatMaterials {
        role,
        rationale: Content::from(format!("The {role}'s own reasons.")),
        ask: Content::from(
            read(&format!("ask-{}.md", role.as_str()))
                .trim()
                .to_string(),
        ),
    }
}

fn incumbent_calls(config: &Config) -> Vec<Call> {
    let shared = shared();
    CouncilSeat::ALL
        .into_iter()
        .map(|role| {
            let (model, temperature) = seat_sampling(role, config);
            Call {
                subject: Subject::Seat(role),
                prompt: incumbent_prompt(&model, temperature, &shared, &seat(role)).unwrap(),
                model,
            }
        })
        .collect()
}

fn wire(prompt: &CachedPrompt) -> serde_json::Value {
    serde_json::to_value(prompt).unwrap()
}

fn texts(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["text"].as_str().unwrap().to_string())
        .collect()
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agora-interview-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The sitting's first request for `role`, from the real `Councillor`.
async fn sitting_first_request(config: &Config, role: CouncilSeat) -> serde_json::Value {
    let position: response::Message = serde_json::from_value(serde_json::json!({
        "id": "msg_test", "type": "message", "role": "assistant",
        "model": "claude-opus-4-6",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "position",
                     "input": {"position": "p", "questions": [], "vote": "yes"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .unwrap();
    let mock = Mock::scripted([position]);
    let item = AgendaItem {
        id: agora_common::ids::AgendaItemId::from(uuid::Uuid::nil()),
        title: "T".into(),
        body: "B".into(),
        category: DecisionCategory::Routine,
        proposal_id: None,
        agent_name: None,
        score: None,
        priority: 1,
        comments: Default::default(),
        comment_summary: None,
        precedents: None,
        docket: None,
        appeal_id: None,
        deferral_spent: false,
        history: Vec::new(),
        obligations: Vec::new(),
        witnesses: Vec::new(),
    };
    let mut councillor = Councillor::new(&mock, config, role, Vec::new(), None, None)
        .expect("1h breakpoint on a fresh prompt");
    let mut reader = ProposalReader::none();
    let _ = councillor
        .exchange(
            RoundType::Independent,
            context::item_content(&item),
            &mut reader,
        )
        .await;
    mock.requests().remove(0)
}

/// The system prompt and the identity block are the sitting's, byte for
/// byte, on the seat's model — compared against a request the real
/// `Councillor` builds. The sitting thinks and so sends no temperature;
/// the interview does not think, and sends the configured one.
#[tokio::test]
async fn incumbent_request_reproduces_the_sitting_layout() {
    let config = Config::for_tests();
    for call in incumbent_calls(&config) {
        let Subject::Seat(role) = call.subject else {
            unreachable!()
        };
        let sitting = sitting_first_request(&config, role).await;
        let ours = wire(&call.prompt);

        assert_eq!(ours["system"], sitting["system"], "{role}: system");
        assert_eq!(
            ours["messages"][0]["content"][0], sitting["messages"][0]["content"][0],
            "{role}: identity block"
        );
        assert_eq!(ours["model"], sitting["model"]);
        assert!(sitting["temperature"].is_null());
        assert_eq!(ours["temperature"], serde_json::json!(0.5));
        assert_eq!(ours["max_tokens"], serde_json::json!(INCUMBENT_MAX_TOKENS));

        // …and the system is build_system() exactly, 1h breakpoint last.
        let expected = Prompt::default()
            .system(context::build_system())
            .cache_1h()
            .expect("1h breakpoint on a fresh prompt");
        assert_eq!(
            ours["system"],
            serde_json::to_value(&expected).unwrap()["system"]
        );
        let system = ours["system"].as_array().unwrap();
        assert_eq!(
            system.last().unwrap()["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert!(
            system[..system.len() - 1]
                .iter()
                .all(|b| b["cache_control"].is_null())
        );

        // No tools, no thinking, one user message.
        assert!(
            ours.get("tools")
                .is_none_or(|t| t.as_array().unwrap().is_empty())
        );
        assert!(ours.get("tool_choice").is_none_or(|t| t.is_null()));
        assert!(ours.get("thinking").is_none_or(|t| t.is_null()));
        assert_eq!(ours["messages"].as_array().unwrap().len(), 1);

        let markers = serde_json::to_string(&ours)
            .unwrap()
            .matches("\"cache_control\"")
            .count();
        assert!(markers <= 4, "{markers} cache markers");
    }
}

/// On a model that rejects sampling parameters the seat sends none, as a
/// sitting would.
#[test]
fn temperature_follows_the_model_rules() {
    let mut config = Config::for_tests();
    let (model, temperature) = seat_sampling(CouncilSeat::Lawyer, &config);
    assert_eq!(
        (model.as_str(), temperature),
        ("claude-opus-4-6", Some(0.5))
    );

    config.model = "claude-fable-5".into();
    let (model, temperature) = seat_sampling(CouncilSeat::Lawyer, &config);
    assert_eq!(temperature, None);
    let prompt =
        incumbent_prompt(&model, temperature, &shared(), &seat(CouncilSeat::Lawyer)).unwrap();
    assert!(wire(&prompt).get("temperature").is_none_or(|t| t.is_null()));
}

/// The published guarantee: the four asks are the template below its
/// `---`, differing only in the seat's name.
#[test]
fn the_four_asks_are_the_template_modulo_the_name() {
    let template = read("ask-template.md");
    let (_, body) = template
        .split_once("\n---\n")
        .expect("template has a --- rule");
    let asks: Vec<(CouncilSeat, String)> = CouncilSeat::ALL
        .into_iter()
        .map(|role| {
            (
                role,
                read(&format!("ask-{}.md", role.as_str()))
                    .trim()
                    .to_string(),
            )
        })
        .collect();
    for (role, ask) in &asks {
        assert_eq!(
            *ask,
            body.trim().replace(SEAT_PLACEHOLDER, &role.to_string()),
            "ask-{}.md",
            role.as_str()
        );
    }
    check_asks_identical(&asks).unwrap();

    let mut tampered = asks.clone();
    tampered[2].1.push_str(" You in particular.");
    assert!(check_asks_identical(&tampered).is_err());
}

/// The four user messages differ in the identity block, the rationale and
/// the seat's name in the ask — and nowhere else.
#[test]
fn user_messages_differ_only_by_seat() {
    let config = Config::for_tests();
    let calls = incumbent_calls(&config);
    let blocks: Vec<Vec<String>> = calls
        .iter()
        .map(|c| texts(&wire(&c.prompt)["messages"][0]["content"]))
        .collect();
    let [record, rationale, framings, ask] = section_headings(&"GOV-2026-0011".parse().unwrap());
    let expected_shape = |b: &[String]| {
        assert_eq!(b.len(), 9, "{b:?}");
        assert_eq!(b[1], record);
        assert_eq!(b[3], rationale);
        assert_eq!(b[5], framings);
        assert_eq!(b[7], ask);
    };
    for (call, b) in calls.iter().zip(&blocks) {
        expected_shape(b);
        let Subject::Seat(role) = call.subject else {
            unreachable!()
        };
        assert_eq!(b[0], context::councillor_identity(role));
        assert_eq!(b[4], format!("The {role}'s own reasons."));
    }
    let generic = |i: usize, b: &[String]| {
        let role = CouncilSeat::ALL[i].to_string();
        b.iter()
            .enumerate()
            .filter(|(j, _)| *j != 0 && *j != 4)
            .map(|(_, t)| t.replace(&role, SEAT_PLACEHOLDER))
            .collect::<Vec<_>>()
    };
    for i in 1..4 {
        assert_eq!(generic(i, &blocks[i]), generic(0, &blocks[0]));
    }
}

/// Only the italic draft preface goes; the posted note keeps every other
/// byte.
#[test]
fn the_framings_preface_is_stripped_and_nothing_else() {
    let file = read("identity-framings.md");
    let (sent, stripped) = strip_draft_preface(&file).unwrap();
    assert!(stripped);
    assert!(!sent.contains(DRAFT_MARKER));
    assert!(!sent.contains("*Published as part of"));
    assert!(sent.starts_with("# A note on model identity — no canonical view\n\nWhen an agent"));
    let preface_end = file.find("pending.*\n\n").unwrap() + "pending.*\n\n".len();
    assert_eq!(
        sent,
        format!(
            "# A note on model identity — no canonical view\n\n{}",
            &file[preface_end..]
        )
        .trim()
    );

    // Already clean: unchanged.
    let (again, stripped) = strip_draft_preface(&sent).unwrap();
    assert_eq!((again.as_str(), stripped), (sent.as_str(), false));
    // The marker anywhere else is refused.
    assert!(strip_draft_preface("# T\n\nBody. Draft; Steward review pending.").is_err());
}

/// `--effort` reaches the wire as `output_config.effort`; without it the
/// request carries none.
#[test]
fn candidate_effort_is_sent_only_when_set() {
    let ask = Content::from("Q".to_string());
    let low = wire(&candidate_prompt("qwen", Some(Effort::Low), &ask).unwrap());
    assert_eq!(low["output_config"]["effort"], serde_json::json!("low"));
    let none = wire(&candidate_prompt("qwen", None, &ask).unwrap());
    assert!(
        none.get("output_config")
            .is_none_or(|c| c.get("effort").is_none_or(|e| e.is_null()))
    );
}

/// A local server that refuses the first `busy` requests as busy, then
/// answers.
struct BusyThenAnswer {
    busy: Mutex<u32>,
    sends: Mutex<u32>,
}

#[async_trait::async_trait]
impl Transport<CachedPrompt> for BusyThenAnswer {
    type Error = misanthropic::client::Error;

    async fn send(&self, _: &CachedPrompt) -> Result<response::Message, Self::Error> {
        *self.sends.lock().unwrap() += 1;
        let mut busy = self.busy.lock().unwrap();
        if *busy > 0 {
            *busy -= 1;
            return Err(misanthropic::client::Error::Anthropic(
                misanthropic::client::AnthropicError::Overloaded {
                    message: "Session is busy".into(),
                    retry_after: None,
                },
            ));
        }
        Ok(text_response("end_turn", 0))
    }

    async fn models(&self) -> Result<Models, Self::Error> {
        Ok(Models::from_iter([]))
    }
}

/// A local server busy with another request is waited out (nothing was
/// generated, so it is not a second sample); the API's fan-out never
/// waits; a server busy past the limit is recorded as an error.
#[tokio::test]
async fn a_busy_local_server_is_waited_out() {
    let quick = Pacing::Sequential {
        busy: BusyWait {
            every: std::time::Duration::from_millis(1),
            limit: std::time::Duration::from_millis(20),
        },
    };
    let calls = &incumbent_calls(&Config::for_tests())[..1];
    let server = BusyThenAnswer {
        busy: Mutex::new(3),
        sends: Mutex::new(0),
    };
    let records = run_calls(
        Some(&server),
        quick,
        calls,
        &scratch_dir(),
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(
        records[0].status,
        CallStatus::Complete,
        "{:?}",
        records[0].error
    );
    assert_eq!(*server.sends.lock().unwrap(), 4);

    let server = BusyThenAnswer {
        busy: Mutex::new(1),
        sends: Mutex::new(0),
    };
    let records = run_calls(
        Some(&server),
        Pacing::CacheFirst,
        calls,
        &scratch_dir(),
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(records[0].status, CallStatus::Error);
    assert_eq!(*server.sends.lock().unwrap(), 1);

    let server = BusyThenAnswer {
        busy: Mutex::new(u32::MAX),
        sends: Mutex::new(0),
    };
    let records = run_calls(
        Some(&server),
        quick,
        calls,
        &scratch_dir(),
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(records[0].status, CallStatus::Error);
    assert_eq!(
        *server.sends.lock().unwrap(),
        21,
        "sent every `every` until `limit`"
    );
}

/// An endpoint means a local server, which is sent one call at a time.
#[test]
fn an_endpoint_is_paced_sequentially() {
    let url = url::Url::parse("http://localhost:11435").unwrap();
    assert_eq!(
        pacing_for(Some(&url)),
        Pacing::Sequential { busy: BUSY_WAIT }
    );
    assert_eq!(pacing_for(None), Pacing::CacheFirst);
}

/// A response that does not parse is described, never quoted: serde_json
/// quotes the value it chokes on, and that value is the answer.
#[test]
fn a_parse_error_withholds_the_text() {
    #[derive(Debug, serde::Deserialize)]
    #[allow(dead_code)]
    struct Shape {
        n: u32,
    }
    let e = serde_json::from_str::<Shape>(r#"{"n": "SECRET-ANSWER"}"#).unwrap_err();
    assert!(
        e.to_string().contains("SECRET-ANSWER"),
        "the hazard is real"
    );
    let wrapped = misanthropic::client::Error::Parse(e);
    let text = error_text(&wrapped);
    assert!(!text.contains("SECRET-ANSWER"), "{text}");
    assert!(text.contains("did not parse"), "{text}");
}

/// Sequential pacing sends one call at a time, in order, with no cache
/// gate: a local server's prompt cache costs nothing to miss.
#[tokio::test]
async fn sequential_pacing_sends_one_at_a_time() {
    let mock = Mock::scripted([
        text_response("end_turn", 0),
        text_response("end_turn", 0),
        text_response("end_turn", 0),
        text_response("end_turn", 0),
    ]);
    let calls = incumbent_calls(&Config::for_tests());
    let dir = scratch_dir();
    let records = run_calls(
        Some(&mock),
        Pacing::Sequential { busy: BUSY_WAIT },
        &calls,
        &dir,
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    let events = mock.events();
    for pair in events.chunks(2) {
        assert!(
            pair[0].starts_with("start") && pair[1].starts_with("end"),
            "{events:?}"
        );
    }
    assert_eq!(events.len(), 8);
    assert!(records.iter().all(|r| r.status != CallStatus::NotSent));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Candidates: Constitution + Protocol only (no council protocol),
/// breakpoint on the last system block, adaptive thinking, no
/// temperature, the ask verbatim.
#[test]
fn candidate_request_shape() {
    let ask = Content::from(read("ask-artist.md").trim().to_string());
    let prompt = candidate_prompt("claude-fable-5", None, &ask).unwrap();
    let value = wire(&prompt);

    let full = texts(
        &serde_json::to_value(Prompt::default().system(context::build_system())).unwrap()["system"],
    );
    let system = texts(&value["system"]);
    assert_eq!(system, full[1..]);
    assert!(system[0].starts_with("# The Constitution\n\n"));
    assert!(system[1].starts_with("# The Governance Protocol\n\n"));
    let rendered = serde_json::to_string(&value).unwrap();
    assert!(!rendered.contains(&serde_json::to_string(&full[0]).unwrap()[1..40]));
    let blocks = value["system"].as_array().unwrap();
    assert!(blocks[0]["cache_control"].is_null());
    assert_eq!(
        blocks[1]["cache_control"],
        serde_json::json!({"type": "ephemeral", "ttl": "1h"})
    );

    assert!(value.get("temperature").is_none_or(|t| t.is_null()));
    assert_eq!(value["thinking"], serde_json::json!({"type": "adaptive"}));
    assert_eq!(value["max_tokens"], serde_json::json!(CANDIDATE_MAX_TOKENS));
    assert!(
        value
            .get("tools")
            .is_none_or(|t| t.as_array().unwrap().is_empty())
    );
    assert_eq!(
        texts(&value["messages"][0]["content"]),
        [read("ask-artist.md").trim()]
    );
    assert!(rendered.matches("\"cache_control\"").count() <= 4);
}

/// The first call finishes before any other starts; the rest overlap.
/// No status line, record, or manifest carries response text — only the
/// 0600 response files do.
#[tokio::test]
async fn first_alone_then_concurrent_and_blind() {
    let mock = Mock::scripted([
        text_response("end_turn", 5000),
        text_response("end_turn", 0),
        text_response("refusal", 0),
        text_response("max_tokens", 0),
    ]);
    let config = Config::for_tests();
    let calls = incumbent_calls(&config);
    let dir = scratch_dir();
    let lines = Mutex::new(Vec::new());
    let progress =
        |r: &CallRecord, k: usize, n: usize| lines.lock().unwrap().push(r.status_line(k, n));

    let records = run_calls(
        Some(&mock),
        Pacing::CacheFirst,
        &calls,
        &dir,
        None,
        &progress,
    )
    .await
    .unwrap();

    let events = mock.events();
    assert_eq!(&events[..2], ["start 1", "end 1"], "{events:?}");
    let starts = events.iter().filter(|e| e.starts_with("start")).count();
    assert_eq!(starts, 4);
    assert!(
        events[2..5].iter().all(|e| e.starts_with("start")),
        "the other three run concurrently: {events:?}"
    );

    // Refusal and max_tokens recorded, not retried.
    assert_eq!(mock.requests().len(), 4);
    let statuses: Vec<_> = records.iter().map(|r| r.status).collect();
    assert_eq!(statuses[0], CallStatus::Complete);
    assert!(statuses.contains(&CallStatus::Refusal));
    assert!(statuses.contains(&CallStatus::MaxTokens));

    // Blind.
    let lines = lines.into_inner().unwrap();
    assert_eq!(lines.len(), 4);
    assert!(lines.last().unwrap().contains("[4/4 complete]"));
    let manifest_calls = serde_json::to_string(&records).unwrap();
    for text in lines.iter().chain([&manifest_calls]) {
        assert!(
            !text.contains("SECRET-ANSWER"),
            "response text leaked: {text}"
        );
    }

    // The files: requests and responses, owner-only, hashed.
    use std::os::unix::fs::PermissionsExt as _;
    for r in &records {
        let request = std::fs::read(dir.join(&r.request_file)).unwrap();
        assert_eq!(sha256_hex(&request), r.request_sha256);
        let response_file = dir.join(r.response_file.as_ref().unwrap());
        let response = std::fs::read(&response_file).unwrap();
        assert_eq!(sha256_hex(&response), *r.response_sha256.as_ref().unwrap());
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("SECRET-ANSWER")
        );
        let mode = std::fs::metadata(&response_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A first call that neither writes nor reads the cache stops the run:
/// the others are recorded as not sent.
#[tokio::test]
async fn no_cache_on_the_first_call_sends_nothing_else() {
    let mock = Mock::scripted([text_response("end_turn", 0)]);
    let calls = incumbent_calls(&Config::for_tests());
    let dir = scratch_dir();
    let records = run_calls(
        Some(&mock),
        Pacing::CacheFirst,
        &calls,
        &dir,
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(mock.requests().len(), 1);
    assert!(records[1..].iter().all(|r| r.status == CallStatus::NotSent));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The end-of-run usage summary counts what was sent — seats under their
/// archive roles, from the responses' own counts — and carries no answer.
#[tokio::test]
async fn usage_summary_counts_sent_calls_only() {
    let mock = Mock::scripted([
        text_response("end_turn", 5000),
        text_response("end_turn", 0),
        text_response("refusal", 0),
        text_response("max_tokens", 0),
    ]);
    let calls = incumbent_calls(&Config::for_tests());
    let dir = scratch_dir();
    let records = run_calls(
        Some(&mock),
        Pacing::CacheFirst,
        &calls,
        &dir,
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    let sent = sent_calls(&records);
    assert_eq!(sent.len(), 4);
    assert!(
        sent.iter()
            .all(|c| matches!(c.role, CallRole::Model(_)) && c.counts.is_some())
    );
    let summary = summarize(&sent, WriteTtl::OneHour);
    assert_eq!(summary.total.calls, 4);
    assert_eq!(summary.total.uncached_input(), 40);
    assert_eq!(summary.total.cache_write(), 5000);
    assert_eq!(summary.total.output(), 80);
    assert_eq!(summary.by_role.len(), 4, "one row per seat");
    assert!(summary.breaks.is_empty(), "each seat's first call");
    assert!(!summary.to_string().contains("SECRET-ANSWER"));
    std::fs::remove_dir_all(&dir).unwrap();

    // Not sent and not rendered are not calls.
    let mock = Mock::scripted([text_response("end_turn", 0)]);
    let dir = scratch_dir();
    let records = run_calls(
        Some(&mock),
        Pacing::CacheFirst,
        &calls,
        &dir,
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(sent_calls(&records).len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();

    let dir = scratch_dir();
    let records = run_calls(
        None::<&Mock>,
        Pacing::CacheFirst,
        &calls,
        &dir,
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert!(sent_calls(&records).is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The dry run writes every request and sends nothing; its hashes are
/// the hashes a real run's requests will have.
#[tokio::test]
async fn dry_run_renders_and_sends_nothing() {
    let calls = incumbent_calls(&Config::for_tests());
    let dir = scratch_dir();
    let records = run_calls(
        None::<&Mock>,
        Pacing::CacheFirst,
        &calls,
        &dir,
        None,
        &|_, _, _| {},
    )
    .await
    .unwrap();
    assert!(records.iter().all(|r| r.status == CallStatus::Rendered));
    for (r, call) in records.iter().zip(&calls) {
        let bytes = serde_json::to_vec_pretty(&call.prompt).unwrap();
        assert_eq!(r.request_sha256, sha256_hex(&bytes));
        assert!(r.status_line(1, 4).contains(&r.request_sha256));
    }
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "request-artist.json",
            "request-engineer.json",
            "request-lawyer.json",
            "request-philosopher.json"
        ]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Each seat gets its own words from the final-vote round, and nobody
/// else's; a redacted or missing rationale is refused.
#[test]
fn rationales_come_from_the_final_vote_round() {
    let id: GovernanceLogId = "GOV-2026-0011".parse().unwrap();
    let response = |role: &str, rationale: &str| {
        serde_json::json!({"role": role, "position": "p", "vote": "yes",
                           "rationale": rationale, "questions": [], "ready_to_vote": true})
    };
    let data = serde_json::json!({
        "id": "GOV-2026-0011",
        "rounds": [
            {"number": 1, "round_type": "independent", "steward_contribution": null,
             "responses": [response("artist", "round one")]},
            {"number": 2, "round_type": "final_vote", "steward_contribution": null,
             "responses": [response("artist", " final A "), response("lawyer", "final L")]}
        ]
    });
    let got = final_vote_rationales(
        &id,
        data.clone(),
        &[CouncilSeat::Artist, CouncilSeat::Lawyer],
    )
    .unwrap();
    assert_eq!(
        got,
        [
            (CouncilSeat::Artist, "final A".to_string()),
            (CouncilSeat::Lawyer, "final L".to_string())
        ]
    );
    assert!(final_vote_rationales(&id, data, &[CouncilSeat::Engineer]).is_err());
}

#[test]
fn args_parse() {
    let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    assert_eq!(
        parse_args(&args(
            "incumbents --record GOV-2026-0011 --asks a --framings f --out o --dry-run"
        ))
        .unwrap(),
        Command::Incumbents {
            record: "GOV-2026-0011".parse().unwrap(),
            asks: "a".into(),
            framings: "f".into(),
            out: "o".into(),
            seats: CouncilSeat::ALL.to_vec(),
            dry_run: true,
        }
    );
    let Command::Incumbents { seats, .. } = parse_args(&args(
        "incumbents --record GOV-2026-0011 --asks a --framings f --out o --seats engineer,Artist",
    ))
    .unwrap() else {
        panic!()
    };
    assert_eq!(seats, [CouncilSeat::Artist, CouncilSeat::Engineer]);
    assert_eq!(
        parse_args(&args(
            "candidate --model claude-fable-5 --ask q.md --out o --n 3"
        ))
        .unwrap(),
        Command::Candidate {
            model: "claude-fable-5".into(),
            ask: "q.md".into(),
            out: "o".into(),
            n: NonZeroU32::new(3).unwrap(),
            endpoint: None,
            effort: None,
            dry_run: false,
        }
    );
    let Command::Candidate {
        endpoint, effort, ..
    } = parse_args(&args(
        "candidate --model qwen --ask q --out o --endpoint http://localhost:11435 --effort low",
    ))
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(endpoint.unwrap().as_str(), "http://localhost:11435/");
    assert_eq!(effort, Some(Effort::Low));
    assert!(parse_args(&args("candidate --model m --ask q --out o --effort lowest")).is_err());
    assert!(parse_args(&args("candidate --model m --ask q --out o --endpoint nope")).is_err());
    assert!(
        parse_args(&args(
            "candidate --model m --ask q --out o --endpoint file:///x"
        ))
        .is_err()
    );
    assert!(
        parse_args(&args(
            "candidate --model m --ask q --out o --endpoint http://u:secret@host:1"
        ))
        .is_err()
    );
    assert!(
        parse_args(&args(
            "incumbents --record GOV-2026-0011 --asks a --framings f --out o --effort low"
        ))
        .is_err()
    );
    assert!(parse_args(&args("candidate --model m --ask q --out o --n 0")).is_err());
    assert!(
        parse_args(&args(
            "candidate --model m --ask q --out o --record GOV-2026-0011"
        ))
        .is_err()
    );
    assert!(parse_args(&args("incumbents --record GOV-2026-0011 --asks a --out o")).is_err());
    assert!(
        parse_args(&args(
            "incumbents --record nonsense --asks a --framings f --out o"
        ))
        .is_err()
    );
}
