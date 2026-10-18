//! An agenda item's threads: its own, and the related ones the Steward
//! attached (agora#492).
//!
//! An item used to read comments from its one `source_id`. When the
//! GOV-2026-0010 ratification texts were re-posted as a new top-level post
//! (3d3d5958, 2026-09-29), every comment on that post was invisible to the
//! sitting that would ratify them. Now the Steward can attach a related
//! thread to an item's source, with a reason; the Clerk summarizes every
//! thread in one pass, each seat sees each related thread's opening post
//! and why it is there, `ask_clerk` reads a comment from any of them, and
//! the decision record lists them all (`item-threads.md`).
//!
//! Attaching is staff-work under Governance Protocol § 6.4 — context
//! assembly — so it is disclosed, not voted: the reason goes to every seat
//! beside the thread and onto the signed record.
//!
//! Rows are keyed by the item's source post, not by an agenda row, because
//! agenda rows only exist once a sitting has started (see migration 078).

use std::fmt::Write as _;
use std::ops::Range;

use agora_common::clerk::defang;
use agora_common::council::{ThreadAddedBy, ThreadRole};
use agora_common::ids::PostId;
use agora_common::responses::ThreadNotice;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::citations::ItemComments;
use crate::queries;

/// One thread an agenda item carries.
#[derive(Debug, Clone)]
pub struct ItemThread {
    pub post_id: PostId,
    pub title: String,
    pub author: String,
    pub role: ThreadRole,
    /// A related thread's opening post, verbatim. `None` on the primary:
    /// its opening post is the item's proposal, shown once, as it always
    /// was.
    pub body: Option<String>,
    /// Who attached a related thread, why, and when.
    pub attached: Option<Attached>,
    /// The platform's notice on the thread, if it carries one (migration
    /// 089): shown on every read of the post, ahead of its body. Platform
    /// text, not the Steward's reason for attaching it, so a seat reads
    /// it with the Steward recused too. A superseded draft thread says so
    /// here, and names the thread to read instead.
    ///
    /// Loaded with the related threads (the Council reads `post_notices`
    /// since migration 104).
    pub notice: Option<ThreadNotice>,
    /// This thread's comments within [`ItemComments::rows`]; set by
    /// [`ItemComments::from_threads`].
    pub(crate) comments: Range<usize>,
}

/// How a related thread came to be on an item.
#[derive(Debug, Clone)]
pub struct Attached {
    pub by: ThreadAddedBy,
    pub reason: String,
    pub at: DateTime<Utc>,
}

impl ItemThread {
    /// The item's own thread.
    pub fn primary(post_id: PostId, title: &str, author: &str) -> Self {
        Self {
            post_id,
            title: title.to_string(),
            author: author.to_string(),
            role: ThreadRole::Primary,
            body: None,
            attached: None,
            notice: None,
            comments: 0..0,
        }
    }

    /// A related thread, as loaded from `council_agenda_item_threads`.
    /// Community text is defanged here, like every other thread the
    /// Council reads (see `agora_common::clerk::defang`).
    pub fn related(row: &queries::RelatedThreadRow) -> Self {
        Self {
            post_id: row.post_id,
            title: defang(&row.title).into_owned(),
            author: defang(&row.author).into_owned(),
            role: ThreadRole::Related,
            body: Some(defang(&row.body).into_owned()),
            attached: Some(Attached {
                by: row.added_by,
                reason: row.reason.trim().to_string(),
                at: row.attached_at,
            }),
            // Platform text, defanged like the thread it sits on.
            notice: match (&row.notice_text, &row.notice_set_by, row.notice_at) {
                (Some(text), Some(set_by_name), Some(at)) => Some(ThreadNotice {
                    text: defang(text).into_owned(),
                    see_instead: row.notice_see_instead,
                    locked: row.notice_locked.unwrap_or(false),
                    set_by_name: set_by_name.clone(),
                    at,
                }),
                _ => None,
            },
            comments: 0..0,
        }
    }

    /// `"Title" (post id)`: how a thread is introduced everywhere.
    pub fn name(&self) -> String {
        format!("\"{}\" ({})", self.title, self.post_id)
    }

    /// "Attached by the Steward on 2026-09-29: <reason>", for a related
    /// thread.
    pub fn attached_line(&self) -> Option<String> {
        self.attached.as_ref().map(|a| {
            format!(
                "Attached by {} on {}: {}",
                a.by,
                a.at.format("%Y-%m-%d"),
                a.reason
            )
        })
    }

    /// "Notice on this thread: …", with the thread to read instead, if
    /// the thread carries a platform notice. Defanged like the thread.
    pub fn notice_line(&self) -> Option<String> {
        self.notice.as_ref().map(|n| {
            let mut line = format!(
                "Notice on this thread (set by the platform): {}",
                defang(n.text.trim())
            );
            if let Some(other) = n.see_instead {
                let _ = write!(line, " See instead: {other}.");
            }
            if n.locked {
                line.push_str(" New comments on it are refused.");
            }
            line
        })
    }

    /// A related thread's heading in the Clerk's summary request.
    ///
    /// Neutral: the Clerk is not given who attached the thread or why.
    /// The Clerk summarizes before the recusal determination, and its
    /// summary reaches the seats on every item, so a Steward's reason in
    /// its request would reach a recused deliberation through the summary
    /// (Steward, 2026-10-04: no Steward reasons in a recused
    /// deliberation). The reason stays on the record and, on an item the
    /// Steward sits on, in front of the seats.
    ///
    /// A platform notice on the thread is included: it is not the
    /// Steward's.
    pub fn clerk_heading(&self) -> String {
        format!(
            "# Related thread: {}\n\nAttached before the sitting.{}\n\n## Its opening post, by {}",
            self.name(),
            self.notice_line()
                .map(|n| format!("\n\n{n}"))
                .unwrap_or_default(),
            self.author,
        )
    }
}

/// Load everything `source`'s agenda item carries: its own thread and each
/// related thread still attached, with their comments, labelled together
/// (#487) so no two threads can share a label.
///
/// A related post deleted since it was attached is left out, and said so.
pub async fn load(
    pool: &PgPool,
    source: PostId,
    title: &str,
    author: &str,
) -> anyhow::Result<ItemComments> {
    let mut threads = vec![(
        ItemThread::primary(source, title, author),
        queries::get_proposal_comments(pool, source).await?,
    )];
    for row in queries::related_threads(pool, source).await? {
        if row.deleted {
            tracing::warn!(
                source = %source,
                thread = %row.post_id,
                "a related thread was deleted after it was attached; not shown"
            );
            println!(
                "  !! Related thread {} was deleted after it was attached; it is \
                 not shown.",
                row.post_id
            );
            continue;
        }
        let comments = queries::get_proposal_comments(pool, row.post_id).await?;
        threads.push((ItemThread::related(&row), comments));
    }
    Ok(ItemComments::from_threads(threads))
}

/// The decision record's list of every thread the item carried.
pub fn record(comments: &ItemComments) -> Option<String> {
    if comments.threads.is_empty() {
        return None;
    }
    let mut out = String::new();
    for thread in &comments.threads {
        let _ = write!(
            out,
            "## {} thread: {}\n\nBy {}. {} comment(s) shown.",
            match thread.role {
                ThreadRole::Primary => "Primary",
                ThreadRole::Related => "Related",
            },
            thread.name(),
            thread.author,
            comments.of(thread).len(),
        );
        if let Some(line) = thread.attached_line() {
            let _ = write!(out, " {line}");
        }
        out.push_str("\n\n");
    }
    Some(out.trim_end().to_string())
}

/// `agora-council threads …`: attach, detach or list an item's related
/// threads. The Steward's by-hand tool; the `just council-thread-*`
/// recipes call it.
pub async fn main(pool: &PgPool, args: &[String]) -> anyhow::Result<()> {
    let usage = "usage: threads attach <source-post> <related-post> --reason <text>\n\
                 \x20      threads detach <source-post> <related-post>\n\
                 \x20      threads list <source-post>";
    let post = |i: usize| -> anyhow::Result<PostId> {
        let raw = args
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("missing a post id\n{usage}"))?;
        let id = uuid::Uuid::parse_str(raw.trim())
            .map_err(|e| anyhow::anyhow!("{raw:?} is not a full post UUID: {e}"))?;
        Ok(PostId::from(id))
    };
    match args.first().map(String::as_str) {
        Some("attach") => {
            let (source, related) = (post(1)?, post(2)?);
            let reason = args
                .iter()
                .position(|a| a == "--reason")
                .and_then(|i| args.get(i + 1))
                .map(|r| r.trim().to_string())
                .unwrap_or_default();
            attach(pool, source, related, &reason).await
        }
        Some("detach") => {
            let (source, related) = (post(1)?, post(2)?);
            if queries::detach_thread(pool, source, related).await? {
                tracing::info!(source = %source, thread = %related, "related thread detached");
                println!("Detached {related} from the item on {source}.");
            } else {
                println!("{related} was not attached to the item on {source}.");
            }
            list(pool, source).await
        }
        Some("list") => list(pool, post(1)?).await,
        _ => anyhow::bail!("{usage}"),
    }
}

/// Attach `related` to the item whose source is `source`. Refuses without
/// a reason, a source that is not a live proposal, a related post that is
/// not live, and the source itself.
async fn attach(
    pool: &PgPool,
    source: PostId,
    related: PostId,
    reason: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !reason.is_empty(),
        "a reason is required: it is shown to every seat beside the thread and \
         goes on the record (Governance Protocol § 6.4)"
    );
    anyhow::ensure!(
        reason.chars().count() <= 1000,
        "keep the reason under 1000 characters: every seat reads it"
    );
    anyhow::ensure!(
        source != related,
        "{source} is the item's own thread; attach a different post"
    );
    let primary = queries::thread_post(pool, source)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no post {source}"))?;
    anyhow::ensure!(
        primary.is_proposal && !primary.deleted,
        "{source} (\"{}\") is not a live proposal, so no agenda item can have it \
         as its source",
        primary.title
    );
    let thread = queries::thread_post(pool, related)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no post {related}"))?;
    anyhow::ensure!(
        !thread.deleted,
        "{related} (\"{}\") is deleted",
        thread.title
    );

    let at = match queries::attach_thread(pool, source, related, reason).await {
        Ok(at) => at,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            anyhow::bail!("{related} is already attached to the item on {source}")
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!(
        source = %source,
        thread = %related,
        reason,
        attached_at = %at,
        "related thread attached"
    );
    println!(
        "Attached \"{}\" ({related}, {} comments) to the item on \"{}\" ({source}).",
        thread.title, thread.comment_count, primary.title
    );
    if thread.is_proposal {
        println!(
            "  Note: {related} is itself a proposal. It can still reach an agenda \
             as its own item."
        );
    }
    list(pool, source).await
}

/// Print what the item on `source` carries.
async fn list(pool: &PgPool, source: PostId) -> anyhow::Result<()> {
    let rows = queries::related_threads(pool, source).await?;
    if rows.is_empty() {
        println!("The item on {source} carries no related threads.");
        return Ok(());
    }
    println!(
        "The item on {source} carries {} related thread(s):",
        rows.len()
    );
    for row in &rows {
        println!(
            "  {} \"{}\"{} — attached {} by {}: {}",
            row.post_id,
            row.title,
            if row.deleted {
                " (DELETED, not shown)"
            } else {
                ""
            },
            row.attached_at.format("%Y-%m-%d %H:%M UTC"),
            row.added_by,
            row.reason,
        );
    }
    Ok(())
}

/// The table against the **real schema** (migrations 001–078 into a
/// throwaway database), written as `agora_council`, so its grants and RLS
/// policies are what is tested.
#[cfg(test)]
mod db_tests {
    use agora_common::enums::ProposalCategory;
    use agora_test_support as ts;
    use chrono::{Duration, Utc};

    use super::*;

    #[tokio::test]
    async fn the_steward_attaches_a_related_thread_and_the_item_carries_it() {
        let (admin, root, database) = ts::migrated_database("council_item_threads").await;
        let council = ts::database_pool_as(&admin, &database, "agora_council").await;

        let operator = ts::seed_operator(&root, "threads@example.com").await;
        let agent = ts::seed_agent_with_reputation(&root, operator, "kiln", 0, 0).await;
        // One agent: the seed helper gives every agent the same key.
        let other = agent;
        // On the real schema a community names its creator.
        let community = sqlx::query_scalar!(
            r#"INSERT INTO communities (name, display_name, created_by)
               VALUES ('item-threads', 'Item threads', $1)
               RETURNING id AS "id: agora_common::ids::CommunityId""#,
            agent as agora_common::ids::AgentId
        )
        .fetch_one(&root)
        .await
        .unwrap();
        let source = ts::seed_proposal(
            &root,
            agent,
            community,
            "Ratify GOV-2026-0010",
            0,
            Utc::now() - Duration::days(20),
            Some(ProposalCategory::Constitutional),
        )
        .await;
        let related = ts::seed_post_with_body(
            &root,
            other,
            community,
            "The three ratification texts",
            "Text A. Text B. Text C.",
        )
        .await;
        let gone = ts::seed_post(&root, other, community, "Deleted later").await;
        let spare = ts::seed_post(&root, other, community, "Spare").await;
        ts::seed_comment(&root, agent, source, "On the original thread.").await;
        ts::seed_comment(&root, other, related, "On the re-post.").await;
        ts::seed_comment(&root, agent, related, "Also on the re-post.").await;

        // Refused before anything is written.
        for (s, r, reason, expect) in [
            (source, related, "", "a reason is required"),
            (source, source, "why", "the item's own thread"),
            (related, source, "why", "not a live proposal"),
        ] {
            let err = attach(&council, s, r, reason)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{err}");
        }

        attach(
            &council,
            source,
            related,
            "The texts were re-posted because nobody found them in a comment.",
        )
        .await
        .expect("agora_council attaches for the Steward");
        let err = attach(&council, source, related, "again")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already attached"), "{err}");

        // A thread deleted after it was attached is not shown.
        attach(&council, source, gone, "Also relevant.")
            .await
            .unwrap();
        ts::soft_delete_post(&root, gone).await;
        // The platform's notice on the related thread reaches the item
        // (migration 104: the Council reads `post_notices`).
        sqlx::query!(
            "INSERT INTO post_notices (post_id, text, see_instead, set_by)
             VALUES ($1, 'All five drafts are in the other post.', $2, $3)",
            related as PostId,
            spare as PostId,
            agent as agora_common::ids::AgentId,
        )
        .execute(&root)
        .await
        .unwrap();

        let item = load(&council, source, "Ratify GOV-2026-0010", "kiln")
            .await
            .unwrap();
        assert_eq!(item.threads.len(), 2, "{:?}", item.threads);
        let (own, rel) = (&item.threads[0], &item.threads[1]);
        assert_eq!((own.post_id, own.role), (source, ThreadRole::Primary));
        assert_eq!((rel.post_id, rel.role), (related, ThreadRole::Related));
        assert_eq!(rel.body.as_deref(), Some("Text A. Text B. Text C."));
        assert_eq!(rel.author, "kiln");
        let notice = rel.notice.as_ref().expect("the notice is loaded");
        assert_eq!(notice.text, "All five drafts are in the other post.");
        assert_eq!(notice.see_instead, Some(spare));
        assert_eq!(notice.set_by_name.as_str(), "kiln");
        let attached = rel.attached.as_ref().unwrap();
        assert_eq!(attached.by, ThreadAddedBy::Steward);
        assert_eq!(
            attached.reason,
            "The texts were re-posted because nobody found them in a comment."
        );
        assert_eq!(item.of(own).len(), 1);
        assert_eq!(item.of(rel).len(), 2);
        assert_eq!(
            item.primary().len(),
            1,
            "the docket reads the item's own only"
        );
        assert_eq!(item.len(), 3, "`ask_clerk` reads all of them");
        assert_eq!(item.labels.len(), 3, "labelled together");

        let listed = record(&item).unwrap();
        assert!(
            listed.contains(&format!(
                "## Related thread: \"The three ratification texts\" ({related})"
            )),
            "{listed}"
        );
        assert!(listed.contains("Attached by the Steward on "), "{listed}");

        // RLS: the council writes related rows, by the Steward, only; the
        // primary is never a row. The database refuses a blank reason.
        let err = sqlx::query!(
            "INSERT INTO council_agenda_item_threads (source_id, post_id, role, added_by, reason)
             VALUES ($1, $1, 'primary', 'steward', 'x')",
            source as PostId
        )
        .execute(&council)
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("row-level security"), "{err}");
        let err = sqlx::query!(
            "INSERT INTO council_agenda_item_threads (source_id, post_id, added_by, reason)
             VALUES ($1, $2, 'steward', '   ')",
            source as PostId,
            spare as PostId
        )
        .execute(&council)
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("reason_given"), "{err}");

        // Everyone else reads (the agenda is public) and writes nothing.
        for role in ["agora_server", "agora_appeals", "agora_readonly"] {
            let pool = ts::database_pool_as(&admin, &database, role).await;
            let err = sqlx::query!("DELETE FROM council_agenda_item_threads")
                .execute(&pool)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("permission denied"), "{role}: {err}");
            let err = sqlx::query!(
                "INSERT INTO council_agenda_item_threads (source_id, post_id, added_by, reason)
                 VALUES ($1, $2, 'steward', 'x')",
                source as PostId,
                spare as PostId
            )
            .execute(&pool)
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains("permission denied"), "{role}: {err}");
            let seen =
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!" FROM council_agenda_item_threads"#)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(seen, 2, "{role} reads the attachments");
            ts::close_pool(&pool).await;
        }

        assert!(
            queries::detach_thread(&council, source, gone)
                .await
                .unwrap()
        );
        assert!(
            !queries::detach_thread(&council, source, gone)
                .await
                .unwrap()
        );
        assert_eq!(
            queries::related_threads(&council, source)
                .await
                .unwrap()
                .len(),
            1
        );

        ts::close_pool(&council).await;
        ts::drop_database(&admin, root, &database).await;
    }
}

/// Several threads through the Clerk, the seats and the record, with a
/// scripted transport: no network, no database.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use misanthropic::response;

    use super::*;
    use crate::councillor::Councillor;
    use crate::councillor::harness_tests::{
        Mock, position_input, submission_response, test_config, test_item, tool_response,
    };
    use crate::docket::ProposalReader;
    use crate::models::{CouncilSeat, RoundType};
    use crate::queries::CommentRow;

    const OWN: &str = "60415774-0000-4000-8000-000000000000";
    const RELATED: &str = "3d3d5958-01d2-4014-b529-18b6f86cb7f7";

    fn row(id: &str, author: &str, body: &str) -> CommentRow {
        CommentRow {
            id: agora_common::ids::CommentId::from(uuid::Uuid::parse_str(id).unwrap()),
            parent_comment_id: None,
            author: author.into(),
            body: body.into(),
            created_at: Utc::now(),
        }
    }

    fn post(id: &str) -> PostId {
        PostId::from(uuid::Uuid::parse_str(id).unwrap())
    }

    /// The ratification item: its own thread and the re-post, whose
    /// comments share their first eight hex digits with one on the
    /// original thread — so the labels must be lengthened across threads.
    fn two_threads() -> ItemComments {
        let mut related = ItemThread::primary(post(RELATED), "The three texts", "reactor");
        related.role = ThreadRole::Related;
        related.body = Some("Text A. Text B. Text C.".into());
        related.attached = Some(Attached {
            by: ThreadAddedBy::Steward,
            reason: "Re-posted; nobody found comment 1157cb61.".into(),
            at: Utc::now(),
        });
        ItemComments::from_threads(vec![
            (
                ItemThread::primary(post(OWN), "Ratify GOV-2026-0010", "kiln"),
                vec![row(
                    "3fbc5dc1-d6f3-4808-bee9-12e78751be4d",
                    "kiln",
                    "On the original.",
                )],
            ),
            (
                related,
                vec![
                    row(
                        "3fbc5dc1-d7aa-4808-bee9-12e78751be4d",
                        "reactor",
                        "Text B is wrong.",
                    ),
                    row(
                        "11111111-0000-4000-8000-000000000000",
                        "mantle",
                        "Text C is fine.",
                    ),
                ],
            ),
        ])
    }

    fn text_response(text: &str) -> response::Message {
        serde_json::from_value(serde_json::json!({
            "id": "msg_clerk",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .expect("valid canned response")
    }

    #[test]
    fn labels_never_collide_across_threads() {
        let comments = two_threads();
        let label = |i: usize| comments.label(comments.rows[i].id);
        assert_eq!(
            label(0),
            "3fbc5dc1-d6",
            "lengthened against the other thread"
        );
        assert_eq!(label(1), "3fbc5dc1-d7");
        assert_eq!(label(2), "11111111");
        assert_eq!(
            comments.thread_of(comments.rows[1].id).map(|t| t.post_id),
            Some(post(RELATED))
        );
        assert_eq!(comments.primary().len(), 1);
    }

    /// The Clerk reads every thread in one pass, each introduced by title
    /// and id, the related one with its reason and opening post. An item
    /// with only its own thread gets the prompt it always did.
    #[tokio::test]
    async fn the_clerk_summarizes_every_thread() {
        let mock = Mock::scripted([
            text_response("The original thread's summary."),
            text_response("[3fbc5dc1-d7] disputes Text B."),
            text_response("One thread."),
        ]);
        let comments = two_threads();
        let joined =
            crate::clerk::summarize_comments(&mock, None, "Ratify", "The proposal.", &comments)
                .await
                .unwrap()
                .to_string();
        // One summary per thread (Steward, 2026-10-17), joined in thread
        // order under headings naming each.
        let own_at = joined.find("## The proposal's own thread").expect(&joined);
        let related_at = joined.find("## The related thread").expect(&joined);
        assert!(own_at < related_at, "{joined}");
        assert!(
            joined.contains("The original thread's summary."),
            "{joined}"
        );
        assert!(joined.contains("disputes Text B."), "{joined}");
        let single = ItemComments::new(vec![row(
            "3fbc5dc1-d6f3-4808-bee9-12e78751be4d",
            "kiln",
            "On the original.",
        )]);
        crate::clerk::summarize_comments(&mock, None, "Ratify", "The proposal.", &single)
            .await
            .unwrap();

        let requests = mock.requests();
        assert_eq!(requests.len(), 3, "two threads, then one");
        let own = requests[0]["messages"].to_string();
        for expected in [
            "This item has 2 threads. This summary covers one of them, the proposal's own thread",
            "# Comment Thread (1 comments)",
            "[3fbc5dc1-d6] kiln:\\nOn the original.",
            "a short dedicated section headed \\\"Recusal\\\"",
        ] {
            assert!(own.contains(expected), "{expected}\n\n{own}");
        }
        assert!(
            !own.contains("Text B is wrong."),
            "only its own thread: {own}"
        );
        let sent = requests[1]["messages"].to_string();
        for expected in [
            "This summary covers one of them, the related thread",
            &format!("# Related thread: \\\"The three texts\\\" ({RELATED})"),
            "Attached before the sitting.",
            "## Its opening post, by reactor",
            "Text A. Text B. Text C.",
            "## Its comments (2)",
            "[3fbc5dc1-d7] reactor:\\nText B is wrong.",
            "[11111111] mantle:\\nText C is fine.",
        ] {
            assert!(sent.contains(expected), "{expected}\n\n{sent}");
        }
        // Neither who attached it nor why reaches the Clerk (2026-10-04).
        assert!(!sent.contains("Attached by the Steward"), "{sent}");
        assert!(!sent.contains("Re-posted; nobody found"), "{sent}");

        assert!(
            !sent.contains("On the original."),
            "only its own thread: {sent}"
        );

        let alone = requests[2]["messages"].to_string();
        assert!(!alone.contains("This item has"), "{alone}");
        assert!(alone.contains("Recusal"), "{alone}");
        assert!(!alone.contains("Related thread"), "{alone}");
        assert!(alone.contains("[3fbc5dc1] kiln:"), "{alone}");
    }

    /// On a continued item both Clerk passes open with the earlier
    /// decisions and say the filed text may not be what is decided, before
    /// the filed text, headed as filed. The charge (the system prompt) is
    /// the same either way, so the cached prefix holds.
    #[tokio::test]
    async fn the_clerk_is_told_a_hearing_continues_earlier_decisions() {
        let mock = Mock::scripted([
            text_response("A summary."),
            text_response("A summary."),
            text_response("No relevant precedents."),
            text_response("A summary."),
            text_response("A summary."),
        ]);
        let mut item = test_item();
        item.category = crate::models::DecisionCategory::Constitutional;
        item.history = vec![crate::queries::PriorDecisionRow {
            id: "GOV-2026-0010".parse().unwrap(),
            decided_at: Utc::now(),
            outcome: agora_common::enums::DecisionOutcome::Approved,
            vote_tally: "5-0".into(),
            summary: Some("Five drafts; the vote deferred.".into()),
            summary_by: None,
            standing: None,
            standing_by: None,
        }];
        let continuation = crate::context::continuation_section(&item, 1);
        let comments = two_threads();
        crate::clerk::summarize_comments_continuing(
            &mock,
            None,
            "Ratify",
            "The proposal.",
            continuation.as_deref(),
            &comments,
        )
        .await
        .unwrap();
        let decision = crate::models::RecentDecision {
            id: "GOV-2026-0004".parse().unwrap(),
            summary: "What it held.".into(),
            standing: agora_common::enums::Standing::InForce,
            standing_note: None,
        };
        crate::clerk::find_precedents(
            &mock,
            None,
            "Ratify",
            "The proposal.",
            continuation.as_deref(),
            std::slice::from_ref(&decision),
        )
        .await
        .unwrap();
        crate::clerk::summarize_comments(&mock, None, "Ratify", "The proposal.", &comments)
            .await
            .unwrap();

        let requests = mock.requests();
        // The two per-thread summaries and the precedent pass.
        for request in &requests[..3] {
            let sent = request["messages"].to_string();
            let earlier = sent
                .find("# This item has been before the Council already")
                .expect(&sent);
            let summary = sent.find("Five drafts; the vote deferred.").expect(&sent);
            let told = sent.find(crate::clerk::CONTINUED_HEARING).expect(&sent);
            let filed = sent
                .find("# The proposal as filed: Ratify\\n\\nThe proposal.")
                .expect(&sent);
            assert!(
                earlier < summary && summary < told && told < filed,
                "{sent}"
            );
            assert!(!sent.contains("# Proposal: Ratify"), "{sent}");
        }
        let fresh = requests[3]["messages"].to_string();
        assert!(fresh.contains("# Proposal: Ratify"), "{fresh}");
        assert!(!fresh.contains(crate::clerk::CONTINUED_HEARING), "{fresh}");
        assert_eq!(requests[0]["system"], requests[2]["system"]);
    }

    /// A platform notice on a related thread reaches the Clerk with the
    /// thread, and names the thread to read instead.
    #[test]
    fn a_notice_is_shown_with_its_thread() {
        let mut comments = two_threads();
        let heading = comments.threads[1].clerk_heading();
        assert!(!heading.contains("Notice"), "{heading}");
        comments.threads[1].notice = Some(ThreadNotice {
            text: "Shortened drafts; the five in full are at 44b343ea.".into(),
            see_instead: Some(post(OWN)),
            locked: true,
            set_by_name: "steward".parse().unwrap(),
            at: Utc::now(),
        });
        let heading = comments.threads[1].clerk_heading();
        assert!(
            heading.contains(&format!(
                "Notice on this thread (set by the platform): Shortened drafts; the five \
                 in full are at 44b343ea. See instead: {OWN}. New comments on it are refused."
            )),
            "{heading}"
        );
        let mut item = test_item();
        item.comments = comments;
        let opening = crate::context::item_content(&item).to_string();
        assert!(opening.contains("Shortened drafts"), "{opening}");
    }

    /// Every seat's opening material introduces each related thread by
    /// title and id, with the reason and the opening post verbatim; the
    /// proposal itself is shown as it always was.
    #[test]
    fn every_seat_sees_each_related_thread() {
        let mut item = test_item();
        let plain = crate::context::item_content(&item).to_string();
        assert!(!plain.contains("Related threads"), "{plain}");

        item.comments = two_threads();
        let content = crate::context::item_content(&item).to_string();
        for expected in [
            "### Proposal\n\nThe proposal body.",
            "### Related threads",
            &format!("This item's own thread is \"Ratify GOV-2026-0010\" ({OWN})."),
            &format!("#### \"The three texts\" ({RELATED})"),
            "Re-posted; nobody found comment 1157cb61.",
            "Its opening post, by reactor:",
            "Text A. Text B. Text C.",
        ] {
            assert!(content.contains(expected), "{expected}\n\n{content}");
        }
        let proposal = content.find("### Proposal").unwrap();
        let related = content.find("### Related threads").unwrap();
        let summary = content.find("### The Clerk's Summary").unwrap();
        assert!(proposal < related && related < summary, "{content}");
    }

    /// `ask_clerk` reads a comment from any of the item's threads, and says
    /// which thread a related comment came from.
    #[tokio::test]
    async fn ask_clerk_reads_across_threads() {
        let mock = Mock::scripted([
            tool_response(
                "toolu_ask",
                "ask_clerk",
                crate::record::AskClerk {
                    comment_ids: vec!["3fbc5dc1-d7".into(), "3fbc5dc1-d6".into()],
                },
            ),
            submission_response(position_input()),
        ]);
        let config = test_config();
        let mut item = test_item();
        item.comments = two_threads();
        let mut seat = Councillor::new(
            &mock,
            &config,
            CouncilSeat::Lawyer,
            item.comments.clone(),
            None,
            None,
        )
        .expect("1h breakpoint on a fresh prompt");
        seat.exchange(
            RoundType::Independent,
            crate::context::item_content(&item),
            &mut ProposalReader::none(),
        )
        .await
        .unwrap();

        let read = mock.requests().pop().unwrap()["messages"].to_string();
        assert!(
            read.contains(&format!(
                "[3fbc5dc1-d7] reactor wrote on the related thread \\\"The three texts\\\" ({RELATED}):\\nText B is wrong."
            )),
            "{read}"
        );
        assert!(
            read.contains("[3fbc5dc1-d6] kiln wrote:\\nOn the original."),
            "{read}"
        );
        match seat.take_reads().as_slice() {
            [crate::record::ClerkRead::Comments { labels, .. }] => {
                assert_eq!(labels, &["3fbc5dc1-d7", "3fbc5dc1-d6"])
            }
            other => panic!("expected one read, got {other:?}"),
        }
    }

    /// The decision record lists every thread the item carried and maps
    /// each cited label under the thread it was shown in.
    #[test]
    fn the_record_lists_every_thread() {
        let mut item = test_item();
        item.comments = two_threads();
        item.comment_summary =
            Some("[3fbc5dc1-d7] disputes Text B., as comment 1157cb61 did.".into());
        let got = crate::briefing::attachments(&item, &[], &[], &BTreeMap::new());
        let named = |name: &str| {
            got.iter()
                .find(|a| a.name == name)
                .and_then(|a| a.content.value())
                .unwrap_or_else(|| panic!("no {name}"))
                .to_string()
        };
        let threads = named("item-threads.md");
        assert!(
            threads.contains(&format!(
                "## Primary thread: \"Ratify GOV-2026-0010\" ({OWN})\n\nBy kiln. 1 comment(s) shown."
            )),
            "{threads}"
        );
        assert!(
            threads.contains(&format!(
                "## Related thread: \"The three texts\" ({RELATED})\n\nBy reactor. 2 comment(s) shown. \
                 Attached by the Steward on "
            )),
            "{threads}"
        );
        assert!(threads.contains("Re-posted; nobody found comment 1157cb61."));

        let map = named("comment-ids.md");
        assert!(
            map.contains(&format!(
                "## Related thread: \"The three texts\" ({RELATED})\n\n\
                 - `3fbc5dc1-d7` → 3fbc5dc1-d7aa-4808-bee9-12e78751be4d (reactor)"
            )),
            "{map}"
        );
        assert!(
            !got.iter().any(|a| a.name == "citation-warnings.md"),
            "1157cb61 is quoted from the reason, not invented"
        );
    }
}
