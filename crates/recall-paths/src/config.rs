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
    /// `RECALL_PROJECT_KEY`: the key this project syncs under, declared
    /// rather than derived from the git remote, and normalised by
    /// [`project::explicit_key`](crate::project::explicit_key).
    ///
    /// [`None`] — the default — means the key is derived, which is what
    /// every project did before this variable existed. Declare one for a
    /// repo with no remote at all, for sub-projects of a monorepo that
    /// should (or should not) share one history, or for a fork that wants to
    /// keep reading the upstream's memory.
    ///
    /// Changing it on a project that has already synced orphans that
    /// project's memory: the server files every file under the key it was
    /// pushed with and moves nothing, so the old history stays where it is
    /// and the new key starts empty.
    pub project_key: Option<String>,
    /// `RECALL_GLOBAL_KEY`: the key for memories that follow the user into
    /// every project, normalised by
    /// [`scope::global_key`](crate::scope::global_key).
    ///
    /// [`None`] — the default — means global sync is off and Recall behaves
    /// exactly as it did before the scope existed. It has to be opt-in:
    /// turning it on makes files appear in every synced project's memory
    /// directory, which is not something to do to someone by surprise.
    pub global_key: Option<String>,
    /// Names of variables that were set to a value the normaliser refused,
    /// so the derived default stands instead.
    ///
    /// Refusing rather than failing is deliberate — a malformed
    /// `RECALL_PROJECT_KEY` should not stop an already-working project from
    /// syncing. But a setting that silently does nothing is the hardest kind
    /// of misconfiguration to notice, so the names are kept here for
    /// `recall status` to report. Empty is the ordinary case.
    pub rejected_vars: Vec<&'static str>,
    /// Where Claude Code keeps its memory on this machine.
    pub claude: Env,
}

/// Every variable [`ClientConfig`] reads that is Recall's own.
///
/// Claude Code's three are [`crate::claude::VARS`]; a caller enumerating
/// everything [`ClientConfig::from_lookup`] consults wants both lists, which
/// is what `reads_exactly_the_variables_it_publishes` asserts. They are kept
/// apart because they are owned by different projects — ours can be renamed
/// here, Claude Code's cannot be renamed at all.
pub const VARS: &[&str] = &[
    "RECALL_URL",
    "RECALL_TOKEN",
    "RECALL_SOURCE_ENV",
    "RECALL_PROJECT_KEY",
    "RECALL_GLOBAL_KEY",
    // Not Recall's, but read here as the last fallback for `source_env`, and
    // through the same lookup as the rest. Reading it from `std::env`
    // directly — which is what this did — left one variable resolving
    // against the shell while every other one resolved against the settings
    // files, which is the precise inversion the layering exists to remove.
    "HOSTNAME",
];

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
        let declared_project = var(&lookup, "RECALL_PROJECT_KEY");
        let project_key = declared_project
            .as_deref()
            .and_then(crate::project::explicit_key);
        let declared_global = var(&lookup, "RECALL_GLOBAL_KEY");
        let global_key = declared_global
            .as_deref()
            .and_then(crate::scope::global_key);

        let mut rejected_vars = Vec::new();
        for (declared, accepted, name) in [
            (
                declared_project.is_some(),
                project_key.is_some(),
                "RECALL_PROJECT_KEY",
            ),
            (
                declared_global.is_some(),
                global_key.is_some(),
                "RECALL_GLOBAL_KEY",
            ),
        ] {
            if declared && !accepted {
                rejected_vars.push(name);
            }
        }

        ClientConfig {
            url: var(&lookup, "RECALL_URL").unwrap_or_default(),
            token: var(&lookup, "RECALL_TOKEN").unwrap_or_default(),
            // Read eagerly, though it is the last fallback of three: it is a
            // map lookup, and threading it in is what lets `hostname` stay
            // free of `std::env` — and what lets the test below see it at
            // all, on a machine where `hostname(1)` answers first.
            source_env: {
                let from_env = var(&lookup, "HOSTNAME");
                resolve_source_env(var(&lookup, "RECALL_SOURCE_ENV"), || hostname(from_env))
            },
            project_key,
            global_key,
            rejected_vars,
            claude: Env::from_lookup(&lookup),
        }
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
fn hostname(from_env: Option<String>) -> Option<String> {
    let from_command = Command::new("hostname")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok());

    from_command
        .or_else(|| std::fs::read_to_string(Path::new("/etc/hostname")).ok())
        .or(from_env)
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

    /// Normalising here rather than at the call site is what lets a machine
    /// that exports `PimLabs/Recall` sync with one that exports
    /// `pimlabs/recall`: the value goes to the server as a `project_key`, and
    /// the two have to be the same string.
    #[test]
    fn a_declared_project_key_is_normalised_on_the_way_in() {
        let client = ClientConfig::from_lookup(env(&[("RECALL_PROJECT_KEY", "  PimLabs/Recall ")]));
        assert_eq!(client.project_key.as_deref(), Some("pimlabs/recall"));
    }

    /// Deriving the key stays the default, and a declaration that cannot be
    /// used has to leave it that way rather than half-apply.
    #[test]
    fn an_absent_or_unusable_project_key_reads_as_unset() {
        for (declared, why) in [
            (None, "unset"),
            (
                Some(""),
                "present but empty, which `var` already treats as unset",
            ),
            (Some("   "), "whitespace only"),
            (
                Some("global:eko"),
                "the global namespace is not a project's",
            ),
            (
                Some("acme/my project"),
                "a space no one retypes identically",
            ),
        ] {
            let pairs: Vec<(&str, &str)> = declared
                .map(|value| vec![("RECALL_PROJECT_KEY", value)])
                .unwrap_or_default();
            assert_eq!(
                ClientConfig::from_lookup(env(&pairs)).project_key,
                None,
                "{why}"
            );
        }
    }

    /// The distinction the field exists for: an unset variable and one set to
    /// a value that was thrown away both leave the key derived, and only this
    /// tells `recall status` which of the two happened.
    #[test]
    fn a_refused_value_is_recorded_but_an_unset_one_is_not() {
        let refused = ClientConfig::from_lookup(env(&[
            ("RECALL_PROJECT_KEY", "global:eko"),
            ("RECALL_GLOBAL_KEY", "   "),
        ]));
        assert_eq!(
            refused.rejected_vars,
            ["RECALL_PROJECT_KEY", "RECALL_GLOBAL_KEY"]
        );

        let clean = ClientConfig::from_lookup(env(&[("RECALL_PROJECT_KEY", "acme/app")]));
        assert!(clean.rejected_vars.is_empty());
        assert!(ClientConfig::from_lookup(env(&[])).rejected_vars.is_empty());
    }

    /// The published list is what `recall status` iterates to say which
    /// variables a settings file declares. If a new variable were added to
    /// the reads and not to the list, status would silently stop reporting
    /// it — the same class of quiet omission this whole type exists to
    /// surface.
    #[test]
    fn reads_exactly_the_variables_it_publishes() {
        use std::cell::RefCell;

        let seen = RefCell::new(Vec::new());
        let _ = ClientConfig::from_lookup(|key| {
            seen.borrow_mut().push(key.to_string());
            None
        });

        let mut seen = seen.into_inner();
        seen.sort();
        seen.dedup();

        let mut published: Vec<String> = VARS
            .iter()
            .chain(crate::claude::VARS.iter())
            .map(|v| (*v).to_string())
            .collect();
        published.sort();

        assert_eq!(
            seen, published,
            "config::VARS and claude::VARS together no longer describe what \
             ClientConfig reads"
        );
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
