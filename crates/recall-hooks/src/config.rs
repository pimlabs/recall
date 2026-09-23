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
use crate::home;

/// Where a value came from, for `recall status` and `recall doctor` to say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Nowhere.
    #[default]
    Unset,
    /// The environment a hook sees: the shell, or a settings file's `env`
    /// block over it. [`declared_env`](crate::declared_env) says which.
    Environment,
    /// `~/.recall/credentials.toml`, written by `recall connect` — tokens.
    CredentialsFile,
    /// `~/.recall/config.toml` — the server and this machine's name.
    ConfigFile,
}

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
    /// Where [`ClientConfig::url`] came from.
    pub url_source: Source,
    /// Where [`ClientConfig::token`] came from.
    pub token_source: Source,
    /// `~/.recall/credentials.toml`, when there is a `~/.recall` to look in —
    /// whether or not the file exists.
    pub credentials_file: Option<std::path::PathBuf>,
    /// `~/.recall/config.toml`, likewise.
    pub config_file: Option<std::path::PathBuf>,
    /// Why a file in `~/.recall` could not be used, when one exists and
    /// could not. Not an error here, for the reason nothing in this type is:
    /// a broken file must not stop `recall status` from saying so.
    pub credentials_error: Option<String>,
    /// Things in `config.toml` that were read and did nothing: keys this
    /// version does not know, and a machine name it cannot use. Empty is
    /// the ordinary case.
    pub config_problems: Vec<String>,
    /// What `config.toml` says this machine is called, whether or not the
    /// environment overrides it — so `recall doctor` can say when it does.
    pub saved_machine_name: Option<String>,
    /// What `config.toml` names as the server, likewise.
    pub saved_server: Option<String>,
    /// The label synced files are stamped with: `RECALL_SOURCE_ENV`, else
    /// the machine name in `config.toml`, else the hostname, else
    /// `"unknown"`.
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
    /// `RECALL_MACHINE_KEY`: the key for memories that describe *this
    /// machine*, normalised by
    /// [`scope::machine_key`](crate::scope::machine_key).
    ///
    /// [`None`] — the default — means there is no machine scope here, and
    /// anything under `machine/` is left alone rather than swept into the
    /// project. Opt-in for a different reason than the global scope: the
    /// content is only true of one machine, so a machine that has not said
    /// which one it is must not receive another's facts. An ephemeral cloud
    /// session is a new machine every time and should leave this unset.
    pub machine_key: Option<String>,
    /// Where [`ClientConfig::machine_key`] came from: `RECALL_MACHINE_KEY`,
    /// or the machine name in `config.toml`.
    pub machine_source: Source,
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
    "RECALL_MACHINE_KEY",
    // Where `recall connect` keeps credentials. Read only when the
    // environment leaves the URL or the token unset.
    home::HOME_VAR,
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
        let declared_machine = var(&lookup, "RECALL_MACHINE_KEY");
        let mut machine_key = declared_machine
            .as_deref()
            .and_then(crate::scope::machine_key);
        let mut machine_source = source_of(&machine_key);

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
            (
                declared_machine.is_some(),
                machine_key.is_some(),
                "RECALL_MACHINE_KEY",
            ),
        ] {
            if declared && !accepted {
                rejected_vars.push(name);
            }
        }

        let mut url = var(&lookup, "RECALL_URL");
        let mut token = var(&lookup, "RECALL_TOKEN");
        let mut url_source = source_of(&url);
        let mut token_source = source_of(&token);

        // `~/.recall` sits below every environment layer: each value it
        // holds is used only where the environment left that value unset.
        let saved = Saved::load(&lookup);

        if url.is_none() {
            url = saved.config.server.clone();
            url_source = source_or(&url, Source::ConfigFile);
        }
        // After the URL, and it has to be: tokens are saved per server, so
        // which token is right depends on which server was chosen — by the
        // environment or by the line above.
        if token.is_none() {
            token = url
                .as_deref()
                .and_then(|u| saved.credentials.token_for(u))
                .map(str::to_string);
            token_source = source_or(&token, Source::CredentialsFile);
        }

        let mut config_problems: Vec<String> = saved
            .config
            .unknown_keys()
            .into_iter()
            .map(|k| format!("`{k}` is not a setting Recall knows, so it does nothing"))
            .collect();
        let saved_machine_name = saved.config.machine.name.clone();
        let usable_name = saved_machine_name.as_deref().and_then(home::machine_name);
        if saved_machine_name.is_some() && usable_name.is_none() {
            config_problems.push(format!(
                "machine.name = {:?} is not a usable name (letters, digits, `.`, `-`, `_`)",
                saved_machine_name.as_deref().unwrap_or_default()
            ));
        }
        if machine_key.is_none() {
            machine_key = usable_name.as_deref().and_then(crate::scope::machine_key);
            machine_source = source_or(&machine_key, Source::ConfigFile);
        }

        ClientConfig {
            url: url.unwrap_or_default(),
            token: token.unwrap_or_default(),
            url_source,
            token_source,
            credentials_file: saved.home.as_ref().map(home::Home::credentials_path),
            config_file: saved.home.as_ref().map(home::Home::config_path),
            credentials_error: saved.error,
            config_problems,
            saved_server: saved.config.server.clone(),
            // Read eagerly, though it is the last fallback: it is a map
            // lookup, and threading it in is what lets `hostname` stay free
            // of `std::env` — and what lets the test below see it at all, on
            // a machine where `hostname(1)` answers first.
            source_env: {
                let from_env = var(&lookup, "HOSTNAME");
                resolve_source_env(var(&lookup, "RECALL_SOURCE_ENV").or(usable_name), || {
                    hostname(from_env)
                })
            },
            saved_machine_name,
            project_key,
            global_key,
            machine_key,
            machine_source,
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

/// What `~/.recall` holds, read once per configuration.
struct Saved {
    home: Option<home::Home>,
    config: home::Config,
    credentials: home::Credentials,
    error: Option<String>,
}

impl Saved {
    /// Both files, empty where they do not exist. A file that exists and
    /// cannot be read contributes nothing and is recorded, never fatal.
    ///
    /// While `credentials.toml` does not exist, 0.3.0's `credentials.json`
    /// is read in its place. The CLI migrates it before this runs, so that
    /// is the path of a migration that failed — and a failed migration must
    /// not be the thing that stops a working machine syncing.
    fn load<F>(lookup: &F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut out = Saved {
            home: home::locate(lookup),
            config: home::Config::default(),
            credentials: home::Credentials::default(),
            error: None,
        };
        let Some(h) = out.home.clone() else {
            return out;
        };
        let mut errors = Vec::new();
        match h.load_config() {
            Ok(c) => out.config = c.unwrap_or_default(),
            Err(e) => errors.push(e.to_string()),
        }
        match h.load_credentials() {
            Ok(Some(c)) => out.credentials = c,
            Ok(None) => match h.read_legacy() {
                Ok(Some((server, creds))) => {
                    if out.config.server.is_none() {
                        out.config.server = server;
                    }
                    out.credentials = creds;
                }
                Ok(None) => {}
                Err(e) => errors.push(e.to_string()),
            },
            Err(e) => errors.push(e.to_string()),
        }
        if !errors.is_empty() {
            out.error = Some(errors.join("; "));
        }
        out
    }
}

fn source_of(value: &Option<String>) -> Source {
    source_or(value, Source::Environment)
}

fn source_or(value: &Option<String>, source: Source) -> Source {
    if value.is_some() {
        source
    } else {
        Source::Unset
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

    /// A `~/.recall` under a temporary directory: `servers` in
    /// `credentials.toml`, and `server` and `name` in `config.toml`.
    fn saved(
        servers: &[(&str, &str)],
        server: Option<&str>,
        name: Option<&str>,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let h = home::Home::at(dir.path());
        let mut c = home::Credentials::default();
        for (url, token) in servers {
            c.insert(url, token);
        }
        h.save_credentials(&c).unwrap();
        h.save_config(&home::Config {
            server: server.map(home::normalize_url),
            machine: home::Machine {
                name: name.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        dir
    }

    fn at(dir: &tempfile::TempDir) -> String {
        dir.path().to_string_lossy().to_string()
    }

    /// `recall connect` alone is a complete setup: the URL, the token and
    /// the machine all come from `~/.recall` when the environment has none
    /// of them.
    #[test]
    fn the_recall_home_supplies_what_the_environment_does_not() {
        let dir = saved(
            &[("https://a.example.com", "ta")],
            Some("https://a.example.com"),
            Some("jarvis"),
        );
        let cfg = ClientConfig::from_lookup(env(&[("RECALL_HOME", &at(&dir))]));

        assert_eq!(cfg.url, "https://a.example.com");
        assert_eq!(cfg.token, "ta");
        assert_eq!(cfg.url_source, Source::ConfigFile);
        assert_eq!(cfg.token_source, Source::CredentialsFile);
        assert_eq!(cfg.require(), Ok(()));
    }

    /// One name, both uses: the label on synced files and the machine
    /// scope's key. They were two variables that had to agree.
    #[test]
    fn the_machine_name_is_both_the_label_and_the_scope() {
        let dir = saved(&[], None, Some("jarvis"));
        let cfg = ClientConfig::from_lookup(env(&[("RECALL_HOME", &at(&dir))]));
        assert_eq!(cfg.source_env, "jarvis");
        assert_eq!(cfg.machine_key.as_deref(), Some("machine:jarvis"));
        assert_eq!(cfg.machine_source, Source::ConfigFile);
    }

    /// Each variable still wins over the file, independently of the others.
    #[test]
    fn the_environment_wins_over_the_recall_home() {
        let dir = saved(
            &[("https://a.example.com", "from-file")],
            Some("https://a.example.com"),
            Some("jarvis"),
        );
        let cfg = ClientConfig::from_lookup(env(&[
            ("RECALL_HOME", &at(&dir)),
            ("RECALL_TOKEN", "from-env"),
            ("RECALL_SOURCE_ENV", "laptop"),
            ("RECALL_MACHINE_KEY", "mbp"),
        ]));
        assert_eq!(
            cfg.url_source,
            Source::ConfigFile,
            "the URL was not overridden"
        );
        assert_eq!(cfg.token, "from-env");
        assert_eq!(cfg.token_source, Source::Environment);
        assert_eq!(cfg.source_env, "laptop");
        assert_eq!(cfg.machine_key.as_deref(), Some("machine:mbp"));
        assert_eq!(cfg.machine_source, Source::Environment);
        // And what the file says is still known, so doctor can name the
        // override.
        assert_eq!(cfg.saved_machine_name.as_deref(), Some("jarvis"));
    }

    /// The ordering coupling, pinned rather than left to a comment. Which
    /// token is right depends on which server: here the environment names
    /// `a` and the config names `b`, and handing `a` the token for `b` would
    /// be the exact mistake per-server keys exist to prevent.
    #[test]
    fn the_token_is_looked_up_for_the_url_in_effect_not_the_configs() {
        let dir = saved(
            &[
                ("https://a.example.com", "ta"),
                ("https://b.example.com", "tb"),
            ],
            Some("https://b.example.com"),
            None,
        );
        let cfg = ClientConfig::from_lookup(env(&[
            ("RECALL_HOME", &at(&dir)),
            ("RECALL_URL", "https://A.example.com/"),
        ]));
        assert_eq!(cfg.url_source, Source::Environment);
        assert_eq!(
            cfg.token, "ta",
            "normalised on read, and for the right server"
        );
        assert_eq!(cfg.token_source, Source::CredentialsFile);

        let cfg = ClientConfig::from_lookup(env(&[
            ("RECALL_HOME", &at(&dir)),
            ("RECALL_URL", "https://c.example.com"),
        ]));
        assert_eq!(cfg.token, "");
        assert_eq!(cfg.require(), Err(ConfigError::MissingToken));
    }

    /// A machine name the scope cannot use is reported, and leaves the
    /// machine scope off rather than inventing one.
    #[test]
    fn an_unusable_machine_name_is_a_problem_not_a_scope() {
        let dir = saved(&[], None, Some("my laptop"));
        let cfg = ClientConfig::from_lookup(env(&[("RECALL_HOME", &at(&dir))]));
        assert_eq!(cfg.machine_key, None);
        assert_eq!(cfg.config_problems.len(), 1, "{:?}", cfg.config_problems);
        assert_ne!(cfg.source_env, "my laptop");
    }

    /// Until the CLI has migrated it, 0.3.0's file still works — a failed
    /// migration must not be what stops a machine syncing.
    #[test]
    fn a_not_yet_migrated_0_3_0_file_still_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("credentials.json"),
            r#"{"version":1,"default":"https://a.example.com","servers":{"https://a.example.com":{"token":"ta"}}}"#,
        )
        .unwrap();
        let cfg = ClientConfig::from_lookup(env(&[("RECALL_HOME", &at(&dir))]));
        assert_eq!(cfg.url, "https://a.example.com");
        assert_eq!(cfg.token, "ta");
    }

    /// A broken file is reported, not fatal and not silently empty.
    #[test]
    fn an_unreadable_file_is_recorded_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("credentials.toml"), "servers = [").unwrap();
        let cfg = ClientConfig::from_lookup(env(&[("RECALL_HOME", &at(&dir))]));
        assert_eq!(cfg.token_source, Source::Unset);
        assert!(cfg.credentials_error.is_some());
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
