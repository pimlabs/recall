//! The worker's settings: the environment, and nothing else, as for the
//! server.
//!
//! The three merge variables keep the names `recall-server` reads, so a
//! deployment moving its merge to a worker copies them across unchanged.

use std::path::PathBuf;
use std::time::Duration;

use recall_wire::jobs::{MAX_LEASE_SECONDS, MAX_WAIT_SECONDS, MIN_LEASE_SECONDS};

/// Why the worker cannot start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// There is no server to work for.
    #[error("RECALL_URL is not set; it names the server, such as http://recall-server:8787")]
    MissingUrl,
    /// `RECALL_URL` is not an `http` or `https` URL.
    #[error("RECALL_URL must be an http:// or https:// URL, got {0}")]
    BadUrl(String),
}

/// What `recall-worker` needs.
///
/// | Field | Variable | Default |
/// |---|---|---|
/// | [`url`] | `RECALL_URL` | *required* |
/// | [`data_dir`] | `RECALL_WORKER_DIR` | `/data` |
/// | [`name`] | `RECALL_WORKER_NAME` | `worker` |
/// | [`claude_bin`] | `RECALL_CLAUDE_BIN` | `claude` |
/// | [`merge_timeout`] | `RECALL_MERGE_TIMEOUT_MS` | 45s |
/// | [`claude_status_interval`] | `RECALL_CLAUDE_STATUS_INTERVAL_MS` | 30m |
/// | [`lease_seconds`] | `RECALL_WORKER_LEASE_SECONDS` | 120 |
///
/// [`url`]: Config::url
/// [`data_dir`]: Config::data_dir
/// [`name`]: Config::name
/// [`claude_bin`]: Config::claude_bin
/// [`merge_timeout`]: Config::merge_timeout
/// [`claude_status_interval`]: Config::claude_status_interval
/// [`lease_seconds`]: Config::lease_seconds
#[derive(Debug, Clone)]
pub struct Config {
    /// The server, such as `http://recall-server:8787` from inside the
    /// compose network, or the public address from another host.
    pub url: String,
    /// Where the worker keeps its device key and id. Never the server's
    /// volume: whoever reads this file can act as the worker.
    pub data_dir: PathBuf,
    /// The name it enrols as, which the owner sees in the device list.
    pub name: String,
    /// The `claude` binary. Never the Anthropic API.
    pub claude_bin: String,
    /// How long one merge may take before it is abandoned and reported as
    /// an error, which the server retries later.
    pub merge_timeout: Duration,
    /// How often to re-check that the CLI is present and logged in.
    pub claude_status_interval: Duration,
    /// How long a claimed job is the worker's before the server hands it
    /// to someone else. Longer than the merge timeout, so a merge that
    /// runs its full course still reports in time.
    pub lease_seconds: u64,
    /// How long one claim waits for a job.
    pub wait_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: String::new(),
            data_dir: PathBuf::from("/data"),
            name: "worker".to_string(),
            claude_bin: "claude".to_string(),
            merge_timeout: Duration::from_secs(45),
            claude_status_interval: Duration::from_secs(30 * 60),
            lease_seconds: 120,
            wait_seconds: 25,
        }
    }
}

impl Config {
    /// Reads the real process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Reads configuration through `lookup`, so tests need not set
    /// process-wide variables. An unparseable number falls back to its
    /// default, as the server's do.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |key: &str| lookup(key).filter(|v| !v.trim().is_empty());
        let num = |key: &str| get(key).and_then(|v| v.trim().parse::<u64>().ok());
        let defaults = Config::default();

        let url = get("RECALL_URL").ok_or(ConfigError::MissingUrl)?;
        let url = url.trim().trim_end_matches('/').to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ConfigError::BadUrl(url));
        }
        let merge_timeout = num("RECALL_MERGE_TIMEOUT_MS")
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(defaults.merge_timeout);
        let lease_seconds = num("RECALL_WORKER_LEASE_SECONDS")
            .unwrap_or(defaults.lease_seconds)
            // A lease shorter than a merge can take would hand every slow
            // merge to the next claim while it is still running.
            .max(merge_timeout.as_secs() + 15)
            .clamp(MIN_LEASE_SECONDS, MAX_LEASE_SECONDS);
        Ok(Config {
            url,
            data_dir: get("RECALL_WORKER_DIR")
                .map(PathBuf::from)
                .unwrap_or(defaults.data_dir),
            name: get("RECALL_WORKER_NAME")
                .map(|n| n.trim().to_string())
                .unwrap_or(defaults.name),
            claude_bin: get("RECALL_CLAUDE_BIN").unwrap_or(defaults.claude_bin),
            merge_timeout,
            claude_status_interval: num("RECALL_CLAUDE_STATUS_INTERVAL_MS")
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis)
                .unwrap_or(defaults.claude_status_interval),
            lease_seconds,
            wait_seconds: defaults.wait_seconds.min(MAX_WAIT_SECONDS),
        })
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
    fn needs_a_server_and_takes_the_servers_merge_variables() {
        assert_eq!(
            Config::from_lookup(env(&[])).unwrap_err(),
            ConfigError::MissingUrl
        );
        assert!(matches!(
            Config::from_lookup(env(&[("RECALL_URL", "recall-server:8787")])),
            Err(ConfigError::BadUrl(_))
        ));
        let cfg = Config::from_lookup(env(&[
            ("RECALL_URL", "http://recall-server:8787/"),
            ("RECALL_CLAUDE_BIN", "/opt/claude"),
            ("RECALL_MERGE_TIMEOUT_MS", "60000"),
        ]))
        .unwrap();
        assert_eq!(cfg.url, "http://recall-server:8787");
        assert_eq!(cfg.claude_bin, "/opt/claude");
        assert_eq!(cfg.merge_timeout, Duration::from_secs(60));
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        assert_eq!(cfg.name, "worker");
    }

    /// The lease always outlasts a merge, and stays inside what the server
    /// accepts.
    #[test]
    fn the_lease_outlasts_the_merge_timeout() {
        let cfg = Config::from_lookup(env(&[
            ("RECALL_URL", "http://s"),
            ("RECALL_MERGE_TIMEOUT_MS", "300000"),
            ("RECALL_WORKER_LEASE_SECONDS", "60"),
        ]))
        .unwrap();
        assert_eq!(cfg.lease_seconds, 315);
        let cfg = Config::from_lookup(env(&[
            ("RECALL_URL", "http://s"),
            ("RECALL_WORKER_LEASE_SECONDS", "100000"),
        ]))
        .unwrap();
        assert_eq!(cfg.lease_seconds, MAX_LEASE_SECONDS);
    }
}
