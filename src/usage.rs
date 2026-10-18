//! What a sitting cost, from its archived prompts.
//!
//! Every model call the Council makes is archived in `prompt_archive`
//! with its usage filled in when the response returns. A sitting's calls
//! are the rows whose `context_id` is
//!
//! * the **meeting id** — the Lawyer's meeting summary at adjournment, on
//!   sittings before 2026-10-17 (it is composed now, and calls no model);
//!   or
//! * one of the meeting's **agenda item ids** — every seat's deliberation
//!   (`councillor.rs`), the Clerk's comment and precedent passes
//!   (`meeting.rs`), its docket thread summaries (`docket.rs`), and the
//!   decision's summary: the Lawyer's drafts and the seats' reviews
//!   (`summary.rs`).
//!
//! Before 2026-10-17 the Clerk's per-decision summary ran unarchived and
//! was not counted; it is gone.
//!
//! The Clerk archives each exchange twice — the prompt, then the prompt
//! with its answer appended as an assistant turn, both carrying the same
//! usage (`agora_common::clerk::ask_on_archived`). The second row is a
//! record, not a call; the query skips rows whose last message is the
//! assistant's. No request the Council sends ends on an assistant turn
//! (prefill is a 400 on every model it runs).
//!
//! Where the call's raw response body was archived
//! (`prompt_archive_responses`: every seat call since migration 085), its
//! `usage` object is read from the body instead of the columns. It carries
//! what the columns drop: the 5-minute/1-hour split of cache writes and
//! the thinking share of output (`output_tokens_details`). Calls without a
//! body (the Clerk, the adjournment Lawyer) fall back to the columns, and
//! their thinking shows as unreported.

use serde::Deserialize;
use sqlx::PgPool;

use agora_common::enums::ModelRole;
use agora_common::ids::{AgendaItemId, CouncilMeetingId};
use agora_common::usage_summary::{
    CallRole, CallUsage, UsageContext, UsageSummary, WriteTtl, summarize,
};
use misanthropic::response::TokenCounts;

/// The part of an archived response body this module reads.
#[derive(Deserialize)]
struct BodyUsage {
    usage: TokenCounts,
}

/// A response body's usage, if it parses. A body that does not (an error
/// page, a truncated read) falls back to the columns; it is the record's
/// business, not the usage report's, to explain it.
fn body_usage(body: &[u8]) -> Option<TokenCounts> {
    serde_json::from_slice::<BodyUsage>(body)
        .ok()
        .map(|b| b.usage)
}

/// Every archived call of one sitting, oldest first.
pub async fn meeting_calls(
    pool: &PgPool,
    meeting: CouncilMeetingId,
) -> Result<Vec<CallUsage>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT pa.role AS "role: ModelRole",
               pa.model,
               pa.created_at,
               pa.context_id AS "context_id!",
               CASE WHEN jsonb_typeof(pa.prompt->'messages') = 'array'
                    THEN jsonb_array_length(pa.prompt->'messages')
               END AS messages,
               pa.input_tokens,
               pa.output_tokens,
               pa.cache_read_tokens,
               pa.cache_creation_tokens,
               r.body AS "body?"
          FROM prompt_archive pa
          LEFT JOIN prompt_archive_responses r ON r.prompt_archive_id = pa.id
         WHERE (pa.context_id = $1
                OR pa.context_id IN (SELECT ai.id
                                       FROM council_agenda_items ai
                                      WHERE ai.meeting_id = $1))
           AND pa.prompt->'messages'->-1->>'role' IS DISTINCT FROM 'assistant'
         ORDER BY pa.created_at, pa.id
        "#,
        meeting as CouncilMeetingId,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            // `context_id` is untyped in the archive because it spans
            // tables; the WHERE clause above says which table each came
            // from, so it is typed here and nowhere later.
            let context = if r.context_id == *meeting.as_uuid() {
                UsageContext::Meeting(meeting)
            } else {
                UsageContext::AgendaItem(AgendaItemId::from(r.context_id))
            };
            let from_columns = r.input_tokens.map(|input| {
                let tokens = |v: i64| u64::try_from(v).unwrap_or(0);
                let mut c = TokenCounts::new(tokens(input), r.output_tokens.map_or(0, tokens));
                c.cache_read_input_tokens = r.cache_read_tokens.map(tokens);
                c.cache_creation_input_tokens = r.cache_creation_tokens.map(tokens);
                c
            });
            let counts = r.body.as_deref().and_then(body_usage).or(from_columns);
            CallUsage {
                role: CallRole::Model(r.role),
                model: r.model,
                at: Some(r.created_at),
                context: Some(context),
                messages: r.messages.and_then(|m| u32::try_from(m).ok()),
                counts,
            }
        })
        .collect())
}

/// One sitting's summary. The Council caches with the 1-hour TTL
/// everywhere, which is what writes are priced at.
pub async fn meeting_summary(
    pool: &PgPool,
    meeting: CouncilMeetingId,
) -> Result<UsageSummary, sqlx::Error> {
    Ok(summarize(
        &meeting_calls(pool, meeting).await?,
        WriteTtl::OneHour,
    ))
}

/// Print and log a sitting's usage. Never fatal: the sitting is over and
/// recorded by the time this runs.
pub async fn report_meeting(pool: &PgPool, meeting: CouncilMeetingId) {
    match meeting_summary(pool, meeting).await {
        Ok(summary) => {
            println!("\n  USAGE — meeting {meeting}");
            print!("{summary}");
            summary.log(&format_args!("meeting {meeting}"));
        }
        Err(e) => {
            tracing::warn!(%meeting, error = %e, "could not read this sitting's usage");
        }
    }
}

/// `agora-council --usage <meeting-uuid>`: a past sitting's usage. Read
/// only.
pub async fn main(pool: &PgPool, raw: &str) -> anyhow::Result<()> {
    let meeting = CouncilMeetingId::from(
        uuid::Uuid::parse_str(raw.trim())
            .map_err(|e| anyhow::anyhow!("--usage: {raw:?} is not a meeting UUID: {e}"))?,
    );
    let summary = meeting_summary(pool, meeting).await?;
    println!("USAGE — meeting {meeting}");
    print!("{summary}");
    summary.log(&format_args!("meeting {meeting}"));
    Ok(())
}

/// The query against the real table shapes (schema-per-test via
/// `agora-test-support`), fed by the production archive path.
#[cfg(test)]
mod db_tests {
    use std::sync::Mutex;

    use agora_common::clerk::{COUNCIL_CLERK_MODEL, ClerkArchive, ask_on_archived};
    use agora_common::prompt_archive::{
        ArchiveEntry, PromptUsage, archive, archive_response, record_usage,
    };
    use agora_test_support as ts;
    use misanthropic::model::Models;
    use misanthropic::prompt::CachedPrompt;
    use misanthropic::response;
    use misanthropic::transport::Transport;

    use super::*;

    /// A Clerk transport: canned answers with canned usage, in order.
    struct Mock(Mutex<Vec<&'static str>>);

    #[derive(Debug)]
    struct Exhausted;
    impl std::fmt::Display for Exhausted {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("script exhausted")
        }
    }
    impl std::error::Error for Exhausted {}

    #[async_trait::async_trait]
    impl Transport<CachedPrompt> for Mock {
        type Error = Exhausted;
        async fn send(&self, _: &CachedPrompt) -> Result<response::Message, Self::Error> {
            let body = self.0.lock().unwrap().pop().ok_or(Exhausted)?;
            Ok(serde_json::from_str(body).expect("valid canned response"))
        }
        async fn models(&self) -> Result<Models, Self::Error> {
            Ok(Models::from_iter([]))
        }
    }

    impl crate::wire::RawTransport for Mock {
        async fn send_raw(
            &self,
            prompt: &CachedPrompt,
        ) -> Result<crate::wire::RawBody, Self::Error> {
            let message = Transport::send(self, prompt).await?;
            Ok(crate::wire::RawBody::new(
                serde_json::to_vec(&message).expect("a message serializes"),
            ))
        }
    }

    const CLERK_FIRST: &str = r#"{
        "id": "msg_1", "type": "message", "role": "assistant",
        "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "A summary."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 300, "output_tokens": 40,
                  "cache_creation_input_tokens": 9000,
                  "cache_read_input_tokens": 0}
    }"#;
    const CLERK_SECOND: &str = r#"{
        "id": "msg_2", "type": "message", "role": "assistant",
        "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "Another summary."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 200, "output_tokens": 30,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 9000}
    }"#;

    #[derive(serde::Serialize)]
    struct FakePrompt {
        messages: Vec<FakeMessage>,
    }
    #[derive(serde::Serialize)]
    struct FakeMessage {
        role: &'static str,
        content: &'static str,
    }

    async fn seat_call(
        pool: &PgPool,
        role: ModelRole,
        context: uuid::Uuid,
        messages: usize,
        usage: Option<PromptUsage>,
    ) {
        seat_call_with_body(pool, role, context, messages, usage, None).await;
    }

    /// A seat call whose raw response body was archived, as `councillor.rs`
    /// does for every sample.
    async fn seat_call_with_body(
        pool: &PgPool,
        role: ModelRole,
        context: uuid::Uuid,
        messages: usize,
        usage: Option<PromptUsage>,
        body: Option<&str>,
    ) {
        let prompt = FakePrompt {
            messages: (0..messages)
                .map(|i| FakeMessage {
                    role: if i % 2 == 0 { "user" } else { "assistant" },
                    content: "…",
                })
                .collect(),
        };
        let entry = ArchiveEntry {
            role,
            model: "claude-opus-4-6",
            subject_agent_id: None,
            context_id: Some(context),
            usage: PromptUsage::default(),
            stage: None,
        };
        let id = archive(pool, &entry, &prompt).await.unwrap();
        if let Some(body) = body {
            archive_response(pool, id, body.as_bytes()).await.unwrap();
        }
        if let Some(usage) = usage {
            record_usage(pool, id, usage).await.unwrap();
        }
    }

    /// A seat's response body: the usage the API sent, with the 1-hour
    /// write split and the thinking share of output the columns drop.
    const SEAT_FIRST: &str = r#"{
        "id": "msg_s1", "type": "message", "role": "assistant",
        "model": "claude-opus-4-6",
        "content": [{"type": "thinking", "thinking": "", "signature": "x"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 900,
                  "cache_creation_input_tokens": 15000,
                  "cache_creation": {"ephemeral_5m_input_tokens": 0,
                                     "ephemeral_1h_input_tokens": 15000},
                  "cache_read_input_tokens": 0,
                  "output_tokens_details": {"thinking_tokens": 700}}
    }"#;
    const SEAT_SECOND: &str = r#"{
        "id": "msg_s2", "type": "message", "role": "assistant",
        "model": "claude-opus-4-6",
        "content": [{"type": "thinking", "thinking": "", "signature": "y"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 12, "output_tokens": 400,
                  "cache_creation_input_tokens": 300,
                  "cache_creation": {"ephemeral_5m_input_tokens": 0,
                                     "ephemeral_1h_input_tokens": 300},
                  "cache_read_input_tokens": 15000,
                  "output_tokens_details": {"thinking_tokens": 200}}
    }"#;

    fn usage(input: i64, write: i64, read: i64, output: i64) -> Option<PromptUsage> {
        Some(PromptUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(read),
            cache_creation_tokens: Some(write),
        })
    }

    #[tokio::test]
    async fn a_sittings_calls_are_its_meeting_and_item_rows() {
        // A fully migrated database (the response archive is migration
        // 085), read and written as `agora_council`: the role `--usage`
        // and the adjournment report run under, so its grants are tested.
        let (admin, root, database) = ts::migrated_database("council_usage_meeting").await;
        let pool = ts::database_pool_as(&admin, &database, "agora_council").await;

        let new_meeting = || async {
            *crate::queries::create_meeting(&pool)
                .await
                .unwrap()
                .as_uuid()
        };
        let new_item = |meeting: uuid::Uuid| {
            let pool = &pool;
            async move {
                sqlx::query_scalar!(
                    r#"INSERT INTO council_agenda_items (meeting_id, source_type, title, category)
                       VALUES ($1, 'internal', 'A test proposal', 'routine')
                       RETURNING id"#,
                    meeting
                )
                .fetch_one(pool)
                .await
                .unwrap()
            }
        };
        let meeting = new_meeting().await;
        let item_a = new_item(meeting).await;
        let item_b = new_item(meeting).await;
        let elsewhere = new_meeting().await;
        let other_item = new_item(elsewhere).await;

        // The Clerk, through the production path: each ask archives the
        // prompt and the completed exchange, both with usage.
        let clerk = Mock(Mutex::new(vec![CLERK_SECOND, CLERK_FIRST]));
        for item in [item_a, item_b] {
            ask_on_archived(
                &clerk,
                "Summarize.",
                COUNCIL_CLERK_MODEL,
                "Some comments.",
                Some(ClerkArchive {
                    pool: &pool,
                    context_id: Some(item),
                }),
            )
            .await
            .unwrap();
        }
        // The Clerk's raw response is archived beside its prompt, byte for
        // byte, as each seat's is (2026-10-17).
        let raw = sqlx::query_scalar!(
            r#"SELECT count(*) as "n!" FROM prompt_archive_responses r
               JOIN prompt_archive p ON p.id = r.prompt_archive_id
               WHERE p.role = 'clerk' AND p.context_id = $1"#,
            item_a
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(raw, 1, "the Clerk's response body is archived");

        // A seat across both items: write, hit, then a break.
        seat_call(
            &pool,
            ModelRole::Artist,
            item_a,
            1,
            usage(10, 20_000, 0, 500),
        )
        .await;
        seat_call(
            &pool,
            ModelRole::Artist,
            item_a,
            3,
            usage(10, 600, 20_000, 500),
        )
        .await;
        seat_call(
            &pool,
            ModelRole::Artist,
            item_b,
            5,
            usage(10, 21_100, 0, 500),
        )
        .await;
        // A seat whose raw bodies were archived: the body's usage wins
        // over the columns, which carry the same totals but no TTL split
        // and no thinking.
        seat_call_with_body(
            &pool,
            ModelRole::Philosopher,
            item_b,
            1,
            usage(12, 15_000, 0, 900),
            Some(SEAT_FIRST),
        )
        .await;
        seat_call_with_body(
            &pool,
            ModelRole::Philosopher,
            item_b,
            3,
            usage(12, 300, 15_000, 400),
            Some(SEAT_SECOND),
        )
        .await;
        // A call that never returned.
        seat_call(&pool, ModelRole::Engineer, item_b, 1, None).await;
        // The adjournment summary, under the meeting id.
        seat_call(
            &pool,
            ModelRole::Lawyer,
            meeting,
            1,
            usage(10, 8_000, 0, 900),
        )
        .await;
        // Not this sitting's.
        seat_call(&pool, ModelRole::Artist, other_item, 1, usage(1, 1, 1, 1)).await;
        seat_call(&pool, ModelRole::Artist, elsewhere, 1, usage(1, 1, 1, 1)).await;
        seat_call(
            &pool,
            ModelRole::Artist,
            uuid::Uuid::new_v4(),
            1,
            usage(1, 1, 1, 1),
        )
        .await;

        let meeting_id = CouncilMeetingId::from(meeting);
        let calls = meeting_calls(&pool, meeting_id).await.unwrap();
        assert_eq!(
            calls.len(),
            9,
            "2 Clerk asks (not 4 rows), 6 seat calls, 1 Lawyer"
        );
        assert_eq!(
            calls.last().unwrap().context,
            Some(UsageContext::Meeting(meeting_id))
        );
        assert_eq!(
            calls[0].context,
            Some(UsageContext::AgendaItem(AgendaItemId::from(item_a)))
        );
        assert_eq!(calls[0].messages, Some(1));

        let summary = summarize(&calls, WriteTtl::OneHour);
        let clerk = summary
            .by_role
            .iter()
            .find(|(r, _)| *r == CallRole::Model(ModelRole::Clerk))
            .map(|(_, t)| *t)
            .unwrap();
        assert_eq!(clerk.calls, 2);
        assert_eq!(clerk.uncached_input(), 500);
        assert_eq!(clerk.cache_read(), 9_000);
        assert_eq!(summary.total.calls, 9);
        let philosopher = summary
            .by_role
            .iter()
            .find(|(r, _)| *r == CallRole::Model(ModelRole::Philosopher))
            .map(|(_, t)| *t)
            .unwrap();
        assert_eq!(philosopher.thinking(), Some(900), "from the bodies");
        assert_eq!(philosopher.thinking_unreported, 0);
        assert_eq!(
            philosopher
                .counts
                .cache_creation
                .map(|c| c.ephemeral_1h_input_tokens),
            Some(15_300),
            "the TTL split, from the bodies"
        );
        // The Clerk and the other seats have no body: thinking unreported.
        assert_eq!(summary.total.thinking(), Some(900));
        assert_eq!(summary.total.thinking_unreported, 6);
        assert_eq!(summary.total.unrecorded, 1);
        assert_eq!(summary.breaks.len(), 1, "{:?}", summary.breaks);
        assert_eq!(summary.breaks[0].role, CallRole::Model(ModelRole::Artist));
        assert_eq!(summary.breaks[0].call, 3);
        assert_eq!(
            summary.breaks[0].context,
            Some(UsageContext::AgendaItem(AgendaItemId::from(item_b)))
        );

        let text = summary.to_string();
        println!("{text}");
        assert!(text.contains("WARNING: 1 suspected cache break"));

        let empty = meeting_calls(&pool, CouncilMeetingId::from(uuid::Uuid::new_v4()))
            .await
            .unwrap();
        assert!(empty.is_empty());

        ts::close_pool(&pool).await;
        ts::drop_database(&admin, root, &database).await;
    }
}
