//! The worker's settings: the environment, and nothing else, as for the
//! server.
//!
//! The server it works for is `RECALL_WORKER_SERVER`, a variable of its
//! own. It never reads `RECALL_URL`: that is the `recall` client's setting,
//! and on any machine with Recall set up it names the public server, so a
//! worker that fell back to it would enrol with whatever server the shell
//! it was started from happened to point at.
//!
//! The three merge variables keep the names `recall-server` reads, so a
//! deployment moving its merge to a worker copies them across unchanged.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use recall_wire::jobs::{MAX_LEASE_SECONDS, MAX_WAIT_SECONDS, MIN_LEASE_SECONDS};

/// How long before its lease ends a merge must have finished, so its result
/// still reaches the server in time.
pub const LEASE_MARGIN_SECONDS: u64 = 15;

/// The longest `RECALL_MERGE_TIMEOUT_MS` may be: the longest lease the
/// server grants, less [`LEASE_MARGIN_SECONDS`]. A merge allowed to run any
/// longer would outlast its lease, and the job would be handed out again
/// while it was still being merged.
pub const MAX_MERGE_TIMEOUT: Duration =
    Duration::from_secs(MAX_LEASE_SECONDS - LEASE_MARGIN_SECONDS);

/// Why the worker cannot start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// There is no server to work for.
    #[error(
        "RECALL_WORKER_SERVER is not set; it names the server this worker merges for, \
         such as http://recall-server:8787"
    )]
    MissingServer,
    /// Only the client's `RECALL_URL` is set, which the worker never reads.
    #[error(
        "RECALL_WORKER_SERVER is not set. RECALL_URL is, but that is the recall client's \
         setting and the worker never reads it: on a machine with Recall set up it names \
         the public server. Set RECALL_WORKER_SERVER to the server this worker merges for, \
         such as http://recall-server:8787"
    )]
    OnlyClientUrl,
    /// `RECALL_WORKER_SERVER` is not an `http` or `https` URL.
    #[error("RECALL_WORKER_SERVER must be an http:// or https:// URL, got {0}")]
    BadServer(String),
    /// There is nowhere to keep the worker's key.
    #[error(
        "RECALL_WORKER_DIR is not set; it names the directory that holds the worker's key, \
         such as /data (the image sets it)"
    )]
    MissingDir,
    /// `RECALL_MERGE_TIMEOUT_MS` would let a merge outlast its lease.
    #[error(
        "RECALL_MERGE_TIMEOUT_MS is {0}, and may be at most {max}: a merge must finish \
         {LEASE_MARGIN_SECONDS}s before the longest lease the server grants ({MAX_LEASE_SECONDS}s) \
         ends, or the job is handed out again while it is still being merged",
        max = MAX_MERGE_TIMEOUT.as_millis()
    )]
    TimeoutTooLong(u64),
}

/// What `recall-worker` needs.
///
/// | Field | Variable | Default |
/// |---|---|---|
/// | [`server`] | `RECALL_WORKER_SERVER` | *required*; `RECALL_URL` is never read |
/// | [`data_dir`] | `RECALL_WORKER_DIR` | *required*; the image sets `/data` |
/// | [`name`] | `RECALL_WORKER_NAME` | `worker` |
/// | [`claude_bin`] | `RECALL_CLAUDE_BIN` | `claude` |
/// | [`merge_timeout`] | `RECALL_MERGE_TIMEOUT_MS` | 45s, at most [`MAX_MERGE_TIMEOUT`] |
/// | [`claude_status_interval`] | `RECALL_CLAUDE_STATUS_INTERVAL_MS` | 30m |
/// | [`lease_seconds`] | `RECALL_WORKER_LEASE_SECONDS` | 120 |
/// | [`eval_stale_days`] | `RECALL_EVAL_STALE_DAYS` | 90 |
///
/// [`server`]: Config::server
/// [`data_dir`]: Config::data_dir
/// [`name`]: Config::name
/// [`claude_bin`]: Config::claude_bin
/// [`merge_timeout`]: Config::merge_timeout
/// [`claude_status_interval`]: Config::claude_status_interval
/// [`lease_seconds`]: Config::lease_seconds
/// [`eval_stale_days`]: Config::eval_stale_days
#[derive(Debug, Clone)]
pub struct Config {
    /// The server, such as `http://recall-server:8787` from inside the
    /// compose network, or the public address from another host. Recorded
    /// in the worker's identity on first start; the worker refuses to run
    /// against any other.
    pub server: String,
    /// Where the worker keeps its device key and id. Never the server's
    /// volume: whoever reads this file can act as the worker. No default,
    /// so a worker started by hand never writes a key somewhere nobody
    /// chose.
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
    /// How many days a file must go unchanged, naming a path or a command,
    /// before an evaluation reports it as `stale` for the owner to
    /// confirm.
    pub eval_stale_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: String::new(),
            data_dir: PathBuf::new(),
            name: "worker".to_string(),
            claude_bin: "claude".to_string(),
            merge_timeout: Duration::from_secs(45),
            claude_status_interval: Duration::from_secs(30 * 60),
            lease_seconds: 120,
            wait_seconds: 25,
            eval_stale_days: 90,
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

        let server = match (get("RECALL_WORKER_SERVER"), get("RECALL_URL")) {
            (Some(server), _) => server,
            (None, Some(_)) => return Err(ConfigError::OnlyClientUrl),
            (None, None) => return Err(ConfigError::MissingServer),
        };
        let server = server.trim().trim_end_matches('/').to_string();
        if !(server.starts_with("http://") || server.starts_with("https://")) {
            return Err(ConfigError::BadServer(server));
        }
        let data_dir = get("RECALL_WORKER_DIR")
            .map(|d| PathBuf::from(d.trim()))
            .ok_or(ConfigError::MissingDir)?;
        let merge_timeout = match num("RECALL_MERGE_TIMEOUT_MS").filter(|ms| *ms > 0) {
            Some(ms) if Duration::from_millis(ms) > MAX_MERGE_TIMEOUT => {
                return Err(ConfigError::TimeoutTooLong(ms))
            }
            Some(ms) => Duration::from_millis(ms),
            None => defaults.merge_timeout,
        };
        let lease_seconds = num("RECALL_WORKER_LEASE_SECONDS")
            .unwrap_or(defaults.lease_seconds)
            // A lease shorter than a merge can take would hand every slow
            // merge to the next claim while it is still running. The
            // timeout is capped above, so this never passes the clamp.
            .max(merge_timeout.as_millis().div_ceil(1000) as u64 + LEASE_MARGIN_SECONDS)
            .clamp(MIN_LEASE_SECONDS, MAX_LEASE_SECONDS);
        Ok(Config {
            server,
            data_dir,
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
            eval_stale_days: num("RECALL_EVAL_STALE_DAYS")
                .filter(|d| *d > 0)
                .unwrap_or(defaults.eval_stale_days),
        })
    }

    /// A warning when [`server`](Config::server) is reached over plain
    /// `http` across a network, and [`None`] when that is fine.
    ///
    /// Requests are signed either way, so nobody on the path can act as the
    /// worker. But a claim's answer carries both versions of a conflicting
    /// file, and over `http` anyone on the path reads them. That is fine on
    /// loopback, and on the compose file's own `backend` network, where the
    /// server is a single-label service name such as `recall-server`;
    /// anywhere else it wants `https`.
    pub fn plaintext_warning(&self) -> Option<String> {
        let rest = self.server.strip_prefix("http://")?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let authority = authority.rsplit('@').next().unwrap_or_default();
        let host = match authority.strip_prefix('[') {
            // [::1]:8787
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        let service = !host.is_empty() && !host.contains('.') && host.parse::<IpAddr>().is_err();
        if loopback || service {
            return None;
        }
        Some(format!(
            "RECALL_WORKER_SERVER is plain http:// to {host}. Requests are signed, but each job \
             carries both versions of a file, readable by anyone on the path; use https:// \
             unless {host} is on a network you trust"
        ))
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

    const DIR: (&str, &str) = ("RECALL_WORKER_DIR", "/data");

    #[test]
    fn needs_a_server_and_takes_the_servers_merge_variables() {
        assert_eq!(
            Config::from_lookup(env(&[DIR])).unwrap_err(),
            ConfigError::MissingServer
        );
        assert!(matches!(
            Config::from_lookup(env(&[("RECALL_WORKER_SERVER", "recall-server:8787"), DIR])),
            Err(ConfigError::BadServer(_))
        ));
        let cfg = Config::from_lookup(env(&[
            ("RECALL_WORKER_SERVER", "http://recall-server:8787/"),
            DIR,
            ("RECALL_CLAUDE_BIN", "/opt/claude"),
            ("RECALL_MERGE_TIMEOUT_MS", "60000"),
        ]))
        .unwrap();
        assert_eq!(cfg.server, "http://recall-server:8787");
        assert_eq!(cfg.claude_bin, "/opt/claude");
        assert_eq!(cfg.merge_timeout, Duration::from_secs(60));
        assert_eq!(cfg.data_dir, PathBuf::from("/data"));
        assert_eq!(cfg.name, "worker");
    }

    /// The client's variable is never the worker's server: a shell with
    /// Recall set up has it naming the public server.
    #[test]
    fn the_clients_recall_url_is_never_read() {
        assert_eq!(
            Config::from_lookup(env(&[("RECALL_URL", "https://recall.example.com"), DIR]))
                .unwrap_err(),
            ConfigError::OnlyClientUrl
        );
        let cfg = Config::from_lookup(env(&[
            ("RECALL_URL", "https://recall.example.com"),
            ("RECALL_WORKER_SERVER", "http://127.0.0.1:8787"),
            DIR,
        ]))
        .unwrap();
        assert_eq!(cfg.server, "http://127.0.0.1:8787");
    }

    /// No directory is assumed: a worker run by hand must be told where its
    /// key goes.
    #[test]
    fn the_data_directory_has_no_default() {
        assert_eq!(
            Config::from_lookup(env(&[("RECALL_WORKER_SERVER", "http://s")])).unwrap_err(),
            ConfigError::MissingDir
        );
        assert_eq!(Config::default().data_dir, PathBuf::new());
    }

    /// The lease always outlasts a merge, and stays inside what the server
    /// accepts.
    #[test]
    fn the_lease_outlasts_the_merge_timeout() {
        let cfg = Config::from_lookup(env(&[
            ("RECALL_WORKER_SERVER", "http://s"),
            DIR,
            ("RECALL_MERGE_TIMEOUT_MS", "300000"),
            ("RECALL_WORKER_LEASE_SECONDS", "60"),
        ]))
        .unwrap();
        assert_eq!(cfg.lease_seconds, 315);
        let cfg = Config::from_lookup(env(&[
            ("RECALL_WORKER_SERVER", "http://s"),
            DIR,
            ("RECALL_WORKER_LEASE_SECONDS", "100000"),
        ]))
        .unwrap();
        assert_eq!(cfg.lease_seconds, MAX_LEASE_SECONDS);
    }

    /// A merge timeout the longest lease cannot cover is refused, rather
    /// than left to run past its lease.
    #[test]
    fn a_merge_timeout_longer_than_any_lease_is_refused() {
        let with = |ms: &'static str| {
            Config::from_lookup(env(&[
                ("RECALL_WORKER_SERVER", "http://s"),
                DIR,
                ("RECALL_MERGE_TIMEOUT_MS", ms),
            ]))
        };
        let longest = with("585000").unwrap();
        assert_eq!(longest.merge_timeout, MAX_MERGE_TIMEOUT);
        assert_eq!(longest.lease_seconds, MAX_LEASE_SECONDS);
        assert!(
            longest.merge_timeout + Duration::from_secs(LEASE_MARGIN_SECONDS)
                <= Duration::from_secs(longest.lease_seconds)
        );
        assert_eq!(
            with("585001").unwrap_err(),
            ConfigError::TimeoutTooLong(585_001)
        );
    }

    #[test]
    fn plain_http_is_quiet_only_on_loopback_and_the_compose_network() {
        let warns = |server: &str| {
            Config {
                server: server.to_string(),
                ..Config::default()
            }
            .plaintext_warning()
            .is_some()
        };
        for quiet in [
            "http://recall-server:8787",
            "http://localhost:8787",
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
            "https://recall.example.com",
        ] {
            assert!(!warns(quiet), "{quiet}");
        }
        for loud in [
            "http://recall.example.com",
            "http://10.0.0.5:8787",
            "http://[2001:db8::1]:8787",
            "http://user@recall.example.com/prefix",
        ] {
            assert!(warns(loud), "{loud}");
        }
    }
}
