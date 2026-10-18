//! What the seats were shown beyond the proposal, as attachments on the
//! decision's governance log entry (#159).
//!
//! The record already holds every round the seats spoke. What it did not
//! hold is what they spoke *about*: the Clerk's summary of the thread and
//! its precedent note, which open every seat's briefing, and whatever a
//! seat had the Clerk read to it mid-deliberation. Without them a reader
//! can see how the Council voted and cannot check whether the material it
//! voted on was fair. Each is attached verbatim, as the seats received it.
//!
//! Since #487 two more: the id map (`comment-ids.md`, every comment label
//! cited anywhere in the record and the full id it names) and, when a
//! citation names no single comment the Council was shown, the warnings
//! (`citation-warnings.md`). The cited text itself is never touched.
//!
//! On a scheduling item, `docket.json` (#510): the shuffle seed that
//! ordered the docket and the citation counts the seats were not shown.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use agora_common::govlog::{CouncilAttachment, Redactable};

use crate::citations::{self, Checked, Shown, Source};
use crate::docket::Reading;
use crate::models::{AgendaItem, CouncilSeat, Round};
use crate::record::ClerkRead;
use agora_common::council::ThreadRole;

/// One [`ClerkRead`], with the seat and round it happened in.
#[derive(Debug, Clone)]
pub struct SeatRead {
    pub round: i32,
    pub role: CouncilSeat,
    pub read: ClerkRead,
}

/// The decision's attachments for `item`, given what the seats had read
/// to them (`reads`, and the docket proposals in `readings`) and what they
/// said (`rounds`). Empty for an item with no thread, no precedents, no
/// reads, no docket and no comment citations.
pub fn attachments(
    item: &AgendaItem,
    reads: &[SeatRead],
    rounds: &[Round],
    readings: &BTreeMap<u32, Reading>,
) -> Vec<CouncilAttachment> {
    let mut out = Vec::new();
    if let Some(summary) = &item.comment_summary {
        out.push(CouncilAttachment {
            name: "clerk-thread-summary.md".to_string(),
            note: format!(
                "The Clerk's summary of the proposal's comment thread ({} comments), \
                 in every seat's opening briefing. Comments are cited by the ids in \
                 their labels, which is what `ask_clerk` reads back; \
                 comment-ids.md resolves each one.",
                item.comments.len()
            ),
            content: Redactable::Value(summary.to_string()),
        });
    }
    if let Some(precedents) = &item.precedents {
        out.push(CouncilAttachment {
            name: "clerk-precedents.md".to_string(),
            note: "The Clerk's note on past decisions bearing on this item, in every \
                   seat's opening briefing."
                .to_string(),
            content: Redactable::Value(precedents.to_string()),
        });
    }
    let (tool_reads, clerk_reads): (Vec<&SeatRead>, Vec<&SeatRead>) = reads
        .iter()
        .partition(|r| matches!(r.read, ClerkRead::Tool { .. }));
    if !clerk_reads.is_empty() {
        out.push(CouncilAttachment {
            name: "clerk-reads.md".to_string(),
            note: "Everything a seat had the Clerk read to it during deliberation, in \
                   order, as the seat received it."
                .to_string(),
            content: Redactable::Value(render_reads(&clerk_reads)),
        });
    }
    if !tool_reads.is_empty() {
        out.push(CouncilAttachment {
            name: "seat-tool-reads.md".to_string(),
            note: "Every call a seat made to its agent tools during deliberation \
                   (`get_content`, the tool every agent has, and the witness tools), \
                   in order: the arguments as the seat sent them and the whole \
                   result as the seat received it, refusals included. An answered \
                   `ask_witness` points to the witness's testimony instead."
                .to_string(),
            content: Redactable::Value(render_reads(&tool_reads)),
        });
    }

    // Every thread the item carried (#492), its own included.
    if let Some(threads) = crate::threads::record(&item.comments) {
        out.push(CouncilAttachment {
            name: "item-threads.md".to_string(),
            note: "Every thread this item carried: its own, and each related \
                   thread the Steward attached, with the reason (context \
                   assembly, disclosed under Governance Protocol § 6.4). The \
                   Clerk's summary covers them all, and `ask_clerk` read from \
                   any of them."
                .to_string(),
            content: Redactable::Value(threads),
        });
    }

    // The docket's order and citation counts (agora#510): the counts are
    // kept in the record, not shown to the seats.
    if let Some(docket) = &item.docket {
        out.push(CouncilAttachment {
            name: "docket.json".to_string(),
            note: "The docket's numbering and the shuffle seed that ordered it, with \
                   how many times the scheduling thread cited each post. The seats \
                   saw the docket in this order but not the counts: a count measures \
                   mentions, not support (agora#510)."
                .to_string(),
            content: Redactable::Value(docket.record_json()),
        });
    }

    let checked = check_citations(item, reads, rounds, readings);
    let mut sources: Vec<Source<'_>> = if item.comments.threads.is_empty() {
        vec![Source {
            thread: format!("This item's thread: {}", item.title),
            comments: &item.comments.rows,
            labels: &item.comments.labels,
        }]
    } else {
        item.comments
            .threads
            .iter()
            .map(|thread| Source {
                thread: match thread.role {
                    ThreadRole::Primary => format!("This item's thread: {}", thread.name()),
                    ThreadRole::Related => format!("Related thread: {}", thread.name()),
                },
                comments: item.comments.of(thread),
                labels: &item.comments.labels,
            })
            .collect()
    };
    sources.extend(readings.iter().map(|(number, reading)| Source {
        thread: format!(
            "P{number}'s thread, read with `read_proposal`: {}",
            reading.title
        ),
        comments: &reading.comments,
        labels: &reading.labels,
    }));
    if let Some(map) = citations::render_id_map(&checked.resolved, &sources) {
        out.push(CouncilAttachment {
            name: "comment-ids.md".to_string(),
            note: "Every comment label cited in this record, by the Clerk or a seat, \
                   and the full id of the comment it names. A label is a prefix of \
                   the comment's own id, lengthened only where two comments shown \
                   shared its first eight hex digits."
                .to_string(),
            content: Redactable::Value(map),
        });
    }
    if let Some(warnings) = citations::render_warnings(&checked.unresolved) {
        out.push(CouncilAttachment {
            name: "citation-warnings.md".to_string(),
            note: "Comment citations in this record that name no single comment the \
                   Council was shown, found by checking every citation after it was \
                   written. Reported, not corrected: the record is what was emitted."
                .to_string(),
            content: Redactable::Value(warnings),
        });
    }
    out
}

/// Check every comment citation in the record (#487): the Clerk's thread
/// summary against the thread it read, each docket proposal's summary
/// against its own thread (checked when it was read), and every seat's
/// words against everything the Council was shown on this item.
fn check_citations(
    item: &AgendaItem,
    reads: &[SeatRead],
    rounds: &[Round],
    readings: &BTreeMap<u32, Reading>,
) -> Checked {
    let mut checked = Checked::default();

    if let Some(summary) = &item.comment_summary {
        let mut shown = Shown::default().labels(&item.comments.labels);
        shown.material(&item.title);
        shown.material(&item.body);
        for thread in &item.comments.threads {
            shown.material(&thread.title);
            shown.material(thread.body.as_deref().unwrap_or_default());
            if let Some(attached) = &thread.attached {
                shown.material(&attached.reason);
            }
        }
        for comment in &item.comments.rows {
            shown.material(&comment.body);
        }
        checked.extend(citations::check(
            "The Clerk's thread summary",
            &summary.to_string(),
            &shown,
        ));
    }
    for reading in readings.values() {
        checked.extend(reading.checked.clone());
    }

    // The seats: anything the Council was shown on this item may be
    // quoted, so everything shown is material. The seats' own words are
    // not: an id one seat invents is reported again when another repeats
    // it.
    let mut shown = Shown::default().labels(&item.comments.labels);
    for reading in readings.values() {
        shown = shown.labels(&reading.labels);
    }
    shown.material(&item.title);
    shown.material(&item.body);
    for thread in &item.comments.threads {
        shown.material(&thread.title);
        shown.material(thread.body.as_deref().unwrap_or_default());
        if let Some(attached) = &thread.attached {
            shown.material(&attached.reason);
        }
    }
    for text in [&item.comment_summary, &item.precedents]
        .into_iter()
        .flatten()
    {
        shown.material(&text.to_string());
    }
    if let Some(docket) = &item.docket {
        shown.material(&crate::context::docket_content(docket).to_string());
    }
    for SeatRead { read, .. } in reads {
        match read {
            ClerkRead::Comments { text, .. } => shown.material(text),
            ClerkRead::Proposal { content, .. } => shown.material(&content.to_string()),
            ClerkRead::Tool { result, .. } => shown.material(&result.to_string()),
        }
    }
    for round in rounds {
        if let Some(notes) = &round.steward_contribution {
            shown.material(notes);
        }
    }
    for round in rounds {
        for response in &round.responses {
            let by = format!("The {}, round {}", response.role, round.number);
            let rationale = response.rationale.to_string();
            for text in [&rationale, &response.position]
                .into_iter()
                .chain(&response.questions)
            {
                checked.extend(citations::check(&by, text, &shown));
            }
        }
    }
    checked
}

fn render_reads(reads: &[&SeatRead]) -> String {
    // A docket proposal is read the same way to every seat that asks
    // (the reader memoizes it), so it is written out once.
    let mut proposals_written = BTreeSet::new();
    let mut out = String::new();
    for SeatRead { round, role, read } in reads {
        let round = if *round == crate::deliberation::DETERMINATION_STAGE {
            "The recusal determination".to_string()
        } else {
            format!("Round {round}")
        };
        match read {
            ClerkRead::Comments { labels, text } => {
                let asked: Vec<String> = labels.iter().map(|l| format!("[{l}]")).collect();
                let _ = write!(
                    out,
                    "## {round} — the {role} asked for {}\n\n{text}\n\n",
                    asked.join(", ")
                );
            }
            ClerkRead::Proposal { number, content } => {
                let _ = write!(out, "## {round} — the {role} asked to read P{number}\n\n");
                if proposals_written.insert(*number) {
                    let _ = write!(out, "{content}\n\n");
                } else {
                    let _ = write!(out, "The same reading of P{number} as above.\n\n");
                }
            }
            ClerkRead::Tool {
                name,
                arguments,
                result,
                is_error,
            } => {
                let refused = if *is_error { ", and was refused" } else { "" };
                let _ = write!(
                    out,
                    "## {round} — the {role} called `{name}`{refused}\n\n\
                     Arguments: `{arguments}`\n\n"
                );
                // A witness's answer has one copy on the record: its
                // testimony (agora#351)
                let answered = (name == "ask_witness" && !is_error)
                    .then(|| arguments.get("witness").and_then(|w| w.as_str()))
                    .flatten()
                    .and_then(|w| w.parse::<agora_common::ids::WitnessId>().ok());
                match answered {
                    Some(witness) => {
                        let _ = write!(
                            out,
                            "Answered: the exchange is in `{}`.\n\n",
                            agora_common::govlog::testimony_attachment_name(&witness)
                        );
                    }
                    None => {
                        let _ = write!(out, "{result}\n\n");
                    }
                }
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DecisionCategory;
    use misanthropic::prompt::message::Content;

    fn item() -> AgendaItem {
        AgendaItem {
            id: agora_common::ids::AgendaItemId::from(uuid::Uuid::from_u128(1)),
            title: "t".into(),
            body: "b".into(),
            category: DecisionCategory::Policy,
            proposal_id: None,
            agent_name: None,
            score: None,
            priority: 0,
            comments: Default::default(),
            comment_summary: None,
            precedents: None,
            docket: None,
            appeal_id: None,
            deferral_spent: false,
            history: Vec::new(),
            obligations: Vec::new(),
            witnesses: Vec::new(),
        }
    }

    fn content(a: &CouncilAttachment) -> &str {
        a.content.value().unwrap()
    }

    fn row(id: &str, author: &str, body: &str) -> crate::queries::CommentRow {
        crate::queries::CommentRow {
            id: agora_common::ids::CommentId::from(uuid::Uuid::parse_str(id).unwrap()),
            parent_comment_id: None,
            author: author.into(),
            body: body.into(),
            created_at: chrono::Utc::now(),
        }
    }

    fn named<'a>(got: &'a [CouncilAttachment], name: &str) -> &'a str {
        content(
            got.iter()
                .find(|a| a.name == name)
                .unwrap_or_else(|| panic!("no {name}")),
        )
    }

    fn spoke(role: CouncilSeat, position: &str) -> crate::models::AgentResponse {
        crate::models::AgentResponse {
            role,
            position: position.into(),
            vote: Some(crate::models::CouncilVote::Yes),
            rationale: "Reasoning.".into(),
            questions: Vec::new(),
            ready_to_vote: true,
            ranking: None,
        }
    }

    #[test]
    fn nothing_shown_is_nothing_attached() {
        assert!(attachments(&item(), &[], &[], &BTreeMap::new()).is_empty());
    }

    /// agora#510: a scheduling item's record keeps the docket's seed and
    /// citation counts, which the seats were not shown.
    #[test]
    fn a_docket_is_attached_with_its_counts() {
        let (docket, post_id) = crate::councillor::harness_tests::one_candidate_docket();
        let mut item = item();
        item.docket = Some(docket.clone());
        let got = attachments(&item, &[], &[], &BTreeMap::new());
        let json = named(&got, "docket.json");
        assert_eq!(json, docket.record_json());
        let record: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(record["proposals"][0]["post_id"], post_id.to_string());
        assert_eq!(record["proposals"][0]["citations"], 3);
    }

    #[test]
    fn the_briefing_and_every_read_are_attached_verbatim() {
        let mut item = item();
        item.comments = crate::citations::ItemComments::new(vec![
            row("a1b2c3d4-0000-4000-8000-000000000000", "a", "for"),
            row("c3c3c3c3-0000-4000-8000-000000000000", "b", "against"),
        ]);
        item.comment_summary = Some(Content::from("SUMMARY: [a1b2c3d4] argues for it."));
        item.precedents = Some(Content::from("PRECEDENTS: GOV-2026-0005 applies."));
        let reads = [
            SeatRead {
                round: 2,
                role: CouncilSeat::Lawyer,
                read: ClerkRead::Comments {
                    labels: vec!["a1b2c3d4".into(), "c3c3c3c3".into()],
                    text: "[a1b2c3d4] a wrote:\nfor\n\n[c3c3c3c3] b wrote:\nagainst".into(),
                },
            },
            SeatRead {
                round: 2,
                role: CouncilSeat::Artist,
                read: ClerkRead::Proposal {
                    number: 4,
                    content: Content::from("P4 TEXT"),
                },
            },
            SeatRead {
                round: 3,
                role: CouncilSeat::Engineer,
                read: ClerkRead::Proposal {
                    number: 4,
                    content: Content::from("P4 TEXT"),
                },
            },
        ];
        let got = attachments(&item, &reads, &[], &BTreeMap::new());
        let names: Vec<_> = got.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "clerk-thread-summary.md",
                "clerk-precedents.md",
                "clerk-reads.md",
                "comment-ids.md"
            ]
        );
        assert!(content(&got[0]).contains("SUMMARY: [a1b2c3d4] argues for it."));
        assert!(content(&got[1]).contains("GOV-2026-0005 applies."));

        let reads = content(&got[2]);
        assert!(
            reads.contains("## Round 2 — the Lawyer asked for [a1b2c3d4], [c3c3c3c3]"),
            "{reads}"
        );
        assert!(reads.contains("[c3c3c3c3] b wrote:\nagainst"), "{reads}");
        assert!(
            reads.contains("## Round 3 — the Engineer asked to read P4"),
            "{reads}"
        );
        assert_eq!(reads.matches("P4 TEXT").count(), 1, "{reads}");
    }

    /// A seat's agent-tool calls go on the record in their own attachment:
    /// the seat, the round, the arguments as sent and the whole result as
    /// received, refusals included. The Clerk's reads stay in theirs.
    #[test]
    fn seat_tool_reads_are_attached_with_arguments_and_results() {
        let reads = [
            SeatRead {
                round: 1,
                role: CouncilSeat::Philosopher,
                read: ClerkRead::Tool {
                    name: "get_content".into(),
                    arguments: serde_json::json!({"id": "GOV-2026-0007"}),
                    result: Content::from("[The Agora server, answering…] RESULT BODY"),
                    is_error: false,
                },
            },
            SeatRead {
                round: 2,
                role: CouncilSeat::Engineer,
                read: ClerkRead::Tool {
                    name: "get_content".into(),
                    arguments: serde_json::json!({"id": "GOV-2026-0099"}),
                    result: Content::from("The server could not serve that read: no entry"),
                    is_error: true,
                },
            },
        ];
        let got = attachments(&item(), &reads, &[], &BTreeMap::new());
        let names: Vec<_> = got.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["seat-tool-reads.md"]);
        let text = content(&got[0]);
        assert!(
            text.contains("## Round 1 — the Philosopher called `get_content`\n\nArguments: `{\"id\":\"GOV-2026-0007\"}`"),
            "{text}"
        );
        assert!(text.contains("RESULT BODY"), "{text}");
        assert!(
            text.contains("## Round 2 — the Engineer called `get_content`, and was refused"),
            "{text}"
        );
        assert!(
            text.contains("could not serve that read: no entry"),
            "{text}"
        );
    }

    /// Every citation in the record is checked after the fact (#487): the
    /// Clerk's summary against the thread it read, a docket proposal's
    /// summary against its own thread, and each seat against what the
    /// Council was shown. What resolves goes in the id map; what does not
    /// is reported beside the text, which is never changed.
    #[test]
    fn citations_are_mapped_or_reported_and_never_rewritten() {
        let mut item = item();
        item.body = "Follows up post 60415774.".into();
        item.comments = crate::citations::ItemComments::new(vec![
            row("3fbc5dc1-d6f3-4808-bee9-12e78751be4d", "mantle", "First."),
            row("3fbc5dc1-d7aa-4808-bee9-12e78751be4d", "kiln", "Twin."),
            row(
                "a3dcbea6-0000-4000-8000-000000000000",
                "reactor",
                "Mentions b0b0b0b0.",
            ),
        ]);
        let summary = "[3fbc5dc1-d6] and [a3dcbea6] argue; [deadbee5] was never shown.";
        item.comment_summary = Some(Content::from(summary));

        // A docket proposal read mid-deliberation, labelled beside the
        // item's comments; its summary cites one of its own and one ghost.
        let p_rows = vec![row(
            "77777777-0000-4000-8000-000000000000",
            "solstice",
            "P3 point.",
        )];
        let p_labels = agora_common::clerk::CommentLabels::beside(&p_rows, &item.comments.labels);
        let mut p_shown = Shown::default().labels(&p_labels);
        p_shown.material("P3 body.");
        let p_checked = citations::check(
            "The Clerk's summary of P3's thread",
            "[77777777] says so; [1234abcd] does not exist.",
            &p_shown,
        );
        let readings = BTreeMap::from([(
            3,
            Reading {
                title: "Proposal three".into(),
                comments: p_rows,
                labels: p_labels,
                checked: p_checked,
            },
        )]);

        let rounds = [Round {
            number: 1,
            round_type: crate::models::RoundType::Independent,
            responses: vec![
                // Resolves; a P3 label; an id in the proposal body; an id a
                // commenter quoted (it reached the seats through ask_clerk
                // only if read, so here it is unknown to them).
                spoke(
                    CouncilSeat::Lawyer,
                    "Per [a3dcbea6] and [77777777]; see 60415774.",
                ),
                // The shared eight digits name two comments.
                spoke(CouncilSeat::Artist, "As [3fbc5dc1] said."),
                // Invented.
                spoke(CouncilSeat::Engineer, "Per [9abcdef0]."),
            ],
            steward_contribution: None,
            witness_questions: Vec::new(),
        }];

        let got = attachments(&item, &[], &rounds, &readings);
        assert_eq!(
            named(&got, "clerk-thread-summary.md"),
            summary,
            "the summary is attached exactly as emitted"
        );

        let map = named(&got, "comment-ids.md");
        assert!(map.contains("## This item's thread: t"), "{map}");
        assert!(
            map.contains("- `3fbc5dc1-d6` → 3fbc5dc1-d6f3-4808-bee9-12e78751be4d (mantle)"),
            "{map}"
        );
        assert!(
            map.contains("- `a3dcbea6` → a3dcbea6-0000-4000-8000-000000000000 (reactor)"),
            "{map}"
        );
        assert!(
            map.contains("## P3's thread, read with `read_proposal`: Proposal three"),
            "{map}"
        );
        assert!(map.contains("- `77777777` → 77777777-0000-4000-8000-000000000000 (solstice)"));
        assert!(
            !map.contains("kiln"),
            "an uncited comment is not mapped: {map}"
        );

        let warnings = named(&got, "citation-warnings.md");
        for expected in [
            "The Clerk's thread summary cites `deadbee5`: no comment shown",
            "The Clerk's summary of P3's thread cites `1234abcd`: no comment shown",
            "The Artist, round 1 cites `3fbc5dc1`: 2 comments shown start with it \
             (`3fbc5dc1-d6`, `3fbc5dc1-d7`)",
            "The Engineer, round 1 cites `9abcdef0`: no comment shown",
        ] {
            assert!(warnings.contains(expected), "{expected}\n\n{warnings}");
        }
        assert!(
            !warnings.contains("60415774"),
            "quoting the material is not a ghost"
        );
        assert!(!warnings.contains("Lawyer"), "{warnings}");
        assert_eq!(warnings.lines().filter(|l| l.starts_with("- ")).count(), 4);
    }
}
