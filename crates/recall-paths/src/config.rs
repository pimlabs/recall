//! Settings loaded from the environment for both halves of the binary.
//!
//! Variable names are deliberately unchanged from the shell/Node
//! implementation this replaces, so no machine and no cloud environment
//! needs re-provisioning to switch over.
//!
//! Every constructor comes in two forms: a `from_lookup` taking a closure
//! that reads one variable, and a `from_process_env` that supplies
//! `std::env::var`. The closure is not ceremony — Rust runs a crate's tests
//! on parallel threads in one process, so a test that set real environment
//! variables would race every other test in the binary.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::claude::Env;

/// Why configuration is unusable. The messages match the ones the Go and
/// Node implementations printed, so existing runbooks still apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("RECALL_URL must be set (e.g. https://recall.example.com)")]
    MissingUrl,
    #[error("RECALL_TOKEN must be set")]
    MissingToken,
    /// Mirrors the Node server's refusal to start without auth — a server
    /// reachable from the internet with no token is not a degraded mode
    /// worth supporting.
    #[error("RECALL_TOKEN is not set; refusing to start with no auth")]
    MissingServerToken,
}

/// What `recall push`, `pull`, `status` and `init` need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Client {
    pub url: String,
    pub token: String,
    pub source_env: String,
    pub claude: Env,
}

impl Client {
    /// Reads client configuration. It does not error on missing values —
    /// callers that need them say so via [`Client::require`], because
    /// `recall status` and `recall init` are specifically useful when
    /// configuration is incomplete and should report that rather than refuse
    /// to run.
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        Client {
            url: var(&lookup, "RECALL_URL").unwrap_or_default(),
            token: var(&lookup, "RECALL_TOKEN").unwrap_or_default(),
            source_env: resolve_source_env(var(&lookup, "RECALL_SOURCE_ENV"), hostname),
            claude: Env::from_lookup(&lookup),
        }
    }

    pub fn from_process_env() -> Self {
        Self::from_lookup(env_var)
    }

    /// Reports what's missing for an operation that actually talks to the
    /// server.
    pub fn require(&self) -> Result<(), ConfigError> {
        if self.url.is_empty() {
            return Err(ConfigError::MissingUrl);
        }
        if self.token.is_empty() {
            return Err(ConfigError::MissingToken);
        }
        Ok(())
    }
}

/// What `recall serve` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub addr: String,
    pub token: String,
    pub db_path: String,
    pub git_commit: String,

    pub backup_dir: Option<String>,
    pub backup_interval: Duration,
    pub backup_keep: i64,

    pub rate_limit_window: Duration,
    pub rate_limit_max: i64,

    pub merge_enabled: bool,
    pub merge_timeout: Duration,
    pub claude_bin: String,
    pub claude_status_interval: Duration,
}

impl Server {
    /// Reads server configuration, applying the same defaults the Node
    /// implementation used.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        // Checked before anything else: there is no useful partially
        // configured server to hand back.
        let token = var(&lookup, "RECALL_TOKEN").ok_or(ConfigError::MissingServerToken)?;

        Ok(Server {
            addr: format!(":{}", var_or(&lookup, "RECALL_PORT", "8787")),
            token,
            db_path: var_or(&lookup, "RECALL_DB_PATH", "data/recall.db"),
            git_commit: var_or(&lookup, "RECALL_GIT_COMMIT", "unknown"),

            backup_dir: var(&lookup, "RECALL_BACKUP_DIR"),
            backup_interval: hours(&lookup, "RECALL_BACKUP_INTERVAL_HOURS", 24),
            backup_keep: int(&lookup, "RECALL_BACKUP_KEEP", 7),

            rate_limit_window: millis(&lookup, "RECALL_RATE_LIMIT_WINDOW_MS", 60_000),
            rate_limit_max: int(&lookup, "RECALL_RATE_LIMIT_MAX", 60),

            // Only the literal "false" disables it; anything else, including
            // unset, leaves merge on. Matches the Node and Go versions, and
            // means a typo fails safe rather than silently degrading the
            // server to last-write-wins.
            merge_enabled: lookup("RECALL_MERGE_ENABLED").as_deref() != Some("false"),
            merge_timeout: millis(&lookup, "RECALL_MERGE_TIMEOUT_MS", 45_000),
            claude_bin: var_or(&lookup, "RECALL_CLAUDE_BIN", "claude"),
            claude_status_interval: millis(&lookup, "RECALL_CLAUDE_STATUS_INTERVAL_MS", 1_800_000),
        })
    }

    pub fn from_process_env() -> Result<Self, ConfigError> {
        Self::from_lookup(env_var)
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// A variable that is present but empty counts as unset, which is what the
/// shell and Node versions did by construction and what Go's `os.Getenv`
/// reports either way.
fn var<F>(lookup: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).filter(|value| !value.is_empty())
}

fn var_or<F>(lookup: &F, key: &str, fallback: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    var(lookup, key).unwrap_or_else(|| fallback.to_string())
}

/// Unparseable values fall back rather than failing the load: an operator
/// typo in a tuning knob should not take the server down.
fn int<F>(lookup: &F, key: &str, fallback: i64) -> i64
where
    F: Fn(&str) -> Option<String>,
{
    var(lookup, key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

/// Non-positive values fall back to the default. `Duration` is unsigned, so
/// a negative setting has no representation to preserve — and the two
/// readings that *would* round-trip are both worse than the default: a zero
/// interval spins the timer, and a zero timeout fails every merge instantly.
/// (The Go version clamped only the two that spin, and let a negative
/// `RECALL_MERGE_TIMEOUT_MS` through as a deadline already in the past.)
fn millis<F>(lookup: &F, key: &str, fallback_ms: i64) -> Duration
where
    F: Fn(&str) -> Option<String>,
{
    let value = int(lookup, key, fallback_ms);
    Duration::from_millis(if value > 0 { value } else { fallback_ms } as u64)
}

fn hours<F>(lookup: &F, key: &str, fallback_hours: i64) -> Duration
where
    F: Fn(&str) -> Option<String>,
{
    let value = int(lookup, key, fallback_hours);
    let hours = if value > 0 { value } else { fallback_hours } as u64;
    Duration::from_secs(hours.saturating_mul(3600))
}

/// The label a synced file is stamped with, so `recall status` can say which
/// machine last touched it. The fallback is a closure for two reasons: the
/// hostname lookup below is not unit-testable, and on the common path — the
/// variable is set — it must not run at all, since it may fork a process.
fn resolve_source_env<F>(explicit: Option<String>, hostname: F) -> String
where
    F: FnOnce() -> Option<String>,
{
    explicit
        .or_else(hostname)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Best-effort hostname. `std` has no portable API for it and this is a
/// display label, not an identity — nothing keys off it — so it is not worth
/// a dependency. `hostname(1)` is what actually agrees with `gethostname` on
/// both Linux and macOS; the other two are for when it is missing (minimal
/// container images) or unspawnable.
fn hostname() -> Option<String> {
    let from_command = Command::new("hostname")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok());

    from_command
        .or_else(|| std::fs::read_to_string(Path::new("/etc/hostname")).ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn client_reads_its_variables_and_the_claude_ones() {
        let client = Client::from_lookup(env(&[
            ("RECALL_URL", "https://recall.example.com"),
            ("RECALL_TOKEN", "s3cret"),
            ("RECALL_SOURCE_ENV", "laptop"),
            ("CLAUDE_CONFIG_DIR", "/cfg"),
            ("HOME", "/home/eko"),
        ]));

        assert_eq!(client.url, "https://recall.example.com");
        assert_eq!(client.token, "s3cret");
        assert_eq!(client.source_env, "laptop");
        assert_eq!(client.claude.config_dir.as_deref(), Some("/cfg"));
        assert_eq!(client.claude.memory_root(), Path::new("/cfg"));
    }

    /// `status` and `init` must still run with nothing configured — that is
    /// exactly when they are useful.
    #[test]
    fn client_load_never_fails_but_require_reports_what_is_missing() {
        let empty = Client::from_lookup(env(&[]));
        assert_eq!(empty.require(), Err(ConfigError::MissingUrl));

        let no_token = Client::from_lookup(env(&[("RECALL_URL", "https://recall.example.com")]));
        assert_eq!(no_token.require(), Err(ConfigError::MissingToken));

        let complete = Client::from_lookup(env(&[
            ("RECALL_URL", "https://recall.example.com"),
            ("RECALL_TOKEN", "s3cret"),
        ]));
        assert_eq!(complete.require(), Ok(()));
    }

    #[test]
    fn source_env_prefers_the_explicit_setting_then_the_hostname() {
        for (explicit, hostname, want, why) in [
            (Some("laptop"), Some("mbp.local"), "laptop", "explicit wins"),
            (None, Some("mbp.local"), "mbp.local", "hostname is next"),
            (None, None, "unknown", "and something is better than empty"),
        ] {
            let got = resolve_source_env(explicit.map(str::to_string), || {
                hostname.map(str::to_string)
            });
            assert_eq!(got, want, "{why}");
        }
    }

    /// Not just an ordering detail: looking up the hostname may fork a
    /// process, and `recall push` runs on every memory write.
    #[test]
    fn source_env_does_not_look_up_the_hostname_when_it_is_set() {
        let got = resolve_source_env(Some("laptop".to_string()), || {
            panic!("the hostname must not be looked up when RECALL_SOURCE_ENV is set")
        });
        assert_eq!(got, "laptop");
    }

    /// A server with no token would be an open write endpoint on the public
    /// internet.
    #[test]
    fn server_refuses_to_load_without_a_token() {
        assert_eq!(
            Server::from_lookup(env(&[("RECALL_PORT", "9000")])),
            Err(ConfigError::MissingServerToken)
        );
        assert_eq!(
            Server::from_lookup(env(&[("RECALL_TOKEN", "")])),
            Err(ConfigError::MissingServerToken),
            "set-but-empty is not set"
        );
    }

    #[test]
    fn server_defaults_match_the_node_implementation() {
        let server = Server::from_lookup(env(&[("RECALL_TOKEN", "s3cret")])).unwrap();

        assert_eq!(server.addr, ":8787");
        assert_eq!(server.db_path, "data/recall.db");
        assert_eq!(server.git_commit, "unknown");
        assert_eq!(server.backup_dir, None);
        assert_eq!(server.backup_interval, Duration::from_secs(24 * 3600));
        assert_eq!(server.backup_keep, 7);
        assert_eq!(server.rate_limit_window, Duration::from_secs(60));
        assert_eq!(server.rate_limit_max, 60);
        assert!(server.merge_enabled);
        assert_eq!(server.merge_timeout, Duration::from_millis(45_000));
        assert_eq!(server.claude_bin, "claude");
        assert_eq!(
            server.claude_status_interval,
            Duration::from_millis(1_800_000)
        );
    }

    #[test]
    fn server_reads_every_override() {
        let server = Server::from_lookup(env(&[
            ("RECALL_PORT", "9000"),
            ("RECALL_TOKEN", "s3cret"),
            ("RECALL_DB_PATH", "/var/lib/recall.db"),
            ("RECALL_GIT_COMMIT", "abc1234"),
            ("RECALL_BACKUP_DIR", "/var/backups/recall"),
            ("RECALL_BACKUP_INTERVAL_HOURS", "6"),
            ("RECALL_BACKUP_KEEP", "30"),
            ("RECALL_RATE_LIMIT_WINDOW_MS", "1000"),
            ("RECALL_RATE_LIMIT_MAX", "5"),
            ("RECALL_MERGE_ENABLED", "false"),
            ("RECALL_MERGE_TIMEOUT_MS", "10000"),
            ("RECALL_CLAUDE_BIN", "/usr/local/bin/claude"),
            ("RECALL_CLAUDE_STATUS_INTERVAL_MS", "60000"),
        ]))
        .unwrap();

        assert_eq!(server.addr, ":9000");
        assert_eq!(server.token, "s3cret");
        assert_eq!(server.db_path, "/var/lib/recall.db");
        assert_eq!(server.git_commit, "abc1234");
        assert_eq!(server.backup_dir.as_deref(), Some("/var/backups/recall"));
        assert_eq!(server.backup_interval, Duration::from_secs(6 * 3600));
        assert_eq!(server.backup_keep, 30);
        assert_eq!(server.rate_limit_window, Duration::from_millis(1000));
        assert_eq!(server.rate_limit_max, 5);
        assert!(!server.merge_enabled);
        assert_eq!(server.merge_timeout, Duration::from_millis(10_000));
        assert_eq!(server.claude_bin, "/usr/local/bin/claude");
        assert_eq!(server.claude_status_interval, Duration::from_millis(60_000));
    }

    /// Only the literal "false" turns merge off, so a typo leaves the server
    /// in its stronger mode instead of silently degrading it.
    #[test]
    fn merge_is_disabled_only_by_the_literal_false() {
        for (value, want, why) in [
            ("false", false, "the one string that disables it"),
            ("FALSE", true, "case-sensitively so"),
            ("0", true, "not shell truthiness"),
            ("no", true, "not english"),
            ("", true, "unset-ish is on"),
            ("true", true, "and the obvious one"),
        ] {
            let server = Server::from_lookup(env(&[
                ("RECALL_TOKEN", "s3cret"),
                ("RECALL_MERGE_ENABLED", value),
            ]))
            .unwrap();
            assert_eq!(server.merge_enabled, want, "{why}: {value:?}");
        }
    }

    /// A zero or negative interval would spin a timer, and a zero timeout
    /// would fail every merge instantly; a garbage value is an operator typo.
    /// All of them are better read as "not configured".
    #[test]
    fn non_positive_and_unparseable_tunings_fall_back_to_defaults() {
        for value in ["0", "-1", "banana", " 6"] {
            let server = Server::from_lookup(env(&[
                ("RECALL_TOKEN", "s3cret"),
                ("RECALL_BACKUP_INTERVAL_HOURS", value),
                ("RECALL_RATE_LIMIT_WINDOW_MS", value),
                ("RECALL_MERGE_TIMEOUT_MS", value),
                ("RECALL_CLAUDE_STATUS_INTERVAL_MS", value),
            ]))
            .unwrap();

            assert_eq!(
                server.backup_interval,
                Duration::from_secs(24 * 3600),
                "for {value:?}"
            );
            assert_eq!(
                server.rate_limit_window,
                Duration::from_secs(60),
                "for {value:?}"
            );
            assert_eq!(
                server.merge_timeout,
                Duration::from_millis(45_000),
                "for {value:?}"
            );
            assert_eq!(
                server.claude_status_interval,
                Duration::from_millis(1_800_000),
                "for {value:?}"
            );
        }
    }

    /// The messages are the operator-facing half of these errors: each one
    /// names the variable to set.
    #[test]
    fn error_messages_name_the_variable_an_operator_has_to_set() {
        assert_eq!(
            ConfigError::MissingUrl.to_string(),
            "RECALL_URL must be set (e.g. https://recall.example.com)"
        );
        assert_eq!(
            ConfigError::MissingToken.to_string(),
            "RECALL_TOKEN must be set"
        );
        assert_eq!(
            ConfigError::MissingServerToken.to_string(),
            "RECALL_TOKEN is not set; refusing to start with no auth"
        );
    }
}
