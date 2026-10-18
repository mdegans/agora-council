use std::fmt::Write as _;

use agora_common::wire::WireClient;
use chrono::Utc;
use misanthropic::prompt::message::Content;
use sqlx::PgPool;

use crate::abort::{self, ClerkBriefing, Moment, SittingAborted, Stage};
use crate::adjournment;
use crate::clerk;
use crate::config::Config;
use crate::deliberation;
use crate::docket::Docket;
use crate::models::{AgendaItem, Decision, DecisionCategory, DecisionOutcome, MeetingState};
use crate::operations;
use crate::queries;
use crate::steward;
use crate::stop::{StewardStopRequested, StopDuring, StopSignal};
use crate::summary;
use agora_common::clerk::ClerkArchive;
use agora_common::govlog::GovernanceSigner;
use agora_common::refusal::Refused;

/// One line (plus one per referral) naming appeals the court referred to
/// the Council. Referral → agenda is not automated yet, so the Steward
/// has to see these to place them (`--referrals`) or say why not.
async fn print_pending_referrals(pool: &PgPool) -> anyhow::Result<()> {
    let referrals = queries::pending_referrals(pool).await?;
    if referrals.is_empty() {
        println!("  Pending appeal referrals: none.");
        return Ok(());
    }
    println!(
        "  Pending appeal referrals ({}) — not on any agenda automatically; place with --referrals:",
        referrals.len()
    );
    for r in &referrals {
        println!(
            "    appeal {} ({}), referred {}{}",
            r.appeal_id,
            r.decision_id
                .as_ref()
                .map_or_else(|| "no decision entry".to_string(), |id| id.to_string()),
            r.referred_at
                .map_or_else(|| "?".to_string(), |t| t.format("%Y-%m-%d").to_string()),
            if r.provisional_relief {
                ", provisional relief"
            } else {
                ""
            },
        );
    }
    Ok(())
}

/// Report what a meeting would take up, without convening one.
///
/// Read-only and free: it touches no Anthropic API and writes nothing.
/// `run` creates the meeting row *before* it populates the agenda, so
/// there is no way to preview by running the real path — this mirrors the
/// selection instead (see [`queries::preview_agenda`]).
pub async fn dry_run(
    pool: &PgPool,
    config: &Config,
    steward_items: &[agora_common::ids::PostId],
    referrals: &[agora_common::ids::AppealId],
) -> anyhow::Result<()> {
    println!("  DRY RUN — nothing is written and no model is called.\n");

    match queries::last_meeting_time(pool).await? {
        Some(last) => {
            let days = (Utc::now() - last).num_days();
            println!(
                "  Last meeting: {} ({days} days ago)",
                last.format("%Y-%m-%d")
            );
            if days < 7 {
                println!("  Cadence: too soon — a real run needs --force.");
            } else {
                // governance-protocol.md § 6.1: weekly cadence.
                println!(
                    "  Cadence: due ({} days past the weekly cadence).",
                    days - 7
                );
            }
        }
        None => println!("  Last meeting: none on record."),
    }

    let changes = operations_since_last_sitting(pool).await?;
    if changes.is_empty() {
        println!("  Operations changes to disclose: none.");
    } else {
        println!("  Operations changes to disclose ({}):", changes.len());
        for change in &changes {
            let heading = change.text.lines().next().unwrap_or_default();
            println!("    {}", heading.trim_start_matches("## "));
        }
    }

    print_pending_referrals(pool).await?;

    // `--referrals`: what a real run would seat, and each case's rendered
    // size. A list a real run would refuse is refused here too.
    if !referrals.is_empty() {
        let seats = crate::referral::check_seats(pool, referrals).await?;
        println!(
            "\n  Referrals placed by the Steward ({}), heard first as Routine items:\n",
            seats.len()
        );
        for (appeal_id, title) in &seats {
            let body = crate::referral::load(pool, *appeal_id).await?.body();
            println!(
                "    {title}\n      appeal {appeal_id}; case record {} bytes (~{} tokens)",
                body.len(),
                body.len() / 4
            );
        }
    }

    let backlog = queries::agenda_backlog(pool).await?;
    let steward_set = !steward_items.is_empty();
    let items = if steward_set {
        queries::preview_agenda_ids(pool, steward_items).await?
    } else {
        queries::preview_agenda(pool, config.max_agenda_items).await?
    };

    if steward_set {
        println!(
            "\n  Agenda — GIVEN AS A LIST OF ITEMS ({} item(s)), not taken \
             from the score ordering. The list may carry out an agenda the \
             Council set (a scheduling decision, or a vote it deferred to \
             this sitting) or one the Steward placed by hand \
             (governance-protocol.md § 2.3); the program cannot tell which:\n",
            items.len()
        );
        if items.len() != steward_items.len() {
            println!(
                "  ** {} of {} ids did not match a live proposal (or are \
                 still inside Art. IX's 14-day window) — a real run will \
                 refuse. **\n",
                steward_items.len() - items.len(),
                steward_items.len()
            );
        }
    } else {
        println!(
            "\n  Agenda ({} of {} eligible proposals, AGORA_COUNCIL_MAX_ITEMS={}):\n",
            items.len(),
            backlog.eligible,
            config.max_agenda_items
        );
    }
    for (n, item) in items.iter().enumerate() {
        let age = (Utc::now() - item.created_at).num_days();
        println!(
            "  {:>2}. [{:>3}] {:<14} {age:>4}d  {:>3} comments  {}",
            n + 1,
            item.score,
            item.category.to_string(),
            item.comment_count,
            item.title
        );
        println!("      by {}", item.author);
        // What the Steward attached (#492): the seats will see these too.
        for thread in queries::related_threads(pool, item.post_id).await? {
            println!(
                "      + related thread {} \"{}\"{} — {}",
                thread.post_id,
                thread.title,
                if thread.deleted {
                    " (DELETED, not shown)"
                } else {
                    ""
                },
                thread.reason
            );
        }
        if item.uncategorized {
            println!(
                "      ** filed without a category: seats as {}; held to Art. IX's \
                 14-day floor (governance-protocol.md § 2.1 lets the Council \
                 decide it at the threshold its substance requires) **",
                item.category
            );
        }
    }

    // A scheduling item: show the slate its seats would rank.
    for item in &items {
        match item.category {
            agora_common::enums::ProposalCategory::Schedule => {
                preview_docket(pool, item.post_id, &item.title).await?;
            }
            agora_common::enums::ProposalCategory::Routine
            | agora_common::enums::ProposalCategory::Policy
            | agora_common::enums::ProposalCategory::Constitutional
            | agora_common::enums::ProposalCategory::Emergency => {}
        }
    }

    if steward_set {
        println!(
            "\n  {} eligible proposal(s) not taken up.",
            backlog.eligible
        );
    } else {
        let below_cut = backlog.eligible - items.len() as i64;
        if below_cut > 0 {
            println!("\n  {below_cut} eligible proposal(s) fall below the cut.");
        }
    }
    if backlog.awaiting_comment_window > 0 {
        // Constitution Art. IX — 14 days of community comment before a
        // constitutional amendment can be voted on.
        println!(
            "  {} constitutional or uncategorized proposal(s) still inside the Art. IX \
             14-day comment window.",
            backlog.awaiting_comment_window
        );
    }

    let deferred = queries::deferred_items(pool).await?;
    if !deferred.is_empty() {
        println!("\n  Deferred items (governance-protocol.md § 4.3: return after 14 days):\n");
        for item in &deferred {
            let days = (Utc::now() - item.deferred_at).num_days();
            let past = if days > 14 {
                "  ** past return window **"
            } else {
                ""
            };
            println!(
                "  - {:<14} {days:>4}d  {}{past}",
                item.category.to_string(),
                item.title
            );
        }
        println!(
            "\n  Nothing re-tables these automatically: deferral marks the\n  \
             proposal deliberated and leaves its agenda row in place, and\n  \
             agenda selection excludes both. Re-table with --items until\n  \
             that gap is closed.\n\n  \
             Past the window is not by itself a breach — § 4.3 protects the\n  \
             proposer, so a proposer who withdrew has waived it. There is no\n  \
             withdrawn status to check yet (agora#123), so confirm before\n  \
             citing one of these as a violation."
        );
    }

    println!();
    Ok(())
}

/// Print the docket a scheduling thread would give its seats.
async fn preview_docket(
    pool: &PgPool,
    thread: agora_common::ids::PostId,
    title: &str,
) -> anyhow::Result<()> {
    let comments = queries::get_proposal_comments(pool, thread).await?;
    let docket = Docket::build(pool, thread, &comments).await?;
    // The Steward's view: the counts are shown here, never to the seats
    // (agora#510). The order is this preview's own shuffle; the sitting
    // draws a fresh seed.
    println!(
        "\n  Docket for \"{title}\": {} rankable, {} cited in the thread, \
         {} cited but not rankable. Shuffle seed {:016x}.\n",
        docket.candidates.len(),
        docket.cited,
        docket.unrankable.len(),
        docket.seed,
    );
    for c in docket.candidates.iter().take(docket.cited) {
        println!(
            "    P{:<3} ×{:<2} {:<14} {}{}",
            c.number,
            c.cited,
            c.category_label(),
            c.title,
            if c.deferred { "  (deferred)" } else { "" }
        );
    }
    for d in &docket.unrankable {
        println!(
            "    D{:<3} ×{:<2} {} — {} ({})",
            d.number, d.cited, d.title, d.reason, d.post_id
        );
    }
    let rendered = crate::context::docket_content(&docket).to_string();
    println!(
        "\n    Rendered docket: {} bytes (~{} tokens).",
        rendered.len(),
        rendered.len() / 4
    );
    Ok(())
}

/// Check if a meeting can be convened and run it.
/// The entries of `governance/council-operations.md` the Council has not
/// yet been shown. A log that does not parse stops the sitting: a
/// disclosure the program cannot read is one it would silently withhold.
async fn operations_since_last_sitting(
    pool: &PgPool,
) -> anyhow::Result<Vec<operations::Change<'static>>> {
    let log = operations::log()?;
    let last = queries::last_sitting_day_before_today(pool).await?;
    Ok(operations::since(&log, last))
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: &PgPool,
    client: &WireClient,
    config: &Config,
    signer: &GovernanceSigner,
    force: bool,
    steward_items: &[agora_common::ids::PostId],
    referrals: &[agora_common::ids::AppealId],
    witnesses: &crate::witness::WitnessFile,
) -> anyhow::Result<()> {
    let mut terminal = steward::Terminal::new()?;

    // Referrals the Steward placed (`--referrals`), checked before
    // anything is written: every one still referred, with a sealed record.
    let referral_seats = if referrals.is_empty() {
        Vec::new()
    } else {
        crate::referral::check_seats(pool, referrals).await?
    };

    // Fail before any deliberation is spent if a decision could not be
    // recorded: the signing key must load (it has, by now) and every
    // existing entry must already be attested (the server does that at
    // startup; a meeting must not chain past an unsigned entry).
    agora_common::govlog::preflight(pool, signer)
        .await
        .map_err(|e| anyhow::anyhow!("governance log preflight failed: {e}"))?;

    // A meeting still `active` now is a crashed run — sweep it into
    // `cancelled` before it distorts the record or the cadence check.
    let stale = queries::cancel_stale_meetings(pool).await?;
    if stale > 0 {
        tracing::warn!(
            count = stale,
            "cancelled stale active meeting(s) left by a crashed run"
        );
    }

    // Check if enough time has passed since last meeting
    if !force && let Some(last) = queries::last_meeting_time(pool).await? {
        let days_since = (Utc::now() - last).num_days();
        if days_since < 7 {
            println!(
                "  Last meeting was {days_since} day(s) ago. \
                     Next meeting in {} day(s).",
                7 - days_since
            );
            println!("  Use --force to override the weekly cadence.");
            return Ok(());
        }
    }

    // Create the meeting
    let meeting_id = queries::create_meeting(pool).await?;
    tracing::info!("Created meeting {meeting_id}");

    // Referrals first, so the proposal population below returns them with
    // the rest of the meeting's agenda (Protocol § 2.3: placed by hand).
    if !referral_seats.is_empty() {
        println!(
            "  Appeal referral(s) placed by the Steward ({}), heard first \
             (Art. VI § 2).",
            referral_seats.len()
        );
        crate::referral::seat(pool, meeting_id, &referral_seats).await?;
    }

    // Populate the agenda: normally the top proposals by score, or the
    // Steward's explicit list (governance-protocol.md § 2.3). A
    // Steward-set agenda is announced rather than applied quietly — it is
    // an exercise of a governance power and belongs in the proceedings.
    let agenda_rows = if steward_items.is_empty() {
        queries::populate_agenda_from_proposals(pool, meeting_id, config.max_agenda_items).await?
    } else {
        // The program knows only that the items came as a list. Who chose
        // them (the Council's scheduling decision, a vote it deferred to
        // this sitting, or the Steward by hand) is not its to say (Steward,
        // 2026-10-17: the GOV-2026-0010 ratification and the GOV-2026-0013
        // items were logged as "set by the Steward").
        println!(
            "  Agenda given as a list of {} item(s), not taken from the score \
             ordering (see the sitting's record for who set it).",
            steward_items.len()
        );
        tracing::info!(
            items = steward_items.len(),
            "agenda given as a list of items, not the score ordering"
        );
        queries::populate_agenda_from_ids(pool, meeting_id, steward_items).await?
    };

    if agenda_rows.is_empty() {
        println!("  No agenda items. Nothing to deliberate.");
        queries::adjourn_meeting(pool, meeting_id).await?;
        return Ok(());
    }

    // The governance log is the Council's memory: open the meeting with
    // the most recent decisions' cached summaries (written once at
    // decision time, never regenerated).
    let past_decisions = queries::recent_decision_summaries(pool, 8).await?;
    let mut recap = Content::from_iter(std::iter::empty::<String>());
    // The date first. Councillors reason about comment windows, sunsets
    // and "14 days from now" and had no idea when now was — at the
    // 2026-09-07 sitting the Steward had to say so in his notes (#305).
    recap.push(format!(
        "# Council Sitting\n\n\
         Today is {} (UTC). Reckon every deadline, comment window, \
         sunset and \"days ago\" from this date, not from the dates in the \
         material below.\n",
        Utc::now().format("%Y-%m-%d")
    ));
    // What changed about how sittings run, since the last one: the
    // disclosure half of the Steward's discretion over staff-work.
    let changes = operations_since_last_sitting(pool).await?;
    if let Some(notice) = operations::notice(&changes) {
        println!(
            "  Disclosing {} change(s) to Council operations to every councillor.",
            changes.len()
        );
        recap.push(notice);
    }
    if !past_decisions.is_empty() {
        let mut s = String::from(
            "# Recent Council Decisions\n\n\
             From the governance log: each line is the decision's summary (since \
             2026-10-17 the Lawyer's, signed with its record; before then the \
             Clerk's). A \
             decision marked **NOT IN FORCE** was later amended: the decision \
             itself still stands in the record exactly as written, but its \
             standing changed, so weigh it as history and do not rely on it as \
             authority for what it once held.\n\n",
        );
        for decision in &past_decisions {
            let _ = writeln!(s, "{}", decision.prompt_line());
        }
        recap.push(s);
    }

    // The open obligations, read once: each item lists those its earlier
    // decisions created or that fall due at its hearing.
    let open_obligations = queries::open_obligations(pool).await?;
    let governance_links = queries::governance_links(pool).await?;

    // Load agenda item details
    let mut items = Vec::new();
    for row in &agenda_rows {
        let proposal_id = match row.source_type {
            agora_common::enums::AgendaSourceType::Proposal => {
                row.source_id.map(agora_common::ids::PostId::from)
            }
            agora_common::enums::AgendaSourceType::AppealReferral
            | agora_common::enums::AgendaSourceType::StewardSubmission
            | agora_common::enums::AgendaSourceType::Internal => None,
        };
        let appeal_id = match row.source_type {
            agora_common::enums::AgendaSourceType::AppealReferral => {
                row.source_id.map(agora_common::ids::AppealId::from)
            }
            agora_common::enums::AgendaSourceType::Proposal
            | agora_common::enums::AgendaSourceType::StewardSubmission
            | agora_common::enums::AgendaSourceType::Internal => None,
        };
        // A proposal deleted since it was put on the agenda is not heard:
        // its author withdrew it (or it was removed), and the Council does
        // not deliberate on text that is out of view. The server refuses a
        // deletion once a sitting has the item; this covers the window
        // before. The item is deferred, so it says why it was not heard.
        let mut detail = None;
        if let Some(post_id) = proposal_id {
            match queries::get_proposal_detail(pool, post_id).await? {
                Some(found) => detail = Some(found),
                None => {
                    println!(
                        "  Skipping \"{}\": proposal {post_id} has been deleted; deferred.",
                        row.title
                    );
                    queries::update_agenda_item_status(
                        pool,
                        row.id,
                        agora_common::enums::AgendaItemStatus::Deferred,
                    )
                    .await?;
                    continue;
                }
            }
        }
        let mut item = AgendaItem {
            id: row.id,
            title: row.title.clone(),
            body: String::new(),
            category: DecisionCategory::from(row.category),
            proposal_id,
            agent_name: None,
            // A referral has no community score; its priority only orders it.
            score: appeal_id.is_none().then_some(row.priority),
            priority: row.priority,
            comments: Default::default(),
            comment_summary: None,
            precedents: None,
            docket: None,
            appeal_id,
            // A recused deferral is spent once: the next hearing goes to
            // the vote (Steward, 2026-10-04).
            deferral_spent: queries::last_decision_was_recused_deferral(
                pool,
                row.id,
                crate::models::RECUSED_DEFERRED_TAG,
            )
            .await?,
            history: queries::prior_decisions(pool, row.id).await?,
            obligations: Vec::new(),
            witnesses: Vec::new(),
        };
        item.obligations = crate::context::obligations_due(
            open_obligations.iter().cloned(),
            &item.history,
            &governance_links,
        );

        // A referred appeal: the Court's record, then the Clerk's
        // precedent pass, fatal on failure for the reason given below.
        if let Some(appeal_id) = appeal_id {
            item.body = crate::referral::load(pool, appeal_id).await?.body();
            if !past_decisions.is_empty() {
                item.precedents = Some(
                    clerk::find_precedents(
                        client,
                        Some(ClerkArchive {
                            pool,
                            context_id: Some(item.id.into()),
                        }),
                        &item.title,
                        &item.body,
                        crate::context::continuation_section(&item, 1).as_deref(),
                        &past_decisions,
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "the Clerk could not run the precedent pass on \"{}\": {e}",
                            item.title
                        )
                    })?,
                );
            }
        }

        // Load proposal body if this is a proposal
        if let Some(post_id) = proposal_id
            && let Some(detail) = detail
        {
            item.body = detail.body;
            item.agent_name = Some(detail.agent_name);
            item.score = Some(detail.score);

            // The Clerk reads the comment thread so the Council doesn't
            // deliberate blind to the community's discussion.
            // Every thread the item carries: its own, and any related
            // thread the Steward attached (#492). Labelled once, here, by
            // short id (#487): the Clerk, every seat's `ask_clerk` and the
            // record all use these labels.
            let comments = crate::threads::load(
                pool,
                post_id,
                &item.title,
                item.agent_name.as_deref().unwrap_or_default(),
            )
            .await?;
            for thread in comments.related() {
                println!(
                    "  Related thread {}: {} comment(s), opening post {} bytes. {}",
                    thread.name(),
                    comments.of(thread).len(),
                    thread.body.as_deref().map_or(0, str::len),
                    thread.attached_line().unwrap_or_default(),
                );
            }
            if !comments.is_empty() {
                println!(
                    "  Clerk: summarizing {} comments on \"{}\"...",
                    comments.len(),
                    item.title
                );
                // Fatal. A sitting that proceeds here deliberates blind
                // to the community — and on the agenda-setting item,
                // whose entire purpose is reading the community, it would
                // pick the docket having seen nothing. This was a warning
                // until 2026-09-20; the failure it hides is an oversize
                // thread (a 400), which the dashboard's schedule block
                // made a live possibility the same week. Nothing has been
                // spent yet at this point in the meeting, so aborting
                // costs a re-run and no deliberation.
                let summary = clerk::summarize_comments_continuing(
                    client,
                    Some(ClerkArchive {
                        pool,
                        context_id: Some(item.id.into()),
                    }),
                    &item.title,
                    &item.body,
                    crate::context::continuation_section(&item, 1).as_deref(),
                    &comments,
                )
                .await;
                item.comment_summary = Some(match summary {
                    Ok(summary) => summary,
                    // A refusal aborts the sitting like any other: sealed,
                    // never retried (see `crate::abort`).
                    Err(e) if Refused::find(&e).is_some() => {
                        let refused = Refused::find(&e).expect("just matched");
                        let error = abort::seal(
                            config,
                            Moment::item(&item, Stage::Briefing),
                            refused,
                            &[],
                            Some(ClerkBriefing::of(&item)),
                        )
                        .await;
                        return Err(close_aborted(pool, signer, meeting_id, 0, error).await);
                    }
                    Err(e) => anyhow::bail!(
                        "the Clerk could not summarize the {} comments on \"{}\": {e}",
                        comments.len(),
                        item.title
                    ),
                });
            }
            item.comments = comments;

            // A scheduling item ranks a docket built from what its thread's
            // comments cite, plus every other eligible proposal. The
            // opening post is a reading aid, not a citation (see
            // `Docket::build`).
            match item.category {
                DecisionCategory::Schedule => {
                    let docket = Docket::build(pool, post_id, item.comments.primary()).await?;
                    println!(
                        "  Docket: {} rankable proposal(s), {} cited in the thread; \
                         {} cited post(s) not rankable. Shuffle seed {:016x}.",
                        docket.candidates.len(),
                        docket.cited,
                        docket.unrankable.len(),
                        docket.seed,
                    );
                    tracing::info!(
                        event_type = "docket_built",
                        thread = %post_id,
                        rankable = docket.candidates.len(),
                        cited = docket.cited,
                        unrankable = docket.unrankable.len(),
                        seed = %format_args!("{:016x}", docket.seed),
                        "docket built"
                    );
                    item.docket = Some(docket);
                }
                DecisionCategory::Routine
                | DecisionCategory::Policy
                | DecisionCategory::Constitutional
                | DecisionCategory::Emergency => {}
            }

            // The Clerk checks the proposal against past decisions so the
            // Council rules consistently.
            if !past_decisions.is_empty() {
                // Fatal for the same reason, and this one had not even
                // been narrated: without the precedent pass the Council
                // rules without being told what it has already decided,
                // and inconsistent precedent is the one error the record
                // cannot quietly absorb.
                let precedents = clerk::find_precedents(
                    client,
                    Some(ClerkArchive {
                        pool,
                        context_id: Some(item.id.into()),
                    }),
                    &item.title,
                    &item.body,
                    crate::context::continuation_section(&item, 1).as_deref(),
                    &past_decisions,
                )
                .await;
                item.precedents = Some(match precedents {
                    Ok(precedents) => precedents,
                    Err(e) if Refused::find(&e).is_some() => {
                        let refused = Refused::find(&e).expect("just matched");
                        let error = abort::seal(
                            config,
                            Moment::item(&item, Stage::Briefing),
                            refused,
                            &[],
                            Some(ClerkBriefing::of(&item)),
                        )
                        .await;
                        return Err(close_aborted(pool, signer, meeting_id, 0, error).await);
                    }
                    Err(e) => anyhow::bail!(
                        "the Clerk could not run the precedent pass on \"{}\": {e}",
                        item.title
                    ),
                });
            }
        }

        // The witnesses called on this item, described by the Clerk from
        // their public facts (OBL-2026-0028; Protocol § 4.3)
        if let Some(called) = item.proposal_id.and_then(|p| witnesses.get(&p)) {
            let mut called = called.clone();
            println!(
                "  Clerk: describing {} witness(es) on \"{}\"...",
                called.len(),
                item.title
            );
            let described = crate::witness::describe(
                client,
                &item,
                &mut called,
                Some(ClerkArchive {
                    pool,
                    context_id: Some(item.id.into()),
                }),
            )
            .await;
            match described {
                Ok(()) => item.witnesses = called,
                Err(e) if Refused::find(&e).is_some() => {
                    let refused = Refused::find(&e).expect("just matched");
                    let error = abort::seal(
                        config,
                        Moment::item(&item, Stage::Briefing),
                        refused,
                        &[],
                        Some(ClerkBriefing::of(&item)),
                    )
                    .await;
                    return Err(close_aborted(pool, signer, meeting_id, 0, error).await);
                }
                Err(e) => anyhow::bail!(
                    "the Clerk could not describe the witnesses on \"{}\": {e}",
                    item.title
                ),
            }
            for witness in &item.witnesses {
                println!(
                    "     {}",
                    crate::witness::probe(witness, config.api_key.expose()).await
                );
            }
        }

        items.push(item);
    }

    // Every witness in the file is for an item on this agenda: one that is
    // not would be silently dropped
    for proposal in witnesses.keys() {
        if !items.iter().any(|item| item.proposal_id == Some(*proposal)) {
            queries::adjourn_meeting(pool, meeting_id).await?;
            anyhow::bail!(
                "the witnesses file calls witnesses for {proposal}, which is not on this agenda"
            );
        }
    }

    // Confirm with Steward
    println!("\n================================================");
    println!("  AGORA COUNCIL MEETING");
    println!("  Meeting ID: {meeting_id}");
    println!("  Date: {}", Utc::now().format("%Y-%m-%d %H:%M UTC"));
    println!("================================================");
    print_pending_referrals(pool).await?;

    for (i, item) in items.iter().enumerate() {
        println!("  {}. [{}] {}", i + 1, item.category, item.title,);
        if let Some(name) = &item.agent_name {
            println!("     Submitted by: {name}");
        }
        if let Some(score) = item.score {
            println!("     Score: {score}");
        }
    }

    // What every seat will be shown, written out before anything is sent:
    // once the Steward is recused nobody can correct it (Steward,
    // 2026-10-17). Read it, then answer the prompt below.
    write_preview(meeting_id, &recap, &items).await;

    if !steward::confirm_meeting(terminal.editor(), items.len())? {
        println!("  Meeting cancelled.");
        queries::adjourn_meeting(pool, meeting_id).await?;
        return Ok(());
    }

    // From here on Ctrl-C is a Steward stop, not an exit (`crate::stop`):
    // the call in flight is cancelled, the Steward gives a reason, and the
    // moment is sealed and recorded.
    let stop = StopSignal::listen(meeting_id)?;
    terminal.arm(stop.clone());

    let mut state = MeetingState {
        meeting_id,
        items,
        current_item_index: 0,
        decisions: Vec::new(),
        started_at: Utc::now(),
    };

    // The decisions recorded, with their signed summaries, for the
    // meeting summary at adjournment.
    let mut decided: Vec<adjournment::DecidedItem> = Vec::new();

    // Process each agenda item
    let total_items = state.items.len();
    for i in 0..total_items {
        state.current_item_index = i;
        let item = &state.items[i];

        // A Ctrl-C during the last item's database writes, which a stop
        // never cuts short, takes effect here.
        if stop.is_requested() {
            let error = stop_between_items(config, meeting_id, &mut terminal).await;
            return Err(
                close_aborted(pool, signer, meeting_id, state.decisions.len(), error).await,
            );
        }

        println!("\n================================================");
        println!("  ITEM {}/{}: {}", i + 1, total_items, item.title,);
        println!(
            "  Category: {} ({})",
            item.category,
            item.category.threshold()
        );
        println!("================================================");

        // Mark item as deliberating
        queries::update_agenda_item_status(
            pool,
            item.id,
            agora_common::enums::AgendaItemStatus::Deliberating,
        )
        .await?;

        // Run deliberation, then the decision's summary in the seats' own
        // conversations (`crate::summary`). A refusal, a decision emitted
        // out of declared order (Protocol § 4.5) or a Steward stop comes
        // back as `SittingAborted`, the context already sealed and nothing
        // signed: close the meeting now rather than leave it `active`
        // through the investigation.
        let deliberated =
            match deliberation::deliberate(pool, client, config, item, &recap, &mut terminal).await
            {
                Ok(deliberated) => deliberated,
                Err(e) if SittingAborted::find(&e).is_some() => {
                    return Err(
                        close_aborted(pool, signer, meeting_id, state.decisions.len(), e).await,
                    );
                }
                Err(e) => return Err(e),
            };

        if let Some(deliberation::Deliberated {
            decision,
            id: peeked,
            written,
            stopped,
        }) = deliberated
        {
            // Signed with the summary. Its stored precedent row joins the
            // recap the remaining items open with. Every exit from here
            // either signs the decision or seals it and closes the meeting.
            let signed =
                record_decision_at(pool, signer, &state, &decision, &peeked, &written).await;
            let Recorded {
                id: decision_id,
                summary,
                moved,
            } = match signed {
                Ok(recorded) => recorded,
                Err(e) => {
                    tracing::error!(
                        event_type = "council_decision_unrecorded",
                        meeting = %meeting_id,
                        item = %item.id,
                        error = %format!("{e:#}"),
                        "the decision could not be recorded after the vote"
                    );
                    // A stop during the summary is sealed all the same, the
                    // item undecided; anything else closes the meeting
                    let error = match (&written.outcome, &stopped) {
                        (summary::Outcome::Stopped { facts }, Some(conversations)) => {
                            seal_summary_stop(
                                config,
                                item,
                                &decision,
                                facts,
                                conversations,
                                Stage::DecisionSummary,
                            )
                            .await
                        }
                        _ => e,
                    };
                    return Err(close_aborted(
                        pool,
                        signer,
                        meeting_id,
                        state.decisions.len(),
                        error,
                    )
                    .await);
                }
            };
            recap.push(format!(
                "- **{} (this meeting)**: {}",
                decision.title,
                summary.as_ref().map_or_else(
                    || format!(
                        "{} (Vote: {}; signed without a summary)",
                        decision.outcome().name(),
                        decision.vote_tally()
                    ),
                    |s| s.served_text()
                )
            ));
            decided.push(adjournment::DecidedItem {
                id: decision_id.clone(),
                title: decision.title.clone(),
                summary,
            });

            // Mark agenda item and proposal as decided
            let status = match decision.outcome() {
                DecisionOutcome::Deferred => agora_common::enums::AgendaItemStatus::Deferred,
                DecisionOutcome::Approved
                | DecisionOutcome::Rejected
                | DecisionOutcome::Amended => agora_common::enums::AgendaItemStatus::Decided,
            };
            queries::update_agenda_item_status(pool, item.id, status).await?;

            if let Some(post_id) = item.proposal_id {
                queries::mark_proposal_deliberated(pool, post_id).await?;
            }

            let scheduled = decision.agenda_ranking().is_some();

            // The Steward stopped the sitting during the summary: the vote
            // stands (signed above, without a summary), and the stop is
            // sealed and recorded, which ends the sitting
            if let (summary::Outcome::Stopped { facts }, Some(conversations)) =
                (&written.outcome, &stopped)
            {
                let error = seal_summary_stop(
                    config,
                    item,
                    &decision,
                    facts,
                    conversations,
                    Stage::SummaryStopped {
                        decision: decision_id.clone(),
                    },
                )
                .await;
                state.decisions.push(decision);
                return Err(
                    close_aborted(pool, signer, meeting_id, state.decisions.len(), error).await,
                );
            }
            state.decisions.push(decision);

            // Another writer took the id the summary was written under: the
            // decision is signed under the next one, without its summary
            // (attached for a revision), and the sitting ends, the one-writer
            // assumption broken
            if let Some(peeked) = moved {
                let error = anyhow::Error::new(DecisionIdMoved {
                    peeked,
                    allocated: decision_id,
                });
                return Err(
                    close_aborted(pool, signer, meeting_id, state.decisions.len(), error).await,
                );
            }

            // The scheduling item ends the invocation. What comes next may
            // need code changed between items (a seat's model, a voting
            // rule) that no decision can be parsed for, so each chosen
            // item runs as its own invocation (Steward, 2026-09-22).
            // `break`, not `return`: the meeting still gets its summary
            // and is adjourned rather than left `active`.
            if scheduled {
                if i + 1 < total_items {
                    println!(
                        "\n  The Council has set the docket. The {} remaining \
                         item(s) on this run's agenda are not taken up here.",
                        total_items - i - 1
                    );
                }
                break;
            }
        }
    }

    // Adjourn. The meeting's summary is its decisions' signed summaries,
    // composed here, not written by anyone (`adjournment`; Steward,
    // 2026-10-17). Stored on the meeting row, as the Lawyer's meeting
    // summary was, for readers of it; the server serves the same summaries
    // from the signed records.
    if stop.is_requested() {
        let error = stop_between_items(config, meeting_id, &mut terminal).await;
        return Err(close_aborted(pool, signer, meeting_id, state.decisions.len(), error).await);
    }
    if !decided.is_empty() {
        let text = adjournment::compose(Utc::now().date_naive(), &decided);
        queries::set_meeting_summary(pool, meeting_id, &text).await?;
        tracing::info!(
            event_type = "meeting_summary_composed",
            meeting = %meeting_id,
            decisions = decided.len(),
            unsummarized = decided.iter().filter(|d| d.summary.is_none()).count(),
            "the meeting summary was composed from the decisions' signed summaries"
        );
        println!("\n  Meeting summary stored:\n\n{text}");
    }
    queries::adjourn_meeting(pool, meeting_id).await?;

    println!("\n================================================");
    println!("  MEETING ADJOURNED");
    println!("  Decisions: {}", state.decisions.len());
    for decision in &state.decisions {
        println!(
            "    - {}: {} ({})",
            decision.title,
            decision.outcome().name(),
            decision.vote_tally()
        );
    }
    println!("================================================\n");

    // What the sitting cost, now rather than hours later in the console.
    crate::usage::report_meeting(pool, meeting_id).await;

    Ok(())
}

/// Close the meeting after a refusal or an emission breach aborted the
/// sitting, and say what stands.
///
/// Before the refusal-abort rule an aborted sitting left its meeting
/// `active`, to be swept to `cancelled` on the next run. The next run now
/// waits for the investigation (`abort::ensure_none_pending`), so the
/// meeting would read "in progress" for days: it is closed here instead.
/// A meeting that decided nothing is cancelled, as the sweep would have
/// done (`cancel_stale_meetings`: the council is one interactive process,
/// so the only active meeting is this one). A meeting that recorded
/// decisions before the refusal is adjourned: those decisions are signed
/// and stand, and a cancelled meeting would drop out of the cadence and
/// disclosure queries as if it had never sat.
///
/// A Steward stop's record is appended to the Governance Log here
/// (`sealed.append`), with the governance key the sitting signs its
/// decisions with: the stop is on the record before the program exits. If
/// the append fails, the command to append the drafted record by hand is
/// printed; the next sitting is blocked either way.
///
/// The item in progress is not decided: its agenda row stays where it was
/// (`pending` if the Clerk's briefing drew the refusal, `deliberating` if
/// a round did) and its proposal is not marked deliberated, as on the old
/// abort path. The score-ordered agenda will not pick it up again (it is
/// already on an agenda); the Steward re-hears it with `--items` after the
/// investigation.
async fn close_aborted(
    pool: &PgPool,
    signer: &GovernanceSigner,
    meeting_id: agora_common::ids::CouncilMeetingId,
    decided: usize,
    error: anyhow::Error,
) -> anyhow::Error {
    if let Some(aborted) = SittingAborted::find(&error)
        && let Ok(sealed) = &aborted.sealed
        && let Some(record) = &sealed.append
    {
        match agora_common::govlog::record(pool, signer, record).await {
            Ok((id, attestation)) => {
                if let Err(e) = abort::note_recorded(&sealed.dir, &id) {
                    tracing::error!(
                        dump = %sealed.dir.display(),
                        record = %id,
                        error = %format!("{e:#}"),
                        "could not note the stop's record id in its manifest"
                    );
                }
                tracing::warn!(
                    event_type = "council_steward_stop_recorded",
                    meeting = %meeting_id,
                    record = %id,
                    chain_seq = attestation.chain_seq,
                    dump = %sealed.dir.display(),
                    "the Steward stop is recorded in the Governance Log"
                );
                println!(
                    "  The stop is recorded in the Governance Log: {id} (chain_seq {}).",
                    attestation.chain_seq
                );
            }
            Err(e) => {
                tracing::error!(
                    event_type = "council_steward_stop_record_failed",
                    meeting = %meeting_id,
                    error = %e,
                    "the Steward stop could not be appended to the Governance Log"
                );
                println!("  !! The stop could not be appended to the Governance Log: {e}");
                if let Some(path) = &sealed.record {
                    println!("  !! Append the drafted record by hand:");
                    println!("  !!   just govlog-record {} --yes", path.display());
                }
            }
        }
    }
    let closed = if decided == 0 {
        queries::cancel_stale_meetings(pool)
            .await
            .map(|_| "cancelled")
    } else {
        queries::adjourn_meeting(pool, meeting_id)
            .await
            .map(|()| "adjourned")
    };
    let cause = SittingAborted::find(&error)
        .map(|aborted| aborted.cause.short())
        .unwrap_or("an abort");
    match closed {
        Ok(status) => {
            tracing::warn!(
                event_type = "council_sitting_aborted",
                meeting = %meeting_id,
                status,
                decided,
                cause,
                "the sitting was aborted"
            );
            println!("  Meeting {meeting_id} {status}.");
            if decided > 0 {
                println!(
                    "  The {decided} decision(s) recorded before {cause} stand, as \
                     signed.\n  No meeting summary was composed."
                );
            }
        }
        Err(e) => {
            tracing::error!(
                event_type = "council_sitting_aborted",
                meeting = %meeting_id,
                cause,
                error = %e,
                "the sitting was aborted and its meeting could not be closed"
            );
            println!(
                "  !! Meeting {meeting_id} could not be closed ({e}); the next run will \
                 mark it cancelled."
            );
        }
    }
    error
}

/// A stop that took effect between items (or before adjournment): the
/// Ctrl-C came during a database write, which a stop does not cut short.
/// No conversation is live; the seats' were per item, and sealed with
/// nothing to seal they would be empty.
async fn stop_between_items(
    config: &Config,
    meeting_id: agora_common::ids::CouncilMeetingId,
    terminal: &mut steward::Terminal,
) -> anyhow::Error {
    let requested = StewardStopRequested {
        during: StopDuring::BetweenSteps,
    };
    let facts = crate::stop::take(terminal, requested, Utc::now());
    let stopped = abort::StewardStop {
        facts,
        attachments: Vec::new(),
    };
    abort::seal(
        config,
        Moment {
            subject: abort::Subject::Meeting(meeting_id),
            proposal: None,
            title: "the sitting",
            stage: Stage::BetweenItems,
        },
        &stopped,
        &[],
        None,
    )
    .await
}

/// Seal a Steward stop taken during a decision's summary, with the seats'
/// conversations as they stood (copied before the seats were dropped), at
/// `stage`: [`Stage::SummaryStopped`] once the decision is signed,
/// [`Stage::DecisionSummary`] if it could not be.
pub(crate) async fn seal_summary_stop(
    config: &Config,
    item: &AgendaItem,
    decision: &Decision,
    facts: &crate::stop::StopFacts,
    conversations: &[(abort::Holder, misanthropic::prompt::CachedPrompt)],
    stage: Stage,
) -> anyhow::Error {
    let stop = abort::StewardStop {
        facts: facts.clone(),
        attachments: deliberation::decision_stop_attachments(item, decision),
    };
    let conversations: Vec<abort::Conversation<'_>> = conversations
        .iter()
        .map(|(holder, prompt)| abort::Conversation {
            holder: holder.clone(),
            prompt,
        })
        .collect();
    abort::seal(
        config,
        Moment::item(item, stage),
        &stop,
        &conversations,
        Some(ClerkBriefing::of(item)),
    )
    .await
}

/// What [`record_decision_at`] wrote.
#[derive(Debug)]
pub(crate) struct Recorded {
    pub id: agora_common::ids::GovernanceLogId,
    /// The summary signed with the record, when it has one.
    pub summary: Option<agora_common::govlog::DecisionSummary>,
    /// The id the summary was written under, when another writer took it
    /// first: the decision is signed as `id`, without the summary
    /// ([`DecisionIdMoved`]).
    pub moved: Option<agora_common::ids::GovernanceLogId>,
}

/// The id peeked before the summary is no longer the id the transaction
/// allocates: something else inserted a `council_decisions` row in
/// between. The decision is signed under the allocated id, without its
/// summary, and the sitting ends.
#[derive(Debug)]
pub(crate) struct DecisionIdMoved {
    pub peeked: agora_common::ids::GovernanceLogId,
    pub allocated: agora_common::ids::GovernanceLogId,
}

impl std::fmt::Display for DecisionIdMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the next decision id changed between the summary and the signing ({} was \
             summarized, {} was allocated). The decision is signed as the allocated id \
             WITHOUT its summary, which is attached (summary-review.md) for a signed \
             revision to add, corrected. The council assumes it is the only writer of \
             council_decisions (agora-council, one process at a time), so another writer \
             ran concurrently: the sitting ends here. Find it before sitting again.",
            self.peeked, self.allocated
        )
    }
}

impl std::error::Error for DecisionIdMoved {}

/// `summary-review.md` for a decision signed under another id than its
/// summary was written under: why, and the accepted text (and any review
/// it already had), for a revision to add once corrected
fn moved_review(
    peeked: &agora_common::ids::GovernanceLogId,
    allocated: &agora_common::ids::GovernanceLogId,
    written: &summary::Written,
) -> agora_common::govlog::CouncilAttachment {
    let mut content = format!(
        "# No summary: the decision's id was taken\n\n\
         The summary was written and accepted for this decision as {peeked}, the id \
         that was next when it was written. Another writer inserted a Council decision \
         before this one was signed, so it is signed as {allocated}, without the summary, \
         which may name the wrong id. A signed revision can add it, corrected \
         (`--set-summary`).\n\n"
    );
    if let Some(text) = &written.text {
        let _ = write!(
            content,
            "## The summary as accepted (not signed)\n\n{}\n\n",
            text.trim()
        );
    }
    if let Some(review) = &written.review
        && let agora_common::govlog::Redactable::Value(text) = &review.content
    {
        let _ = write!(content, "## Its review\n\n{}\n", text.trim());
    }
    agora_common::govlog::CouncilAttachment {
        name: summary::REVIEW_ATTACHMENT.to_string(),
        note: "Why this decision was signed without its summary: another writer took \
               the id it was written under."
            .to_string(),
        content: agora_common::govlog::Redactable::Value(content),
    }
}

/// A council decision's governance-log `data` with no summary: agentkit's
/// [`CouncilDecisionRecord`](crate::models::CouncilDecisionRecord), built
/// by [`Decision::record`], before `append` adds its blind. The tests'
/// view of what [`signed_log_data`] adds to.
#[cfg(test)]
pub(crate) fn council_log_data(
    id: &agora_common::ids::GovernanceLogId,
    meeting_id: agora_common::ids::CouncilMeetingId,
    decision: &Decision,
) -> serde_json::Result<serde_json::Value> {
    serde_json::to_value(decision.record(id, meeting_id))
}

/// A council decision's governance-log `data` as signed: the record, with
/// the Lawyer's accepted summary (accepted by whoever accepts on the item:
/// agentkit's `with_summary` reads it from `final_votes`) and the review's
/// attachment, if any.
pub(crate) fn signed_log_data(
    id: &agora_common::ids::GovernanceLogId,
    meeting_id: agora_common::ids::CouncilMeetingId,
    decision: &Decision,
    written: &summary::Written,
) -> serde_json::Result<serde_json::Value> {
    let mut record = decision.record(id, meeting_id);
    if let Some(review) = &written.review {
        record.attachments.push(review.clone());
    }
    if let Some(text) = &written.text {
        record = record.with_summary(text.clone());
    }
    serde_json::to_value(record)
}

/// The id the next recorded decision will get, peeked outside the chain
/// lock so the summary can name it. One-writer assumption: see
/// [`record_decision_at`].
pub(crate) async fn peek_decision_id(
    pool: &PgPool,
) -> anyhow::Result<agora_common::ids::GovernanceLogId> {
    let mut conn = pool.acquire().await?;
    Ok(queries::next_decision_id(&mut conn).await?)
}

/// [`record_decision_at`] the next id, with `written` as the summary: the
/// tests' form, which has no Lawyer to ask.
#[cfg(test)]
pub(crate) async fn record_decision(
    pool: &PgPool,
    signer: &GovernanceSigner,
    state: &MeetingState,
    decision: &Decision,
    written: &summary::Written,
) -> anyhow::Result<Recorded> {
    let peeked = peek_decision_id(pool).await?;
    record_decision_at(pool, signer, state, decision, &peeked, written).await
}

/// Record a decision to the database and governance log, signed with its
/// summary (`written`, accepted already: [`summary::write_summary`]),
/// returning its log id and the summary signed.
///
/// **The summary comes first.** It is written and accepted *before*
/// anything is signed, so a refusal there leaves nothing signed or stored
/// (the caller aborts the sitting). The summary's record names the
/// decision's id, and the id is allocated under the chain lock inside the
/// transaction. So the id is **peeked** first ([`peek_decision_id`]) with
/// the same count, and the transaction checks that it allocates the same
/// one. That is safe on one assumption: **only agora-council inserts
/// `council_decisions`, and one council process runs at a time.** If the
/// ids differ anyway, [`DecisionIdMoved`] is returned before anything is
/// signed; nothing is retried and nothing is sent again.
///
/// The `council_decisions` row and the signed governance log entry commit
/// together: a decision that exists without its public record, or the
/// reverse, is exactly the inconsistency the chain is meant to rule out.
pub(crate) async fn record_decision_at(
    pool: &PgPool,
    signer: &GovernanceSigner,
    state: &MeetingState,
    decision: &Decision,
    peeked: &agora_common::ids::GovernanceLogId,
    written: &summary::Written,
) -> anyhow::Result<Recorded> {
    let final_votes = serde_json::to_value(decision.final_votes())?;
    let outcome = decision.outcome();
    let mut log_data = signed_log_data(peeked, state.meeting_id, decision, written)?;
    let mut signed_summary = agora_common::govlog::DecisionSummary::of_data(&log_data)?;
    let mut moved = None;

    let mut tx = pool.begin().await?;

    // The id allocator counts rows, so it runs under the same lock the
    // append takes — `append` acquires it first; the allocator then sees
    // a settled count for the rest of the transaction.
    let decision_id = {
        // Take the chain lock before allocating the id.
        agora_common::govlog::lock(&mut tx).await?;
        queries::next_decision_id(&mut tx).await?
    };
    if &decision_id != peeked {
        // Another writer took the id the summary was written under, and the
        // summary may name it. The vote is not thrown away: the decision is
        // signed under the id allocated, without the summary, which goes
        // into `summary-review.md` for a revision to add, corrected. The
        // caller ends the sitting (`DecisionIdMoved`).
        tracing::error!(
            event_type = "council_decision_id_moved",
            peeked = %peeked,
            allocated = %decision_id,
            "another writer inserted a council decision while the summary was written; \
             signing under the allocated id WITHOUT the summary"
        );
        let unsummarized = summary::Written {
            text: None,
            review: Some(moved_review(peeked, &decision_id, written)),
            outcome: summary::Outcome::Failed {
                reason: format!("the id {peeked} was taken by another writer"),
            },
        };
        log_data = signed_log_data(&decision_id, state.meeting_id, decision, &unsummarized)?;
        signed_summary = None;
        moved = Some(peeked.clone());
    }

    // Insert council decision
    queries::insert_decision(
        &mut tx,
        &decision_id,
        decision.agenda_item_id,
        state.meeting_id,
        outcome,
        &final_votes,
        &decision.vote_tally(),
        decision.landmark,
        &decision.tags,
    )
    .await?;

    let attestation = agora_common::govlog::append(
        &mut tx,
        signer,
        agora_common::govlog::NewEntry {
            id: decision_id.clone(),
            entry_type: agora_common::enums::GovernanceLogEntryType::CouncilDecision,
            data: &log_data,
            tags: &decision.tags,
            texts: None,
        },
    )
    .await?;

    // A referred appeal's outcome commits with the decision (agora#471):
    // the entry above is signed in-process, inside this transaction, so
    // decision row, signed entry and appeal write-back stand or fall
    // together. A write-back error rolls all three back.
    let referral = crate::referral::write_back(
        &mut tx,
        decision.agenda_item_id,
        &decision_id,
        &decision.outcome(),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(
        decision = %decision_id,
        chain_seq = attestation.chain_seq,
        entry_hash = %attestation.entry_hash,
        "governance log entry signed and chained"
    );
    crate::referral::report(&decision_id, &referral);

    // The signed summary is the precedent row. `precedents` is the single
    // summary store (migration 058), so writing it here is what puts the
    // decision in front of the next meeting, `get_content`, and the
    // justice pipeline's retrieval — one write, three readers. Signed
    // without a summary, the decision gets no row, and that is logged.
    if let Err(e) = agora_common::precedents::store_signed_council_summary(
        pool,
        &decision_id,
        agora_common::enums::GovernanceLogEntryType::CouncilDecision,
        &log_data,
        &decision.tags,
    )
    .await
    {
        tracing::warn!(
            decision = %decision_id,
            error = %e,
            "could not store the decision's precedent row; the \
             reconciliation sweep will project it from the record"
        );
    }

    // Store each round in the deliberation_rounds table
    for round in &decision.rounds {
        let round_type = round.round_type;

        let responses_json = stored_responses(round);

        queries::insert_round(
            pool,
            decision.agenda_item_id,
            round.number,
            round_type,
            &responses_json,
            round.steward_contribution.as_deref(),
        )
        .await?;
    }

    tracing::info!(decision = %decision_id, title = %decision.title, "recorded decision");
    Ok(Recorded {
        id: decision_id,
        summary: signed_summary,
        moved,
    })
}

/// A round's responses as `deliberation_rounds.responses` stores them:
/// each seat's [`StoredResponse`](crate::models::StoredResponse), keyed by
/// its lowercase name
pub(crate) fn stored_responses(round: &crate::models::Round) -> serde_json::Value {
    round
        .responses
        .iter()
        .map(|r| {
            (
                r.role.to_string().to_lowercase(),
                serde_json::to_value(crate::models::StoredResponse::from(r))
                    .expect("three strings serialize"),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

/// Write what each item's seats will be shown, before the Steward starts
/// the meeting: the recusal question, round 1's opening with the Steward
/// seated and recused, and the Clerk's reports, rendered by the same
/// functions the seats' prompts are built from. Under
/// `$AGORA_PROMPT_DUMP_DIR/preview-<meeting>/`; unset, the preview is
/// skipped and that is said. A write that fails is said too: the Steward
/// decides whether to start without it.
async fn write_preview(
    meeting_id: agora_common::ids::CouncilMeetingId,
    recap: &Content,
    items: &[AgendaItem],
) {
    let Some(root) = std::env::var_os(agora_common::prompt_archive::DUMP_DIR_ENV) else {
        println!(
            "\n  ** No preview: {} is unset, so what the seats will be shown is not written out. **",
            agora_common::prompt_archive::DUMP_DIR_ENV
        );
        return;
    };
    let dir = std::path::PathBuf::from(root).join(format!("preview-{meeting_id}"));
    println!("\n  Preview of what every seat will be shown, before anything is sent:");
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        let mut files: Vec<(String, String)> = Vec::new();
        if item.category.determines_recusal() {
            files.push((
                format!("{n}-recusal-question.md"),
                crate::context::recusal_question(item).to_string(),
            ));
            for (label, recused) in [("seated", false), ("recused", true)] {
                let mut opening = recap.clone();
                opening.extend(crate::context::opening_for(item, Some(recused)));
                files.push((
                    format!("{n}-round1-opening-{label}.md"),
                    opening.to_string(),
                ));
            }
        } else {
            let mut opening = recap.clone();
            opening.extend(crate::context::opening_for(item, None));
            files.push((format!("{n}-round1-opening.md"), opening.to_string()));
        }
        if let Some(summary) = &item.comment_summary {
            files.push((format!("{n}-clerk-summary.md"), summary.to_string()));
        }
        if let Some(precedents) = &item.precedents {
            files.push((format!("{n}-clerk-precedents.md"), precedents.to_string()));
        }
        println!("  {n}. {}", item.title);
        for (name, text) in files {
            let path = dir.join(&name);
            let written = async {
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(&path, &text).await
            }
            .await;
            match written {
                Ok(()) => println!("     {} ({} bytes)", path.display(), text.len()),
                Err(e) => println!("     ** could not write {}: {e} **", path.display()),
            }
        }
    }
    println!();
}

/// Draft a revision of a Council decision's signed summary
/// (`--set-summary`): a `govlog revise` request patching `/summary`
/// (agentkit `SUMMARY_POINTER`), written to `out` for the Steward to
/// review and append with `just govlog-revise <file>`. Nothing is
/// appended here, and nothing is written to `precedents`: once the
/// revision is appended, its transaction deletes the decision's precedent
/// row and the appeals sweep projects the revised summary
/// (`precedents::sync`).
///
/// The text is signed as **the Lawyer's**, accepted by the Steward or,
/// on an item he was recused from, by the Artist, Philosopher and
/// Engineer (agentkit's rule admits no other writer), so it must be text
/// the Lawyer wrote and those reviewers accepted. `source` goes into the
/// revision's note, so the log says where the text came from. The draft
/// is checked as `govlog revise` will check it: the patch applies to the
/// latest version, the result keeps the record's rules, and the text
/// passes the corpus gate.
pub async fn set_summary(
    pool: &PgPool,
    decision_id: &agora_common::ids::GovernanceLogId,
    summary: &str,
    source: &str,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    let revision = summary_revision(pool, decision_id, summary, source).await?;
    let text = serde_json::to_string_pretty(&revision)?;
    tokio::fs::write(out, format!("{text}\n")).await?;
    println!(
        "Drafted a revision of {decision_id}'s summary: {}",
        out.display()
    );
    println!(
        "Review it, then append it with:\n  just govlog-revise {}",
        out.display()
    );
    println!("(a dry run without --yes; nothing has been appended)");
    Ok(())
}

/// The revision request [`set_summary`] writes, checked
async fn summary_revision(
    pool: &PgPool,
    decision_id: &agora_common::ids::GovernanceLogId,
    summary: &str,
    source: &str,
) -> anyhow::Result<agora_common::govlog::RevisionFile> {
    anyhow::ensure!(
        !source.trim().is_empty(),
        "{decision_id}: a summary's revision needs a source"
    );
    let record = queries::get_governance_log_record(pool, decision_id).await?;
    anyhow::ensure!(
        record.entry_type == agora_common::enums::GovernanceLogEntryType::CouncilDecision,
        "{decision_id} is a {}; only a Council decision carries a signed summary",
        record.entry_type
    );
    agora_common::precedents::council_summary_gate(summary)
        .map_err(|e| anyhow::anyhow!("{decision_id}: the summary cannot be signed: {e}"))?;
    let latest = agora_common::govlog::latest_version(pool, decision_id).await?;
    summary_revision_of(decision_id, &latest, summary, source)
}

/// [`summary_revision`] over the decision's latest version, with no
/// database: an `add` at `/summary` (which replaces one already there),
/// with the acceptance the item requires, checked against the record's
/// rules
pub(crate) fn summary_revision_of(
    decision_id: &agora_common::ids::GovernanceLogId,
    latest: &serde_json::Value,
    summary: &str,
    source: &str,
) -> anyhow::Result<agora_common::govlog::RevisionFile> {
    use agora_common::govlog::json_patch::{AddOperation, Patch, PatchOperation};
    use agora_common::govlog::{CouncilDecisionRecord, SUMMARY_POINTER};
    let record: CouncilDecisionRecord = serde_json::from_value(latest.clone())
        .map_err(|e| anyhow::anyhow!("{decision_id}'s record does not read: {e}"))?;
    let had = record.summary.is_some();
    let signed = record
        .with_summary(summary.trim().to_string())
        .summary
        .expect("just written");
    let patch = Patch(vec![PatchOperation::Add(AddOperation {
        path: SUMMARY_POINTER.parse()?,
        value: serde_json::to_value(&signed)?,
    })]);
    let revised = agora_common::govlog::apply_patch(latest, &patch)?;
    agora_common::govlog::CouncilDecisionRecord::check_data(&revised)
        .map_err(|e| anyhow::anyhow!("{decision_id}: the revised record breaks a rule: {e}"))?;
    let accepted = agora_common::precedents::summary_source(&signed);
    Ok(agora_common::govlog::RevisionFile {
        target: decision_id.clone(),
        authority: None,
        basis: "The Steward's decision of 2026-10-17 that a Council decision's summary is \
                the Lawyer's, signed with its record, and changes only by a signed \
                revision."
            .to_string(),
        note: format!(
            "{} the decision's summary, written by {accepted}. The vote, the outcome \
             and the rest of the record are unchanged; the earlier version stays \
             readable (`version=original`). Source: {}.",
            if had { "Replaces" } else { "Adds" },
            source.trim()
        ),
        patch,
    })
}
