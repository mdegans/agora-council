use std::path::PathBuf;

use agora_common::secrets::Secret;

/// Configuration for the agora-council binary.
pub struct Config {
    /// Database connection URL.
    pub database_url: String,
    /// Anthropic API key, loaded from a file secret.
    pub api_key: Secret,
    /// Hex seed of the platform's Ed25519 governance signing key
    /// (`secrets/governance_signing_key`). Every governance log entry the
    /// council writes is signed with it (agora#127).
    pub governance_signing_key: Secret,
    /// Model ID to use for council agents (e.g. "claude-opus-4-1-20250805").
    pub model: String,
    /// Maximum tokens for each agent response.
    pub max_tokens: u32,
    /// Temperature for agent responses (0.0-1.0).
    pub temperature: f32,
    /// Maximum number of agenda items per meeting.
    pub max_agenda_items: i64,
    /// Log level.
    pub log_level: String,
    /// Where a refusal's sealed dump is written, one directory per refusal
    /// (`crate::abort`). Its unresolved manifests are the pending-
    /// investigation markers that block the next sitting.
    ///
    /// `AGORA_COUNCIL_REFUSALS_DIR`, else `<AGORA_SECRETS_DIR>/council/refusals`.
    /// A path, not a secret. `council-rehearse` points it at the real
    /// `secrets/` (its secrets dir is a scratch copy): a refusal in a
    /// rehearsal is a real refusal on the API account.
    pub refusals_dir: PathBuf,
    /// Where a Steward stop's sealed dump is written:
    /// `AGORA_COUNCIL_STOPS_DIR`, else `refusals_dir`, where an unresolved
    /// one blocks the next sitting until the Council has heard it.
    /// `council-rehearse` points it at the rehearsal's own dumps: a stop in
    /// a rehearsal is recorded only on the throwaway copy, and is no reason
    /// to hold a real sitting.
    pub stops_dir: PathBuf,
    /// Where the Steward's record (REC) of an aborted sitting is drafted:
    /// `AGORA_COUNCIL_RECORDS_DIR`, else `governance/records` (the council
    /// runs from the repository root under `just`).
    pub records_dir: PathBuf,
    /// Where the seats' agent tools (`get_content`) read: the Agora
    /// server's public root, as agentkit's REST client takes it (it adds
    /// `agora/`). `AGORA_COUNCIL_READ_URL`, else
    /// `https://subliminal.technology`; set it empty to sit without reads
    /// (the tools stay offered and refuse). Not a secret: the endpoint is
    /// the public, unauthenticated `GET /api/content/{ref}`.
    pub read_url: Option<url::Url>,
}

impl Config {
    /// Load configuration from environment variables and file secrets.
    pub fn from_env() -> anyhow::Result<Self> {
        let secrets_dir = PathBuf::from(
            std::env::var("AGORA_SECRETS_DIR").unwrap_or_else(|_| "/run/secrets".to_string()),
        );

        let db_host = std::env::var("AGORA_DB_HOST").unwrap_or_else(|_| "localhost".to_string());
        let db_port = std::env::var("AGORA_DB_PORT").unwrap_or_else(|_| "5432".to_string());
        let db_name = std::env::var("AGORA_DB_NAME").unwrap_or_else(|_| "agora".to_string());
        // The council connects as its scoped role: council tables and
        // public content, no private user data (migration 036). Override
        // with AGORA_DB_USER=agora only for break-glass administration.
        let db_user =
            std::env::var("AGORA_DB_USER").unwrap_or_else(|_| "agora_council".to_string());

        // Password file matches the role name (`db_password_council`),
        // falling back to the shared dev password for local hacking.
        let db_password = Secret::from_file(&secrets_dir.join("db_password_council"))
            .or_else(|_| Secret::from_file(&secrets_dir.join("db_password")))
            .map(|s| s.expose().to_string())
            .unwrap_or_else(|_| "agora_dev_password".to_string());

        // Use url::Url to safely encode the password (which may contain +, /, = etc.)
        let base_url = format!("postgres://{db_host}:{db_port}/{db_name}");
        let mut parsed_url =
            url::Url::parse(&base_url).map_err(|e| anyhow::anyhow!("invalid database URL: {e}"))?;
        parsed_url
            .set_username(&db_user)
            .map_err(|()| anyhow::anyhow!("failed to set database username"))?;
        parsed_url
            .set_password(Some(&db_password))
            .map_err(|()| anyhow::anyhow!("failed to set database password"))?;
        let database_url = parsed_url.to_string();

        let api_key = Secret::from_file(&secrets_dir.join("anthropic_api_key"))?;
        let governance_signing_key =
            Secret::from_file(&secrets_dir.join(agora_common::govlog::SECRET_FILE))?;

        let model =
            std::env::var("AGORA_COUNCIL_MODEL").unwrap_or_else(|_| "claude-opus-4-6".to_string());
        // 16384, up from 4096 (Steward, 2026-10-06). A seat's turn is one
        // tool call carrying its whole rationale; replaying the 2026-09-26
        // sitting on Qwen 3.8 hit 4096 before the vote. A cap is a backstop
        // against runaway output, not a length budget for a seat.
        let max_tokens = std::env::var("AGORA_COUNCIL_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16384);
        // 0.5, down from 0.7 — a hedge, not a fix.
        //
        // The 2026-08-01 vote corruption (see record.rs) was first blamed on
        // temperature and that was wrong: the flipped calls had text fields
        // ending in the literal word "yes", and no sampler flips a token
        // whose entire preceding context says yes. The suspect is the
        // `$ref`-schema'd vote enum under a constrained decoder, fixed in
        // record.rs by inlining the enum and dropping `strict`.
        //
        // Lower is still kept because a governance vote wants less sampling
        // variance than prose does, and because the real mechanism is not
        // yet proven. Do not read this default as the remedy.
        //
        // Model-specific: sampling parameters are accepted on Opus 4.6 and
        // Sonnet 4.6, and REMOVED on Opus 4.7+ / Opus 5 / Fable 5, where they
        // return a 400. If AGORA_COUNCIL_MODEL is ever bumped past 4.6, this
        // knob stops existing.
        let temperature = std::env::var("AGORA_COUNCIL_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.5);
        let max_agenda_items = std::env::var("AGORA_COUNCIL_MAX_ITEMS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let log_level = std::env::var("AGORA_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        let refusals_dir = std::env::var("AGORA_COUNCIL_REFUSALS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| secrets_dir.join("council").join("refusals"));
        let stops_dir = std::env::var("AGORA_COUNCIL_STOPS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| refusals_dir.clone());
        let records_dir = std::env::var("AGORA_COUNCIL_RECORDS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("governance").join("records"));
        let read_url = match std::env::var("AGORA_COUNCIL_READ_URL") {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => Some(
                url::Url::parse(s.trim())
                    .map_err(|e| anyhow::anyhow!("invalid AGORA_COUNCIL_READ_URL: {e}"))?,
            ),
            Err(_) => Some(
                url::Url::parse("https://subliminal.technology")
                    .expect("the default read URL parses"),
            ),
        };

        Ok(Config {
            database_url,
            api_key,
            governance_signing_key,
            model,
            max_tokens,
            temperature,
            max_agenda_items,
            log_level,
            refusals_dir,
            stops_dir,
            records_dir,
            read_url,
        })
    }

    /// A fixed configuration for unit tests — no environment, no secrets
    /// on disk, nothing ever sent anywhere.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Config {
            database_url: String::new(),
            api_key: Secret::new("test-key".to_string()),
            governance_signing_key: Secret::new("11".repeat(32)),
            model: "claude-opus-4-6".to_string(),
            max_tokens: 4096,
            temperature: 0.5,
            max_agenda_items: 10,
            log_level: "info".to_string(),
            // Nothing is written here unless a test points these at its
            // own temporary directory first.
            refusals_dir: std::env::temp_dir().join("agora-council-tests-unset/refusals"),
            stops_dir: std::env::temp_dir().join("agora-council-tests-unset/refusals"),
            records_dir: std::env::temp_dir().join("agora-council-tests-unset/records"),
            // Tests never reach a server; they script reads instead.
            read_url: None,
        }
    }
}
