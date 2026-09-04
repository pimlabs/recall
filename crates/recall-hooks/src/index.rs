//! Making globally synced memories reachable.
//!
//! Writing a file into the memory directory is not enough. `MEMORY.md` is
//! the index Claude Code reads first, and a file it does not link is not
//! reliably loaded — so this module keeps a link in `MEMORY.md` for every
//! file in the global directory.
//!
//! # What was measured, and what it is worth
//!
//! Against CLI 2.1.260, planting files by hand and asking a fresh `claude -p`
//! session about them:
//!
//! - a file linked from `MEMORY.md` is read, at the root or in a
//!   subdirectory;
//! - a file that is linked from nothing came back `UNKNOWN`;
//! - **the same files and the same question do not always give the same
//!   answer.** One configuration returned `UNKNOWN` on four runs and the
//!   correct value on the fifth, with nothing changed between them.
//!
//! That last point is the one worth carrying. Retrieval here is a model
//! deciding which files to open from their one-line glosses, not a loader
//! walking a tree — so a single failed probe proves nothing, and two
//! confident conclusions drawn from single runs during this work (that a
//! directory named `global` was special, and that an extra hop through a
//! generated index never resolved) were both wrong and are retracted.
//!
//! # Why the links are direct anyway
//!
//! Not because indirection was measured broken — it was not — but because
//! there is nothing to gain from it. Linking each file straight from
//! `MEMORY.md` is one hop instead of two, needs no generated file, and needs
//! no rule about never pushing that file. Fewer moving parts for the same
//! result.
//!
//! The gloss matters more than the structure does: it is what the model sees
//! when choosing what to open. Each link carries the file's own front-matter
//! `description`, which is what Claude Code writes there for exactly this
//! purpose.
//!
//! # Owning lines rather than a block
//!
//! A line belongs to Recall if its link target starts with `global/`. That is
//! the whole rule — no marker comments, and nothing that breaks if the user
//! reorders or rewords the rest of the file.

use std::fs;
use std::io;
use std::path::Path;

use recall_paths::scope::GLOBAL_DIR;

use crate::atomic;
use crate::state;

/// Rewrites the `MEMORY.md` lines that point into the global directory, so
/// they name exactly the files that are there.
///
/// Idempotent: with nothing changed the file comes out byte-identical, so
/// this does not itself cause a push.
pub(crate) fn refresh(memory_dir: &Path) -> io::Result<()> {
    let entries = list(&memory_dir.join(GLOBAL_DIR))?;
    let path = memory_dir.join("MEMORY.md");
    let existing = read_or_empty(&path)?;

    let updated = rewrite(&existing, &entries);
    if updated != existing {
        atomic::write(&path, ".recall-memory-", ".md", updated.as_bytes())?;
    }
    Ok(())
}

/// Whether `MEMORY.md` links anything in the global directory.
///
/// Re-exported as `recall_hooks::global_index_is_linked`: "the files are
/// here" and "Claude Code can reach them" are different questions, and
/// `recall status` has to answer the second one.
pub fn is_linked(memory_md: &[u8]) -> bool {
    String::from_utf8_lossy(memory_md).contains(&format!("]({GLOBAL_DIR}/"))
}

/// Replaces every Recall-owned line with the current set, keeping everything
/// else exactly as it was.
fn rewrite(existing: &str, entries: &[(String, Option<String>)]) -> String {
    let marker = format!("]({GLOBAL_DIR}/");
    let mut out = String::new();
    for line in existing.lines() {
        if line.contains(&marker) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    // Trailing blank lines left behind by a removal, so the file does not
    // grow a gap every time a global file goes away.
    while out.ends_with("\n\n") {
        out.pop();
    }

    for (path, description) in entries {
        let title = title_of(path);
        match description {
            Some(d) => out.push_str(&format!("- [{title}]({GLOBAL_DIR}/{path}) — {d}\n")),
            None => out.push_str(&format!("- [{title}]({GLOBAL_DIR}/{path})\n")),
        }
    }
    out
}

/// Every file in the global directory, as (path relative to that directory,
/// description), sorted.
fn list(global_dir: &Path) -> io::Result<Vec<(String, Option<String>)>> {
    let mut out = Vec::new();
    for rel in state::list_memory_files(global_dir)? {
        let description = fs::read_to_string(state::join_relative(global_dir, &rel))
            .ok()
            .and_then(|body| description_of(&body));
        out.push((rel, description));
    }
    Ok(out)
}

/// Pulls `description:` out of a memory file's front matter.
///
/// Claude Code writes one; a file a human dropped in by hand may not, and
/// that is fine — the link still works, it just has no gloss.
fn description_of(body: &str) -> Option<String> {
    let mut lines = body.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return None;
        }
        if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// A readable title for a path: the file stem, underscores and dashes turned
/// back into spaces.
fn title_of(path: &str) -> String {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md");
    stem.replace(['_', '-'], " ")
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn every_global_file_is_linked_directly_from_memory_md() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(
            &mem.join("global/editor.md"),
            "---\nname: editor\ndescription: \"Prefers helix\"\n---\n\nbody\n",
        );
        write(&mem.join("global/prefs/shell.md"), "no front matter here\n");

        refresh(mem).unwrap();

        let memory_md = read(&mem.join("MEMORY.md"));
        assert!(
            memory_md.contains("- [editor](global/editor.md) — Prefers helix"),
            "{memory_md}"
        );
        assert!(
            memory_md.contains("- [shell](global/prefs/shell.md)\n"),
            "a file without a description still gets a link: {memory_md}"
        );
        assert!(
            !memory_md.contains("INDEX.md"),
            "links are direct, with no generated index in between: {memory_md}"
        );
    }

    #[test]
    fn the_users_own_entries_survive_and_links_are_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(
            &mem.join("MEMORY.md"),
            "- [Existing note](note.md) — kept\n- [Deep](topics/a/b.md) — also kept\n",
        );
        write(&mem.join("global/editor.md"), "x\n");

        for _ in 0..3 {
            refresh(mem).unwrap();
        }

        let memory_md = read(&mem.join("MEMORY.md"));
        assert_eq!(
            memory_md.matches("global/editor.md").count(),
            1,
            "the link was duplicated:\n{memory_md}"
        );
        assert!(
            memory_md.contains("- [Existing note](note.md) — kept"),
            "{memory_md}"
        );
        assert!(
            memory_md.contains("- [Deep](topics/a/b.md) — also kept"),
            "{memory_md}"
        );
    }

    /// A global file that goes away must lose its line, or `MEMORY.md` points
    /// Claude at a file that is not there.
    #[test]
    fn a_removed_global_file_loses_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(&mem.join("MEMORY.md"), "- [Mine](note.md) — kept\n");
        write(&mem.join("global/editor.md"), "x\n");
        write(&mem.join("global/shell.md"), "y\n");
        refresh(mem).unwrap();
        assert!(read(&mem.join("MEMORY.md")).contains("global/shell.md"));

        fs::remove_file(mem.join("global/shell.md")).unwrap();
        refresh(mem).unwrap();

        let memory_md = read(&mem.join("MEMORY.md"));
        assert!(!memory_md.contains("global/shell.md"), "{memory_md}");
        assert!(memory_md.contains("global/editor.md"), "{memory_md}");
        assert!(
            memory_md.contains("- [Mine](note.md) — kept"),
            "{memory_md}"
        );
    }

    #[test]
    fn an_empty_global_directory_leaves_memory_md_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(&mem.join("MEMORY.md"), "- [Mine](note.md) — kept\n");
        fs::create_dir_all(mem.join(GLOBAL_DIR)).unwrap();

        refresh(mem).unwrap();

        assert_eq!(read(&mem.join("MEMORY.md")), "- [Mine](note.md) — kept\n");
        assert!(!is_linked(read(&mem.join("MEMORY.md")).as_bytes()));
    }

    /// Idempotence matters for more than tidiness: a rewrite looks like an
    /// edit, and the push hook would ship `MEMORY.md` on every single run.
    #[test]
    fn a_second_run_changes_no_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(&mem.join("MEMORY.md"), "- [Mine](note.md) — kept\n");
        write(&mem.join("global/editor.md"), "---\ndescription: x\n---\n");

        refresh(mem).unwrap();
        let first = read(&mem.join("MEMORY.md"));
        refresh(mem).unwrap();
        assert_eq!(first, read(&mem.join("MEMORY.md")));
    }

    #[test]
    fn a_memory_md_without_a_trailing_newline_does_not_run_lines_together() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(
            &mem.join("MEMORY.md"),
            "- [Note](note.md) — no newline at end",
        );
        write(&mem.join("global/editor.md"), "x\n");

        refresh(mem).unwrap();

        let memory_md = read(&mem.join("MEMORY.md"));
        assert!(
            memory_md.contains("no newline at end\n- [editor](global/editor.md)"),
            "{memory_md:?}"
        );
    }

    #[test]
    fn removing_the_last_global_file_does_not_leave_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(&mem.join("MEMORY.md"), "- [Mine](note.md) — kept\n");
        write(&mem.join("global/editor.md"), "x\n");
        refresh(mem).unwrap();

        fs::remove_file(mem.join("global/editor.md")).unwrap();
        refresh(mem).unwrap();

        assert_eq!(read(&mem.join("MEMORY.md")), "- [Mine](note.md) — kept\n");
    }

    #[test]
    fn descriptions_are_read_only_from_real_front_matter() {
        assert_eq!(
            description_of("---\ndescription: \"Quoted\"\n---\nbody"),
            Some("Quoted".into())
        );
        assert_eq!(
            description_of("---\nname: x\ndescription: unquoted here\n---\n"),
            Some("unquoted here".into())
        );
        assert_eq!(description_of("description: not front matter\n"), None);
        assert_eq!(
            description_of("---\nname: x\n---\ndescription: later\n"),
            None
        );
        assert_eq!(description_of(""), None);
        assert_eq!(description_of("---\ndescription:   \n---\n"), None);
    }

    #[test]
    fn titles_are_readable() {
        assert_eq!(title_of("editor.md"), "editor");
        assert_eq!(title_of("prefs/user_shell.md"), "user shell");
        assert_eq!(title_of("a/b/my-notes.md"), "my notes");
    }
}
