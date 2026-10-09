//! `recall init` — opt one project in.

use std::path::Path;

use recall_hooks::{exit, settings};

use crate::project;
use crate::ui::{self, Tone};

/// Wires `.claude/settings.json`, then says what is still missing.
///
/// This is loud where the rest of the CLI is quiet: it edits a file the user
/// is expected to read and commit, so it refuses to guess at a location, and
/// it reports incomplete configuration rather than pretending sync is
/// working.
pub fn run(path: Option<&Path>) -> anyhow::Result<i32> {
    let root = match path {
        Some(p) => p.to_path_buf(),
        None => match project::git_root() {
            Some(root) => root,
            None => anyhow::bail!(
                "not inside a git repository. Run this from the project you want to sync."
            ),
        },
    };
    let settings_path = root.join(".claude").join("settings.json");
    let wired_now = settings::wire_file(&settings_path)?;

    ui::title("recall init", &shown(&root));
    anstream::println!();
    if wired_now {
        ui::step(
            Tone::Good,
            "Wired Recall's hooks into .claude/settings.json",
            None,
        );
    } else {
        ui::step(
            Tone::Good,
            "Hooks already wired in .claude/settings.json, nothing to change",
            None,
        );
    }

    let missing = missing_configuration(&root);
    for (what, _) in &missing {
        ui::step(Tone::Warn, what, None);
    }

    // Committing the change is not a nicety: it is what makes a fresh clone
    // or a cloud session pick sync up without per-machine setup.
    let mut next = Vec::new();
    if wired_now {
        next.push(format!(
            "Commit it, so fresh clones and cloud sessions sync too: `{} add \
             .claude/settings.json && {0} commit -m \"Enable Recall memory sync\"`",
            git_in(&root)
        ));
    }
    if let Some((_, connect)) = missing.first() {
        next.push(connect.clone());
    }
    ui::next_steps(&next);

    let name = project_name(&root);
    match (missing.is_empty(), wired_now) {
        (true, true) => ui::verdict(
            Tone::Good,
            &format!("{name} syncs from its next Claude Code session."),
        ),
        (true, false) => ui::verdict(Tone::Good, &format!("{name} syncs.")),
        (false, _) => ui::verdict(
            Tone::Warn,
            &format!("{name} is wired, and syncs once this machine is connected."),
        ),
    }
    Ok(exit::OK)
}

/// `git`, run in `root`: plain `git` when that is where the command was run,
/// else `git -C <root>`, with home as `~` and the path quoted when the shell
/// would split it.
pub(crate) fn git_in(root: &Path) -> String {
    match std::env::current_dir() {
        Ok(cwd) if cwd == root => "git".to_string(),
        _ => format!("git -C {}", shell_path(&shown(root))),
    }
}

/// A path as it is typed into a shell: left alone when it is plain, else
/// single-quoted, with a leading `~/` kept outside the quotes so the shell
/// still expands it.
fn shell_path(path: &str) -> String {
    let plain = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~+:@%,=".contains(c) || c == '\\')
    };
    if plain(path) {
        return path.to_string();
    }
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", quote(rest)),
        None => quote(path),
    }
}

/// `path` for display: home as `~`.
fn shown(path: &Path) -> String {
    ui::tilde(&path.display().to_string())
}

/// The project's directory name, which is what a person calls it.
fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| shown(root))
}

/// What is still missing for the hooks to sync, each with the next step
/// that supplies it — reading the environment the *hooks* will run under
/// rather than this shell's.
///
/// The difference is not academic: a project that declares these in the
/// `.claude/settings.json` this command just wired is fully configured, and
/// warning about them there would send someone editing a shell profile to
/// fix something that is not broken. A machine with a device key, or a
/// session with an enrolment key, needs no token either.
fn missing_configuration(root: &Path) -> Vec<(String, String)> {
    let cfg = project::resolve_at(root.to_path_buf()).config();
    let has_credential = !cfg.token.is_empty() || cfg.device.is_some() || cfg.authkey.is_some();
    let connect = if project::remote_session() {
        "Set RECALL_URL and RECALL_AUTHKEY on the cloud environment: recall connect saves \
         nothing in a remote session, which ends with it. See docs/reference/token-setup.md."
            .to_string()
    } else {
        "Connect this machine, once: it asks for the token and checks it. \
         `recall connect https://your-recall-host` A claude.ai cloud environment sets \
         RECALL_URL and RECALL_AUTHKEY as its own variables instead, see \
         docs/reference/token-setup.md."
            .to_string()
    };
    let mut missing = Vec::new();
    if cfg.url.is_empty() {
        missing.push((
            "No server yet: RECALL_URL is not set, and ~/.recall names none".to_string(),
            connect.clone(),
        ));
    }
    if !has_credential {
        missing.push((
            "No credential yet: RECALL_TOKEN is not set, and this machine has no device key"
                .to_string(),
            connect,
        ));
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_quoted_only_when_the_shell_would_split_it() {
        assert_eq!(shell_path("~/code/app"), "~/code/app");
        assert_eq!(shell_path("/srv/app-2.0"), "/srv/app-2.0");
        assert_eq!(shell_path("~/My Projects/app"), "~/'My Projects/app'");
        assert_eq!(shell_path("/tmp/it's"), r"'/tmp/it'\''s'");
    }
}
