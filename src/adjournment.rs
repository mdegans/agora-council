//! The meeting summary at adjournment, composed from its decisions'
//! signed summaries (Steward, 2026-10-17).
//!
//! From 2026-09-25 the Lawyer wrote a separate meeting summary at
//! adjournment, reviewed by the Steward (and before that the Clerk wrote it
//! alone, with factual errors Protocol § 5 records). Since 2026-10-17 each
//! decision carries its own summary, the Lawyer's, accepted and signed with
//! the record ([`crate::summary`]), and there is **one summary per
//! decision**: the meeting's is not written by anyone. It is composed here,
//! mechanically, from what was signed: each decision's title, citation id,
//! summary and who accepted it, in the order heard. Nothing is sent to a
//! model, so nothing here can be refused, and nothing here is reviewed:
//! every word in it was reviewed and signed already.
//!
//! The text is stored on the meeting row, as the Lawyer's was, for readers
//! of `council_meetings.summary`; the server serves the same summaries
//! structured, as `CouncilMeetingResponse.decision_summaries`, from the
//! signed records themselves (revisions applied).

use std::fmt::Write as _;

use agora_common::govlog::DecisionSummary;
use agora_common::ids::GovernanceLogId;

/// One decision of the sitting, as the meeting summary lists it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecidedItem {
    pub id: GovernanceLogId,
    pub title: String,
    /// The summary signed with the record; `None` when it was signed
    /// without one
    pub summary: Option<DecisionSummary>,
}

/// The meeting summary: the sitting's decisions' signed summaries, in the
/// order heard, each with its title, id and who accepted it
pub fn compose(meeting_date: chrono::NaiveDate, decided: &[DecidedItem]) -> String {
    let mut s = format!(
        "The Council sat on {meeting_date} and decided {} item(s). Each summary below \
         is the decision's own, written by the Lawyer and signed with its record; the \
         record is authoritative.\n",
        decided.len()
    );
    for item in decided {
        let _ = write!(s, "\n## {}: {}\n\n", item.id, item.title.trim());
        match &item.summary {
            Some(summary) => {
                let _ = write!(
                    s,
                    "{}\n\n*{}.*\n",
                    summary.served_text().trim(),
                    capitalized(&summary.authorship().sentence())
                );
            }
            None => {
                let _ = writeln!(
                    s,
                    "*No summary was signed with this decision; its record is the account \
                     (get_content(\"{}\")).*",
                    item.id
                );
            }
        }
    }
    s
}

fn capitalized(sentence: &str) -> String {
    let mut chars = sentence.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> GovernanceLogId {
        format!("GOV-2026-{n:04}").parse().unwrap()
    }

    /// The meeting summary is its decisions' signed summaries, in order,
    /// each with who accepted it, and nothing the program did not take
    /// from a signed record
    #[test]
    fn the_meeting_summary_is_composed_from_the_decisions() {
        let decided = [
            DecidedItem {
                id: id(19),
                title: "Define unanimous".into(),
                summary: Some(DecisionSummary::by_lawyer("Approved 5-0.", false)),
            },
            DecidedItem {
                id: id(20),
                title: "A recused item".into(),
                summary: Some(DecisionSummary::by_lawyer("Rejected 1-3.", true)),
            },
            DecidedItem {
                id: id(21),
                title: "Unagreed".into(),
                summary: None,
            },
        ];
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let text = compose(date, &decided);
        assert!(text.starts_with("The Council sat on 2026-10-17 and decided 3 item(s)."));
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in {text}"))
        };
        assert!(at("## GOV-2026-0019: Define unanimous") < at("Approved 5-0."));
        assert!(at("Approved 5-0.") < at("*Written by the Lawyer, accepted by the Steward.*"));
        assert!(
            at("*Written by the Lawyer, accepted by the Steward.*")
                < at("## GOV-2026-0020: A recused item")
        );
        at(
            "*Written by the Lawyer, accepted by the Artist, Philosopher and Engineer, the \
            Steward being recused.*",
        );
        assert!(at("## GOV-2026-0021: Unagreed") < at("No summary was signed"));
        assert!(text.contains("get_content(\"GOV-2026-0021\")"));
    }
}
