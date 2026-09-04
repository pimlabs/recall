//! Server settings.
//!
//! Environment variable names are deliberately unchanged from the
//! shell/Node implementation this replaces, so no machine and no cloud
//! environment needs re-provisioning to switch over.

use std::env;
use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    /// Mirrors the Node server's refusal to start without auth — a server
    /// reachable from the internet with no token is not a degraded mode
    /// worth supporting.
    #[error("RECALL_TOKEN is not set; refusing to start with no auth")]
    MissingToken,
}

/// What `recall serve` needs.
#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub token: String,
    pub db_path: String,
    pub git_commit: String,

    pub backup_dir: String,
    pub backup_interval: Duration,
    pub backup_keep: usize,

    pub rate_limit_window: Duration,
    pub rate_limit_max: u32,

    pub merge_enabled: bool,
    pub merge_timeout: Duration,
    pub claude_bin: String,
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
            merge_enabled: true,
            merge_timeout: Duration::from_secs(45),
            claude_bin: "claude".to_string(),
            claude_status_interval: Duration::from_secs(30 * 60),
        }
    }
}

impl Config {
    /// Reads configuration from the environment, applying the same
    /// defaults the Node implementation used.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut cfg = Config {
            addr: format!("0.0.0.0:{}", env_or("RECALL_PORT", "8787")),
            token: env::var("RECALL_TOKEN").unwrap_or_default(),
            db_path: env_or("RECALL_DB_PATH", "data/recall.db"),
            git_commit: env_or("RECALL_GIT_COMMIT", "unknown"),
            backup_dir: env::var("RECALL_BACKUP_DIR").unwrap_or_default(),
            backup_interval: Duration::from_secs(
                env_u64("RECALL_BACKUP_INTERVAL_HOURS", 24).saturating_mul(3600),
            ),
            backup_keep: env_u64("RECALL_BACKUP_KEEP", 7) as usize,
            rate_limit_window: Duration::from_millis(env_u64("RECALL_RATE_LIMIT_WINDOW_MS", 60_000)),
            rate_limit_max: env_u64("RECALL_RATE_LIMIT_MAX", 60) as u32,
            // Opt-out, not opt-in: only the literal "false" disables it.
            merge_enabled: env::var("RECALL_MERGE_ENABLED").as_deref() != Ok("false"),
            merge_timeout: Duration::from_millis(env_u64("RECALL_MERGE_TIMEOUT_MS", 45_000)),
            claude_bin: env_or("RECALL_CLAUDE_BIN", "claude"),
            claude_status_interval: Duration::from_millis(env_u64(
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
        Ok(cfg)
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback.to_string(),
    }
}

/// An unparseable value falls back rather than failing the boot: the Node
/// and Go implementations both did, and a typo in one tunable should not
/// keep the server from starting.
fn env_u64(key: &str, fallback: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
