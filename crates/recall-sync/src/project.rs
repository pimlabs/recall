//! Answering "where am I, and what is this project called" — once, for every
//! command that needs it.

use std::path::PathBuf;
use std::process::Command;

use recall_hooks::{client::Client, Context};
use recall_paths::{claude, project, scope, ClientConfig};

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

/// The memory directory Claude Code uses for the project at `root` on this
/// machine.
///
/// Separate from [`hook_context`] on purpose: `recall push` needs this
/// answer *before* it asks for a server URL or a token, so an unconfigured
/// machine doesn't error on every unrelated edit.
pub fn memory_dir(root: &std::path::Path) -> PathBuf {
    claude::Env::from_process_env().memory_dir(&root.to_string_lossy())
}

/// Everything the hook commands need, assembled from the environment.
///
/// Fails when the server is not configured, which is the one thing a hook
/// cannot work around.
pub fn hook_context() -> anyhow::Result<Context> {
    let cfg = ClientConfig::from_process_env();
    cfg.require()?;

    let root = root();
    let root_str = root.to_string_lossy().to_string();
    let claude = claude::Env::from_process_env();

    Ok(Context {
        memory_dir: claude.memory_dir(&root_str),
        state_file: claude.state_file(&root_str),
        scopes: scope::scopes(project::key(&remote(), &root_str), cfg.global_key.clone()),
        source_env: cfg.source_env.clone(),
        client: Client::new(&cfg.url, &cfg.token)?,
    })
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}
