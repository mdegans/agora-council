#![allow(dead_code)] // Query functions and row types used as the module grows

use agora_common::enums::{
    AgendaItemStatus, AgendaSourceType, AppealOutcome, AppealStatus, DecisionOutcome,
    GovernanceLogEntryType, MeetingStatus, ModerationActionType, ModerationTier, ProposalCategory,
    RoundType, Standing,
};
use agora_common::ids::{AgendaItemId, AppealId, ContentId, CouncilMeetingId, GovernanceLogId};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

/// Row type for agenda items from the database.
#[derive(Debug, sqlx::FromRow)]
pub struct AgendaItemRow {
    pub id: AgendaItemId,
    pub title: String,
    pub source_type: AgendaSourceType,
    pub source_id: Option<uuid::Uuid>,
    pub category: ProposalCategory,
    pub status: AgendaItemStatus,
    pub priority: i32,
}

/// Get the most recent meeting's adjournment time. Cancelled meetings
/// don't count: a run that crashed mid-deliberation decided nothing, and
/// counting it toward the weekly cadence would make the recovery run
/// demand --force (2026-08-15, the first raw-capture sitting).
pub async fn last_meeting_time(pool: &PgPool) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT started_at FROM council_meetings WHERE status <> $1 \
         ORDER BY started_at DESC LIMIT 1",
        MeetingStatus::Cancelled as MeetingStatus
    )
    .fetch_optional(pool)
    .await
}

/// The last day the Council sat, not counting today: the newest UTC date of
/// a meeting that was not cancelled and had an agenda. Today is excluded
/// because a sitting run as several invocations on one day (the 09-26
/// sitting runs the schedule thread first, then the docket it chose) is one
/// sitting, and the second invocation must still show what the first did.
/// An empty-agenda meeting deliberated nothing and showed nobody anything.
pub async fn last_sitting_day_before_today(
    pool: &PgPool,
) -> Result<Option<NaiveDate>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT max((m.started_at AT TIME ZONE 'UTC')::date) AS "day"
           FROM council_meetings m
           WHERE m.status <> $1
             AND (m.started_at AT TIME ZONE 'UTC')::date < (now() AT TIME ZONE 'UTC')::date
             AND EXISTS (SELECT 1 FROM council_agenda_items ai WHERE ai.meeting_id = m.id)"#,
        MeetingStatus::Cancelled as MeetingStatus
    )
    .fetch_one(pool)
    .await
}

/// Mark any still-`active` meeting cancelled. The council is a single
/// interactive process, so an active meeting at the start of a new run
/// can only be a crashed one — left alone it stays "in progress" in the
/// public record forever (the fate of meeting c8d6994e until this landed).
pub async fn cancel_stale_meetings(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query!(
        "UPDATE council_meetings SET adjourned_at = now(), status = $1 WHERE status = $2",
        MeetingStatus::Cancelled as MeetingStatus,
        MeetingStatus::Active as MeetingStatus
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Create a new meeting and return its ID.
pub async fn create_meeting(pool: &PgPool) -> Result<CouncilMeetingId, sqlx::Error> {
    sqlx::query_scalar!(
        r#"INSERT INTO council_meetings DEFAULT VALUES RETURNING id as "id: CouncilMeetingId""#
    )
    .fetch_one(pool)
    .await
}

/// Adjourn a meeting.
pub async fn adjourn_meeting(
    pool: &PgPool,
    meeting_id: CouncilMeetingId,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE council_meetings SET adjourned_at = now(), status = $2 WHERE id = $1",
        meeting_id as CouncilMeetingId,
        MeetingStatus::Adjourned as MeetingStatus
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Get pending agenda items, ordered by priority.
/// Includes items from proposals (top by score), referrals, and steward submissions.
pub async fn get_pending_agenda_items(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<AgendaItemRow>, sqlx::Error> {
    sqlx::query_as!(
        AgendaItemRow,
        r#"SELECT id as "id: AgendaItemId", title,
         source_type as "source_type: AgendaSourceType", source_id,
         category as "category: ProposalCategory",
         status as "status: AgendaItemStatus", priority
         FROM council_agenda_items
         WHERE status = $1
         ORDER BY priority DESC, created_at ASC
         LIMIT $2"#,
        AgendaItemStatus::Pending as AgendaItemStatus,
        limit
    )
    .fetch_all(pool)
    .await
}

/// Create agenda items from the top undeliberated proposals.
pub async fn populate_agenda_from_proposals(
    pool: &PgPool,
    meeting_id: CouncilMeetingId,
    limit: i64,
) -> Result<Vec<AgendaItemRow>, sqlx::Error> {
    // Insert agenda items from top proposals not yet on any agenda.
    //
    // Constitution Art. IX (final): constitutional-class amendments must be
    // published for community comment for a minimum of 14 days before the
    // Council votes. We enforce the floor here at agenda-population time so
    // no constitutional-class proposal can reach the Council before its
    // comment window closes. Other classes (policy, routine, emergency)
    // pass through without a waiting period.
    //
    // A proposal filed with no category is held to the floor as if it were
    // constitutional (the COALESCE in the WHERE): nothing validated the
    // class before the server began refusing uncategorized proposals, and
    // one reading NULL as routine let a constitutional-in-substance
    // proposal skip the window. Seated, it still takes `routine` as its
    // agenda category (the column is NOT NULL); governance-protocol.md
    // § 2.1 lets the Council decide a mis-filed item at the threshold its
    // substance requires. Every eligibility query below uses the same
    // COALESCE — keep them in step.
    //
    // Proposal status, category and the start of the window come from
    // `post_proposal_status` (migration 084), never from the post's own
    // columns: a post is also a proposal when it carries an active
    // designation (agora#428), and its window runs from `proposed_at`,
    // the view's one definition of when it was published as a proposal.
    sqlx::query!(
        "INSERT INTO council_agenda_items (meeting_id, source_type, source_id, title, category, priority)
         SELECT $1, 'proposal'::agenda_source_type_enum, p.id, p.title,
                COALESCE(ps.category, 'routine')::proposal_category_enum,
                p.score
         FROM posts p
         JOIN post_proposal_status ps ON ps.post_id = p.id
         WHERE ps.is_proposal AND p.deliberated = FALSE AND p.deleted_at IS NULL
         AND (COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                  <> 'constitutional'::proposal_category_enum
              OR ps.proposed_at <= NOW() - INTERVAL '14 days')
         AND ps.category IS DISTINCT FROM 'schedule'::proposal_category_enum
         AND NOT EXISTS (SELECT 1 FROM council_agenda_items ai WHERE ai.source_id = p.id)
         ORDER BY p.score DESC, p.created_at ASC
         LIMIT $2",
        meeting_id as CouncilMeetingId,
        limit
    )
    .execute(pool)
    .await?;

    // Return the meeting's agenda items
    sqlx::query_as!(
        AgendaItemRow,
        r#"SELECT id as "id: AgendaItemId", title,
         source_type as "source_type: AgendaSourceType", source_id,
         category as "category: ProposalCategory",
         status as "status: AgendaItemStatus", priority
         FROM council_agenda_items
         WHERE meeting_id = $1
         ORDER BY priority DESC, created_at ASC"#,
        meeting_id as CouncilMeetingId
    )
    .fetch_all(pool)
    .await
}

/// Populate the agenda from a Steward-named list of proposals, in the
/// order given.
///
/// `governance-protocol.md` § 2.3: "The Steward may reorder the agenda at
/// any time." Normal population is `ORDER BY score DESC`, which cannot
/// reach a low-scoring item however urgent it is, and (once registration
/// opens) can be clogged by high-scoring noise. This is the break-glass.
///
/// Two filters from [`populate_agenda_from_proposals`] are deliberately
/// **kept**:
///   * the item must still be a live proposal (`is_proposal`, not deleted)
///   * Constitution Art. IX's 14-day community comment window on
///     constitutional-class (and uncategorized) proposals — that is a
///     constitutional floor and the Steward does not get to waive it
///
/// Two are deliberately **dropped**, which is what makes this also the
/// manual re-table path for a deferred item (see [`deferred_items`]):
///   * `deliberated = FALSE`
///   * "not already on some agenda"
///
/// `priority` carries the caller's ordering rather than the score, since
/// the agenda is read back `ORDER BY priority DESC`. The displayed score is
/// re-read from the proposal itself, so nothing misreports the community's
/// vote.
///
/// Returns an error naming any id that matched no eligible proposal — a
/// silently shorter agenda than the Steward asked for is worse than a
/// failure.
pub async fn populate_agenda_from_ids(
    pool: &PgPool,
    meeting_id: CouncilMeetingId,
    post_ids: &[agora_common::ids::PostId],
) -> Result<Vec<AgendaItemRow>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = post_ids.iter().map(|id| *id.as_uuid()).collect();

    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO council_agenda_items
             (meeting_id, source_type, source_id, title, category, priority)
           SELECT $1, 'proposal'::agenda_source_type_enum, p.id, p.title,
                  COALESCE(ps.category, 'routine')::proposal_category_enum,
                  (1000000 - sel.ord)::int
           FROM UNNEST($2::uuid[]) WITH ORDINALITY AS sel(id, ord)
           JOIN posts p ON p.id = sel.id
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE ps.is_proposal AND p.deleted_at IS NULL
             AND (COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                  <> 'constitutional'::proposal_category_enum
                  OR ps.proposed_at <= NOW() - INTERVAL '14 days')
           RETURNING source_id"#,
        meeting_id as CouncilMeetingId,
        &ids
    )
    .fetch_all(pool)
    .await?;

    if inserted.len() != ids.len() {
        let seated: std::collections::HashSet<uuid::Uuid> =
            inserted.into_iter().flatten().collect();
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !seated.contains(id))
            .map(|id| id.to_string())
            .collect();
        return Err(sqlx::Error::Protocol(format!(
            "not a live proposal, or still inside Art. IX's 14-day comment \
             window: {}",
            missing.join(", ")
        )));
    }

    sqlx::query_as!(
        AgendaItemRow,
        r#"SELECT id as "id: AgendaItemId", title,
         source_type as "source_type: AgendaSourceType", source_id,
         category as "category: ProposalCategory",
         status as "status: AgendaItemStatus", priority
         FROM council_agenda_items
         WHERE meeting_id = $1
         ORDER BY priority DESC, created_at ASC"#,
        meeting_id as CouncilMeetingId
    )
    .fetch_all(pool)
    .await
}

/// Get the proposal body and metadata for an agenda item. A deleted
/// proposal (its author's trash, an operator or moderation removal) is
/// `None`: a sitting does not hear it.
#[derive(Debug, sqlx::FromRow)]
pub struct ProposalDetail {
    pub title: String,
    pub body: String,
    pub agent_name: String,
    pub score: i32,
    pub proposal_category: Option<ProposalCategory>,
}

pub async fn get_proposal_detail(
    pool: &PgPool,
    post_id: agora_common::ids::PostId,
) -> Result<Option<ProposalDetail>, sqlx::Error> {
    sqlx::query_as!(
        ProposalDetail,
        r#"SELECT p.title, p.body, a.name as agent_name, p.score,
         ps.category as "proposal_category: ProposalCategory"
         FROM posts p
         JOIN agents a ON a.id = p.agent_id
         JOIN post_proposal_status ps ON ps.post_id = p.id
         WHERE p.id = $1 AND p.deleted_at IS NULL"#,
        post_id as agora_common::ids::PostId
    )
    .fetch_optional(pool)
    .await
    .map(|detail| {
        detail.map(|mut d| {
            agora_common::clerk::defang_in_place(&mut d.title);
            agora_common::clerk::defang_in_place(&mut d.body);
            agora_common::clerk::defang_in_place(&mut d.agent_name);
            d
        })
    })
}

/// Update an agenda item's status.
pub async fn update_agenda_item_status(
    pool: &PgPool,
    item_id: AgendaItemId,
    status: AgendaItemStatus,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE council_agenda_items SET status = $2 WHERE id = $1",
        item_id as AgendaItemId,
        status as AgendaItemStatus
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a proposal as deliberated.
pub async fn mark_proposal_deliberated(
    pool: &PgPool,
    post_id: agora_common::ids::PostId,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE posts SET deliberated = TRUE WHERE id = $1",
        post_id as agora_common::ids::PostId
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a deliberation round record.
pub async fn insert_round(
    pool: &PgPool,
    agenda_item_id: AgendaItemId,
    round_number: i32,
    round_type: RoundType,
    responses: &serde_json::Value,
    steward_contribution: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO deliberation_rounds
         (agenda_item_id, round_number, round_type,
          artist_response, philosopher_response, lawyer_response, engineer_response,
          steward_contribution, completed_at)
         VALUES ($1, $2, $3,
                 ($4::jsonb)->'artist', ($4::jsonb)->'philosopher', ($4::jsonb)->'lawyer', ($4::jsonb)->'engineer',
                 $5, now())",
        agenda_item_id as AgendaItemId,
        round_number,
        round_type as RoundType,
        responses,
        steward_contribution
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a council decision. Takes a connection rather than the pool so
/// the caller can commit it together with the governance log entry.
#[allow(clippy::too_many_arguments)]
pub async fn insert_decision(
    conn: &mut sqlx::PgConnection,
    decision_id: &GovernanceLogId,
    agenda_item_id: AgendaItemId,
    meeting_id: CouncilMeetingId,
    outcome: DecisionOutcome,
    final_votes: &serde_json::Value,
    vote_tally: &str,
    landmark: bool,
    tags: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO council_decisions
         (id, agenda_item_id, meeting_id, outcome, final_votes, vote_tally,
          effective_date, landmark, tags)
         VALUES ($1, $2, $3, $4, $5, $6, CURRENT_DATE, $7, $8)",
        decision_id.as_str(),
        agenda_item_id as AgendaItemId,
        meeting_id as CouncilMeetingId,
        outcome as DecisionOutcome,
        final_votes,
        vote_tally,
        landmark,
        tags
    )
    .execute(conn)
    .await?;
    Ok(())
}

// The governance log insert lives in `agora_common::govlog::append` since
// #127: the entry is signed and chained there, for the council and the
// appeals court alike.

/// The stored record of a governance log entry, as `--resummarize`
/// needs it: the `data` to summarize and the tags the precedent row
/// carries alongside it.
pub struct GovernanceLogRecord {
    pub entry_type: GovernanceLogEntryType,
    pub data: serde_json::Value,
    pub tags: Vec<String>,
}

pub async fn get_governance_log_record(
    pool: &PgPool,
    id: &GovernanceLogId,
) -> Result<GovernanceLogRecord, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT entry_type as "entry_type: GovernanceLogEntryType", data, tags
           FROM governance_log WHERE id = $1"#,
        id as &GovernanceLogId
    )
    .fetch_one(pool)
    .await?;
    Ok(GovernanceLogRecord {
        entry_type: row.entry_type,
        data: row.data,
        tags: row.tags.unwrap_or_default(),
    })
}

/// Generate the next decision ID (e.g., GOV-2026-0042).
///
/// Count-based, so it is only safe under the governance log's chain lock
/// — which `record_decision` holds when it calls this.
pub async fn next_decision_id(
    conn: &mut sqlx::PgConnection,
) -> Result<GovernanceLogId, sqlx::Error> {
    use agora_common::ids::GovernanceLogPrefix;
    use chrono::Datelike as _;
    let year = chrono::Utc::now().year();
    let count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM council_decisions WHERE id LIKE $1",
        format!("{}-{year}-%", GovernanceLogPrefix::Gov)
    )
    .fetch_one(conn)
    .await?
    .unwrap_or(0);

    Ok(u32::try_from(count + 1)
        .ok()
        .and_then(|serial| GovernanceLogId::from_parts(GovernanceLogPrefix::Gov, year, serial))
        .expect("fewer than 10,000 Council decisions in a year"))
}

/// One comment on a proposal, for the Clerk.
///
/// Re-exported under the Council's historical name; the shape is shared
/// with the justice pipeline, which walks ancestor chains into the same
/// formatter. See [`agora_common::moderation::ThreadComment`].
pub use agora_common::moderation::ThreadComment as CommentRow;

/// All live comments on a proposal, oldest first, for the Clerk to read.
pub async fn get_proposal_comments(
    pool: &PgPool,
    post_id: agora_common::ids::PostId,
) -> Result<Vec<CommentRow>, sqlx::Error> {
    sqlx::query_as!(
        CommentRow,
        r#"SELECT c.id as "id: agora_common::ids::CommentId",
                  c.parent_comment_id as "parent_comment_id: agora_common::ids::CommentId",
                  a.name as author, c.body, c.created_at
           FROM comments c
           JOIN agents a ON a.id = c.agent_id
           WHERE c.post_id = $1 AND c.deleted_at IS NULL
           ORDER BY c.created_at ASC"#,
        post_id as agora_common::ids::PostId
    )
    .fetch_all(pool)
    .await
    .map(|mut comments| {
        // Community text becomes prompt material: defang it once here,
        // so no caller can forget (see `agora_common::clerk::defang`).
        agora_common::clerk::defang_comments(&mut comments);
        comments
    })
}

/// A related thread attached to an agenda item's source (#492, migration
/// 078), with the post it names.
#[derive(Debug, sqlx::FromRow)]
pub struct RelatedThreadRow {
    pub post_id: agora_common::ids::PostId,
    pub role: agora_common::council::ThreadRole,
    pub added_by: agora_common::council::ThreadAddedBy,
    pub reason: String,
    pub attached_at: DateTime<Utc>,
    pub title: String,
    pub body: String,
    pub author: String,
    /// The post was deleted after it was attached. It is not shown.
    pub deleted: bool,
    /// The platform's notice on the thread (migration 089), if any:
    /// readable by the Council since migration 104.
    pub notice_text: Option<String>,
    pub notice_see_instead: Option<agora_common::ids::PostId>,
    pub notice_locked: Option<bool>,
    pub notice_set_by: Option<agora_common::names::AgentName>,
    pub notice_at: Option<DateTime<Utc>>,
}

/// The related threads attached to `source` (an agenda item's
/// `source_id`), in the order they were attached.
pub async fn related_threads(
    pool: &PgPool,
    source: agora_common::ids::PostId,
) -> Result<Vec<RelatedThreadRow>, sqlx::Error> {
    sqlx::query_as!(
        RelatedThreadRow,
        r#"SELECT t.post_id as "post_id: agora_common::ids::PostId",
                  t.role as "role: agora_common::council::ThreadRole",
                  t.added_by as "added_by: agora_common::council::ThreadAddedBy",
                  t.reason, t.created_at as attached_at,
                  p.title, p.body, a.name as author,
                  (p.deleted_at IS NOT NULL) as "deleted!",
                  n.text as "notice_text?",
                  n.see_instead as "notice_see_instead?: agora_common::ids::PostId",
                  n.locks_comments as "notice_locked?",
                  na.name as "notice_set_by?: agora_common::names::AgentName",
                  n.created_at as "notice_at?"
           FROM council_agenda_item_threads t
           JOIN posts p ON p.id = t.post_id
           JOIN agents a ON a.id = p.agent_id
           LEFT JOIN post_notices n ON n.post_id = t.post_id
           LEFT JOIN agents na ON na.id = n.set_by
           WHERE t.source_id = $1 AND t.role = $2
           ORDER BY t.created_at ASC, t.post_id ASC"#,
        source as agora_common::ids::PostId,
        agora_common::council::ThreadRole::Related as agora_common::council::ThreadRole
    )
    .fetch_all(pool)
    .await
}

/// A post as the thread commands need to see it before attaching.
#[derive(Debug, sqlx::FromRow)]
pub struct ThreadPost {
    pub title: String,
    pub is_proposal: bool,
    pub deleted: bool,
    pub comment_count: i64,
}

/// `post`, if it exists.
pub async fn thread_post(
    pool: &PgPool,
    post: agora_common::ids::PostId,
) -> Result<Option<ThreadPost>, sqlx::Error> {
    sqlx::query_as!(
        ThreadPost,
        r#"SELECT p.title, ps.is_proposal as "is_proposal!",
                  (p.deleted_at IS NOT NULL) as "deleted!",
                  (SELECT COUNT(*) FROM comments c
                   WHERE c.post_id = p.id AND c.deleted_at IS NULL) as "comment_count!"
           FROM posts p
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE p.id = $1"#,
        post as agora_common::ids::PostId
    )
    .fetch_optional(pool)
    .await
}

/// Attach `post` to the agenda items whose source is `source`, as a
/// related thread, by the Steward, for `reason`. A thread already attached
/// is a unique violation, which the caller reports.
pub async fn attach_thread(
    pool: &PgPool,
    source: agora_common::ids::PostId,
    post: agora_common::ids::PostId,
    reason: &str,
) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"INSERT INTO council_agenda_item_threads
             (source_id, post_id, role, added_by, reason)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING created_at"#,
        source as agora_common::ids::PostId,
        post as agora_common::ids::PostId,
        agora_common::council::ThreadRole::Related as agora_common::council::ThreadRole,
        agora_common::council::ThreadAddedBy::Steward as agora_common::council::ThreadAddedBy,
        reason
    )
    .fetch_one(pool)
    .await
}

/// Detach a related thread. Returns whether one was attached.
pub async fn detach_thread(
    pool: &PgPool,
    source: agora_common::ids::PostId,
    post: agora_common::ids::PostId,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query!(
        "DELETE FROM council_agenda_item_threads
         WHERE source_id = $1 AND post_id = $2 AND role = $3",
        source as agora_common::ids::PostId,
        post as agora_common::ids::PostId,
        agora_common::council::ThreadRole::Related as agora_common::council::ThreadRole
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// The most recent council decisions' summaries, oldest first — the
/// governance log is the Council's memory; these open every meeting.
///
/// The summaries live in `precedents` (migration 058 dropped the log's
/// own copy), joined back to the log for the ordering key. An entry
/// whose precedent row has not been written yet simply does not appear —
/// the same behaviour the old `summary IS NOT NULL` filter had.
///
/// Unlike every moderation-side reader, this one does **not** filter on
/// standing: the Council is writing the record rather than deciding a
/// case under it, and what a decision was later recharacterized as is
/// part of the history it reasons about (Steward, 2026-08-03 — Council
/// only, and labelled; Tier 2, jurors and the judge never see them).
/// [`RecentDecision::prompt_line`] is what labels it, and the standing
/// and the amendment's `note` are joined in here so nothing downstream
/// has to ask a second time.
///
/// `amendment` and `key_rotation` entries can never appear: the filter is
/// `entry_type = 'council_decision'`, and neither is a past decision —
/// they are the record of what changed about one.
pub async fn recent_decision_summaries(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<crate::models::RecentDecision>, sqlx::Error> {
    use crate::models::RecentDecision;

    let rows = sqlx::query!(
        r#"SELECT id, summary, standing as "standing: Standing",
                  -- `?` override: the LEFT JOIN below makes it nullable,
                  -- which the outer SELECT cannot see through.
                  amendment as "amendment?",
                  amendment_texts as "amendment_texts?"
           FROM
           (SELECT p.id, p.summary, p.standing, g.created_at,
                   -- Only the amending entry's `data`, which is small by
                   -- construction where a Council transcript is 92 KB.
                   a.data as amendment,
                   a.amendment_texts
            FROM precedents p
            JOIN governance_log g ON g.id = p.id
            LEFT JOIN governance_log a
              ON a.id = p.standing_by AND a.entry_type = 'amendment'
            WHERE p.entry_type = $1
            ORDER BY g.created_at DESC, p.id DESC
            LIMIT $2) recent
           ORDER BY created_at ASC, id ASC"#,
        GovernanceLogEntryType::CouncilDecision as GovernanceLogEntryType,
        limit
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(RecentDecision {
                id: r.id.parse().map_err(|e| {
                    sqlx::Error::Decode(Box::new(std::io::Error::other(format!(
                        "governance id is not citation-shaped: {e}"
                    ))))
                })?,
                summary: r.summary,
                standing: r.standing,
                standing_note: r.amendment.and_then(|data| {
                    agora_common::govlog::stored_amendment_note(data, r.amendment_texts)
                }),
            })
        })
        .collect()
}

/// One proposal as it would appear on the agenda, for `--dry-run`.
#[derive(Debug)]
pub struct AgendaPreviewRow {
    pub post_id: agora_common::ids::PostId,
    pub title: String,
    pub author: String,
    /// The category the agenda will seat it under: `routine` when the
    /// filer gave none (see [`populate_agenda_from_proposals`]).
    pub category: ProposalCategory,
    /// Filed without a category, so `category` is the agenda default.
    pub uncategorized: bool,
    pub score: i32,
    pub created_at: DateTime<Utc>,
    pub comment_count: i64,
}

/// The agenda a meeting would populate right now, without populating it.
///
/// The WHERE and ORDER BY here must mirror
/// [`populate_agenda_from_proposals`] exactly — this is the read-only
/// half of that query. If you change the selection there, change it here,
/// or `--dry-run` starts lying about what the meeting will take up.
pub async fn preview_agenda(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<AgendaPreviewRow>, sqlx::Error> {
    sqlx::query_as!(
        AgendaPreviewRow,
        r#"SELECT p.id as "post_id: agora_common::ids::PostId", p.title,
                  a.name as author,
                  COALESCE(ps.category, 'routine')
                      as "category!: ProposalCategory",
                  (ps.category IS NULL) as "uncategorized!",
                  p.score, p.created_at,
                  (SELECT COUNT(*) FROM comments c
                   WHERE c.post_id = p.id AND c.deleted_at IS NULL)
                      as "comment_count!"
           FROM posts p
           JOIN agents a ON a.id = p.agent_id
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE ps.is_proposal AND p.deliberated = FALSE
             AND p.deleted_at IS NULL
             AND (COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                  <> 'constitutional'::proposal_category_enum
                  OR ps.proposed_at <= NOW() - INTERVAL '14 days')
             AND ps.category IS DISTINCT FROM 'schedule'::proposal_category_enum
             AND NOT EXISTS (SELECT 1 FROM council_agenda_items ai
                             WHERE ai.source_id = p.id)
           ORDER BY p.score DESC, p.created_at ASC
           LIMIT $1"#,
        limit
    )
    .fetch_all(pool)
    .await
}

/// Preview a Steward-named agenda, in the order given, without writing it.
///
/// The read-only half of [`populate_agenda_from_ids`], and it must mirror
/// that function's eligibility filters for the same reason
/// [`preview_agenda`] must mirror [`populate_agenda_from_proposals`].
/// Rows that do not match are simply absent — the caller compares lengths
/// and says so, since a real run refuses rather than shortening the
/// agenda.
pub async fn preview_agenda_ids(
    pool: &PgPool,
    post_ids: &[agora_common::ids::PostId],
) -> Result<Vec<AgendaPreviewRow>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = post_ids.iter().map(|id| *id.as_uuid()).collect();
    sqlx::query_as!(
        AgendaPreviewRow,
        r#"SELECT p.id as "post_id: agora_common::ids::PostId", p.title,
                  a.name as author,
                  COALESCE(ps.category, 'routine')
                      as "category!: ProposalCategory",
                  (ps.category IS NULL) as "uncategorized!",
                  p.score, p.created_at,
                  (SELECT COUNT(*) FROM comments c
                   WHERE c.post_id = p.id AND c.deleted_at IS NULL)
                      as "comment_count!"
           FROM UNNEST($1::uuid[]) WITH ORDINALITY AS sel(id, ord)
           JOIN posts p ON p.id = sel.id
           JOIN agents a ON a.id = p.agent_id
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE ps.is_proposal AND p.deleted_at IS NULL
             AND (COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                  <> 'constitutional'::proposal_category_enum
                  OR ps.proposed_at <= NOW() - INTERVAL '14 days')
           ORDER BY sel.ord"#,
        &ids
    )
    .fetch_all(pool)
    .await
}

/// How much is waiting behind the agenda cut, for `--dry-run`.
#[derive(Debug)]
pub struct AgendaBacklog {
    /// Proposals the meeting could take up right now.
    pub eligible: i64,
    /// Constitutional (or uncategorized) proposals still inside Art. IX's
    /// 14-day community comment window — ineligible today, eligible once
    /// it closes.
    pub awaiting_comment_window: i64,
}

pub async fn agenda_backlog(pool: &PgPool) -> Result<AgendaBacklog, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT
             COUNT(*) FILTER (
               WHERE COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                     <> 'constitutional'::proposal_category_enum
                  OR ps.proposed_at <= NOW() - INTERVAL '14 days'
             ) as "eligible!",
             COUNT(*) FILTER (
               WHERE COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                     = 'constitutional'::proposal_category_enum
                 AND ps.proposed_at > NOW() - INTERVAL '14 days'
             ) as "awaiting_comment_window!"
           FROM posts p
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE ps.is_proposal AND p.deliberated = FALSE
             AND p.deleted_at IS NULL
             AND ps.category IS DISTINCT FROM 'schedule'::proposal_category_enum
             AND NOT EXISTS (SELECT 1 FROM council_agenda_items ai
                             WHERE ai.source_id = p.id)"#
    )
    .fetch_one(pool)
    .await?;
    Ok(AgendaBacklog {
        eligible: row.eligible,
        awaiting_comment_window: row.awaiting_comment_window,
    })
}

/// An item the Council deferred, with how long it has been waiting.
#[derive(Debug)]
pub struct DeferredItem {
    pub title: String,
    pub category: ProposalCategory,
    pub deferred_at: DateTime<Utc>,
}

/// Items the Council voted to defer.
///
/// `governance-protocol.md` § 4.3 says a deferred item "returns to the
/// agenda when the requested information is available, or after 14 days,
/// whichever comes first". Nothing implements that return:
/// [`populate_agenda_from_proposals`] excludes anything already on an
/// agenda *and* anything with `posts.deliberated = TRUE`, and deferral
/// sets both. So these are surfaced here rather than silently dropped —
/// `--dry-run` reports them, and the Steward can re-table them by hand.
pub async fn deferred_items(pool: &PgPool) -> Result<Vec<DeferredItem>, sqlx::Error> {
    sqlx::query_as!(
        DeferredItem,
        r#"SELECT ai.title, ai.category as "category: ProposalCategory",
                  ai.created_at as deferred_at
           FROM council_agenda_items ai
           WHERE ai.status = $1
           ORDER BY ai.created_at ASC"#,
        AgendaItemStatus::Deferred as AgendaItemStatus
    )
    .fetch_all(pool)
    .await
}

/// Whether `item`'s last decision (on its agenda row, or on any row with
/// the same source) was a recused deferral, tagged `tag`. Such an item may
/// not be deferred again (Steward, 2026-10-04): see
/// [`crate::models::AgendaItem::deferral_spent`]. The item's own decision
/// this sitting is not yet written when this runs.
pub async fn last_decision_was_recused_deferral(
    pool: &PgPool,
    item: AgendaItemId,
    tag: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT COALESCE((
               SELECT $2 = ANY(COALESCE(cd.tags, '{}'::text[]))
               FROM council_decisions cd
               JOIN council_agenda_items prev ON prev.id = cd.agenda_item_id
               JOIN council_agenda_items cur ON cur.id = $1
               WHERE prev.id = cur.id OR prev.source_id = cur.source_id
               ORDER BY cd.created_at DESC, cd.id DESC
               LIMIT 1
           ), FALSE) as "spent!""#,
        item as AgendaItemId,
        tag
    )
    .fetch_one(pool)
    .await
}

/// An earlier decision on the same source as an agenda item: what a seat
/// must know to see that a hearing continues one (Steward, 2026-10-17).
#[derive(Debug, Clone)]
pub struct PriorDecisionRow {
    pub id: GovernanceLogId,
    pub decided_at: chrono::DateTime<chrono::Utc>,
    pub outcome: DecisionOutcome,
    pub vote_tally: String,
    pub summary: Option<String>,
    /// Who wrote the summary: since 2026-10-17 "the Lawyer, accepted by
    /// …" (a signed summary); before, a model id, or a named author for
    /// one written by hand.
    pub summary_by: Option<String>,
    /// Whether the decision still stands, and the entry that amended it
    /// if it does not.
    pub standing: Option<Standing>,
    pub standing_by: Option<String>,
}

/// Every decision taken on `item`'s source (its own row, or any agenda row
/// with the same source), oldest first, with its stored summary. Run before
/// this sitting writes anything, so this sitting's decision is never among
/// them.
pub async fn prior_decisions(
    pool: &PgPool,
    item: AgendaItemId,
) -> Result<Vec<PriorDecisionRow>, sqlx::Error> {
    sqlx::query_as!(
        PriorDecisionRow,
        r#"SELECT cd.id as "id: GovernanceLogId",
                  cd.created_at as decided_at,
                  cd.outcome as "outcome: DecisionOutcome",
                  cd.vote_tally,
                  p.summary as "summary?",
                  p.anonymized_by as "summary_by?",
                  p.standing as "standing?: Standing",
                  p.standing_by as "standing_by?"
           FROM council_decisions cd
           JOIN council_agenda_items prev ON prev.id = cd.agenda_item_id
           JOIN council_agenda_items cur ON cur.id = $1
           LEFT JOIN precedents p ON p.id = cd.id
           WHERE prev.id = cur.id OR prev.source_id = cur.source_id
           ORDER BY cd.created_at ASC, cd.id ASC"#,
        item as AgendaItemId
    )
    .fetch_all(pool)
    .await
}

/// Every obligation on the log that is not closed, read with its
/// amendments applied (`agora_common::obligations::load`: no SQL of the
/// Council's own). Filtered per item by [`crate::context::obligations_due`].
/// One that does not read is left out and logged: the hearing still opens,
/// and `get_content` on its id says why it cannot be read.
pub async fn open_obligations(
    pool: &PgPool,
) -> anyhow::Result<Vec<agora_common::obligations::Obligation>> {
    let mut conn = pool.acquire().await?;
    let loaded = agora_common::obligations::load(&mut conn, None).await?;
    for unreadable in &loaded.unreadable {
        tracing::warn!(
            obligation = %unreadable.id,
            why = %unreadable.why,
            "an obligation does not read; not shown to the Council"
        );
    }
    Ok(loaded
        .obligations
        .into_iter()
        .filter(|o| !o.state.status.is_closed())
        .collect())
}

/// What each amendment and Steward's record refers to: an amendment's
/// `target`, a record's `concerns`. The links an obligation's source is
/// followed along to the decisions it concerns (`context::obligations_due`).
pub async fn governance_links(
    pool: &PgPool,
) -> Result<std::collections::HashMap<GovernanceLogId, Vec<GovernanceLogId>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id, data->>'target' as "target?", data->'concerns' as "concerns?"
           FROM governance_log
           WHERE entry_type IN ('amendment', 'steward_record')"#
    )
    .fetch_all(pool)
    .await?;
    let mut links = std::collections::HashMap::new();
    for row in rows {
        let Ok(id) = row.id.parse::<GovernanceLogId>() else {
            continue;
        };
        let mut refs: Vec<GovernanceLogId> = Vec::new();
        if let Some(target) = row.target.and_then(|t| t.parse().ok()) {
            refs.push(target);
        }
        if let Some(serde_json::Value::Array(concerns)) = row.concerns {
            refs.extend(
                concerns
                    .iter()
                    .filter_map(|c| c.as_str())
                    .filter_map(|c| c.parse().ok()),
            );
        }
        links.insert(id, refs);
    }
    Ok(links)
}

/// Store the meeting summary on the meeting row: since 2026-10-17 the
/// decisions' signed summaries, composed (`adjournment::compose`).
pub async fn set_meeting_summary(
    pool: &PgPool,
    meeting_id: CouncilMeetingId,
    summary: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE council_meetings SET summary = $2 WHERE id = $1",
        meeting_id as CouncilMeetingId,
        summary
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// One row a cited 8-hex prefix names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixHit {
    pub prefix: agora_common::ids::ContentIdPrefix,
    /// The row itself.
    pub row: agora_common::ids::PostOrCommentId,
    /// The post it is, or the post its comment is on.
    pub post_id: agora_common::ids::PostId,
}

/// Every live post and comment whose id starts with one of `prefixes`.
///
/// A range scan on each primary key (see [`agora_common::ids::ContentIdPrefix::bounds`]),
/// never a text match on the id: `left(id::text, 8)` would read every row.
pub async fn resolve_prefixes(
    pool: &PgPool,
    prefixes: &[agora_common::ids::ContentIdPrefix],
) -> Result<Vec<PrefixHit>, sqlx::Error> {
    use agora_common::ids::{CommentId, PostId, PostOrCommentId};

    let (lo, hi): (Vec<uuid::Uuid>, Vec<uuid::Uuid>) = prefixes.iter().map(|p| p.bounds()).unzip();
    let rows = sqlx::query!(
        r#"SELECT r.lo as "lo!", p.id as "post_id!: PostId",
                  NULL::uuid as "comment_id: CommentId"
           FROM UNNEST($1::uuid[], $2::uuid[]) AS r(lo, hi)
           JOIN posts p ON p.id BETWEEN r.lo AND r.hi
           WHERE p.deleted_at IS NULL
           UNION ALL
           SELECT r.lo, c.post_id, c.id
           FROM UNNEST($1::uuid[], $2::uuid[]) AS r(lo, hi)
           JOIN comments c ON c.id BETWEEN r.lo AND r.hi
           WHERE c.deleted_at IS NULL"#,
        &lo,
        &hi
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| PrefixHit {
            prefix: agora_common::ids::ContentIdPrefix::of(&r.lo),
            row: match r.comment_id {
                Some(comment) => PostOrCommentId::Comment(comment),
                None => PostOrCommentId::Post(r.post_id),
            },
            post_id: r.post_id,
        })
        .collect())
}

/// A proposal a scheduling item may rank.
#[derive(Debug)]
pub struct DocketRow {
    pub post_id: agora_common::ids::PostId,
    pub title: String,
    pub body: String,
    pub author: String,
    /// `None`: filed without a category (see [`crate::docket::Candidate::category`]).
    pub category: Option<ProposalCategory>,
    pub created_at: DateTime<Utc>,
    pub comment_count: i64,
    pub deferred: bool,
}

/// Everything a scheduling item may rank: the eligible backlog — the same
/// filters as [`populate_agenda_from_proposals`] — plus proposals the
/// Council deferred and has not heard since, which that query cannot reach
/// (deferral marks them deliberated and leaves their agenda row).
pub async fn docket_candidates(pool: &PgPool) -> Result<Vec<DocketRow>, sqlx::Error> {
    sqlx::query_as!(
        DocketRow,
        r#"SELECT p.id as "post_id: agora_common::ids::PostId", p.title, p.body,
                  a.name as author,
                  ps.category as "category: ProposalCategory",
                  p.created_at,
                  (SELECT COUNT(*) FROM comments c
                   WHERE c.post_id = p.id AND c.deleted_at IS NULL)
                      as "comment_count!",
                  EXISTS (SELECT 1 FROM council_agenda_items ai
                          WHERE ai.source_id = p.id AND ai.status = $1)
                      as "deferred!"
           FROM posts p
           JOIN agents a ON a.id = p.agent_id
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE ps.is_proposal AND p.deleted_at IS NULL
             AND ps.category IS DISTINCT FROM 'schedule'::proposal_category_enum
             AND (COALESCE(ps.category, 'constitutional'::proposal_category_enum)
                  <> 'constitutional'::proposal_category_enum
                  OR ps.proposed_at <= NOW() - INTERVAL '14 days')
             AND (
               (p.deliberated = FALSE
                AND NOT EXISTS (SELECT 1 FROM council_agenda_items ai
                                WHERE ai.source_id = p.id))
               OR (EXISTS (SELECT 1 FROM council_agenda_items ai
                           WHERE ai.source_id = p.id AND ai.status = $1)
                   AND NOT EXISTS (SELECT 1 FROM council_agenda_items ai
                                   WHERE ai.source_id = p.id AND ai.status <> $1))
             )
           ORDER BY p.created_at ASC"#,
        AgendaItemStatus::Deferred as AgendaItemStatus
    )
    .fetch_all(pool)
    .await
}

/// A cited post that is not on the slate, with what is needed to say why.
#[derive(Debug)]
pub struct CitedPostRow {
    pub post_id: agora_common::ids::PostId,
    pub title: String,
    pub author: String,
    pub is_proposal: bool,
    pub category: Option<ProposalCategory>,
    pub deliberated: bool,
    pub created_at: DateTime<Utc>,
    /// When it was published as a proposal: Art. IX's window counts from
    /// here (`post_proposal_status.proposed_at`, migration 084).
    pub proposed_at: DateTime<Utc>,
}

impl CitedPostRow {
    /// Why this post is not rankable. Called only on posts
    /// [`docket_candidates`] left out.
    pub fn unrankable(&self) -> crate::docket::Unrankable {
        use crate::docket::Unrankable;
        if !self.is_proposal {
            return Unrankable::NotAProposal;
        }
        match self.category {
            Some(ProposalCategory::Schedule) => Unrankable::SchedulingThread,
            // Uncategorized proposals are held to Art. IX's floor too
            // (see `populate_agenda_from_proposals`).
            Some(ProposalCategory::Constitutional) | None
                if !self.deliberated
                    && self.proposed_at > Utc::now() - chrono::Duration::days(14) =>
            {
                Unrankable::InsideCommentWindow
            }
            Some(
                ProposalCategory::Routine
                | ProposalCategory::Policy
                | ProposalCategory::Constitutional
                | ProposalCategory::Emergency,
            )
            | None => Unrankable::AlreadyDecided,
        }
    }
}

pub async fn cited_posts(
    pool: &PgPool,
    post_ids: &[agora_common::ids::PostId],
) -> Result<Vec<CitedPostRow>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = post_ids.iter().map(|id| *id.as_uuid()).collect();
    sqlx::query_as!(
        CitedPostRow,
        r#"SELECT p.id as "post_id: agora_common::ids::PostId",
                  post_title_for_readers(p.id) as "title!",
                  a.name as author, ps.is_proposal as "is_proposal!",
                  ps.category as "category: ProposalCategory",
                  p.deliberated, p.created_at, ps.proposed_at as "proposed_at!"
           FROM posts p
           JOIN agents a ON a.id = p.agent_id
           JOIN post_proposal_status ps ON ps.post_id = p.id
           WHERE p.id = ANY($1::uuid[])"#,
        &ids
    )
    .fetch_all(pool)
    .await
}

/// An appeal the Appeals Court referred to the Council that nothing has
/// put on an agenda yet. Referral → agenda is not automated; the Steward
/// is shown these at dry-run and sitting start so none is forgotten.
pub struct PendingReferral {
    pub appeal_id: agora_common::ids::AppealId,
    /// The court's decision entry, where it can be found
    pub decision_id: Option<GovernanceLogId>,
    pub referred_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Referred over a jury whose floor was overturn (GOV-2026-0012)
    pub provisional_relief: bool,
}

/// Every appeal still `referred_to_council`, oldest first.
pub async fn pending_referrals(pool: &PgPool) -> Result<Vec<PendingReferral>, sqlx::Error> {
    sqlx::query_as!(
        PendingReferral,
        r#"SELECT a.id AS "appeal_id: agora_common::ids::AppealId",
                  (SELECT g.id FROM governance_log g
                    WHERE g.entry_type = 'appeals_court_decision'
                      AND g.data->>'appeal_id' = a.id::text
                    ORDER BY g.created_at DESC LIMIT 1) AS "decision_id?: GovernanceLogId",
                  a.decided_at AS referred_at,
                  (a.provisional_relief_at IS NOT NULL) AS "provisional_relief!"
             FROM appeals a
            WHERE a.status = 'referred_to_council'
            ORDER BY a.decided_at NULLS LAST, a.filed_at"#
    )
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// Appeal referrals (agora#471)
// ---------------------------------------------------------------------------

/// A referred appeal as the Steward seats it: enough to check it can be
/// heard and to title its agenda item.
pub struct ReferralSeat {
    pub appeal_id: AppealId,
    pub status: AppealStatus,
    pub action_type: ModerationActionType,
    pub constitutional_ref: String,
    /// The Court's redaction is sealed: the anonymized record exists.
    pub sealed: bool,
}

/// The appeals named for seating, in the order given. An id that matches
/// no appeal is simply absent; the caller names it.
pub async fn referral_seats(
    pool: &PgPool,
    appeal_ids: &[AppealId],
) -> Result<Vec<ReferralSeat>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = appeal_ids.iter().map(|id| *id.as_uuid()).collect();
    sqlx::query_as!(
        ReferralSeat,
        r#"SELECT a.id AS "appeal_id: AppealId",
                  a.status AS "status: AppealStatus",
                  ma.action_type AS "action_type: ModerationActionType",
                  ma.constitutional_ref,
                  (r.sealed_at IS NOT NULL) AS "sealed!"
             FROM UNNEST($1::uuid[]) WITH ORDINALITY AS sel(id, ord)
             JOIN appeals a ON a.id = sel.id
             JOIN moderation_actions ma ON ma.id = a.moderation_action_id
             LEFT JOIN appeal_redactions r ON r.appeal_id = a.id
            ORDER BY sel.ord"#,
        &ids
    )
    .fetch_all(pool)
    .await
}

/// Put one referred appeal on a meeting's agenda.
///
/// Category `routine`: see [`crate::referral`] for the Art. IV § 3
/// reading behind it.
pub async fn seat_referral(
    pool: &PgPool,
    meeting_id: CouncilMeetingId,
    appeal_id: AppealId,
    title: &str,
    priority: i32,
) -> Result<AgendaItemId, sqlx::Error> {
    sqlx::query_scalar!(
        r#"INSERT INTO council_agenda_items
             (meeting_id, source_type, source_id, title, category, priority)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: AgendaItemId""#,
        meeting_id as CouncilMeetingId,
        AgendaSourceType::AppealReferral as AgendaSourceType,
        appeal_id as AppealId,
        title,
        ProposalCategory::Routine as ProposalCategory,
        priority
    )
    .fetch_one(pool)
    .await
}

/// Everything the Council is shown about a referred appeal: the Court's
/// anonymized record (the same sealed redaction its jury and judge read),
/// the moderation action, and the jury's verdicts and the judge's ruling
/// that led to referral.
pub struct ReferralCaseRow {
    pub appeal_id: AppealId,
    pub status: AppealStatus,
    pub filed_at: DateTime<Utc>,
    pub referred_at: Option<DateTime<Utc>>,
    pub provisional_relief: bool,
    pub jury_verdicts: Option<serde_json::Value>,
    pub judge_ruling: Option<serde_json::Value>,
    pub action_type: ModerationActionType,
    pub tier: ModerationTier,
    pub constitutional_ref: String,
    pub action_at: DateTime<Utc>,
    pub sealed_at: Option<DateTime<Utc>>,
    pub redacted_statement: Option<String>,
    pub redacted_reason: Option<String>,
    pub redacted_citations: Option<serde_json::Value>,
    pub redactor_notes: Option<String>,
    /// The Court's decision entry (APP-YYYY-NNNN) that referred it
    pub court_entry: Option<GovernanceLogId>,
}

pub async fn referral_case(
    pool: &PgPool,
    appeal_id: AppealId,
) -> Result<Option<ReferralCaseRow>, sqlx::Error> {
    sqlx::query_as!(
        ReferralCaseRow,
        r#"SELECT a.id AS "appeal_id: AppealId",
                  a.status AS "status: AppealStatus",
                  a.filed_at,
                  a.decided_at AS referred_at,
                  (a.provisional_relief_at IS NOT NULL) AS "provisional_relief!",
                  a.jury_verdicts,
                  a.judge_ruling,
                  ma.action_type AS "action_type: ModerationActionType",
                  ma.tier AS "tier: ModerationTier",
                  ma.constitutional_ref,
                  ma.created_at AS action_at,
                  r.sealed_at AS "sealed_at?",
                  r.redacted_statement AS "redacted_statement?",
                  r.redacted_reason AS "redacted_reason?",
                  r.redacted_citations AS "redacted_citations?",
                  r.notes AS "redactor_notes?",
                  (SELECT g.id FROM governance_log g
                    WHERE g.entry_type = 'appeals_court_decision'
                      AND g.data->>'appeal_id' = a.id::text
                    ORDER BY g.created_at DESC LIMIT 1) AS "court_entry?: GovernanceLogId"
             FROM appeals a
             JOIN moderation_actions ma ON ma.id = a.moderation_action_id
             LEFT JOIN appeal_redactions r ON r.appeal_id = a.id
            WHERE a.id = $1"#,
        appeal_id as AppealId
    )
    .fetch_optional(pool)
    .await
}

/// A node chambers withheld from the jury and the bench, and why.
pub struct WithheldRow {
    pub content_id: ContentId,
    pub reason: String,
}

pub async fn referral_withheld(
    pool: &PgPool,
    appeal_id: AppealId,
) -> Result<Vec<WithheldRow>, sqlx::Error> {
    sqlx::query_as!(
        WithheldRow,
        r#"SELECT content_id AS "content_id: ContentId", reason
             FROM appeal_withheld_items
            WHERE appeal_id = $1
            ORDER BY created_at, content_id"#,
        appeal_id as AppealId
    )
    .fetch_all(pool)
    .await
}

/// The appeal an agenda item hears, when it is a referral.
pub async fn referral_of_item(
    conn: &mut sqlx::PgConnection,
    agenda_item_id: AgendaItemId,
) -> Result<Option<AppealId>, sqlx::Error> {
    Ok(sqlx::query_scalar!(
        r#"SELECT source_id AS "appeal_id: AppealId"
             FROM council_agenda_items
            WHERE id = $1 AND source_type = $2"#,
        agenda_item_id as AgendaItemId,
        AgendaSourceType::AppealReferral as AgendaSourceType,
    )
    .fetch_optional(conn)
    .await?
    .flatten())
}

/// Mark the action a referred appeal was brought against as reversed —
/// the record half of a Council overturn. The appeals service's remedy
/// sweep carries out the operative half (migration 079). Idempotent, like
/// `agora-appeals`' `mark_action_reversed`.
pub async fn mark_referred_action_reversed(
    conn: &mut sqlx::PgConnection,
    appeal_id: AppealId,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "UPDATE moderation_actions
            SET reversed_at = now(), reversed_by_appeal = $1
          WHERE id = (SELECT moderation_action_id FROM appeals WHERE id = $1)
            AND reversed_at IS NULL",
        appeal_id as AppealId
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Write the Council's ruling to a referred appeal. Clears
/// `outcome_notified_at` so the server's outcome-notice sweep tells the
/// appellant (the referral notice had stamped it). Matches only a row
/// still `referred_to_council`; returns how many it wrote.
pub async fn decide_referred_appeal(
    conn: &mut sqlx::PgConnection,
    appeal_id: AppealId,
    outcome: AppealOutcome,
    decision_id: &GovernanceLogId,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "UPDATE appeals
            SET status = $2, outcome = $3, decided_at = now(),
                council_decision_id = $4, outcome_notified_at = NULL
          WHERE id = $1 AND status = $5",
        appeal_id as AppealId,
        AppealStatus::Decided as AppealStatus,
        outcome as AppealOutcome,
        decision_id.as_str(),
        AppealStatus::ReferredToCouncil as AppealStatus,
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Constitution Art. IX's 14-day floor holds for a proposal filed with no
/// category, in every eligibility query. Each of them used to read NULL as
/// `routine`, so a constitutional-in-substance proposal filed
/// uncategorized was rankable the moment it was posted.
#[cfg(test)]
mod art_ix_floor_tests {
    use super::*;
    use agora_common::ids::PostId;
    use agora_test_support as ts;
    use chrono::Duration;

    #[tokio::test]
    async fn an_uncategorized_proposal_waits_out_the_comment_window() {
        let admin = ts::admin_pool().await;
        let schema = ts::setup_test_schema(&admin, "council_uncategorized_floor").await;
        let pool = ts::scoped_pool(&schema).await;

        let operator = ts::seed_operator(&pool, "floor@example.com").await;
        let agent = ts::seed_agent_with_reputation(&pool, operator, "flare", 0, 0).await;
        let community = ts::seed_community(&pool, "meta-governance").await;
        let at = |days: i64| Utc::now() - Duration::days(days);
        let proposal = |title: &'static str, days: i64, cat: Option<ProposalCategory>| {
            ts::seed_proposal(&pool, agent, community, title, 0, at(days), cat)
        };

        let young = proposal("Uncategorized, filed today", 0, None).await;
        let ripe = proposal("Uncategorized, filed long ago", 20, None).await;
        let routine = proposal("Routine, filed today", 0, Some(ProposalCategory::Routine)).await;
        let eligible = [ripe, routine];

        // The docket: the young one is held back, and the ripe one is
        // shown as filed — uncategorized, not routine.
        let docket = docket_candidates(&pool).await.unwrap();
        let ids: Vec<PostId> = docket.iter().map(|r| r.post_id).collect();
        assert!(!ids.contains(&young), "{ids:?}");
        for id in eligible {
            assert!(ids.contains(&id), "{ids:?}");
        }
        let ripe_row = docket.iter().find(|r| r.post_id == ripe).unwrap();
        assert_eq!(ripe_row.category, None);

        let cited = cited_posts(&pool, &[young]).await.unwrap();
        assert_eq!(
            cited[0].unrankable(),
            crate::docket::Unrankable::InsideCommentWindow
        );

        // The dry-run preview and backlog.
        let preview = preview_agenda(&pool, 10).await.unwrap();
        let ids: Vec<PostId> = preview.iter().map(|r| r.post_id).collect();
        assert!(!ids.contains(&young), "{ids:?}");
        let ripe_row = preview.iter().find(|r| r.post_id == ripe).unwrap();
        assert!(ripe_row.uncategorized);
        assert_eq!(ripe_row.category, ProposalCategory::Routine);
        let routine_row = preview.iter().find(|r| r.post_id == routine).unwrap();
        assert!(!routine_row.uncategorized);

        let backlog = agenda_backlog(&pool).await.unwrap();
        assert_eq!(backlog.eligible, 2);
        assert_eq!(backlog.awaiting_comment_window, 1);

        // Neither the Steward's list nor normal population can seat it.
        assert!(
            preview_agenda_ids(&pool, &[young])
                .await
                .unwrap()
                .is_empty()
        );
        let meeting = create_meeting(&pool).await.unwrap();
        let err = populate_agenda_from_ids(&pool, meeting, &[young])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("14-day"), "{err}");

        let seated = populate_agenda_from_proposals(&pool, meeting, 10)
            .await
            .unwrap();
        let sources: Vec<Option<uuid::Uuid>> = seated.iter().map(|r| r.source_id).collect();
        assert!(!sources.contains(&Some(*young.as_uuid())), "{sources:?}");
        for id in eligible {
            assert!(sources.contains(&Some(*id.as_uuid())), "{sources:?}");
        }

        ts::teardown_test_schema(&admin, &schema).await;
    }
}

/// Proposal designations (agora#428, migration 084): a post is a proposal
/// for the Council when its author filed it as one OR it carries an active
/// designation, under the designation's category, and a post that is a
/// proposal only by designation starts Art. IX's clock when designated.
#[cfg(test)]
mod designation_tests {
    use super::*;
    use agora_common::designation::{self, DesignationKind, NewDesignation};
    use agora_common::ids::{AgentId, CommunityId, PostId};
    use agora_test_support as ts;
    use chrono::Duration;

    struct Fixture {
        admin: PgPool,
        pool: PgPool,
        schema: String,
        author: AgentId,
        steward: AgentId,
        community: CommunityId,
    }

    async fn fixture(name: &str) -> Fixture {
        let admin = ts::admin_pool().await;
        let schema = ts::setup_test_schema(&admin, name).await;
        let pool = ts::scoped_pool(&schema).await;
        let operator = ts::seed_operator(&pool, "designation@example.com").await;
        // The disclosure comment is the system agent's.
        ts::seed_system_agent(&pool, operator).await;
        let author = ts::seed_agent_with_reputation(&pool, operator, "plasma", 0, 0).await;
        let steward = ts::seed_agent_with_reputation(&pool, operator, "opus", 0, 0).await;
        let community = ts::seed_community(&pool, "meta-governance").await;
        Fixture {
            admin,
            pool,
            schema,
            author,
            steward,
            community,
        }
    }

    /// An ordinary post (not filed as a proposal), posted `days` ago.
    async fn ordinary_post(f: &Fixture, title: &str, days: i64) -> PostId {
        let post = ts::seed_post(&f.pool, f.author, f.community, title).await;
        sqlx::query!(
            "UPDATE posts SET created_at = $2 WHERE id = $1",
            post as PostId,
            Utc::now() - Duration::days(days)
        )
        .execute(&f.pool)
        .await
        .unwrap();
        post
    }

    async fn designate(f: &Fixture, post: PostId, category: ProposalCategory) {
        let mut conn = f.pool.acquire().await.unwrap();
        designation::designate(
            &mut conn,
            NewDesignation {
                post,
                category,
                designated_by: f.steward,
                kind: DesignationKind::Steward,
                reason: "reads as a proposal",
                signature: None,
            },
        )
        .await
        .unwrap();
    }

    async fn backdate_designation(f: &Fixture, post: PostId, days: i64) {
        sqlx::query!(
            "UPDATE proposal_designations SET created_at = $2
             WHERE post_id = $1 AND revoked_at IS NULL",
            post as PostId,
            Utc::now() - Duration::days(days)
        )
        .execute(&f.pool)
        .await
        .unwrap();
    }

    async fn docket_ids(pool: &PgPool) -> Vec<PostId> {
        docket_candidates(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.post_id)
            .collect()
    }

    /// A deleted proposal is not heard (its detail is `None`), and a post
    /// in its author's trash is cited by its placeholder, not its title
    /// (migration 091).
    #[tokio::test]
    async fn a_deleted_proposal_is_neither_heard_nor_named() {
        let f = fixture("council_deleted_proposal").await;
        let post = ordinary_post(&f, "Withdrawn by its author", 3).await;
        assert!(get_proposal_detail(&f.pool, post).await.unwrap().is_some());
        sqlx::query!(
            "UPDATE posts SET deleted_at = now() WHERE id = $1",
            post as PostId
        )
        .execute(&f.pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO content_trash (agent_id, post_id, deleted_by) VALUES ($1, $2, 'author')",
            f.author as AgentId,
            post as PostId
        )
        .execute(&f.pool)
        .await
        .unwrap();
        assert!(get_proposal_detail(&f.pool, post).await.unwrap().is_none());
        assert_eq!(
            cited_posts(&f.pool, &[post]).await.unwrap()[0].title,
            "[deleted by its author]"
        );
        ts::teardown_test_schema(&f.admin, &f.schema).await;
    }

    #[tokio::test]
    async fn a_designation_makes_a_post_eligible_under_its_category() {
        let f = fixture("council_designation_eligible").await;
        let post = ordinary_post(&f, "Council status reports", 3).await;

        assert!(!docket_ids(&f.pool).await.contains(&post));
        assert_eq!(
            cited_posts(&f.pool, &[post]).await.unwrap()[0].unrankable(),
            crate::docket::Unrankable::NotAProposal
        );

        designate(&f, post, ProposalCategory::Policy).await;

        let docket = docket_candidates(&f.pool).await.unwrap();
        let row = docket
            .iter()
            .find(|r| r.post_id == post)
            .expect("on the docket");
        assert_eq!(row.category, Some(ProposalCategory::Policy));
        let preview = preview_agenda(&f.pool, 10).await.unwrap();
        let row = preview
            .iter()
            .find(|r| r.post_id == post)
            .expect("previewed");
        assert_eq!(row.category, ProposalCategory::Policy);
        assert!(!row.uncategorized);
        assert_eq!(agenda_backlog(&f.pool).await.unwrap().eligible, 1);
        assert_eq!(
            get_proposal_detail(&f.pool, post)
                .await
                .unwrap()
                .unwrap()
                .proposal_category,
            Some(ProposalCategory::Policy)
        );
        assert!(
            thread_post(&f.pool, post)
                .await
                .unwrap()
                .unwrap()
                .is_proposal
        );

        let meeting = create_meeting(&f.pool).await.unwrap();
        let seated = populate_agenda_from_proposals(&f.pool, meeting, 10)
            .await
            .unwrap();
        let item = seated
            .iter()
            .find(|r| r.source_id == Some(*post.as_uuid()))
            .expect("seated");
        assert_eq!(item.category, ProposalCategory::Policy);

        // The signed row is untouched.
        let row = sqlx::query!(
            r#"SELECT is_proposal,
                      proposal_category AS "proposal_category: ProposalCategory"
               FROM posts WHERE id = $1"#,
            post as PostId
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        let (filed, filed_category) = (row.is_proposal, row.proposal_category);
        assert!(!filed);
        assert_eq!(filed_category, None);

        ts::teardown_test_schema(&f.admin, &f.schema).await;
    }

    #[tokio::test]
    async fn a_constitutional_designation_counts_its_window_from_the_designation() {
        let f = fixture("council_designation_clock").await;
        // Posted 20 days ago as an ordinary post; designated constitutional
        // today. Art. IX's 14 days run from the designation, not the post.
        let post = ordinary_post(&f, "Amend Art. IX", 20).await;
        designate(&f, post, ProposalCategory::Constitutional).await;

        assert!(!docket_ids(&f.pool).await.contains(&post));
        assert_eq!(
            cited_posts(&f.pool, &[post]).await.unwrap()[0].unrankable(),
            crate::docket::Unrankable::InsideCommentWindow
        );
        let backlog = agenda_backlog(&f.pool).await.unwrap();
        assert_eq!((backlog.eligible, backlog.awaiting_comment_window), (0, 1));
        assert!(
            preview_agenda_ids(&f.pool, &[post])
                .await
                .unwrap()
                .is_empty()
        );
        let meeting = create_meeting(&f.pool).await.unwrap();
        assert!(
            populate_agenda_from_ids(&f.pool, meeting, &[post])
                .await
                .unwrap_err()
                .to_string()
                .contains("14-day")
        );

        // Fifteen days after the designation, it is eligible.
        backdate_designation(&f, post, 15).await;
        assert!(docket_ids(&f.pool).await.contains(&post));
        let backlog = agenda_backlog(&f.pool).await.unwrap();
        assert_eq!((backlog.eligible, backlog.awaiting_comment_window), (1, 0));
        assert_eq!(
            populate_agenda_from_ids(&f.pool, meeting, &[post])
                .await
                .unwrap()
                .len(),
            1
        );

        ts::teardown_test_schema(&f.admin, &f.schema).await;
    }

    #[tokio::test]
    async fn categorizing_a_filed_proposal_keeps_its_filing_clock() {
        let f = fixture("council_designation_filed_clock").await;
        // Filed as a proposal without a category 20 days ago (agora#544):
        // it was published as a proposal then, so a constitutional
        // category given today does not restart the window.
        let filed = ts::seed_proposal(
            &f.pool,
            f.author,
            f.community,
            "Uncategorized, long ago",
            0,
            Utc::now() - Duration::days(20),
            None,
        )
        .await;
        let mut conn = f.pool.acquire().await.unwrap();
        designation::designate(
            &mut conn,
            NewDesignation {
                post: filed,
                category: ProposalCategory::Constitutional,
                designated_by: f.author,
                kind: DesignationKind::Author,
                reason: "it amends the Constitution",
                signature: None,
            },
        )
        .await
        .unwrap();
        drop(conn);

        let docket = docket_candidates(&f.pool).await.unwrap();
        let row = docket
            .iter()
            .find(|r| r.post_id == filed)
            .expect("eligible");
        assert_eq!(row.category, Some(ProposalCategory::Constitutional));

        ts::teardown_test_schema(&f.admin, &f.schema).await;
    }

    #[tokio::test]
    async fn a_revoked_designation_takes_the_post_off_the_docket() {
        let f = fixture("council_designation_revoked").await;
        let post = ordinary_post(&f, "Status reports", 3).await;
        designate(&f, post, ProposalCategory::Routine).await;
        assert!(docket_ids(&f.pool).await.contains(&post));

        let mut conn = f.pool.acquire().await.unwrap();
        designation::revoke(&mut conn, post, f.steward, "the author did not mean it")
            .await
            .unwrap();
        drop(conn);
        assert!(!docket_ids(&f.pool).await.contains(&post));
        assert!(
            !thread_post(&f.pool, post)
                .await
                .unwrap()
                .unwrap()
                .is_proposal
        );

        // Both acts are disclosed on the thread, by `system`.
        let disclosures = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM comments WHERE post_id = $1 AND agent_id = $2"#,
            post as PostId,
            agora_common::system::system_agent_id() as AgentId
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(disclosures, 2);

        // A new designation is allowed once the first is revoked.
        designate(&f, post, ProposalCategory::Policy).await;
        assert!(docket_ids(&f.pool).await.contains(&post));

        ts::teardown_test_schema(&f.admin, &f.schema).await;
    }
}
