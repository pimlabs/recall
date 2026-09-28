//! Making one suggested edit to a local memory file, and pushing it: the
//! path `recall eval apply` and `recall review apply` share.
//!
//! An edit is a [`SuggestedEdit`]: lines of one file, replaced, only if the
//! file is still exactly the version the edit was made against. It is shown,
//! confirmed (or `--yes`), checked again after the answer, written, and
//! pushed through [`recall_hooks::push`], which sends the file's own base so
//! the server takes it as the next edit rather than merging it.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use recall_hooks::exit;
use recall_wire::SuggestedEdit;

/// Why an edit was not made, for the command to say in its own name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Refused, with what happened and what to do next (may be empty).
    Refused { what: String, then: String },
    /// The prompt itself was cancelled; nothing to add.
    Cancelled,
}

pub(crate) fn refused(what: impl Into<String>, then: impl Into<String>) -> Stop {
    Stop::Refused {
        what: what.into(),
        then: then.into(),
    }
}

/// What to tell the owner to run when an edit no longer fits: the command
/// that makes a new report, and the one that tries the edit again.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hints {
    /// Makes a new report, such as `recall eval run`.
    pub rerun: &'static str,
    /// Tries again, such as `recall eval apply`.
    pub again: &'static str,
}

/// `line 3` or `lines 3-5`.
pub(crate) fn lines(f: &[u32; 2]) -> String {
    if f[0] == f[1] {
        format!("line {}", f[0])
    } else {
        format!("lines {}-{}", f[0], f[1])
    }
}

/// `s` made safe to write to a terminal: a control character (other than
/// a tab or a newline) becomes a space, so text from a note, a server or
/// `claude` cannot move the cursor or rewrite what came before it, the edit
/// being confirmed included.
pub(crate) fn printable(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && c != '\t' && c != '\n' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// `text`, each line indented to sit under a label.
pub(crate) fn indented(text: &str) -> String {
    text.lines()
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where the file an edit names is on this machine: under this project's
/// memory directory, in the scope whose key the edit names.
pub(crate) fn local_path(
    ctx: &recall_hooks::Context,
    edit: &SuggestedEdit,
) -> Result<PathBuf, Stop> {
    let Some(scope) = ctx.scopes.iter().find(|s| s.key == edit.project_key) else {
        let here: Vec<&str> = ctx.scopes.iter().map(|s| s.key.as_str()).collect();
        return Err(refused(
            format!(
                "the edit is to {} in {}, which is not synced here (this directory syncs {}).",
                edit.file_path,
                edit.project_key,
                here.join(", ")
            ),
            "Run it from a checkout of that project, or on a machine that syncs that scope.",
        ));
    };
    if recall_wire::validate_file_path(&edit.file_path).is_err() {
        return Err(refused(
            format!("{} is not a path Recall syncs.", edit.file_path),
            "",
        ));
    }
    let mut path = ctx.memory_dir.clone();
    if let Some(prefix) = &scope.prefix {
        path.push(prefix);
    }
    for part in edit.file_path.split('/') {
        path.push(part);
    }
    Ok(path)
}

/// Asks `question` on the terminal, unless `yes`. Without a terminal to
/// ask on, refuses rather than guessing.
pub(crate) fn confirmed(question: &str, yes: bool) -> Result<bool, Stop> {
    if yes {
        return Ok(true);
    }
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Err(refused(
            "needs a terminal to ask first.",
            "In a script, pass --yes.",
        ));
    }
    cliclack::confirm(question)
        .initial_value(false)
        .interact()
        .map_err(|_| Stop::Cancelled)
}

/// What [`make_edit`] came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Made {
    /// The edit is in the file.
    Written,
    /// The owner said no; nothing was changed.
    Declined,
}

/// Makes `edit` to the file at `path`, once `confirm` agrees, given what the
/// edit is, as text to show.
///
/// The file is read, and the edit checked against the version the report
/// read, before asking. Asking takes as long as the owner does, and a
/// Claude Code session may edit and push the same file meanwhile; writing
/// the edit made from the first read would then replace that edit here
/// and, pushed from a base that is now the session's, everywhere. So the
/// file is read again after the answer, and the edit is written only if it
/// is still exactly what was read before, with nothing between that check
/// and the write but the write. `--yes` goes through the same check.
pub(crate) fn make_edit(
    path: &Path,
    edit: &SuggestedEdit,
    hints: Hints,
    confirm: impl FnOnce(&str) -> Result<bool, Stop>,
) -> Result<Made, Stop> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|e| {
            refused(
                format!("cannot read {}: {e}", path.display()),
                "Start a Claude Code session here first, so the file is on this machine.",
            )
        })
    };
    let local = read(path)?;
    let changed = edit.apply_to(&local).map_err(|why| {
        refused(
            format!("{why}, here or on another machine, so the edit may no longer fit."),
            format!("Pull, then make a new report: {}", hints.rerun),
        )
    })?;
    let shown = if edit.replacement.is_empty() {
        format!("Edit   remove {}", lines(&edit.lines))
    } else {
        format!(
            "Edit   {} become:\n{}",
            lines(&edit.lines),
            indented(&printable(&edit.replacement))
        )
    };
    if !confirm(&shown)? {
        return Ok(Made::Declined);
    }
    if read(path)? != local {
        return Err(refused(
            format!(
                "{} changed while you were asked, so the edit was not made: it would have \
                 replaced that change.",
                path.display()
            ),
            format!(
                "Run {} again; if the report no longer fits, make a new one.",
                hints.again
            ),
        ));
    }
    std::fs::write(path, changed)
        .map_err(|e| refused(format!("cannot write {}: {e}", path.display()), ""))?;
    Ok(Made::Written)
}

/// Says `stop` on stderr as `command` (`recall eval`, `recall review`),
/// and the exit code it comes to.
pub(crate) fn said(command: &str, stop: Stop) -> i32 {
    if let Stop::Refused { what, then } = stop {
        eprintln!("{command}: {what}");
        if !then.is_empty() {
            eprintln!("  {then}");
        }
    }
    exit::CONFIG
}

/// Makes `edit` (the finding or claim `id`, described by `heading`) to its
/// local file after asking, or at once with `yes`, and pushes it. Returns
/// the exit code, having said everything on the way.
pub(crate) async fn apply_and_push(
    command: &str,
    id: &str,
    heading: &str,
    edit: &SuggestedEdit,
    hints: Hints,
    yes: bool,
) -> i32 {
    let ctx = match crate::project::resolve().hook_context() {
        Ok(ctx) => ctx,
        Err(e) => return said(command, refused(format!("{e:#}"), "")),
    };
    let path = match local_path(&ctx, edit) {
        Ok(path) => path,
        Err(stop) => return said(command, stop),
    };
    let question = format!("Make this edit to {} and push it?", edit.file_path);
    let made = make_edit(&path, edit, hints, |shown| {
        eprintln!("{heading}");
        eprintln!("File   {}", path.display());
        eprintln!("{shown}");
        confirmed(&question, yes)
    });
    match made {
        Err(stop) => return said(command, stop),
        Ok(Made::Declined) => {
            eprintln!("Nothing was changed.");
            return exit::CONFIG;
        }
        Ok(Made::Written) => {}
    }
    match recall_hooks::push(&ctx, &path).await {
        Ok(_) => {
            println!("Applied {id} to {} and pushed it.", path.display());
            exit::OK
        }
        Err(e) => {
            eprintln!("{command}: the edit is made here, but pushing it failed: {e}");
            eprintln!("  The file is changed here, and goes with the next edit made to it.");
            exit::SERVER
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINTS: Hints = Hints {
        rerun: "recall eval run",
        again: "recall eval apply",
    };

    fn edit_for(content: &str) -> SuggestedEdit {
        SuggestedEdit {
            project_key: "acme/app".into(),
            file_path: "deploy.md".into(),
            base_sha256: recall_wire::content_sha256(content),
            lines: [2, 2],
            replacement: "- key: [removed]\n".into(),
        }
    }

    /// A session that edits the file while the owner is asked keeps its
    /// edit: the suggested one is not written over it.
    #[test]
    fn an_edit_made_while_asking_is_not_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.md");
        let content = "# Deploy\n- key: abc123\n";
        std::fs::write(&path, content).unwrap();
        let theirs = "# Deploy\n- key: abc123\n- a line a session just added\n";
        let made = make_edit(&path, &edit_for(content), HINTS, |_| {
            std::fs::write(&path, theirs).unwrap();
            Ok(true)
        });
        assert!(matches!(made, Err(Stop::Refused { .. })), "{made:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
    }

    #[test]
    fn an_unchanged_file_gets_the_edit_and_a_no_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.md");
        let content = "# Deploy\n- key: abc123\n";
        std::fs::write(&path, content).unwrap();
        assert_eq!(
            make_edit(&path, &edit_for(content), HINTS, |_| Ok(false)).unwrap(),
            Made::Declined
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert_eq!(
            make_edit(&path, &edit_for(content), HINTS, |_| Ok(true)).unwrap(),
            Made::Written
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Deploy\n- key: [removed]\n"
        );
    }

    /// An edit made against another version of the file is refused before
    /// anything is asked, and names the command that makes a new report.
    #[test]
    fn an_edit_against_an_older_version_is_refused_before_asking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.md");
        std::fs::write(&path, "# Deploy\n- key: changed since\n").unwrap();
        let made = make_edit(&path, &edit_for("# Deploy\n- key: abc123\n"), HINTS, |_| {
            panic!("asked about an edit that no longer fits")
        });
        match made {
            Err(Stop::Refused { then, .. }) => assert!(then.contains("recall eval run"), "{then}"),
            other => panic!("{other:?}"),
        }
    }
}
