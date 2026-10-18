mod abort;
mod adjournment;
mod briefing;
mod citations;
mod clerk;
mod config;
mod context;
mod councillor;
mod deliberation;
mod docket;
mod interview;
#[cfg(test)]
mod live_replay_tests;
mod meeting;
mod models;
mod operations;
mod queries;
mod record;
#[cfg(test)]
mod record_tests;
mod referral;
mod seat_tools;
mod steward;
mod stop;
mod summary;
mod threads;
mod usage;
mod wire;
mod witness;
#[cfg(test)]
mod witness_tests;

use agora_common::wire::WireClient;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration
    let config = Config::from_env()?;

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    agora_common::refusal::set_service("agora-council");
    // A sitting's log is its terminal, which a Claude session may be
    // reading: never quote refused context there. Every refusal in a
    // sitting is sealed whole instead (abort.rs), and the seal logs the
    // dump's path.
    agora_common::refusal::omit_prompt_tail(
        "[omitted: agora-council seals the refused request; see the council_refusal_sealed event]",
    );

    let args: Vec<String> = std::env::args().collect();

    // `interview incumbents|candidate …`: fresh-context asks, blind. See
    // interview.rs; it never prints a response.
    if args.get(1).map(String::as_str) == Some("interview") {
        return interview::main(&config, &args[2..]).await;
    }
    // `resolve-refusal <dump-dir> --resolution … --reason …`: close the
    // investigation of a refusal that aborted a sitting (see abort.rs).
    if args.get(1).map(String::as_str) == Some("resolve-refusal") {
        return abort::resolve_main(&config, &args[2..]).await;
    }
    let force = args.iter().any(|a| a == "--force");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // A refusal under investigation blocks every path below that calls a
    // model (the sitting, a rehearsal, --clerk-summary):
    // any of them could re-send the material that drew it, and repeat
    // refusals put the API account at risk. The dry run calls no model;
    // it only reports. This replaces GOV-2026-0011's escalation counter,
    // pending the Council's approval of the amendment.
    if dry_run {
        let pending = abort::pending(&config.refusals_dir)?;
        if !pending.is_empty() {
            abort::print_pending(&pending);
        }
    } else {
        abort::ensure_none_pending(&config)?;
    }

    // --items <uuid,uuid,...>: the Steward names the agenda and its order
    // (governance-protocol.md § 2.3), instead of taking the top proposals
    // by score. The break-glass for an item score cannot reach — and, once
    // registration opens, for an agenda clogged with high-scoring noise.
    let steward_items: Vec<agora_common::ids::PostId> =
        match args.iter().position(|a| a == "--items") {
            Some(pos) => {
                let list = args.get(pos + 1).ok_or_else(|| {
                    anyhow::anyhow!("--items requires a comma-separated list of proposal UUIDs")
                })?;
                list.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        uuid::Uuid::parse_str(s)
                            .map(agora_common::ids::PostId::from)
                            .map_err(|e| anyhow::anyhow!("--items: {s:?} is not a UUID: {e}"))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
            }
            None => Vec::new(),
        };

    // --referrals <appeal-uuid,...>: appeals the Court referred, placed on
    // this sitting's agenda by the Steward (governance-protocol.md § 2.3:
    // "placed on an agenda by hand"), heard first. A flag of its own, not
    // `--items`: an appeal id and a post id are the same shape, and one
    // list holding either would let an untyped id pick its table.
    let referrals: Vec<agora_common::ids::AppealId> =
        match args.iter().position(|a| a == "--referrals") {
            Some(pos) => {
                let list = args.get(pos + 1).ok_or_else(|| {
                    anyhow::anyhow!("--referrals requires a comma-separated list of appeal UUIDs")
                })?;
                list.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        uuid::Uuid::parse_str(s)
                            .map(agora_common::ids::AppealId::from)
                            .map_err(|e| anyhow::anyhow!("--referrals: {s:?} is not a UUID: {e}"))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
            }
            None => Vec::new(),
        };

    // --witnesses <file>: the witnesses called for each item, by proposal
    // (OBL-2026-0028; see witness.rs). Checked before anything is sent.
    let witnesses = match args.iter().position(|a| a == "--witnesses") {
        Some(pos) => {
            let path = args
                .get(pos + 1)
                .ok_or_else(|| anyhow::anyhow!("--witnesses requires a file"))?;
            let file = witness::load(std::path::Path::new(path))?;
            for (proposal, called) in &file {
                let ids: Vec<String> = called.iter().map(|w| w.id.to_string()).collect();
                println!("  Witnesses for {proposal}: {}", ids.join(", "));
            }
            file
        }
        None => witness::WitnessFile::new(),
    };

    // Connect to database
    let pool = agora_common::db::create_pool(&config.database_url).await?;
    tracing::info!("Connected to database");

    // `threads attach|detach|list …`: the Steward attaches related threads
    // to an agenda item by hand (#492). Database only, like --dry-run.
    if args.get(1).map(String::as_str) == Some("threads") {
        return threads::main(&pool, &args[2..]).await;
    }

    // --usage <meeting-uuid>: what a past sitting cost, per model and
    // role, with suspected cache breaks. Read only; no API call.
    if let Some(pos) = args.iter().position(|a| a == "--usage") {
        let raw = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--usage requires a meeting UUID"))?;
        return usage::main(&pool, raw).await;
    }

    // --dry-run: report the agenda a meeting would take up, then exit.
    // Read-only, and deliberately ahead of the client — a preflight
    // should not need a working Anthropic connection.
    if dry_run {
        return meeting::dry_run(&pool, &config, &steward_items, &referrals).await;
    }

    // Create Anthropic client
    let client = WireClient::new(misanthropic::Client::new(
        config.api_key.expose().to_string(),
    )?);

    // The governance signing key (agora#127). Loaded before anything is
    // deliberated: a council that cannot sign its decisions must not sit.
    let signer =
        agora_common::govlog::GovernanceSigner::from_secret(&config.governance_signing_key)?;

    // --clerk-summary <post_id>: print the Clerk's summary of a proposal's
    // comment thread and exit. A preview: no meeting, no deliberation, no
    // vote, nothing written. For checking the Clerk's work before a
    // sitting without re-running one.
    if let Some(pos) = args.iter().position(|a| a == "--clerk-summary") {
        let raw = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("--clerk-summary requires a proposal UUID"))?;
        let post_id = agora_common::ids::PostId::from(uuid::Uuid::parse_str(raw)?);
        let detail = queries::get_proposal_detail(&pool, post_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no proposal {post_id}"))?;
        // Every thread the item would carry (#492), as a sitting loads them.
        let comments = threads::load(&pool, post_id, &detail.title, &detail.agent_name).await?;
        eprintln!("Clerk: summarizing {} comments...", comments.len());
        let summary =
            clerk::summarize_comments(&client, None, &detail.title, &detail.body, &comments)
                .await?;
        println!("{summary}");
        return Ok(());
    }

    // --resummarize was here: it regenerated a decision's summary with
    // the Clerk. A decision's summary is the Lawyer's, signed with its
    // record, and nothing regenerates it (Steward, 2026-10-17).
    if args.iter().any(|a| a == "--resummarize") {
        anyhow::bail!(
            "--resummarize is retired: a decision's summary is the Lawyer's, signed with \
             its record. Change one with --set-summary, which drafts a signed revision."
        );
    }

    // --set-summary <decision_id> <file> <source> [<out.json>]: draft a
    // revision of a Council decision's signed summary to `out.json`
    // (default governance/revisions/<date>-<id>-summary.json), for the
    // Steward to append with `just govlog-revise`. Nothing is appended.
    if let Some(pos) = args.iter().position(|a| a == "--set-summary") {
        let usage = "--set-summary <decision_id> <file> <source> [<out.json>]";
        let id = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("{usage}"))?
            .parse::<agora_common::ids::GovernanceLogId>()?;
        let file = args
            .get(pos + 2)
            .ok_or_else(|| anyhow::anyhow!("{usage}"))?;
        let source = args
            .get(pos + 3)
            .ok_or_else(|| anyhow::anyhow!("{usage}"))?;
        let out = args.get(pos + 4).map_or_else(
            || {
                std::path::PathBuf::from("governance/revisions").join(format!(
                    "{}-{}-summary.json",
                    chrono::Utc::now().format("%Y-%m-%d"),
                    id.as_str().to_lowercase()
                ))
            },
            std::path::PathBuf::from,
        );
        let summary = tokio::fs::read_to_string(file).await?;
        return meeting::set_summary(&pool, &id, &summary, source, &out).await;
    }

    // --set-legacy-summary <decision_id> <file> <source>: replace the
    // unsigned summary of a decision signed before agentkit 0.96, naming
    // who wrote it. Append a correction amendment with both texts too.
    if let Some(pos) = args.iter().position(|a| a == "--set-legacy-summary") {
        let usage = "--set-legacy-summary <decision_id> <file> <source>";
        let id = args
            .get(pos + 1)
            .ok_or_else(|| anyhow::anyhow!("{usage}"))?
            .parse::<agora_common::ids::GovernanceLogId>()?;
        let file = args
            .get(pos + 2)
            .ok_or_else(|| anyhow::anyhow!("{usage}"))?;
        let source = args
            .get(pos + 3)
            .ok_or_else(|| anyhow::anyhow!("{usage}"))?;
        let summary = tokio::fs::read_to_string(file).await?;
        let record = queries::get_governance_log_record(&pool, &id).await?;
        let latest = agora_common::govlog::latest_version(&pool, &id).await?;
        agora_common::precedents::store_legacy_council_summary(
            &pool,
            &id,
            record.entry_type,
            &latest,
            &record.tags,
            &summary,
            source,
        )
        .await?;
        println!("Stored {id}'s summary, written by {source}. Append its correction amendment.");
        return Ok(());
    }

    println!();
    println!("  ╔═══════════════════════════════════╗");
    println!("  ║       AGORA COUNCIL               ║");
    println!("  ║   Governance Deliberation Engine   ║");
    println!("  ╚═══════════════════════════════════╝");
    println!();

    tracing::info!("Anthropic client initialized (model: {})", config.model);

    // Run the meeting
    meeting::run(
        &pool,
        &client,
        &config,
        &signer,
        force,
        &steward_items,
        &referrals,
        &witnesses,
    )
    .await?;

    Ok(())
}
