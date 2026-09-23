//! `recall init` — opt one project in.

use std::path::Path;

use recall_hooks::{exit, settings};

use crate::project;

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

    if settings::wire_file(&settings_path)? {
        println!("recall: wired hooks into {}", settings_path.display());
        print_commit_hint(&root);
    } else {
        println!(
            "recall: already wired, nothing to change ({})",
            settings_path.display()
        );
    }

    warn_about_unset_variables(&root);
    Ok(exit::OK)
}

/// Committing the change is not a nicety — it is what makes a fresh clone or
/// a cloud session pick sync up without per-machine setup.
fn print_commit_hint(root: &Path) {
    println!(
        "
  Next: commit it, so fresh clones and cloud sessions sync too.

    git -C {0} diff .claude/settings.json
    git -C {0} add .claude/settings.json && git -C {0} commit -m \"Enable Recall memory sync\"",
        root.display()
    );
}

/// Reports what is still missing, reading the environment the *hooks* will
/// run under rather than this shell's.
///
/// The difference is not academic: a project that declares these in the
/// `.claude/settings.json` this command just wired is fully configured, and
/// warning about them there would send someone editing a shell profile to
/// fix something that is not broken.
fn warn_about_unset_variables(root: &Path) {
    let cfg = project::resolve_at(root.to_path_buf()).config();
    if !cfg.url.is_empty() && !cfg.token.is_empty() {
        return;
    }

    println!();
    if cfg.url.is_empty() {
        println!("  ! No server yet: RECALL_URL is not set, and ~/.recall names none");
    }
    if cfg.token.is_empty() {
        println!("  ! No token yet: RECALL_TOKEN is not set, and ~/.recall holds none");
    }
    println!(
        "
  Connect this machine once. It asks for the token and checks it:

    recall connect https://your-recall-host

  A claude.ai cloud environment sets RECALL_URL and RECALL_TOKEN as its own
  variables instead. See docs/reference/token-setup.md."
    );
}
