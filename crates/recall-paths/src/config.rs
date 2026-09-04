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

use crate::claude::Env;

/// Why configuration is unusable. The messages match the ones the Go and
/// Node implementations printed, so existing runbooks still apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No server to talk to.
    #[error("RECALL_URL must be set (e.g. https://recall.example.com)")]
    MissingUrl,
    /// A server, but no way to authenticate against it.
    #[error("RECALL_TOKEN must be set")]
    MissingToken,
}

/// What `recall push`, `pull`, `status` and `init` need.
///
/// Named for what it is — configuration — to keep it distinct from
/// `recall_hooks::client::Client`, which is the thing that actually makes
/// requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientConfig {
    /// `RECALL_URL`: where the server lives. Empty when unset.
    pub url: String,
    /// `RECALL_TOKEN`: the single bearer token. Empty when unset.
    pub token: String,
    /// `RECALL_SOURCE_ENV`: the label synced files are stamped with,
    /// falling back to the hostname and then to `"unknown"`.
    pub source_env: String,
    /// Where Claude Code keeps its memory on this machine.
    pub claude: Env,
}

impl ClientConfig {
    /// Reads client configuration. It does not error on missing values —
    /// callers that need them say so via [`ClientConfig::require`], because
    /// `recall status` and `recall init` are specifically useful when
    /// configuration is incomplete and should report that rather than refuse
    /// to run.
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        ClientConfig {
            url: var(&lookup, "RECALL_URL").unwrap_or_default(),
            token: var(&lookup, "RECALL_TOKEN").unwrap_or_default(),
            source_env: resolve_source_env(var(&lookup, "RECALL_SOURCE_ENV"), hostname),
            claude: Env::from_lookup(&lookup),
        }
    }

    /// Reads client configuration from the real process environment.
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
        let client = ClientConfig::from_lookup(env(&[
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
        let empty = ClientConfig::from_lookup(env(&[]));
        assert_eq!(empty.require(), Err(ConfigError::MissingUrl));

        let no_token =
            ClientConfig::from_lookup(env(&[("RECALL_URL", "https://recall.example.com")]));
        assert_eq!(no_token.require(), Err(ConfigError::MissingToken));

        let complete = ClientConfig::from_lookup(env(&[
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
    }
}
