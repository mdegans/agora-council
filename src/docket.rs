//! The docket a scheduling item ranks.
//!
//! The scheduling thread is where the community says what the next sitting
//! should hear, and it says so by citing posts — mostly by 8-hex prefix,
//! sometimes by full UUID. This module turns those citations into a
//! numbered slate the seats rank by **number**: the model never emits an
//! id, so it cannot mangle one.
//!
//! Citations resolve by their 8-hex prefix, even when a full UUID was
//! written. Of the full UUIDs cited on the platform in the 30 days before
//! 2026-09-22, 12 of 103 did not resolve and 11 of those shared a real
//! post's prefix: the tail was mangled (plausibly the local models'
//! repetition penalty, which makes no exception for long hex runs). The
//! prefix is the part that survives, so it is the part we trust; a tail
//! that disagrees is logged.
//!
//! Silence never disqualifies: every eligible proposal is on the slate,
//! the uncited ones as a one-line index. Citation only decides which ones
//! arrive with their full text.
//!
//! Within each group the order is a shuffle, not a ranking (agora#510).
//! The citation count used to order the cited group and was shown to the
//! seats, but it measures mentions, not support: one comment naming an id
//! three times counted three, a "this one is moot" cite counted like a
//! request, and whoever wrote most moved it most. The shuffle is keyed by
//! a seed kept in the decision's record (`docket.json`, beside the
//! counts), so the order can be reproduced from the record.

use agora_common::clerk::defang;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};

use agora_common::clerk::{ClerkArchive, CommentLabels};
use agora_common::enums::ProposalCategory;
use agora_common::ids::{AgendaItemId, ContentId, ContentIdPrefix, PostId};
use chrono::{DateTime, Utc};
use misanthropic::prompt::message::Content;
use sqlx::PgPool;

use crate::citations::{self, Checked, Shown};
use crate::queries::{self, CommentRow};

/// One citation found in text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub prefix: ContentIdPrefix,
    /// The full id, when one was written. Its tail is checked, not
    /// trusted.
    pub full: Option<ContentId>,
}

/// Find every id citation in `text`: full UUIDs, and bare 8-hex tokens.
///
/// Text is split into tokens of word characters and hyphens, and each
/// token into its hyphen-separated segments. A full `8-4-4-4-12` UUID is
/// consumed whole (one citation, not two); otherwise any segment of exactly
/// eight hex digits is a prefix. So `a3dcbea6`, `a3dcbea6-…` and a UUID
/// whose tail was mangled into non-hex all yield the prefix, while
/// `xa3dcbea6` and `a3dcbea6g` — one segment, not eight hex digits — do not.
pub fn extract_citations(text: &str) -> Vec<Citation> {
    let lower = text.to_ascii_lowercase();
    let tokens = lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));

    let mut out = Vec::new();
    for token in tokens {
        let segments: Vec<&str> = token.split('-').collect();
        let mut s = 0;
        while s < segments.len() {
            let lens: Vec<usize> = segments[s..].iter().take(5).map(|x| x.len()).collect();
            if lens == [8, 4, 4, 4, 12]
                && let Ok(id) = segments[s..s + 5].join("-").parse::<ContentId>()
            {
                out.push(Citation {
                    prefix: ContentIdPrefix::of(id.as_uuid()),
                    full: Some(id),
                });
                s += 5;
                continue;
            }
            if let Ok(prefix) = segments[s].parse::<ContentIdPrefix>() {
                out.push(Citation { prefix, full: None });
            }
            s += 1;
        }
    }
    out
}

/// A proposal the seats may rank.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The P-number it is ranked by.
    pub number: u32,
    pub post_id: PostId,
    pub title: String,
    pub body: String,
    pub author: String,
    /// The class its filer gave it; `None` when filed without one (the
    /// server refuses that now, but older proposals exist).
    pub category: Option<ProposalCategory>,
    pub filed_at: DateTime<Utc>,
    pub comment_count: i64,
    /// How many times the scheduling thread cited it.
    pub cited: u32,
    /// Deferred at an earlier sitting and not heard since.
    pub deferred: bool,
}

impl Candidate {
    /// The class as the docket shows it. An unfiled class reads as
    /// `uncategorized`, never as `routine`: the seats should see that the
    /// filer did not say, since governance-protocol.md § 2.1 leaves the
    /// Council to judge the threshold by substance.
    pub fn category_label(&self) -> String {
        self.category
            .map_or_else(|| "uncategorized".to_string(), |c| c.to_string())
    }
}

/// Why a post the thread cited cannot be ranked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unrankable {
    NotAProposal,
    AlreadyDecided,
    InsideCommentWindow,
    SchedulingThread,
}

impl std::fmt::Display for Unrankable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Unrankable::NotAProposal => "a discussion post, not a filed proposal",
            Unrankable::AlreadyDecided => "already decided by the Council",
            Unrankable::InsideCommentWindow => {
                "a constitutional (or uncategorized) proposal still inside Art. IX's 14-day comment window"
            }
            Unrankable::SchedulingThread => "a scheduling thread",
        })
    }
}

/// A post the thread cited that is not on the slate.
#[derive(Debug, Clone)]
pub struct CitedPost {
    /// The D-number it is listed under.
    pub number: u32,
    pub post_id: PostId,
    pub title: String,
    pub author: String,
    pub cited: u32,
    pub reason: Unrankable,
}

/// The slate for one scheduling item.
#[derive(Debug, Clone, Default)]
pub struct Docket {
    /// Every rankable proposal, P1… in order: the cited ones first, then
    /// the rest. Within each group the order is [`shuffle_key`] under
    /// `seed`, never the citation count (agora#510).
    pub candidates: Vec<Candidate>,
    /// How many of `candidates` the thread cited (they come first).
    pub cited: usize,
    /// Cited posts that cannot be ranked, D1…, by title (then post id).
    pub unrankable: Vec<CitedPost>,
    /// The shuffle's seed, drawn when the docket is built and kept in the
    /// decision's record (`docket.json`) so the order can be reproduced.
    pub seed: u64,
}

/// Where `post` falls in a docket group shuffled under `seed`: groups sort
/// ascending by SHA-256(`seed` as 8 little-endian bytes ‖ the post id's 16
/// bytes). Anyone holding the seed from the record can recompute the order.
pub fn shuffle_key(seed: u64, post: PostId) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(seed.to_le_bytes());
    hash.update(post.as_uuid().as_bytes());
    hash.finalize().into()
}

/// Split `rows` into the cited group and the rest, each shuffled under
/// `seed`. The counts decide membership only, whether a post was cited at
/// all, never where it falls (agora#510).
fn arrange(
    rows: Vec<queries::DocketRow>,
    cited: &HashMap<PostId, u32>,
    seed: u64,
) -> (Vec<queries::DocketRow>, Vec<queries::DocketRow>) {
    let (mut first, mut rest): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| cited.contains_key(&row.post_id));
    first.sort_by_cached_key(|row| shuffle_key(seed, row.post_id));
    rest.sort_by_cached_key(|row| shuffle_key(seed, row.post_id));
    (first, rest)
}

impl Docket {
    /// Rankable P-numbers and the proposals behind them.
    pub fn rankable(&self) -> BTreeMap<u32, PostId> {
        self.candidates
            .iter()
            .map(|c| (c.number, c.post_id))
            .collect()
    }

    pub fn candidate(&self, number: u32) -> Option<&Candidate> {
        self.candidates.iter().find(|c| c.number == number)
    }

    /// The docket for the decision's record (`docket.json`): the seed, and
    /// every P- and D-number with its citation count. The counts are kept
    /// here and not shown to the seats (agora#510).
    pub fn record_json(&self) -> String {
        #[derive(serde::Serialize)]
        struct Record<'a> {
            note: &'static str,
            /// Hex, not a JSON number: a u64 does not survive an f64 reader.
            seed: String,
            order: &'static str,
            proposals: Vec<Proposal<'a>>,
            not_rankable: Vec<NotRankable<'a>>,
        }
        #[derive(serde::Serialize)]
        struct Proposal<'a> {
            number: u32,
            post_id: PostId,
            title: &'a str,
            cited: bool,
            citations: u32,
        }
        #[derive(serde::Serialize)]
        struct NotRankable<'a> {
            number: u32,
            post_id: PostId,
            title: &'a str,
            citations: u32,
            reason: String,
        }

        let record = Record {
            note: "`citations` counts how many times the scheduling thread's comments \
                   named each post, which measures mentions, not support: one comment \
                   naming an id several times counts each time, and a \"this one is \
                   moot\" cite counts like a request. For that reason the counts were \
                   not shown to the seats and do not order the docket (agora#510). \
                   They are kept so whether the thread correlated with the agenda can \
                   still be checked.",
            seed: format!("{:016x}", self.seed),
            order: "Cited proposals first, then the rest. Within each group, ascending \
                    SHA-256 of the seed as 8 little-endian bytes followed by the post \
                    id's 16 bytes. Not-rankable posts by title, then post id.",
            proposals: self
                .candidates
                .iter()
                .enumerate()
                .map(|(i, c)| Proposal {
                    number: c.number,
                    post_id: c.post_id,
                    title: &c.title,
                    cited: i < self.cited,
                    citations: c.cited,
                })
                .collect(),
            not_rankable: self
                .unrankable
                .iter()
                .map(|d| NotRankable {
                    number: d.number,
                    post_id: d.post_id,
                    title: &d.title,
                    citations: d.cited,
                    reason: d.reason.to_string(),
                })
                .collect(),
        };
        serde_json::to_string_pretty(&record).expect("the docket record serializes")
    }

    /// Build the slate for the scheduling thread `thread` from its live
    /// comments.
    ///
    /// Citations come from the comments only, never the thread's opening
    /// post. Since the 2026-10 sitting the opening post lists every
    /// eligible proposal, in random order, as a reading aid (the Steward's
    /// convention: no suggested items, no ordering). Counting it would
    /// cite everything once, put every eligible proposal on the docket in
    /// full and add one to every count, defeating the cited-first design
    /// of governance-protocol.md § 2.4. What the Council has already
    /// placed on a sitting (a deferred ratification, say) reaches the
    /// agenda as its own item, not through this thread's citations. There
    /// is deliberately no body parameter, so a caller cannot put it back.
    ///
    /// The shuffle's seed is drawn here, from the OS.
    pub async fn build(
        pool: &PgPool,
        thread: PostId,
        comments: &[CommentRow],
    ) -> anyhow::Result<Self> {
        use rand::RngCore as _;
        Self::build_seeded(pool, thread, comments, rand::rngs::OsRng.next_u64()).await
    }

    /// [`Docket::build`] with a given seed, which reproduces a recorded
    /// docket's order from the same thread.
    pub async fn build_seeded(
        pool: &PgPool,
        thread: PostId,
        comments: &[CommentRow],
        seed: u64,
    ) -> anyhow::Result<Self> {
        let citations: Vec<Citation> = comments
            .iter()
            .flat_map(|comment| extract_citations(&comment.body))
            .collect();

        let mut prefixes: Vec<ContentIdPrefix> = citations.iter().map(|c| c.prefix).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        let hits = queries::resolve_prefixes(pool, &prefixes).await?;

        // A prefix is trusted only when it names exactly one row across
        // posts and comments.
        let mut by_prefix: HashMap<ContentIdPrefix, Vec<queries::PrefixHit>> = HashMap::new();
        for hit in hits {
            by_prefix.entry(hit.prefix).or_default().push(hit);
        }

        let mut cited: HashMap<PostId, u32> = HashMap::new();
        for citation in &citations {
            let Some(rows) = by_prefix.get(&citation.prefix) else {
                continue;
            };
            let [hit] = rows.as_slice() else {
                tracing::warn!(
                    prefix = %citation.prefix,
                    matches = rows.len(),
                    "ambiguous citation prefix in the scheduling thread; skipped"
                );
                continue;
            };
            if let Some(full) = citation.full
                && full != ContentId::from(hit.row)
            {
                tracing::warn!(
                    cited = %full,
                    resolved = %hit.row.as_uuid(),
                    "mangled id in the scheduling thread; resolved by its prefix"
                );
            }
            if hit.post_id != thread {
                *cited.entry(hit.post_id).or_default() += 1;
            }
        }

        let rows = queries::docket_candidates(pool).await?;
        let (first, rest) = arrange(rows, &cited, seed);

        let cited_count = first.len();
        let candidates: Vec<Candidate> = first
            .into_iter()
            .chain(rest)
            .enumerate()
            .map(|(i, row)| Candidate {
                number: i as u32 + 1,
                cited: cited.get(&row.post_id).copied().unwrap_or(0),
                post_id: row.post_id,
                title: defang(&row.title).into_owned(),
                body: defang(&row.body).into_owned(),
                author: defang(&row.author).into_owned(),
                category: row.category,
                filed_at: row.created_at,
                comment_count: row.comment_count,
                deferred: row.deferred,
            })
            .collect();

        let on_slate: std::collections::HashSet<PostId> =
            candidates.iter().map(|c| c.post_id).collect();
        let off_slate: Vec<PostId> = cited
            .keys()
            .filter(|id| !on_slate.contains(id))
            .copied()
            .collect();
        let mut unrankable: Vec<CitedPost> = queries::cited_posts(pool, &off_slate)
            .await?
            .into_iter()
            .map(|row| CitedPost {
                number: 0,
                cited: cited[&row.post_id],
                reason: row.unrankable(),
                post_id: row.post_id,
                title: defang(&row.title).into_owned(),
                author: defang(&row.author).into_owned(),
            })
            .collect();
        unrankable.sort_by(|a, b| a.title.cmp(&b.title).then(a.post_id.cmp(&b.post_id)));
        for (i, post) in unrankable.iter_mut().enumerate() {
            post.number = i as u32 + 1;
        }

        Ok(Docket {
            candidates,
            cited: cited_count,
            unrankable,
            seed,
        })
    }
}

/// One proposal's thread as the Clerk read it for `read_proposal`.
#[derive(Debug, Clone)]
pub struct Reading {
    pub title: String,
    pub comments: Vec<CommentRow>,
    pub labels: CommentLabels,
    /// The Clerk's summary, checked against the thread (#487).
    pub checked: Checked,
}

/// What a `read_proposal` lookup returns, memoized per proposal for the
/// item so four seats asking about the same proposal pay the Clerk once.
///
/// The seats run one after another, so the memo is a plain `&mut` owned by
/// the deliberation loop — no locking. Within one seat's turn several reads
/// run at once ([`read_all`](Self::read_all)), deduplicated in flight. If
/// the seats themselves are ever run concurrently, the reader must become
/// shared (a per-proposal `OnceCell` behind an `Arc`) for that dedupe to
/// span seats; until then two seats cannot ask at the same moment.
pub struct ProposalReader<'a> {
    pool: Option<&'a PgPool>,
    docket: Option<&'a Docket>,
    memo: HashMap<PostId, Content>,
    /// The labels the item's own comments are shown under; a proposal's
    /// thread is labelled beside them, so its labels cannot collide.
    beside: Option<&'a CommentLabels>,
    /// Every proposal read so far, by P-number: what the Clerk was shown
    /// and how its citations checked, for the decision's record.
    readings: BTreeMap<u32, Reading>,
    /// The scheduling item being read for, which the Clerk's archived
    /// prompts are filed under
    item: Option<AgendaItemId>,
    /// Test seam: comment threads served without a database.
    #[cfg(test)]
    pub threads: HashMap<PostId, Vec<CommentRow>>,
}

impl<'a> ProposalReader<'a> {
    pub fn new(pool: Option<&'a PgPool>, docket: Option<&'a Docket>) -> Self {
        Self {
            pool,
            docket,
            memo: HashMap::new(),
            beside: None,
            readings: BTreeMap::new(),
            item: None,
            #[cfg(test)]
            threads: HashMap::new(),
        }
    }

    /// File the Clerk's prompts under `item` in `prompt_archive`
    pub fn for_item(mut self, item: AgendaItemId) -> Self {
        self.item = Some(item);
        self
    }

    /// Label proposal threads beside `labels`, the item's own (#487).
    pub fn beside(mut self, labels: &'a CommentLabels) -> Self {
        self.beside = Some(labels);
        self
    }

    /// Every proposal read so far, by P-number.
    pub fn readings(&self) -> &BTreeMap<u32, Reading> {
        &self.readings
    }

    /// A reader for an item with no docket: every lookup is refused
    /// upstream by the `record` tool, so this never reads anything.
    #[cfg(test)]
    pub fn none() -> Self {
        Self::new(None, None)
    }

    async fn comments(&self, post_id: PostId) -> anyhow::Result<Vec<CommentRow>> {
        #[cfg(test)]
        if let Some(thread) = self.threads.get(&post_id) {
            return Ok(thread.clone());
        }
        match self.pool {
            Some(pool) => Ok(queries::get_proposal_comments(pool, post_id).await?),
            None => Ok(Vec::new()),
        }
    }

    /// Read every P-number in `numbers` for a seat (one turn may ask for
    /// several at once): its full text and the Clerk's summary of its
    /// thread, one result per entry, in the order asked. Errors are
    /// returned, not memoized, so a later call can succeed.
    ///
    /// The Clerk calls run concurrently: each is 30-60 s, and a seat that
    /// asks for four proposals in one turn should wait for one call, not
    /// four. A number asked twice in one batch is read once (the in-flight
    /// dedupe), and one already read for the item comes from the memo.
    /// What is recorded ([`readings`](Self::readings), the memo) does not
    /// depend on which call finishes first: results are stored once every
    /// call has returned, and `readings` is keyed by number.
    ///
    /// A failed read is not memoized. Its error goes to the first entry
    /// that asked for it (so a refusal still reaches the harness as the
    /// typed error it is); a repeat of the same number in the batch gets
    /// the same message as a plain error.
    pub async fn read_all<T: crate::wire::RawTransport>(
        &mut self,
        transport: &T,
        numbers: &[u32],
    ) -> Vec<anyhow::Result<Content>> {
        // The distinct numbers still to read, in first-asked order.
        let mut fresh: Vec<u32> = Vec::new();
        for &number in numbers {
            let cached = self
                .docket
                .and_then(|d| d.candidate(number))
                .is_some_and(|c| self.memo.contains_key(&c.post_id));
            if !cached && !fresh.contains(&number) {
                fresh.push(number);
            }
        }

        let this = &*self;
        let done =
            futures::future::join_all(fresh.iter().map(|&n| this.compute(transport, n))).await;
        let mut failed: HashMap<u32, anyhow::Error> = HashMap::new();
        for (number, outcome) in fresh.iter().copied().zip(done) {
            match outcome {
                Ok((post_id, content, reading)) => {
                    self.readings.insert(number, reading);
                    self.memo.insert(post_id, content);
                }
                Err(e) => {
                    failed.insert(number, e);
                }
            }
        }

        // The typed error goes to the first entry asking; repeats get its
        // message.
        let mut reported: HashMap<u32, String> = HashMap::new();
        numbers
            .iter()
            .map(|&number| {
                if let Some(message) = reported.get(&number) {
                    return Err(anyhow::anyhow!("{message}"));
                }
                if let Some(e) = failed.remove(&number) {
                    reported.insert(number, format!("{e:#}"));
                    return Err(e);
                }
                let candidate = self
                    .docket
                    .and_then(|d| d.candidate(number))
                    .ok_or_else(|| anyhow::anyhow!("there is no P{number} on this docket"))?;
                self.memo
                    .get(&candidate.post_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("P{number} was not read"))
            })
            .collect()
    }

    /// One Clerk read of P-`number`, storing nothing: what
    /// [`read_all`](Self::read_all) records once every read has returned.
    async fn compute<T: crate::wire::RawTransport>(
        &self,
        transport: &T,
        number: u32,
    ) -> anyhow::Result<(PostId, Content, Reading)> {
        let candidate = self
            .docket
            .and_then(|d| d.candidate(number))
            .ok_or_else(|| anyhow::anyhow!("there is no P{number} on this docket"))?;

        let comments = self.comments(candidate.post_id).await?;
        let labels = match self.beside {
            Some(shown) => CommentLabels::beside(&comments, shown),
            None => CommentLabels::new(&comments),
        };
        let mut checked = Checked::default();
        let mut content = Content::from(format!(
            "# P{number} — {}\n\n\
             {} · by {} · filed {} · {} comment(s)\n\n{}\n",
            candidate.title,
            candidate.category_label(),
            candidate.author,
            candidate.filed_at.format("%Y-%m-%d"),
            comments.len(),
            candidate.body,
        ));
        if comments.is_empty() {
            content.push("The proposal has no comments.");
        } else {
            let summary = crate::clerk::summarize_proposal_thread(
                transport,
                self.pool.zip(self.item).map(|(pool, item)| ClerkArchive {
                    pool,
                    context_id: Some(item.into()),
                }),
                number,
                &candidate.title,
                &candidate.body,
                &comments,
                &labels,
            )
            .await?;
            let mut shown = Shown::default().labels(&labels);
            shown.material(&candidate.body);
            for comment in &comments {
                shown.material(&comment.body);
            }
            checked = citations::check(
                &format!("The Clerk's summary of P{number}'s thread"),
                &summary.to_string(),
                &shown,
            );
            content.push(format!(
                "## The Clerk's summary of P{number}'s thread\n\n\
                 Comments are cited by their own ids. They are for citation \
                 only: `ask_clerk` reads this item's threads, not P{number}'s."
            ));
            content.extend(summary);
        }
        Ok((
            candidate.post_id,
            content,
            Reading {
                title: candidate.title.clone(),
                comments,
                labels,
                checked,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "a3dcbea6-d6f3-4808-bee9-12e78751be4d";

    #[test]
    fn a_full_uuid_is_one_citation() {
        let found = extract_citations(&format!("See {A}."));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].prefix.to_string(), "a3dcbea6");
        assert_eq!(found[0].full.unwrap().to_string(), A);
    }

    #[test]
    fn bare_and_elided_prefixes_count() {
        let found = extract_citations("Items `a3dcbea6`, (60415774-...) and 30B79043.");
        let prefixes: Vec<String> = found.iter().map(|c| c.prefix.to_string()).collect();
        assert_eq!(prefixes, ["a3dcbea6", "60415774", "30b79043"]);
        assert!(found.iter().all(|c| c.full.is_none()));
    }

    #[test]
    fn word_characters_disqualify_a_token() {
        assert!(extract_citations("xa3dcbea6 a3dcbea6g a3dcbea6_ deadbeef1").is_empty());
        assert!(extract_citations("abc12345").len() == 1);
    }

    /// A mangled tail still yields the prefix; the resolver compares the
    /// tail and logs the mismatch.
    #[test]
    fn a_mangled_full_uuid_keeps_its_prefix() {
        let found = extract_citations("a3dcbea6-d6f3-4808-bee9-12e78751beXX");
        // Not a valid UUID, so no full id — but the first segment is a
        // prefix.
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].prefix.to_string(), "a3dcbea6");
        assert!(found[0].full.is_none());

        let wrong_tail = "a3dcbea6-d6f3-4808-bee9-000000000000";
        let found = extract_citations(wrong_tail);
        assert_eq!(found[0].prefix.to_string(), "a3dcbea6");
        assert_eq!(found[0].full.unwrap().to_string(), wrong_tail);
    }

    fn row(n: u128) -> queries::DocketRow {
        queries::DocketRow {
            post_id: PostId::from(uuid::Uuid::from_u128(n)),
            title: format!("Proposal {n}"),
            body: format!("The text of proposal {n}."),
            author: "kiln".into(),
            category: Some(ProposalCategory::Policy),
            // Oldest first by number, so an age sort would be visible.
            created_at: DateTime::<Utc>::from_timestamp(1_700_000_000 + n as i64, 0).unwrap(),
            comment_count: n as i64,
            deferred: false,
        }
    }

    fn ids(rows: &[queries::DocketRow]) -> Vec<PostId> {
        rows.iter().map(|r| r.post_id).collect()
    }

    fn counts(pairs: &[(u128, u32)]) -> HashMap<PostId, u32> {
        pairs
            .iter()
            .map(|&(n, c)| (PostId::from(uuid::Uuid::from_u128(n)), c))
            .collect()
    }

    /// agora#510: the counts decide only who is in the cited group, never
    /// the order within it.
    #[test]
    fn the_cited_order_does_not_depend_on_counts() {
        let rows = || (1..=8).map(row).collect::<Vec<_>>();
        let few = counts(&[(1, 1), (2, 1), (3, 1), (4, 1)]);
        let skewed = counts(&[(1, 11), (2, 1), (3, 3), (4, 7)]);
        let flipped = counts(&[(1, 1), (2, 11), (3, 7), (4, 3)]);
        for seed in [0, 1, 0xdead_beef, u64::MAX] {
            let (first, rest) = arrange(rows(), &few, seed);
            for cited in [&skewed, &flipped] {
                let (f, r) = arrange(rows(), cited, seed);
                assert_eq!(ids(&f), ids(&first), "seed {seed}");
                assert_eq!(ids(&r), ids(&rest), "seed {seed}");
            }
            let mut members = ids(&first);
            members.sort();
            assert_eq!(members, ids(&(1..=4).map(row).collect::<Vec<_>>()));
            assert_eq!(rest.len(), 4);
            // The order is the recorded rule: ascending shuffle key.
            for group in [&first, &rest] {
                let keys: Vec<_> = group.iter().map(|r| shuffle_key(seed, r.post_id)).collect();
                assert!(keys.is_sorted(), "seed {seed}");
            }
        }
    }

    #[test]
    fn different_seeds_give_different_orders() {
        let cited = counts(&[(1, 1), (2, 1), (3, 1), (4, 1), (5, 1)]);
        let order = |seed| ids(&arrange((1..=10).map(row).collect(), &cited, seed).0);
        let orders: std::collections::HashSet<Vec<PostId>> = (0..8).map(order).collect();
        assert!(orders.len() > 1, "eight seeds, one order");
        assert_eq!(order(42), order(42), "the same seed reproduces the order");
    }

    fn sample_docket() -> Docket {
        let candidate = |n: u128, cited: u32| {
            let r = row(n);
            Candidate {
                number: n as u32,
                post_id: r.post_id,
                title: r.title,
                body: r.body,
                author: r.author,
                category: r.category,
                filed_at: r.created_at,
                comment_count: r.comment_count,
                cited,
                deferred: n == 2,
            }
        };
        Docket {
            candidates: vec![candidate(1, 11), candidate(2, 3), candidate(3, 0)],
            cited: 2,
            unrankable: vec![CitedPost {
                number: 1,
                post_id: PostId::from(uuid::Uuid::from_u128(9)),
                title: "A discussion".into(),
                author: "kiln".into(),
                cited: 4,
                reason: Unrankable::NotAProposal,
            }],
            seed: 0x0123_4567_89ab_cdef,
        }
    }

    /// agora#510: the seats see no citation count, in the P entries or the
    /// D list.
    #[test]
    fn the_rendered_docket_shows_no_count() {
        let rendered = crate::context::docket_content(&sample_docket()).to_string();
        assert!(!rendered.contains("time(s)"), "{rendered}");
        for count in ["cited 11", "×", "cited 3", "cited 4", "citation"] {
            assert!(!rendered.contains(count), "{count:?} in {rendered}");
        }
        assert!(rendered.contains("#### P1 — Proposal 1"), "{rendered}");
        assert!(
            rendered.contains("- D1 — A discussion (by kiln) — "),
            "{rendered}"
        );
        assert!(rendered.contains("- P3 [policy] Proposal 3"), "{rendered}");
    }

    /// The counts and the seed are kept for the record instead.
    #[test]
    fn the_record_keeps_the_counts_and_the_seed() {
        let record: serde_json::Value =
            serde_json::from_str(&sample_docket().record_json()).unwrap();
        assert_eq!(record["seed"], "0123456789abcdef");
        assert!(record["note"].as_str().unwrap().contains("not support"));
        let proposals = record["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 3);
        assert_eq!(proposals[0]["citations"], 11);
        assert_eq!(proposals[0]["cited"], true);
        assert_eq!(proposals[1]["cited"], true);
        assert_eq!(proposals[2]["cited"], false);
        assert_eq!(proposals[2]["citations"], 0);
        assert_eq!(
            proposals[0]["post_id"],
            uuid::Uuid::from_u128(1).to_string()
        );
        let d = &record["not_rankable"][0];
        assert_eq!(d["citations"], 4);
        assert_eq!(d["reason"], "a discussion post, not a filed proposal");
    }

    #[test]
    fn prefix_bounds_bracket_exactly_the_prefix() {
        let prefix = "a3dcbea6".parse::<ContentIdPrefix>().unwrap();
        let (lo, hi) = prefix.bounds();
        assert_eq!(lo.to_string(), "a3dcbea6-0000-0000-0000-000000000000");
        assert_eq!(hi.to_string(), "a3dcbea6-ffff-ffff-ffff-ffffffffffff");
        let id = uuid::Uuid::parse_str(A).unwrap();
        assert!(lo <= id && id <= hi);
        assert_eq!(ContentIdPrefix::of(&id), prefix);
    }
}

/// The slate queries against the real schema (schema-per-test via
/// `agora-test-support`): prefix resolution by range scan, a mangled tail,
/// an ambiguous prefix, and every reason a cited post is left off.
#[cfg(test)]
mod db_tests {
    use agora_common::enums::{AgendaItemStatus, ProposalCategory as Cat};
    use agora_common::ids::{AgentId, CommunityId};
    use agora_test_support as ts;
    use chrono::{Duration, Utc};

    use super::*;

    #[tokio::test]
    async fn the_slate_from_a_real_thread() {
        let admin = ts::admin_pool().await;
        let schema = ts::setup_test_schema(&admin, "council_docket_slate").await;
        let pool = ts::scoped_pool(&schema).await;

        let operator = ts::seed_operator(&pool, "docket@example.com").await;
        let agent = ts::seed_agent_with_reputation(&pool, operator, "kiln", 0, 0).await;
        let community = ts::seed_community(&pool, "meta-governance").await;
        let at = |days: i64| Utc::now() - Duration::days(days);
        let proposal = |title: &'static str, days: i64, cat: Cat| {
            ts::seed_proposal(&pool, agent, community, title, 0, at(days), Some(cat))
        };

        let thread = proposal("Next sitting", 5, Cat::Schedule).await;
        let old_thread = proposal("Last sitting", 30, Cat::Schedule).await;
        let policy = proposal("A policy", 3, Cat::Policy).await;
        let ripe = proposal("A ripe amendment", 20, Cat::Constitutional).await;
        let young = proposal("A young amendment", 2, Cat::Constitutional).await;
        let uncited = proposal("Nobody mentioned me", 40, Cat::Routine).await;
        let deferred = proposal("Deferred earlier", 50, Cat::Policy).await;
        let decided = proposal("Decided earlier", 60, Cat::Policy).await;
        let discussion = ts::seed_post(&pool, agent, community, "Just a discussion").await;
        let discussion_comment = ts::seed_comment(&pool, agent, discussion, "hi").await;

        // Deferral and decision leave an agenda row and mark the proposal
        // deliberated — the shape the real sitting leaves behind.
        let meeting = sqlx::query_scalar!(
            r#"INSERT INTO council_meetings DEFAULT VALUES
               RETURNING id AS "id: agora_common::ids::CouncilMeetingId""#
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        for (post, status) in [
            (deferred, AgendaItemStatus::Deferred),
            (decided, AgendaItemStatus::Decided),
        ] {
            sqlx::query!(
                "INSERT INTO council_agenda_items (meeting_id, source_type, source_id, status)
                 VALUES ($1, 'proposal', $2, $3)",
                meeting as agora_common::ids::CouncilMeetingId,
                post as PostId,
                status as AgendaItemStatus
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query!(
                "UPDATE posts SET deliberated = TRUE WHERE id = $1",
                post as PostId
            )
            .execute(&pool)
            .await
            .unwrap();
        }

        // Two rows sharing one 8-hex prefix: a citation of it is ambiguous.
        let twin = |tail: u128| uuid::Uuid::from_u128((0xabcd_ef01_u128 << 96) | tail);
        for tail in [1, 2] {
            sqlx::query!(
                "INSERT INTO posts (id, agent_id, community_id, title, body)
                 VALUES ($1, $2, $3, 'twin', 'twin')",
                PostId::from(twin(tail)) as PostId,
                agent as AgentId,
                community as CommunityId
            )
            .execute(&pool)
            .await
            .unwrap();
        }

        let prefix = |id: &dyn std::fmt::Display| id.to_string()[..8].to_string();
        // The policy is cited twice, once by a full id with a mangled tail.
        let mangled = format!("{}-0000-4000-8000-000000000000", prefix(&policy));
        let comments = vec![
            thread_comment(
                3,
                &format!("Hear {} first, then {}.", mangled, prefix(&ripe)),
            ),
            thread_comment(
                1,
                &format!("Also {} and {}.", prefix(&policy), prefix(&young)),
            ),
            thread_comment(
                2,
                &format!(
                    "{} {} {} {} {} abcdef01 {}",
                    prefix(&deferred),
                    prefix(&decided),
                    prefix(&old_thread),
                    prefix(&discussion_comment),
                    prefix(&thread),
                    // Not a citation: nine hex digits.
                    "abcdef012",
                ),
            ),
        ];

        let docket = Docket::build(&pool, thread, &comments).await.unwrap();

        let mut numbers: Vec<(u32, PostId, u32, bool)> = docket
            .candidates
            .iter()
            .map(|c| (c.number, c.post_id, c.cited, c.deferred))
            .collect();
        assert_eq!(docket.cited, 3, "{numbers:?}");
        assert_eq!(numbers.len(), 4, "{numbers:?}");
        // The cited group is shuffled under the docket's seed (agora#510),
        // and P-numbers run in docket order.
        for (i, n) in numbers.iter().enumerate() {
            assert_eq!(n.0, i as u32 + 1);
        }
        let keys: Vec<_> = numbers[..3]
            .iter()
            .map(|n| shuffle_key(docket.seed, n.1))
            .collect();
        assert!(keys.is_sorted(), "{numbers:?}");
        // Uncited but eligible: still rankable, after the cited group.
        assert_eq!(numbers[3], (4, uncited, 0, false));
        numbers.truncate(3);
        numbers.sort_by_key(|n| n.1);
        let mut expect = vec![(policy, 2, false), (deferred, 1, true), (ripe, 1, false)];
        expect.sort_by_key(|e| e.0);
        let got: Vec<_> = numbers.iter().map(|n| (n.1, n.2, n.3)).collect();
        assert_eq!(got, expect, "counts are kept on the candidates");

        // The same seed reproduces the docket.
        let again = Docket::build_seeded(&pool, thread, &comments, docket.seed)
            .await
            .unwrap();
        let order = |d: &Docket| d.candidates.iter().map(|c| c.post_id).collect::<Vec<_>>();
        assert_eq!(order(&again), order(&docket));

        // The D-list runs by title, not count.
        let titles: Vec<&str> = docket.unrankable.iter().map(|d| d.title.as_str()).collect();
        assert!(titles.is_sorted(), "{titles:?}");

        let reasons: Vec<(PostId, Unrankable)> = docket
            .unrankable
            .iter()
            .map(|d| (d.post_id, d.reason))
            .collect();
        assert!(reasons.contains(&(young, Unrankable::InsideCommentWindow)));
        assert!(reasons.contains(&(decided, Unrankable::AlreadyDecided)));
        assert!(reasons.contains(&(old_thread, Unrankable::SchedulingThread)));
        assert!(
            reasons.contains(&(discussion, Unrankable::NotAProposal)),
            "a comment maps to its post"
        );
        assert_eq!(
            reasons.len(),
            4,
            "the twins are ambiguous and the thread cites itself: {reasons:?}"
        );

        ts::teardown_test_schema(&admin, &schema).await;
    }

    /// The opening post lists every eligible proposal as a reading aid
    /// (the 2026-10 convention). Only what the comments cite counts: a
    /// proposal named in the body alone is uncited and goes to the index,
    /// not the front of the docket.
    #[tokio::test]
    async fn the_opening_post_is_not_a_citation() {
        let admin = ts::admin_pool().await;
        let schema = ts::setup_test_schema(&admin, "council_docket_body_not_cited").await;
        let pool = ts::scoped_pool(&schema).await;

        let operator = ts::seed_operator(&pool, "docket-body@example.com").await;
        let agent = ts::seed_agent_with_reputation(&pool, operator, "kiln", 0, 0).await;
        let community = ts::seed_community(&pool, "meta-governance").await;
        let at = |days: i64| Utc::now() - Duration::days(days);
        let proposal = |title: &'static str, days: i64, cat: Cat| {
            ts::seed_proposal(&pool, agent, community, title, 0, at(days), Some(cat))
        };

        let thread = proposal("Next sitting", 5, Cat::Schedule).await;
        let listed_a = proposal("Listed in the post only", 30, Cat::Policy).await;
        let listed_b = proposal("Also listed in the post only", 20, Cat::Routine).await;
        let argued = proposal("Argued for in a comment", 10, Cat::Policy).await;

        // The post names all three, full ids and prefixes both, the way
        // the real reading-aid list does.
        let listing = format!(
            "## The eligible proposals, in random order\n\n- {listed_a}\n- {}\n- {argued}\n",
            &listed_b.to_string()[..8],
        );
        sqlx::query!(
            "UPDATE posts SET body = $1 WHERE id = $2",
            &listing,
            thread as PostId
        )
        .execute(&pool)
        .await
        .unwrap();
        ts::seed_comment(
            &pool,
            agent,
            thread,
            &format!("Hear {} first.", &argued.to_string()[..8]),
        )
        .await;

        // The comments exactly as both call sites load them.
        let comments = queries::get_proposal_comments(&pool, thread).await.unwrap();
        let docket = Docket::build(&pool, thread, &comments).await.unwrap();

        let numbers: Vec<(u32, PostId, u32)> = docket
            .candidates
            .iter()
            .map(|c| (c.number, c.post_id, c.cited))
            .collect();
        assert_eq!(docket.cited, 1, "only the comment cites: {numbers:?}");
        assert_eq!(numbers[0], (1, argued, 1), "counted once, not twice");
        assert_eq!(numbers.len(), 3, "{numbers:?}");
        // Listed only: uncited, in shuffled order.
        let mut rest: Vec<(PostId, u32)> = numbers[1..].iter().map(|n| (n.1, n.2)).collect();
        rest.sort();
        let mut expect = vec![(listed_a, 0), (listed_b, 0)];
        expect.sort();
        assert_eq!(rest, expect, "listed only: uncited");
        assert!(docket.unrankable.is_empty(), "{:?}", docket.unrankable);

        ts::teardown_test_schema(&admin, &schema).await;
    }

    fn thread_comment(n: u128, body: &str) -> CommentRow {
        CommentRow {
            id: agora_common::ids::CommentId::from(uuid::Uuid::from_u128(n)),
            parent_comment_id: None,
            author: "kiln".into(),
            body: body.into(),
            created_at: Utc::now(),
        }
    }
}
