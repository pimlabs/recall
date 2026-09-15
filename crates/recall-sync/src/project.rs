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

use recall_hooks::{client::Client, declared_env, Context};
use recall_paths::{project, scope, ClientConfig};

/// The project root, resolved the way Claude Code resolves it: the git root,
/// falling back to the working directory.
pub fn root() -> PathBuf {
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        return PathBuf::from(root);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The `origin` remote, or an empty string outside a repository — which
/// makes [`project::key`] fall back to a path-derived key.
pub fn remote() -> String {
    git(&["remote", "get-url", "origin"]).unwrap_or_default()
}

/// The git root, or [`None`] if the working directory isn't in a
/// repository. Distinct from [`root`], which always answers.
pub fn git_root() -> Option<PathBuf> {
    git(&["rev-parse", "--show-toplevel"]).map(PathBuf::from)
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
    /// Configuration read through [`Resolved::env`], not through
    /// `std::env` — which is the whole point of this type.
    pub cfg: ClientConfig,
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
    let cfg = ClientConfig::from_lookup(env.lookup());
    Resolved { root, env, cfg }
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
        self.cfg.claude.memory_dir(&self.root.to_string_lossy())
    }

    /// The key this project syncs under: the declared one if there is a
    /// usable one, otherwise derived from `remote`.
    pub fn project_key(&self, remote: &str) -> String {
        project::key_with_override(
            self.cfg.project_key.as_deref(),
            remote,
            &self.root.to_string_lossy(),
        )
    }

    /// Everything the hook commands need.
    ///
    /// Fails when the server is not configured, which is the one thing a
    /// hook cannot work around.
    pub fn hook_context(&self) -> anyhow::Result<Context> {
        self.cfg.require()?;

        let root_str = self.root.to_string_lossy().to_string();
        Ok(Context {
            memory_dir: self.cfg.claude.memory_dir(&root_str),
            state_file: self.cfg.claude.state_file(&root_str),
            scopes: scope::scopes(self.project_key(&remote()), self.cfg.global_key.clone()),
            source_env: self.cfg.source_env.clone(),
            client: Client::new(&self.cfg.url, &self.cfg.token)?,
        })
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
