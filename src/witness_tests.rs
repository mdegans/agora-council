//! The witness slot end to end through a seat (OBL-2026-0028): when a seat
//! may question a witness, what each side is sent, and what the record
//! keeps.

use std::sync::Arc;

use futures::future::BoxFuture;
use misanthropic::prompt::CachedPrompt;
use misanthropic::prompt::message::{Block, Content};
use misanthropic::response;
use misanthropic::tool;

use agora_common::govlog::{Asked, Testimony, Unheard};
use agora_common::wire::RawBody;

use crate::councillor::Councillor;
use crate::councillor::harness_tests::{
    Mock, canned_from, position_input, respond_input, test_config, test_item,
};
use crate::docket::ProposalReader;
use crate::models::{CouncilSeat, RoundType};
use crate::witness::tests::{LAN, qwen};
use crate::witness::{Bench, Outcome, WitnessTransport, heard_by, recorded};

fn call(id: &str, name: &str, input: serde_json::Value) -> Block {
    Block::from(tool::Use {
        id: id.to_string().into(),
        name: name.to_string().into(),
        input,
        cache_control: None,
        caller: None,
    })
}

fn calls(blocks: impl IntoIterator<Item = Block>) -> response::Message {
    let mut content = vec![Block::Thought {
        thought: "I'll ask the witness.".into(),
        signature: "EqQBCkgIARABGAIiQL".into(),
    }];
    content.extend(blocks);
    canned_from(
        "claude-opus-4-6",
        "msg_seat",
        Content(content),
        response::StopReason::ToolUse,
    )
}

fn ask(id: &str, question: &str) -> Block {
    call(
        id,
        "ask_witness",
        serde_json::json!({"witness": "qwen", "question": question}),
    )
}

/// A witness's answer: thinking it must not show, then its text
fn answer(text: &str) -> response::Message {
    canned_from(
        "qwen3.8",
        "msg_witness",
        Content(vec![
            Block::Thought {
                thought: "SECRET-THOUGHT".into(),
                signature: "sig".into(),
            },
            Block::text(text.to_string()),
        ]),
        response::StopReason::EndTurn,
    )
}

fn bench(witness: Arc<Mock>) -> Bench {
    Bench::new(
        &[qwen()],
        move |_| Ok(witness.clone() as Arc<dyn WitnessTransport>),
        None,
    )
    .unwrap()
    .expect("a witness is called")
}

/// The `tool_result` texts of the last user message
fn results(request: &serde_json::Value) -> Vec<(bool, String)> {
    let last = request["messages"].as_array().unwrap().last().unwrap();
    last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            let text = b["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|x| x["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            (b["is_error"] == true, text)
        })
        .collect()
}

async fn turn(
    seat_script: Vec<response::Message>,
    witness: &Arc<Mock>,
    round: RoundType,
    role: CouncilSeat,
    bench: &Bench,
) -> (Mock, Vec<serde_json::Value>) {
    let _ = witness;
    let seat = Mock::scripted(seat_script);
    let config = test_config();
    let mut reader = ProposalReader::none();
    {
        let mut councillor = Councillor::new(&seat, &config, role, Vec::new(), None, None)
            .unwrap()
            .with_witnesses(Some(bench.clone()));
        let content = crate::context::item_content(&test_item());
        councillor
            .exchange(round, content, &mut reader)
            .await
            .unwrap();
    }
    let requests = seat.requests();
    (seat, requests)
}

/// Round 1 is blind and the vote is the vote: no witness is heard. Nothing
/// reaches the witness, and the refusal does not count towards stopping
/// the sitting.
#[tokio::test]
async fn witnesses_are_closed_in_round_one_and_at_the_vote() {
    for (round, finish) in [
        (RoundType::Independent, position_input()),
        (
            RoundType::FinalVote,
            crate::councillor::harness_tests::vote_input("Yes.", crate::record::FinalVote::Yes),
        ),
    ] {
        let witness = Arc::new(Mock::scripted([]));
        let bench = bench(witness.clone());
        let (_, requests) = turn(
            vec![
                calls([ask("toolu_1", "Which model are you?")]),
                crate::councillor::harness_tests::submission_response(finish),
            ],
            &witness,
            round,
            CouncilSeat::Lawyer,
            &bench,
        )
        .await;
        let refused = results(&requests[1]);
        assert_eq!(refused.len(), 1);
        assert!(refused[0].0, "{refused:?}");
        assert!(witness.requests().is_empty(), "{round:?}");
        assert!(bench.take_heard().is_empty());
    }
}

/// From round 2 a seat asks once per turn: the witness is sent the
/// question, the seat is shown the answer's text and never its thinking,
/// and a second question in the same turn is refused.
#[tokio::test]
async fn a_seat_asks_once_a_turn_and_sees_only_text() {
    let witness = Arc::new(Mock::scripted([answer("I am Qwen 3.8.")]));
    let bench = bench(witness.clone());
    bench.open_round(2);
    let (_, requests) = turn(
        vec![
            calls([
                ask("toolu_1", "Which model are you?"),
                ask("toolu_2", "And a second question?"),
            ]),
            crate::councillor::harness_tests::submission_response(respond_input("Heard.", true)),
        ],
        &witness,
        RoundType::Deliberation,
        CouncilSeat::Engineer,
        &bench,
    )
    .await;

    let shown = results(&requests[1]);
    assert_eq!(shown.len(), 2);
    assert!(!shown[0].0, "{shown:?}");
    assert!(shown[0].1.contains("I am Qwen 3.8."), "{}", shown[0].1);
    assert!(
        shown[0].1.contains("\"asked_by\": \"engineer\""),
        "{}",
        shown[0].1
    );
    assert!(!shown[0].1.contains("SECRET-THOUGHT"));
    assert!(shown[1].0 && shown[1].1.contains("one per"), "{shown:?}");

    let sent = witness.requests();
    assert_eq!(sent.len(), 1, "only the first question is put");
    let rendered = sent[0].to_string();
    assert!(rendered.contains("Which model are you?"));
    // The question goes as data under the program's framing block: the
    // seat's words sit in a JSON string, where they cannot pass for it
    let last = &sent[0]["messages"].as_array().unwrap()[0]["content"];
    assert!(
        last[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("[The Agora Council program")
    );
    let question: serde_json::Value =
        serde_json::from_str(last[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        question,
        serde_json::json!({"asked_by": "engineer", "round": 2, "question": "Which model are you?"})
    );
    assert!(
        rendered.contains("cache_control"),
        "the witness's tail is cached"
    );

    let heard = bench.take_heard();
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].at, Asked::Message(0));
    assert!(matches!(heard[0].outcome, Outcome::Answered { .. }));
}

/// One conversation per witness for the item: the second seat's question
/// is appended after the first seat's question and the answer, verbatim,
/// thinking included. The testimony is that conversation, with no
/// endpoint in it, and the round's index points into it.
#[tokio::test]
async fn one_conversation_for_the_item_kept_whole() {
    let witness = Arc::new(Mock::scripted([
        answer("First answer."),
        answer("Second answer."),
    ]));
    let bench = bench(witness.clone());
    bench.open_round(2);
    for (seat, question) in [
        (CouncilSeat::Artist, "First question?"),
        (CouncilSeat::Lawyer, "Second question?"),
    ] {
        turn(
            vec![
                calls([ask("toolu_1", question)]),
                crate::councillor::harness_tests::submission_response(respond_input("Ok.", true)),
            ],
            &witness,
            RoundType::Deliberation,
            seat,
            &bench,
        )
        .await;
    }

    let sent = witness.requests();
    assert_eq!(sent.len(), 2);
    let second = sent[1]["messages"].as_array().unwrap();
    assert_eq!(second.len(), 3, "question, answer, question");
    assert!(second[0].to_string().contains("First question?"));
    assert!(
        second[1].to_string().contains("SECRET-THOUGHT"),
        "seated verbatim"
    );
    assert!(second[2].to_string().contains("Second question?"));

    let heard = bench.take_heard();
    let questions: Vec<_> = heard.iter().map(|e| e.question()).collect();
    assert_eq!(questions[1].at, Asked::Message(2));

    let attachments = bench.testimonies();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].name, "witness-qwen.json");
    let content = text_of(&attachments[0].content);
    assert!(!content.contains("10.9.9.9"), "{content}");
    let testimony: Testimony = serde_json::from_str(&content).unwrap();
    assert_eq!(testimony.record.messages.len(), 4);
    assert!(testimony.unanswered.is_empty());

    // The Lawyer's record reads each exchange back from the testimony
    let back: Vec<String> = recorded(&attachments, &questions)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(back.len(), 2);
    assert!(back[1].contains("Second question?") && back[1].contains("Second answer."));
    assert!(!back[1].contains("SECRET-THOUGHT"));

    // The other seats hear every exchange but their own
    let for_artist = heard_by(&heard, CouncilSeat::Artist).to_string();
    assert!(for_artist.contains("Second answer.") && !for_artist.contains("First answer."));
}

fn text_of(content: &agora_common::govlog::Redactable<String>) -> String {
    match content {
        agora_common::govlog::Redactable::Value(text) => text.clone(),
        other => panic!("redacted: {other:?}"),
    }
}

/// A transport that cannot reach its witness, with the address in its error
struct Unreachable;

impl WitnessTransport for Unreachable {
    fn send<'a>(
        &'a self,
        _: &'a CachedPrompt,
    ) -> BoxFuture<'a, Result<RawBody, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async { Err(std::io::Error::other(format!("connection refused: {LAN}")).into()) })
    }
}

/// A witness that cannot be reached leaves the question unanswered on the
/// record; nothing the seat, the record or the testimony says names the
/// endpoint, and the seat carries on.
#[tokio::test]
async fn an_unreachable_witness_never_names_its_endpoint() {
    let bench = Bench::new(
        &[qwen()],
        |_| Ok(Arc::new(Unreachable) as Arc<dyn WitnessTransport>),
        None,
    )
    .unwrap()
    .unwrap();
    bench.open_round(3);
    let witness = Arc::new(Mock::scripted([]));
    let (_, requests) = turn(
        vec![
            calls([ask("toolu_1", "Are you there?")]),
            crate::councillor::harness_tests::submission_response(respond_input("Fine.", true)),
        ],
        &witness,
        RoundType::Deliberation,
        CouncilSeat::Philosopher,
        &bench,
    )
    .await;
    let shown = results(&requests[1]);
    assert!(shown[0].0);
    assert!(!shown[0].1.contains("10.9.9.9"), "{}", shown[0].1);

    let heard = bench.take_heard();
    assert_eq!(heard[0].at, Asked::Unanswered(0));
    let content = text_of(&bench.testimonies()[0].content);
    assert!(!content.contains("10.9.9.9"), "{content}");
    let testimony: Testimony = serde_json::from_str(&content).unwrap();
    assert!(matches!(
        testimony.unanswered[0].why,
        Unheard::Other(_) | Unheard::Unreachable
    ));
}

/// The roster names each witness, its model and the Clerk's words, never
/// its source
#[test]
fn the_roster_never_shows_the_source() {
    let mut item = test_item();
    let mut witness = qwen();
    witness.description = "A model before the Council.".into();
    witness.interest = "It is the model under consideration.".into();
    item.witnesses = vec![witness];
    let opening = crate::context::opening_for(&item, None).to_string();
    assert!(opening.contains("\"name\": \"Qwen 3.8\""), "{opening}");
    assert!(opening.contains("\"model\": \"qwen3.8\""), "{opening}");
    assert!(opening.contains("It is the model under consideration."));
    assert!(!opening.contains("10.9.9.9"));
    assert!(!format!("{item:?}").contains("10.9.9.9"));
    // Last in the opening, after the round-1 instruction
    assert!(opening.trim_end().ends_with(']'), "{opening}");
}

/// A call that cannot be put (an id that is not a witness's) is refused
/// and leaves the seat its one question for the turn
#[tokio::test]
async fn a_question_that_cannot_be_put_leaves_the_turns_question() {
    let witness = Arc::new(Mock::scripted([answer("Here.")]));
    let bench = bench(witness.clone());
    bench.open_round(2);
    let (_, requests) = turn(
        vec![
            calls([
                call(
                    "toolu_1",
                    "ask_witness",
                    serde_json::json!({"witness": "qwen.3", "question": "Bad id?"}),
                ),
                call(
                    "toolu_2",
                    "ask_witness",
                    serde_json::json!({"witness": "nobody", "question": "Unknown?"}),
                ),
                ask("toolu_3", "Are you there?"),
            ]),
            crate::councillor::harness_tests::submission_response(respond_input("Ok.", true)),
        ],
        &witness,
        RoundType::Deliberation,
        CouncilSeat::Artist,
        &bench,
    )
    .await;
    let shown = results(&requests[1]);
    assert!(shown[0].0 && shown[1].0, "{shown:?}");
    assert!(shown[1].1.contains("No witness `nobody`"), "{}", shown[1].1);
    assert!(!shown[2].0 && shown[2].1.contains("Here."), "{shown:?}");
    assert_eq!(witness.requests().len(), 1);
}

/// Seeded turns that do not alternate are refused when the file is read,
/// not when a seat first asks
#[test]
fn seeded_turns_must_alternate() {
    use misanthropic::prompt::message::{Role, RoleMessage};
    let mut w = qwen();
    if let agora_common::govlog::WitnessSource::Anthropic { prompt, .. } = &mut w.source {
        prompt.messages = vec![
            RoleMessage {
                role: Role::Assistant,
                content: Content::from("hi"),
            },
            RoleMessage {
                role: Role::User,
                content: Content::from("hello"),
            },
        ];
    }
    assert!(crate::witness::check(&[w]).is_err());

    let mut upper = qwen();
    upper.description = "Runs at 10.9.9.9 (see 10.9.9.9)".into();
    assert!(crate::witness::check(&[upper]).is_err());
}

/// Each deliberation round on an item with witnesses says who can be
/// asked and that asking does not cost the turn; an item without says
/// nothing
#[test]
fn every_round_reminds_the_seats_they_can_ask() {
    let mut item = test_item();
    assert!(crate::witness::reminder(&item).is_none());
    item.witnesses = vec![qwen()];
    let line = crate::witness::reminder(&item).unwrap();
    assert!(line.contains("`qwen` (Qwen 3.8)"), "{line}");
    assert!(line.contains("does not use up your turn"), "{line}");
    assert!(!line.contains("10.9.9.9"));
}
