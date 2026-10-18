//! Comment citations: labelled at prompt build, checked after emission,
//! never rewritten (#487).
//!
//! Every comment the Council is shown is labelled by its own id (see
//! [`CommentLabels`]), and the seats and the Clerk cite comments by those
//! labels. What they emit is the record, so nothing here edits it: a
//! citation that names no comment the Council was shown is *reported*, in
//! a warning attachment on the decision and a structured log event, and
//! the text stays exactly as emitted. Same principle as the redactor's
//! seal: catch, don't silently fix.
//!
//! Each decision also carries an id map — every label cited, with the full
//! id it names — so a citation resolves even if its eight hex digits later
//! stop being unique on the platform.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use agora_common::clerk::{CommentLabels, LabelMatch};
use agora_common::council::ThreadRole;
use agora_common::ids::CommentId;

use crate::queries::CommentRow;
pub use crate::threads::ItemThread;

/// The comments an item's seats can cite and have read back, with the
/// labels they were shown under. Labelled once, when the item is loaded,
/// and carried unchanged to the Clerk, every seat and the record.
///
/// Since #492 an item can carry several threads. `rows` holds all of
/// their comments, thread by thread, and the labels are built over all of
/// them at once, so no two threads can share one. `threads` says which
/// rows belong to which thread; it is empty only where no thread was
/// named (tests, and items that came from no post), and then every row is
/// the item's own.
#[derive(Debug, Clone, Default)]
pub struct ItemComments {
    pub rows: Vec<CommentRow>,
    pub labels: CommentLabels,
    pub threads: Vec<ItemThread>,
}

impl ItemComments {
    /// Label `rows`, all shown together, as one unnamed thread.
    pub fn new(rows: Vec<CommentRow>) -> Self {
        let labels = CommentLabels::new(&rows);
        Self {
            rows,
            labels,
            threads: Vec::new(),
        }
    }

    /// Several threads, each with its comments, labelled together. The
    /// order given is the order they are shown in; the primary comes first
    /// by convention.
    pub fn from_threads(threads: Vec<(ItemThread, Vec<CommentRow>)>) -> Self {
        let mut rows = Vec::new();
        let threads = threads
            .into_iter()
            .map(|(mut thread, comments)| {
                let start = rows.len();
                rows.extend(comments);
                thread.comments = start..rows.len();
                thread
            })
            .collect();
        let labels = CommentLabels::new(&rows);
        Self {
            rows,
            labels,
            threads,
        }
    }

    /// The comments of `thread`, one of [`Self::threads`].
    pub fn of(&self, thread: &ItemThread) -> &[CommentRow] {
        self.rows.get(thread.comments.clone()).unwrap_or_default()
    }

    /// The item's own thread's comments: what its docket cites from.
    pub fn primary(&self) -> &[CommentRow] {
        match self.threads.iter().find(|t| t.role == ThreadRole::Primary) {
            Some(thread) => self.of(thread),
            None if self.threads.is_empty() => &self.rows,
            None => &[],
        }
    }

    /// The thread comment `id` was shown in, when threads are named.
    pub fn thread_of(&self, id: CommentId) -> Option<&ItemThread> {
        let index = self.rows.iter().position(|row| row.id == id)?;
        self.threads.iter().find(|t| t.comments.contains(&index))
    }

    /// The threads beyond the item's own.
    pub fn related(&self) -> impl Iterator<Item = &ItemThread> {
        self.threads
            .iter()
            .filter(|t| t.role == ThreadRole::Related)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The comment `id`, if it was shown.
    pub fn get(&self, id: CommentId) -> Option<&CommentRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// The label `id` was shown under. A comment that was not labelled
    /// (never the case for rows built here) falls back to its full id.
    pub fn label(&self, id: CommentId) -> String {
        self.labels
            .label(id)
            .map_or_else(|| id.as_uuid().to_string(), str::to_string)
    }
}

impl From<Vec<CommentRow>> for ItemComments {
    fn from(rows: Vec<CommentRow>) -> Self {
        Self::new(rows)
    }
}

/// Every comment-id-shaped token in `text`, lowercased, in order, without
/// repeats.
///
/// A token is a run of word characters and hyphens (so `x3fbc5dc1` and
/// `3fbc5dc1_` are not ids) whose first hyphen-separated segment is 8 to
/// 32 hex digits; the hex segments after it are kept, so a lengthened
/// label (`3fbc5dc1-d`) or a full UUID comes back whole.
pub fn extract(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for token in lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        let mut segments = token.split('-');
        let Some(first) = segments.next() else {
            continue;
        };
        if !(8..=32).contains(&first.len()) || !is_hex(first) {
            continue;
        }
        let mut cited = first.to_string();
        for segment in segments {
            if segment.is_empty() || !is_hex(segment) {
                break;
            }
            cited.push('-');
            cited.push_str(segment);
        }
        if seen.insert(cited.clone()) {
            out.push(cited);
        }
    }
    out
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What one model was shown: the labelled comments it could cite, and
/// every other id-shaped token in its material (a post id in a proposal,
/// a comment id quoted by a commenter). Citing one of those is quoting
/// the material, not inventing a comment, so it is not reported.
#[derive(Debug, Default)]
pub struct Shown<'a> {
    labels: Vec<&'a CommentLabels>,
    /// First eight hex digits of every id-shaped token in the material.
    material: BTreeSet<String>,
}

impl<'a> Shown<'a> {
    pub fn labels(mut self, labels: &'a CommentLabels) -> Self {
        self.labels.push(labels);
        self
    }

    /// Add the ids that appear in `text`.
    pub fn material(&mut self, text: &str) {
        self.material
            .extend(extract(text).into_iter().map(|t| t[..8].to_string()));
    }

    /// Resolve `cited` against the labels: an exact label anywhere first,
    /// then a prefix of any shown comment's id. A token that matches
    /// nothing is retried without its trailing hyphen-segments, so a label
    /// glued to an English word (`3fbc5dc1-add`) still resolves.
    fn resolve(&self, cited: &str) -> LabelMatch {
        let mut candidate = cited;
        loop {
            if let Some(id) = self
                .labels
                .iter()
                .flat_map(|labels| labels.iter())
                .find_map(|(id, label)| (label == candidate).then_some(id))
            {
                return LabelMatch::One(id);
            }
            let mut hits: Vec<CommentId> = self
                .labels
                .iter()
                .filter_map(|labels| match labels.find(candidate) {
                    LabelMatch::One(id) => Some(vec![id]),
                    LabelMatch::Many(ids) => Some(ids),
                    LabelMatch::None => None,
                })
                .flatten()
                .collect();
            hits.sort_unstable();
            hits.dedup();
            match hits.len() {
                0 => {}
                1 => return LabelMatch::One(hits[0]),
                _ => return LabelMatch::Many(hits),
            }
            match candidate.rfind('-') {
                Some(cut) if cut >= 8 => candidate = &candidate[..cut],
                _ => return LabelMatch::None,
            }
        }
    }

    /// The label a shown comment was shown under.
    fn label(&self, id: CommentId) -> Option<&str> {
        self.labels.iter().find_map(|labels| labels.label(id))
    }
}

/// Why a citation did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// No comment shown has an id starting with it, and it is not an id
    /// that appeared in the material.
    Unknown,
    /// Several shown comments start with it: their labels, which tell them
    /// apart.
    Ambiguous(Vec<String>),
}

/// A citation that names no single comment the model was shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    /// Who emitted it, for the reader: "the Clerk's thread summary", "the
    /// Lawyer, round 2".
    pub by: String,
    /// As emitted, lowercased.
    pub cited: String,
    pub problem: Problem,
}

/// The result of checking emitted text.
#[derive(Debug, Clone, Default)]
pub struct Checked {
    /// Citations that name one shown comment.
    pub resolved: BTreeSet<CommentId>,
    pub unresolved: Vec<Unresolved>,
}

impl Checked {
    pub fn extend(&mut self, other: Checked) {
        self.resolved.extend(other.resolved);
        for u in other.unresolved {
            if !self.unresolved.contains(&u) {
                self.unresolved.push(u);
            }
        }
    }
}

/// Check the comment citations in `text`, emitted by `by`, against what it
/// was shown. Reports; never edits. Each unresolved citation is also a
/// `council_citation_unresolved` warning in the log.
///
/// A token neither shown as a comment nor present in the material is
/// reported only if it mixes digits and letters: an eight-digit number
/// (`20260929`) or an all-letter word is prose, not a citation.
pub fn check(by: &str, text: &str, shown: &Shown<'_>) -> Checked {
    let mut checked = Checked::default();
    for cited in extract(text) {
        let problem = match shown.resolve(&cited) {
            LabelMatch::One(id) => {
                checked.resolved.insert(id);
                continue;
            }
            LabelMatch::Many(ids) => Problem::Ambiguous(
                ids.into_iter()
                    .map(|id| {
                        shown
                            .label(id)
                            .map_or_else(|| id.as_uuid().to_string(), str::to_string)
                    })
                    .collect(),
            ),
            LabelMatch::None => {
                let first8 = &cited[..8];
                let mixed = first8.bytes().any(|b| b.is_ascii_digit())
                    && first8.bytes().any(|b| b.is_ascii_alphabetic());
                if shown.material.contains(first8) || !mixed {
                    continue;
                }
                Problem::Unknown
            }
        };
        tracing::warn!(
            event_type = "council_citation_unresolved",
            by,
            cited = %cited,
            problem = match &problem {
                Problem::Unknown => "unknown",
                Problem::Ambiguous(_) => "ambiguous",
            },
            "a cited comment id names no single comment shown; the text is kept as emitted"
        );
        checked.unresolved.push(Unresolved {
            by: by.to_string(),
            cited,
            problem,
        });
    }
    checked
}

/// Where a cited comment was shown, for the id map.
#[derive(Debug, Clone)]
pub struct Source<'a> {
    /// "this item's thread", "P3's thread (read with `read_proposal`)"
    pub thread: String,
    pub comments: &'a [CommentRow],
    pub labels: &'a CommentLabels,
}

/// The id map attachment's body: every cited label and the comment it
/// names, grouped by the thread it was shown in. Only what was cited: the
/// map is for resolving the record, and a thread of a thousand comments
/// would otherwise add its whole index to every decision.
pub fn render_id_map(cited: &BTreeSet<CommentId>, sources: &[Source<'_>]) -> Option<String> {
    let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for &id in cited {
        let Some((i, source, row)) = sources.iter().enumerate().find_map(|(i, s)| {
            s.comments
                .iter()
                .find(|row| row.id == id)
                .map(|row| (i, s, row))
        }) else {
            continue;
        };
        let label = source
            .labels
            .label(id)
            .map_or_else(|| id.as_uuid().to_string(), str::to_string);
        groups.entry(i).or_default().push(format!(
            "- `{label}` → {} ({})",
            id.as_uuid(),
            row.author
        ));
    }
    if groups.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, lines) in groups {
        let _ = write!(out, "## {}\n\n{}\n\n", sources[i].thread, lines.join("\n"));
    }
    Some(out.trim_end().to_string())
}

/// The warning attachment's body, or `None` when every citation resolved.
pub fn render_warnings(unresolved: &[Unresolved]) -> Option<String> {
    if unresolved.is_empty() {
        return None;
    }
    let mut out = String::from(
        "These citations in this record name no single comment shown on this \
         item. The record keeps them exactly as emitted.\n\n",
    );
    for u in unresolved {
        let _ = match &u.problem {
            Problem::Unknown => writeln!(
                out,
                "- {} cites `{}`: no comment shown has an id starting with it.",
                u.by, u.cited
            ),
            Problem::Ambiguous(labels) => writeln!(
                out,
                "- {} cites `{}`: {} comments shown start with it ({}).",
                u.by,
                u.cited,
                labels.len(),
                labels
                    .iter()
                    .map(|l| format!("`{l}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    Some(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn row(id: &str, author: &str) -> CommentRow {
        CommentRow {
            id: CommentId::from(uuid::Uuid::parse_str(id).unwrap()),
            parent_comment_id: None,
            author: author.into(),
            body: format!("{author} speaks."),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn extraction_finds_labels_ids_and_nothing_glued_to_a_word() {
        assert_eq!(
            extract(
                "Per [3FBC5DC1] and [3fbc5dc1-d], see a3dcbea6-d6f3-4808-bee9-12e78751be4d; \
                 not x3fbc5dc1, 3fbc5dc1_, abc1234 or GOV-2026-0006. Again: 3fbc5dc1."
            ),
            [
                "3fbc5dc1",
                "3fbc5dc1-d",
                "a3dcbea6-d6f3-4808-bee9-12e78751be4d"
            ]
        );
    }

    #[test]
    fn a_citation_resolves_to_the_comment_shown_or_is_reported() {
        let item = ItemComments::new(vec![
            row("3fbc5dc1-d6f3-4808-bee9-12e78751be4d", "a"),
            row("3fbc5dc1-d7aa-4808-bee9-12e78751be4d", "b"),
            row("11111111-0000-4000-8000-000000000000", "c"),
        ]);
        let mut shown = Shown::default().labels(&item.labels);
        shown.material("The proposal cites post 60415774 and 20260929.");

        let checked = check(
            "the Lawyer, round 1",
            "Per [3fbc5dc1-d6] and [11111111], [3fbc5dc1] is split, \
             [9abcdef0] is invented, 60415774 is the post, 20260929 a date, \
             and [11111111-add] is a label glued to a word.",
            &shown,
        );
        assert_eq!(
            checked.resolved,
            [item.rows[0].id, item.rows[2].id].into_iter().collect()
        );
        assert_eq!(
            checked.unresolved,
            [
                Unresolved {
                    by: "the Lawyer, round 1".into(),
                    cited: "3fbc5dc1".into(),
                    problem: Problem::Ambiguous(vec!["3fbc5dc1-d6".into(), "3fbc5dc1-d7".into()]),
                },
                Unresolved {
                    by: "the Lawyer, round 1".into(),
                    cited: "9abcdef0".into(),
                    problem: Problem::Unknown,
                },
            ]
        );

        let warnings = render_warnings(&checked.unresolved).unwrap();
        assert!(
            warnings.contains("`9abcdef0`: no comment shown"),
            "{warnings}"
        );
        assert!(
            warnings.contains("2 comments shown start with it (`3fbc5dc1-d6`, `3fbc5dc1-d7`)"),
            "{warnings}"
        );
        assert_eq!(render_warnings(&[]), None);
    }

    #[test]
    fn the_id_map_lists_only_what_was_cited() {
        let item = ItemComments::new(vec![
            row("3fbc5dc1-d6f3-4808-bee9-12e78751be4d", "mantle"),
            row("11111111-0000-4000-8000-000000000000", "reactor"),
        ]);
        let cited = [item.rows[1].id].into_iter().collect();
        let map = render_id_map(
            &cited,
            &[Source {
                thread: "This item's thread".into(),
                comments: &item.rows,
                labels: &item.labels,
            }],
        )
        .unwrap();
        assert_eq!(
            map,
            "## This item's thread\n\n\
             - `11111111` → 11111111-0000-4000-8000-000000000000 (reactor)"
        );
        assert_eq!(render_id_map(&BTreeSet::new(), &[]), None);
    }
}
