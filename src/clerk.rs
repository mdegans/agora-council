//! The Council's tasks for the Clerk.
//!
//! The Clerk itself — its charge, its cached prefix, its request path —
//! lives in [`agora_common::clerk`], shared with the justice pipeline. What
//! remains here is what the *Council* asks it to do: summarize a proposal's
//! comment thread before deliberation, flag past decisions that bear on an
//! item (so the Council rules consistently), and read docket proposals'
//! threads. It no longer summarizes decisions: since 2026-10-17 each
//! decision's summary is the Lawyer's, signed with its record
//! (`summary.rs`), and the meeting's is composed from them
//! (`adjournment.rs`). The Clerk wrote the meeting summary until
//! 2026-09-25, and was its fallback until 2026-10-13.
//!
//! Summaries are written once and stored — the governance log's cached
//! summaries are the Council's memory, re-read at meeting start rather than
//! re-generated.
//!
//! The councillors' `ask_clerk` record method reads comments verbatim from
//! the thread, by the short-id labels the Clerk cites them by (#487);
//! free-form questions answered by the Clerk from its raw context are still
//! TODO (see `record.rs`).

use std::fmt::Write as _;

use agora_common::clerk::{
    CLERK_CHARGE, COMMENT_SUMMARY_BUDGET, COUNCIL_CLERK_MODEL, ClerkArchive, CommentLabels,
    THREAD_SUMMARY_ACCURACY, ask_on_archived, comment_blocks_labelled,
};

use misanthropic::prompt::message::Content;

use crate::citations::{ItemComments, ItemThread};
use crate::models::RecentDecision;
use crate::queries::CommentRow;

/// What the Clerk is told on an item the Council has decided on before,
/// after the earlier decisions and before the filed text: the filed text
/// may not be what this hearing decides (the GOV-2026-0010 ratification
/// heard five drafts that reversed it).
pub const CONTINUED_HEARING: &str = "This hearing continues the decisions above; \
the filed text may not be what is being decided. Report counterfactuals and \
precedents against the texts before the Council.";

/// The per-item opening of a Clerk request: on a continued hearing, the
/// earlier decisions ([`crate::context::continuation_section`]) and
/// [`CONTINUED_HEARING`], then the filed text under a heading that says it
/// is the filed one; otherwise the proposal as it always was. The charge
/// and the system prompt are untouched, so the cached prefix holds.
fn proposal_section(title: &str, proposal_body: &str, continuation: Option<&str>) -> String {
    match continuation {
        None => format!("# Proposal: {title}\n\n{proposal_body}"),
        Some(continuation) => format!(
            "{}\n\n{CONTINUED_HEARING}\n\n# The proposal as filed: {title}\n\n{proposal_body}",
            continuation.trim_end()
        ),
    }
}

/// How the Clerk is asked to cite comments, in both thread summaries.
const CITE_BY_LABEL: &str = "Reference comments by the id in their label, copied \
exactly: the bracketed hex that opens each comment, such as [3fbc5dc1]. A label \
is longer where two comments share its first digits; copy it whole. Never \
shorten, number or invent one.";

/// Summarize a proposal's comment thread for the Council, citing comments
/// by the labels in `comments` — the same labels every seat's `ask_clerk`
/// takes.
///
/// An item that carries related threads (#492) gets all of them in one
/// pass, each introduced by its title and id, a related thread with the
/// reason it was attached and its opening post verbatim. An item with only
/// its own thread gets exactly the prompt it always did.
pub async fn summarize_comments<T: crate::wire::RawTransport>(
    transport: &T,
    archive: Option<ClerkArchive<'_>>,
    title: &str,
    proposal_body: &str,
    comments: &ItemComments,
) -> anyhow::Result<Content> {
    summarize_comments_continuing(transport, archive, title, proposal_body, None, comments).await
}

/// [`summarize_comments`] on an item that may continue earlier decisions:
/// `continuation` is the item's
/// [`continuation_section`](crate::context::continuation_section) at level
/// 1, `None` for an item never decided before.
pub async fn summarize_comments_continuing<T: crate::wire::RawTransport>(
    transport: &T,
    archive: Option<ClerkArchive<'_>>,
    title: &str,
    proposal_body: &str,
    continuation: Option<&str>,
    comments: &ItemComments,
) -> anyhow::Result<Content> {
    let proposal = proposal_section(title, proposal_body, continuation);
    let related: Vec<&ItemThread> = comments.related().collect();
    if related.is_empty() {
        let mut user = Content::from(summary_request(&proposal, None));
        user.push(format!(
            "# Comment Thread ({} comments)",
            comments.primary().len()
        ));
        // One block per comment: see `comment_blocks_labelled` for why.
        user.extend(comment_blocks_labelled(
            comments.primary(),
            &comments.labels,
        ));
        return ask_on_archived(transport, CLERK_CHARGE, COUNCIL_CLERK_MODEL, user, archive).await;
    }

    // One summary per thread (Steward, 2026-10-17): at the stopped
    // GOV-2026-0010 ratification, one summary of 464 comments across three
    // threads dropped all four of the Steward's comments arguing for his
    // own recusal. Each thread gets a summary of its own, asked for
    // together; they are joined in thread order, whatever order they
    // finish in.
    let total = related.len() + 1;
    let mut requests = Vec::with_capacity(total);
    let primary_name = comments
        .threads
        .iter()
        .find(|t| t.role == agora_common::council::ThreadRole::Primary)
        .map(|t| t.name())
        .unwrap_or_else(|| format!("\"{title}\""));
    {
        let mut user = Content::from(summary_request(
            &proposal,
            Some(format!(
                "This item has {total} threads. This summary covers one of \
                 them, the proposal's own thread {primary_name}; each of the \
                 others is summarized separately."
            )),
        ));
        user.push(format!(
            "# Comment Thread ({} comments)",
            comments.primary().len()
        ));
        user.extend(comment_blocks_labelled(
            comments.primary(),
            &comments.labels,
        ));
        requests.push((format!("The proposal's own thread, {primary_name}"), user));
    }
    for thread in related {
        let mut user = Content::from(summary_request(
            &proposal,
            Some(format!(
                "This item has {total} threads. This summary covers one of \
                 them, the related thread {}, attached before the sitting; \
                 each of the others is summarized separately.",
                thread.name()
            )),
        ));
        // Heading, opening post and comments each in their own blocks.
        user.push(thread.clerk_heading());
        if let Some(body) = &thread.body {
            user.push(body.clone());
        }
        let own = comments.of(thread);
        user.push(format!("## Its comments ({})", own.len()));
        user.extend(comment_blocks_labelled(own, &comments.labels));
        requests.push((format!("The related thread {}", thread.name()), user));
    }
    let answers =
        futures::future::join_all(requests.into_iter().map(|(heading, user)| async move {
            let summary =
                ask_on_archived(transport, CLERK_CHARGE, COUNCIL_CLERK_MODEL, user, archive).await;
            (heading, summary)
        }))
        .await;
    let mut out = Content::from_iter(std::iter::empty::<String>());
    for (heading, summary) in answers {
        // Any one failing fails the whole: a Council that could not be
        // shown a thread's discussion does not sit on a partial summary.
        let summary =
            anyhow::Context::with_context(summary, || format!("the Clerk's summary of {heading}"))?;
        out.push(format!("## {heading}"));
        out.extend(summary);
    }
    Ok(out)
}

/// The comment-summary request's instructions and the proposal, before
/// the thread itself. `scope` says which of the item's threads this
/// summary covers, when it has more than one.
fn summary_request(proposal: &str, scope: Option<String>) -> String {
    let scope = scope.map(|s| format!("{s}\n\n")).unwrap_or_default();
    format!(
        // The counterfactual section is Council direction, not a style
        // choice: GOV-2026-0006 rejected the Jester Pause Rule but every
        // councillor endorsed Clerk-surfaced community counterfactuals as
        // the lighter mechanism. Announced by the Steward on the proposal
        // thread; remove only with a Council decision superseding it.
        //
        // The recusal section is the Steward's (2026-10-17): the seats
        // determine recusal first, with this summary in front of them, and
        // a comment on whether the item limits the Steward's powers must
        // not be summarized away.
        "The Council will deliberate the following item. Summarize the \
         community's discussion of it for the councillors.\n\n\
         {scope}\
         Cover: the substantive arguments for and against (attributed by \
         name), any constitutional points raised, questions the community \
         wants answered, and the overall temperature of the discussion. \
         Then flag counterfactuals in a short dedicated section: arguments \
         of the form \"which assumption, if false, would invalidate this \
         proposal\", whether or not the commenter labeled them that way. \
         If the thread contains none, say so rather than manufacturing \
         one. \
         Then, in a short dedicated section headed \"Recusal\", list every \
         comment that argues whether this item would limit the Steward's \
         powers, or asks for or against the Steward's recusal on it, the \
         Steward's own comments included: each with its author and id, \
         and one line on what it says. If there are none, say so. \
         {CITE_BY_LABEL} A councillor asks for a comment verbatim by its \
         id. Do not editorialize.\n\n\
         {THREAD_SUMMARY_ACCURACY}\n\n\
         {COMMENT_SUMMARY_BUDGET}\n\n\
         {proposal}"
    )
}

/// Summarize one candidate proposal's thread for a seat that asked to read
/// it during a scheduling item (`read_proposal`).
///
/// Comments are cited by their own short ids, labelled beside the item's
/// own comments (`labels`, from [`CommentLabels::beside`]) so no label here
/// can collide with one the seat already holds. Until #487 they were
/// numbered `[P{n}·C#]`, because a bare `C12` would have fetched the
/// scheduling thread's twelfth comment through `ask_clerk`; an id cannot
/// be confused that way.
pub async fn summarize_proposal_thread<T: crate::wire::RawTransport>(
    transport: &T,
    archive: Option<ClerkArchive<'_>>,
    number: u32,
    title: &str,
    proposal_body: &str,
    comments: &[CommentRow],
    labels: &CommentLabels,
) -> anyhow::Result<Content> {
    let mut user = Content::from(format!(
        "A councillor is choosing the Council's agenda and asked to read \
         proposal P{number}. Summarize its community discussion so they can \
         judge how ready it is to be heard: the substantive arguments for \
         and against (attributed by name), constitutional points raised, \
         open questions, and whether the discussion has converged or is \
         still moving. Do not argue for or against hearing it. \
         {CITE_BY_LABEL} Do not editorialize.\n\n\
         {THREAD_SUMMARY_ACCURACY}\n\n\
         {COMMENT_SUMMARY_BUDGET}\n\n\
         # Proposal P{number}: {title}\n\n{proposal_body}\n\n\
         # Comment Thread ({} comments)",
        comments.len(),
    ));
    user.extend(comment_blocks_labelled(comments, labels));
    ask_on_archived(transport, CLERK_CHARGE, COUNCIL_CLERK_MODEL, user, archive).await
}

/// Read a proposal against the governance log's decided cases and report
/// which bear on it — the consistency check the Council opens each item
/// with. `decisions` are the log's cached summaries, each already
/// carrying its standing.
///
/// Decisions that are no longer in force are included and labelled rather
/// than dropped: the Council is writing the record, and a proposal that
/// would reinstate what a later entry recharacterized is exactly the case
/// the Clerk should surface. The moderation path does the opposite —
/// Tier 2, jurors and the judge see only what is in force.
///
/// `continuation` is as in [`summarize_comments_continuing`].
pub async fn find_precedents<T: crate::wire::RawTransport>(
    transport: &T,
    archive: Option<ClerkArchive<'_>>,
    title: &str,
    proposal_body: &str,
    continuation: Option<&str>,
    decisions: &[RecentDecision],
) -> anyhow::Result<Content> {
    let proposal = proposal_section(title, proposal_body, continuation);
    let mut log = String::new();
    for decision in decisions {
        let _ = writeln!(log, "{}", decision.prompt_line());
    }
    let user = format!(
        "The Council will deliberate the following proposal. From the past \
         decisions below, report the ones that bear on it — where a prior \
         ruling construed the same provision, decided a similar question, \
         or would be contradicted by a particular outcome here. Cite each \
         by ID with one sentence on why it is relevant. If none are \
         relevant, say exactly: No relevant precedents.\n\n\
         A decision marked **NOT IN FORCE** was amended by a later \
         governance entry. Report it if it bears on the proposal, but say \
         in the same sentence that it is not in force and what its label \
         says — a councillor who cites it as current law because you \
         listed it plainly has been misled by this note.\n\n\
         {proposal}\n\n\
         # Past Decisions\n\n{log}",
    );
    ask_on_archived(transport, CLERK_CHARGE, COUNCIL_CLERK_MODEL, user, archive).await
}

#[cfg(test)]
mod precedent_label_tests {
    use std::sync::Mutex;

    use agora_common::enums::Standing;
    use misanthropic::model::Models;
    use misanthropic::prompt::CachedPrompt;
    use misanthropic::response;
    use misanthropic::transport::Transport;

    use super::*;
    use crate::models::RecentDecision;

    /// A transport that answers anything and keeps the request, so a
    /// prompt can be asserted on without a network or a key.
    struct Recorder {
        requests: Mutex<Vec<serde_json::Value>>,
    }

    #[derive(Debug)]
    struct NeverFails;

    impl std::fmt::Display for NeverFails {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "unreachable")
        }
    }

    impl std::error::Error for NeverFails {}

    #[async_trait::async_trait]
    impl Transport<CachedPrompt> for Recorder {
        type Error = NeverFails;

        async fn send(&self, prompt: &CachedPrompt) -> Result<response::Message, Self::Error> {
            self.requests
                .lock()
                .unwrap()
                .push(serde_json::to_value(prompt).unwrap());
            Ok(serde_json::from_value(serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4-6",
                "content": [{"type": "text", "text": "No relevant precedents."}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }))
            .expect("valid canned response"))
        }

        async fn models(&self) -> Result<Models, Self::Error> {
            Ok(Models::from_iter([]))
        }
    }

    impl crate::wire::RawTransport for Recorder {
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

    fn decision(id: &str, standing: Standing, note: Option<&str>) -> RecentDecision {
        RecentDecision {
            id: id.parse().unwrap(),
            summary: format!("What {id} held."),
            standing,
            standing_note: note.map(str::to_string),
        }
    }

    /// The Council sees decisions that are no longer in force — it is
    /// writing the record, not deciding a case under it — so everything
    /// rests on the label arriving with them. Steward, 2026-08-03:
    /// Council only, and labelled; the bench sees only what is in force.
    #[tokio::test]
    async fn the_clerk_is_shown_which_past_decisions_are_no_longer_in_force() {
        let transport = Recorder {
            requests: Mutex::new(Vec::new()),
        };
        let decisions = vec![
            decision("GOV-2026-0001", Standing::InForce, None),
            decision(
                "GOV-2026-0002",
                Standing::NonPrecedential,
                Some("diagnostic finding — not citable as moderation precedent"),
            ),
            decision("GOV-2026-0003", Standing::Overruled, None),
        ];

        find_precedents(
            &transport,
            None,
            "A proposal",
            "Its body.",
            None,
            &decisions,
        )
        .await
        .expect("the canned transport answers");

        let sent = transport.requests.lock().unwrap()[0].to_string();
        assert!(
            sent.contains("- **GOV-2026-0001**: What GOV-2026-0001 held."),
            "an in-force decision reads as it always did: {sent}"
        );
        assert!(
            sent.contains("**NOT IN FORCE (non_precedential).**")
                && sent.contains("not citable as moderation precedent"),
            "the amendment's own note is the label: {sent}"
        );
        assert!(
            sent.contains("**NOT IN FORCE (overruled).**")
                && sent.contains("Do not rely on it as authority"),
            "an entry retired without a note still gets one: {sent}"
        );
        assert!(
            sent.contains("A decision marked **NOT IN FORCE** was amended"),
            "and the Clerk is told to pass the label on: {sent}"
        );
    }

    /// The label leads. A summary read before the standing is a summary
    /// read as current law.
    #[test]
    fn the_standing_comes_before_the_summary() {
        let line = decision("GOV-2026-0002", Standing::Superseded, Some("replaced")).prompt_line();
        let banner = line.find("NOT IN FORCE").expect("labelled");
        let summary = line.find("What GOV-2026-0002 held.").expect("summarized");
        assert!(banner < summary, "{line}");
    }
}
