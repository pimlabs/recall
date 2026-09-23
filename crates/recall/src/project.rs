//! Answering "where am I, what is this project called, and what does the
//! environment say" — once, for every command that needs it.
//!
//! The "what does the environment say" half is not `std::env`. Claude Code
//! applies the `env` block of the settings files in scope to the hooks it
//! spawns, replacing what the shell exported, so the environment a hook runs
//! under and the one an interactive shell holds are different things.
//! Resolving through [`Resolved`] is what keeps `recall status` describing
//! the first rather than the second.

use std::path::PathBuf;
use std::process::Command;

use recall_hooks::{claude, declared_env, device, home, project, scope, ClientConfig, Context};

/// The project root, resolved the way Claude Code resolves it: the git root,
/// falling back to the working directory.
pub fn root() -> PathBuf {
    git_toplevel().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The `origin` remote, or an empty string outside a repository — which
/// makes [`project::key`] fall back to a path-derived key.
pub fn remote() -> String {
    git(&["remote", "get-url", "origin"]).unwrap_or_default()
}

/// The git root, or [`None`] if the working directory isn't in a
/// repository. Distinct from [`root`], which always answers.
pub fn git_root() -> Option<PathBuf> {
    git_toplevel()
}

/// `git rev-parse --show-toplevel`, with its separators made native.
///
/// Git prints `/`-separated paths unconditionally, on every platform
/// including Windows — a stable property of git's own output, not an
/// artifact of this checkout. Everything downstream of [`root`]/[`git_root`]
/// either joins onto it with [`Path::join`](std::path::Path::join), prints
/// it with `display()`, or compares it against a canonicalized path, and all
/// three expect `\` on Windows: left as `/`, a join like
/// `.claude/settings.json` (a single string containing its own separators)
/// does not split into components at all when the base is later
/// canonicalized to a verbatim (`\\?\`) path, and every other join produces
/// the mixed `C:/Users/...\.claude\...` that `recall status` used to print.
/// The project-slug rule this feeds is unaffected either way: `slug()` maps
/// `:`, `/` and `\` to one dash each, see `claude.rs`.
fn git_toplevel() -> Option<PathBuf> {
    git(&["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(native_separators(s)))
}

#[cfg(windows)]
fn native_separators(path: String) -> String {
    path.replace('/', "\\")
}

#[cfg(not(windows))]
fn native_separators(path: String) -> String {
    path
}

/// Whether this is a remote or cloud session, per `CLAUDE_CODE_REMOTE`.
///
/// Read from the process environment rather than through the settings
/// layers: it describes the harness the command is running under, and a
/// settings file claiming otherwise would be describing something it cannot
/// know. The same signal `.claude/hooks/session-start.sh` keys off.
pub fn remote_session() -> bool {
    matches!(
        std::env::var("CLAUDE_CODE_REMOTE").ok().as_deref(),
        Some("true") | Some("1")
    )
}

/// Where a command is, and what the environment says there.
///
/// One type rather than three lookups because the three used to disagree:
/// `recall push` read the Claude variables once for the memory directory and
/// again for its context, and `recall status` read the process environment
/// while the hooks it was reporting on ran under a different one.
pub struct Resolved {
    /// The project root.
    pub root: PathBuf,
    /// The settings files layered over the shell, as Claude Code layers
    /// them. Kept so `recall status` can say *where* a value came from.
    pub env: declared_env::Environment,
}

/// Resolves the current directory's project and environment.
///
/// Never fails: every command here has to be useful on a machine where
/// nothing is configured yet, which is exactly when someone runs them.
pub fn resolve() -> Resolved {
    resolve_at(root())
}

/// The same, for a project root the caller already knows.
///
/// `recall init` takes one on the command line, and resolving the current
/// directory instead would report on a different project than the one it
/// just wired.
pub fn resolve_at(root: PathBuf) -> Resolved {
    let env = declared_env::Environment::discover(&root);
    Resolved { root, env }
}

impl Resolved {
    /// The memory directory Claude Code uses for this project on this
    /// machine.
    ///
    /// `recall push` asks this before it asks whether a server is configured
    /// at all, so that a machine which has cloned a wired project without
    /// being set up yet — the exact case Recall exists for — does not report
    /// a missing token on every unrelated file the user touches.
    pub fn memory_dir(&self) -> PathBuf {
        claude::Env::from_lookup(self.env.lookup()).memory_dir(&self.root.to_string_lossy())
    }

    /// The root every project's memory lives under, which is the same for
    /// all of them. Needed to tell "not a memory file" apart from "memory
    /// for a project that is not this one".
    pub fn memory_root(&self) -> PathBuf {
        claude::Env::from_lookup(self.env.lookup()).memory_root()
    }

    /// Recall's configuration, resolved through [`Resolved::env`].
    ///
    /// Built on demand rather than alongside the rest, and that is
    /// load-bearing: resolving `source_env` falls back to the hostname,
    /// which may fork a `hostname(1)`. `recall push` asks for
    /// [`Resolved::memory_dir`] on every Edit and Write *before* it knows
    /// whether the file concerns it at all, and that path must stay free of
    /// work this size.
    pub fn config(&self) -> ClientConfig {
        // 0.3.0's `credentials.json` becomes the two TOML files, once. Here
        // because every command that reads configuration comes through this,
        // hooks included, so no machine is left on the old file for want of
        // running one particular command. Failing is not fatal: the loader
        // still reads the old file, and `recall doctor` names the problem.
        if let Some(h) = home::locate(self.env.lookup()) {
            let _ = h.migrate_legacy();
        }
        ClientConfig::from_lookup(self.env.lookup())
    }

    /// The key this project syncs under: the declared one if there is a
    /// usable one, otherwise derived from `remote`.
    ///
    /// Takes the configuration rather than building its own, so a caller
    /// that already has one does not pay for a second.
    pub fn project_key(&self, cfg: &ClientConfig, remote: &str) -> String {
        project::key_with_override(
            cfg.project_key.as_deref(),
            remote,
            &self.root.to_string_lossy(),
        )
    }

    /// Everything the hook commands need.
    ///
    /// Fails when the server is not configured, which is the one thing a
    /// hook cannot work around.
    pub fn hook_context(&self) -> anyhow::Result<Context> {
        self.hook_context_for(&self.config())
    }

    /// The same, from a configuration the caller already holds: one a hook
    /// has just enrolled a device into, say.
    pub fn hook_context_for(&self, cfg: &ClientConfig) -> anyhow::Result<Context> {
        cfg.require()?;

        let root_str = self.root.to_string_lossy().to_string();
        Ok(Context {
            memory_dir: cfg.claude.memory_dir(&root_str),
            state_file: cfg.claude.state_file(&root_str),
            scopes: scope::scopes(
                self.project_key(cfg, &remote()),
                cfg.global_key.clone(),
                cfg.machine_key.clone(),
            ),
            source_env: cfg.source_env.clone(),
            client: cfg.client()?,
        })
    }

    /// Enrols this machine with `RECALL_AUTHKEY` when it has no device
    /// key for the server in effect: a cloud session's first hook.
    ///
    /// Never an error, because it runs inside a hook. What happened is one
    /// line on stderr, prefixed with `hook`, and the configuration comes
    /// back either holding the new device or as it was, in which case the
    /// token, if there is one, is used as before.
    pub async fn enroll_if_needed(&self, mut cfg: ClientConfig, hook: &str) -> ClientConfig {
        let Some(authkey) = cfg.authkey.clone() else {
            return cfg;
        };
        if cfg.device.is_some() || cfg.url.is_empty() {
            return cfg;
        }
        let Some(h) = home::locate(self.env.lookup()) else {
            eprintln!("{hook}: RECALL_AUTHKEY is set, but there is no home directory to keep a device key in");
            return cfg;
        };
        match device::enrol_with_authkey(&h, &cfg.url, &authkey, &cfg.source_env).await {
            Ok(entry) => {
                eprintln!(
                    "{hook}: enrolled this session as device {} with RECALL_AUTHKEY",
                    entry.name
                );
                cfg.device = Some(entry);
            }
            Err(e) => {
                let fallback = if cfg.token.is_empty() {
                    ""
                } else {
                    ", using RECALL_TOKEN instead"
                };
                eprintln!(
                    "{hook}: could not enrol with RECALL_AUTHKEY ({}){fallback}",
                    enroll_failure(&e)
                );
            }
        }
        cfg
    }

    /// After the server refused this machine's device as unknown or
    /// revoked: enrols afresh when `RECALL_AUTHKEY` allows it, and
    /// otherwise says what to do. [`Some`] with the new configuration only
    /// when there is something worth retrying with.
    pub async fn reenroll(
        &self,
        cfg: &ClientConfig,
        hook: &str,
        why: &str,
    ) -> Option<ClientConfig> {
        if cfg.authkey.is_none() {
            eprintln!("{hook}: the server no longer accepts this machine's device key ({why})");
            eprintln!(
                "{hook}:   run recall connect to enrol this machine again, or set \
                 RECALL_AUTHKEY to have a cloud session do it by itself"
            );
            return None;
        }
        let h = home::locate(self.env.lookup())?;
        // The old key is dropped, not kept for a retry: a revoked device
        // stays revoked, and a swept one no longer exists.
        let _ = h.forget_device(&cfg.url);
        let mut fresh = cfg.clone();
        fresh.device = None;
        eprintln!(
            "{hook}: the server no longer knows this session's device ({why}), enrolling again"
        );
        let fresh = self.enroll_if_needed(fresh, hook).await;
        fresh.device.is_some().then_some(fresh)
    }
}

/// Why an enrolment with an authkey failed, in a line.
fn enroll_failure(e: &device::Error) -> String {
    match e {
        device::Error::Client(recall_hooks::client::Error::Status { code: 404, .. }) => {
            "this server does not enrol devices; it is older than 0.4.1".to_string()
        }
        device::Error::Client(c) => c.reason(),
        other => other.to_string(),
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}
