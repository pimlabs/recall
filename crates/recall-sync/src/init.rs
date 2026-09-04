//! `recall init` — opt one project in.

use std::path::Path;

use recall_hooks::{exit, settings};
use recall_paths::ClientConfig;

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
                "not inside a git repository — run this from the project you want to sync"
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

    warn_about_unset_variables();
    Ok(exit::OK)
}

/// Committing the change is not a nicety — it is what makes a fresh clone or
/// a cloud session pick sync up without per-machine setup.
fn print_commit_hint(root: &Path) {
    println!(
        "
  Next: review and commit the change, so fresh clones and cloud sessions
  pick it up too — that's what makes this work without per-machine setup.

    git -C {0} diff .claude/settings.json
    git -C {0} add .claude/settings.json && git -C {0} commit -m \"Enable Recall memory sync\"",
        root.display()
    );
}

fn warn_about_unset_variables() {
    let cfg = ClientConfig::from_process_env();
    if !cfg.url.is_empty() && !cfg.token.is_empty() {
        return;
    }

    println!();
    if cfg.url.is_empty() {
        println!("  ! RECALL_URL is not set in this shell");
    }
    if cfg.token.is_empty() {
        println!("  ! RECALL_TOKEN is not set in this shell");
    }
    println!(
        "
  Add these to your shell profile (~/.zshrc, ~/.bashrc) before sync works:

    export RECALL_URL=\"https://your-recall-host\"
    export RECALL_TOKEN=\"<your token>\"

  See docs/token-setup.md for generating the token, and for the extra
  variables a claude.ai cloud environment needs."
    );
}
