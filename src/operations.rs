//! Changes to Council operations — the disclosure duty that comes with the
//! Steward's discretion over the Council's staff-work (2026-08-15): the
//! Clerk, context assembly, retrieval, this program.
//!
//! The log is `governance/council-operations.md`, version-controlled so
//! every entry is reviewable. Entries dated after the last sitting day open
//! every item of the next sitting, beside the date and the recent decisions.
//! That includes round 1: an entry describes the process every item runs
//! through, not an argument about any of them, so it does not compromise
//! round 1's independence the way a Steward note would.

use std::fmt;

use chrono::NaiveDate;

const LOG: &str = include_str!("../../../governance/council-operations.md");

/// One entry: a `## YYYY-MM-DD — title` heading and everything under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change<'a> {
    pub effective: NaiveDate,
    /// The whole entry, heading included, as it will be shown.
    pub text: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    heading: String,
    reason: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "governance/council-operations.md: {} in heading {:?}",
            self.reason, self.heading
        )
    }
}

impl std::error::Error for ParseError {}

/// Parse a log. Everything before the first `## ` heading is preamble and
/// is not shown. A heading that does not start with a date is an error, not
/// a skipped entry: a typo must fail the build, not silently withhold a
/// disclosure.
pub fn parse(log: &str) -> Result<Vec<Change<'_>>, ParseError> {
    // Byte offsets of each `## ` line; entries run from one to the next.
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in log.split_inclusive('\n') {
        if line.starts_with("## ") {
            starts.push(offset);
        }
        offset += line.len();
    }

    let mut changes: Vec<Change<'_>> = Vec::with_capacity(starts.len());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(log.len());
        let text = log[start..end].trim_end();
        let heading = text.lines().next().unwrap_or_default();
        let err = |reason| ParseError {
            heading: heading.to_string(),
            reason,
        };
        let date = heading
            .strip_prefix("## ")
            .and_then(|h| h.get(..10))
            .ok_or_else(|| err("no date"))?;
        let effective = NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .map_err(|_| err("not a YYYY-MM-DD date"))?;
        if changes
            .last()
            .is_some_and(|prev| prev.effective > effective)
        {
            return Err(err("out of order (entries run oldest first)"));
        }
        changes.push(Change { effective, text });
    }
    Ok(changes)
}

/// The entries the Council has not been shown: dated after the last day it
/// sat. `None` (no sitting on record) shows everything.
pub fn since(
    changes: &[Change<'static>],
    last_sitting_day: Option<NaiveDate>,
) -> Vec<Change<'static>> {
    changes
        .iter()
        .filter(|c| last_sitting_day.is_none_or(|last| c.effective > last))
        .copied()
        .collect()
}

/// This build's log. Parsed at every call; it is a few kilobytes.
pub fn log() -> Result<Vec<Change<'static>>, ParseError> {
    parse(LOG)
}

/// The block that opens each item, or `None` when nothing changed.
pub fn notice(changes: &[Change<'_>]) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    let mut s = String::from(
        "# Changes to Council Operations Since Your Last Sitting\n\n\
         The Steward reports these changes to how your sittings are run (the \
         Clerk, the material assembled for you, this program). They are \
         disclosed so that you know what changed, not put to you for a \
         vote. If one bears on an item before you, say so.\n\n",
    );
    for change in changes {
        // Demote each entry one level so it nests under the header above.
        s.push('#');
        s.push_str(change.text);
        s.push_str("\n\n");
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// The shipped log must parse, or `include_str!` would carry a
    /// disclosure the program then refuses to show.
    #[test]
    fn the_shipped_log_parses() {
        let changes = log().expect("governance/council-operations.md must parse");
        assert!(!changes.is_empty());
        assert!(changes[0].text.contains("Sonnet 5"));
    }

    #[test]
    fn preamble_is_not_an_entry() {
        let changes = parse("# Title\n\nintro\n\n## 2026-09-21 — A\n\nbody a\n").unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].effective, day("2026-09-21"));
        assert_eq!(changes[0].text, "## 2026-09-21 — A\n\nbody a");
    }

    #[test]
    fn entries_split_at_each_heading() {
        let changes = parse("## 2026-09-01 — A\na\n## 2026-09-02 — B\nb\n").unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[1].text, "## 2026-09-02 — B\nb");
    }

    #[test]
    fn an_undated_heading_is_an_error_not_a_skip() {
        assert!(parse("## Sonnet Clerk\nbody\n").is_err());
        assert!(parse("## 2026-9-21 — short month\nbody\n").is_err());
    }

    #[test]
    fn out_of_order_entries_are_rejected() {
        assert!(parse("## 2026-09-02 — B\n## 2026-09-01 — A\n").is_err());
    }

    #[test]
    fn since_shows_only_what_postdates_the_last_sitting_day() {
        let changes = parse("## 2026-09-07 — old\n## 2026-09-21 — new\n").unwrap();
        // Same day as the last sitting: already shown then.
        assert_eq!(since(&changes, Some(day("2026-09-07"))).len(), 1);
        assert_eq!(since(&changes, Some(day("2026-09-21"))).len(), 0);
        assert_eq!(since(&changes, None).len(), 2);
    }

    #[test]
    fn notice_nests_entries_under_its_header() {
        let changes = parse("## 2026-09-21 — A\nbody\n").unwrap();
        let s = notice(&changes).unwrap();
        assert!(s.starts_with("# Changes to Council Operations"));
        assert!(s.contains("\n### 2026-09-21 — A\nbody"));
        assert_eq!(notice(&[]), None);
    }
}
