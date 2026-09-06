//! Server settings.
//!
//! Environment variable names are deliberately unchanged from the
//! shell/Node implementation this replaces, so no machine and no cloud
//! environment needs re-provisioning to switch over.

use std::env;
use std::time::Duration;

/// Why the server cannot start.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    /// Mirrors the Node server's refusal to start without auth — a server
    /// reachable from the internet with no token is not a degraded mode
    /// worth supporting.
    #[error("RECALL_TOKEN is not set; refusing to start with no auth")]
    MissingToken,
}

/// What `recall serve` needs.
///
/// Every field has a default that is safe to run with, except [`token`],
/// which has none — see [`ConfigError::MissingToken`].
///
/// | Field | Variable | Default |
/// |---|---|---|
/// | [`addr`] | `RECALL_HOST`, `RECALL_PORT` | `0.0.0.0:8787` |
/// | [`token`] | `RECALL_TOKEN` | *required* |
/// | [`db_path`] | `RECALL_DB_PATH` | `data/recall.db` |
/// | [`git_commit`] | `RECALL_GIT_COMMIT` | `unknown` |
/// | [`backup_dir`] | `RECALL_BACKUP_DIR` | off |
/// | [`backup_interval`] | `RECALL_BACKUP_INTERVAL_MS` | 24h |
/// | [`backup_keep`] | `RECALL_BACKUP_KEEP` | 7 |
/// | [`rate_limit_window`] | `RECALL_RATE_LIMIT_WINDOW_MS` | 60s |
/// | [`rate_limit_max`] | `RECALL_RATE_LIMIT_MAX` | 60 |
/// | [`trusted_ip_header`] | `RECALL_TRUSTED_IP_HEADER` | `cf-connecting-ip` |
/// | [`merge_enabled`] | `RECALL_MERGE_ENABLED` | on |
/// | [`merge_timeout`] | `RECALL_MERGE_TIMEOUT_MS` | 45s |
/// | [`claude_bin`] | `RECALL_CLAUDE_BIN` | `claude` |
/// | [`claude_status_interval`] | `RECALL_CLAUDE_STATUS_INTERVAL_MS` | 30m |
///
/// [`addr`]: Config::addr
/// [`token`]: Config::token
/// [`db_path`]: Config::db_path
/// [`git_commit`]: Config::git_commit
/// [`backup_dir`]: Config::backup_dir
/// [`backup_interval`]: Config::backup_interval
/// [`backup_keep`]: Config::backup_keep
/// [`rate_limit_window`]: Config::rate_limit_window
/// [`rate_limit_max`]: Config::rate_limit_max
/// [`trusted_ip_header`]: Config::trusted_ip_header
/// [`merge_enabled`]: Config::merge_enabled
/// [`merge_timeout`]: Config::merge_timeout
/// [`claude_bin`]: Config::claude_bin
/// [`claude_status_interval`]: Config::claude_status_interval
#[derive(Debug, Clone)]
pub struct Config {
    /// The socket to bind, assembled from host and port.
    pub addr: String,
    /// The single bearer token. There is no second one, by design.
    pub token: String,
    /// The SQLite file. Opened, never created from a schema migration — it
    /// is the same file the Node server wrote.
    pub db_path: String,
    /// Reported by `GET /health` so a deploy can be confirmed from outside.
    pub git_commit: String,

    /// Where periodic database snapshots go. Empty disables backups.
    pub backup_dir: String,
    /// How often to take one.
    pub backup_interval: Duration,
    /// How many to keep before deleting the oldest.
    pub backup_keep: usize,

    /// The window rate limiting counts requests over.
    pub rate_limit_window: Duration,
    /// How many requests one client may make in that window.
    pub rate_limit_max: u32,
    /// The one request header whose value is taken as the client's address,
    /// or empty to trust none and use the socket's peer address.
    ///
    /// Rate limiting keys off this, and rate limiting runs *before* auth
    /// precisely so a flood of invalid tokens is limited too — so a client
    /// that can choose its own value here can rotate it and get unlimited
    /// attempts at guessing the token.
    ///
    /// That makes this a statement about the deployment, not a preference:
    /// it names the header the *ingress* sets, and it is only safe when
    /// nothing can reach this server except through that ingress. Exactly
    /// one header is read, so a value the client supplies under any other
    /// name is ignored.
    ///
    /// | Ingress | Set this to |
    /// |---|---|
    /// | Cloudflare Tunnel | `cf-connecting-ip` (the default) |
    /// | Traefik, nginx, Caddy | `x-real-ip` |
    /// | None — reached directly | empty |
    ///
    /// Deliberately *not* `x-forwarded-for`: a proxy appends to it, so the
    /// first entry is whatever the client sent. Reading it as one value is
    /// the classic way to make this setting useless.
    pub trusted_ip_header: String,

    /// Whether to attempt semantic merge at all. Off means last-write-wins.
    pub merge_enabled: bool,
    /// How long a merge may take before it is abandoned — and, like every
    /// other merge failure, degraded to last-write-wins.
    pub merge_timeout: Duration,
    /// The `claude` binary to shell out to. Never the Anthropic API.
    pub claude_bin: String,
    /// How often to re-check that the binary is present and logged in.
    pub claude_status_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:8787".to_string(),
            token: String::new(),
            db_path: "data/recall.db".to_string(),
            git_commit: "unknown".to_string(),
            backup_dir: String::new(),
            backup_interval: Duration::from_secs(24 * 60 * 60),
            backup_keep: 7,
            rate_limit_window: Duration::from_secs(60),
            rate_limit_max: 60,
            trusted_ip_header: "cf-connecting-ip".to_string(),
            merge_enabled: true,
            merge_timeout: Duration::from_secs(45),
            claude_bin: "claude".to_string(),
            claude_status_interval: Duration::from_secs(30 * 60),
        }
    }
}

impl Config {
    /// Reads configuration from the real process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Reads configuration through a caller-supplied lookup, applying the
    /// same defaults the Node implementation used.
    ///
    /// The lookup is injected rather than read from `std::env` inside so the
    /// clamping below is testable: `set_var` is process-global, and Rust runs
    /// tests in parallel threads, so an env-reading test races every other
    /// test in the binary.
    ///
    /// An unparseable value falls back rather than failing the boot, matching
    /// the Node and Go implementations — a typo in one tunable should not be
    /// the reason a server won't start.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |key: &str| lookup(key).filter(|v| !v.is_empty());
        let or = |key: &str, fallback: &str| get(key).unwrap_or_else(|| fallback.to_string());
        let num =
            |key: &str, fallback: u64| get(key).and_then(|v| v.parse().ok()).unwrap_or(fallback);

        let mut cfg = Config {
            addr: format!("0.0.0.0:{}", or("RECALL_PORT", "8787")),
            token: get("RECALL_TOKEN").unwrap_or_default(),
            db_path: or("RECALL_DB_PATH", "data/recall.db"),
            git_commit: or("RECALL_GIT_COMMIT", "unknown"),
            backup_dir: get("RECALL_BACKUP_DIR").unwrap_or_default(),
            backup_interval: Duration::from_secs(
                num("RECALL_BACKUP_INTERVAL_HOURS", 24).saturating_mul(3600),
            ),
            backup_keep: num("RECALL_BACKUP_KEEP", 7) as usize,
            rate_limit_window: Duration::from_millis(num("RECALL_RATE_LIMIT_WINDOW_MS", 60_000)),
            rate_limit_max: num("RECALL_RATE_LIMIT_MAX", 60) as u32,
            // Lowercased because HeaderMap lookups are case-insensitive but
            // this is compared as a plain string.
            trusted_ip_header: lookup("RECALL_TRUSTED_IP_HEADER")
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "cf-connecting-ip".to_string()),
            // Opt-out, not opt-in: only the literal "false" disables it, so a
            // typo leaves merge on rather than silently off.
            merge_enabled: lookup("RECALL_MERGE_ENABLED").as_deref() != Some("false"),
            merge_timeout: Duration::from_millis(num("RECALL_MERGE_TIMEOUT_MS", 45_000)),
            claude_bin: or("RECALL_CLAUDE_BIN", "claude"),
            claude_status_interval: Duration::from_millis(num(
                "RECALL_CLAUDE_STATUS_INTERVAL_MS",
                30 * 60_000,
            )),
        };
        if cfg.token.is_empty() {
            return Err(ConfigError::MissingToken);
        }
        // A zero interval would spin a background loop as fast as the
        // scheduler allows.
        if cfg.backup_interval.is_zero() {
            cfg.backup_interval = Duration::from_secs(24 * 60 * 60);
        }
        if cfg.rate_limit_window.is_zero() {
            cfg.rate_limit_window = Duration::from_secs(60);
        }
        if cfg.claude_status_interval.is_zero() {
            cfg.claude_status_interval = Duration::from_secs(30 * 60);
        }
        // A zero timeout is worse than a spinning loop: every merge would
        // hit an already-expired deadline and fail instantly, silently
        // degrading to last-write-wins with nothing in the logs that points
        // at the typo responsible.
        if cfg.merge_timeout.is_zero() {
            cfg.merge_timeout = Duration::from_millis(45_000);
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn defaults_match_the_node_implementation() {
        let cfg = Config::default();
        assert_eq!(cfg.rate_limit_max, 60);
        assert_eq!(cfg.rate_limit_window, Duration::from_secs(60));
        assert_eq!(cfg.merge_timeout, Duration::from_secs(45));
        assert_eq!(cfg.backup_keep, 7);
        assert!(cfg.merge_enabled);
        assert_eq!(cfg.claude_bin, "claude");
    }

    #[test]
    fn refuses_to_start_without_a_token() {
        assert!(
            matches!(
                Config::from_lookup(env(&[])),
                Err(ConfigError::MissingToken)
            ),
            "a server reachable from the internet with no auth is not a degraded mode worth supporting"
        );
        // Set-but-empty is not set. Go's os.Getenv couldn't tell the two
        // apart; here it would otherwise boot with a token of "".
        assert!(matches!(
            Config::from_lookup(env(&[("RECALL_TOKEN", "")])),
            Err(ConfigError::MissingToken)
        ));
    }

    #[test]
    fn reads_every_override() {
        let cfg = Config::from_lookup(env(&[
            ("RECALL_TOKEN", "t"),
            ("RECALL_PORT", "9000"),
            ("RECALL_DB_PATH", "/data/x.db"),
            ("RECALL_GIT_COMMIT", "abc1234"),
            ("RECALL_BACKUP_DIR", "/backups"),
            ("RECALL_BACKUP_INTERVAL_HOURS", "6"),
            ("RECALL_BACKUP_KEEP", "3"),
            ("RECALL_RATE_LIMIT_WINDOW_MS", "1000"),
            ("RECALL_RATE_LIMIT_MAX", "5"),
            ("RECALL_MERGE_TIMEOUT_MS", "1234"),
            ("RECALL_CLAUDE_BIN", "/usr/bin/claude"),
            ("RECALL_CLAUDE_STATUS_INTERVAL_MS", "60000"),
        ]))
        .unwrap();

        assert_eq!(cfg.addr, "0.0.0.0:9000");
        assert_eq!(cfg.db_path, "/data/x.db");
        assert_eq!(cfg.git_commit, "abc1234");
        assert_eq!(cfg.backup_dir, "/backups");
        assert_eq!(cfg.backup_interval, Duration::from_secs(6 * 3600));
        assert_eq!(cfg.backup_keep, 3);
        assert_eq!(cfg.rate_limit_window, Duration::from_millis(1000));
        assert_eq!(cfg.rate_limit_max, 5);
        assert_eq!(cfg.merge_timeout, Duration::from_millis(1234));
        assert_eq!(cfg.claude_bin, "/usr/bin/claude");
        assert_eq!(cfg.claude_status_interval, Duration::from_millis(60_000));
    }

    #[test]
    fn merge_is_disabled_only_by_the_literal_false() {
        for (value, want) in [("false", false), ("true", true), ("0", true), ("", true)] {
            let cfg = Config::from_lookup(env(&[
                ("RECALL_TOKEN", "t"),
                ("RECALL_MERGE_ENABLED", value),
            ]))
            .unwrap();
            assert_eq!(cfg.merge_enabled, want, "RECALL_MERGE_ENABLED={value:?}");
        }
    }

    /// Every duration is clamped, not just the ones whose failure is loud.
    ///
    /// The Go implementation clamped only two of the four. A zero
    /// `CLAUDE_STATUS_INTERVAL` reached `time.NewTicker`, which panics on a
    /// non-positive duration — one config typo crashing the server at
    /// startup. A zero `MERGE_TIMEOUT` is quieter and worse: every merge
    /// hits an already-expired deadline and fails instantly, silently
    /// degrading to last-write-wins with nothing pointing at the cause.
    #[test]
    fn zero_and_unparseable_durations_fall_back_to_their_defaults() {
        for value in ["0", "not-a-number", "-5", " 6"] {
            let cfg = Config::from_lookup(env(&[
                ("RECALL_TOKEN", "t"),
                ("RECALL_BACKUP_INTERVAL_HOURS", value),
                ("RECALL_RATE_LIMIT_WINDOW_MS", value),
                ("RECALL_CLAUDE_STATUS_INTERVAL_MS", value),
                ("RECALL_MERGE_TIMEOUT_MS", value),
            ]))
            .unwrap();

            assert_eq!(
                cfg.backup_interval,
                Duration::from_secs(24 * 3600),
                "{value:?}"
            );
            assert_eq!(cfg.rate_limit_window, Duration::from_secs(60), "{value:?}");
            assert_eq!(
                cfg.claude_status_interval,
                Duration::from_secs(30 * 60),
                "{value:?}"
            );
            assert_eq!(
                cfg.merge_timeout,
                Duration::from_millis(45_000),
                "{value:?}"
            );
            assert!(
                !cfg.merge_timeout.is_zero(),
                "a zero merge timeout fails every merge instantly and silently"
            );
        }
    }
}
