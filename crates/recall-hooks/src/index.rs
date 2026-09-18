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
//!
//! There is exactly one exception, and it is [`refresh_forgetting`]: a
//! promotion empties a path this file may link, and a link Recall itself
//! made dead is Recall's to remove.

use std::fs;
use std::io;
use std::path::Path;

use crate::scope::{GLOBAL_DIR, MACHINE_DIR, RESERVED_DIRS};

use crate::atomic;
use crate::state;

/// Rewrites the `MEMORY.md` lines that point into a reserved directory, so
/// they name exactly the files that are there.
///
/// Every scope with a directory of its own needs this, not just the global
/// one. A synced file that nothing links is memory that exists and is never
/// read — `docs/history/memory-loading-findings.md` established by probing
/// the real CLI that Claude Code opens what `MEMORY.md` links and nothing
/// else. The machine scope shipped without it and was inert for exactly that
/// reason, which is why this is driven off [`RESERVED_DIRS`] rather than
/// naming a directory.
///
/// Idempotent: with nothing changed the file comes out byte-identical, so
/// this does not itself cause a push.
pub(crate) fn refresh(memory_dir: &Path) -> io::Result<()> {
    refresh_forgetting(memory_dir, None).map(|_| ())
}

/// As [`refresh`], and additionally drops any link to `vacated`.
///
/// The one place a project's own index line is removed rather than left
/// alone. A promotion moves a note out from under a link the user or Claude
/// wrote, and a link to a file that is no longer there is worse than no link:
/// the model spends a read on it and gets nothing. Recall is what made it
/// dead, so Recall removes it — and only that one, named exactly.
///
/// Returns whether a link to `vacated` was actually removed — deliberately
/// not "whether the file changed", which is also true whenever the global
/// links move and is the wrong question. Those are derived: every machine
/// regenerates them for itself after a pull, and pushing them would be
/// noise. A removed line is content, and content removed only here comes
/// straight back with the next pull, so the caller owes it a push.
pub(crate) fn refresh_forgetting(memory_dir: &Path, vacated: Option<&str>) -> io::Result<bool> {
    let mut sets = Vec::new();
    for dir in RESERVED_DIRS {
        sets.push((dir, list(&memory_dir.join(dir))?));
    }
    let path = memory_dir.join("MEMORY.md");
    let existing = read_or_empty(&path)?;

    let updated = rewrite(&existing, &sets, vacated);
    if updated != existing {
        atomic::write(&path, ".recall-memory-", ".md", updated.as_bytes())?;
    }
    Ok(vacated.is_some_and(|path| links_to(&existing, path)))
}

/// Whether `MEMORY.md` links anything in the global directory.
///
/// Re-exported as `recall_hooks::global_index_is_linked`: "the files are
/// here" and "Claude Code can reach them" are different questions, and
/// `recall status` has to answer the second one.
pub fn is_linked(memory_md: &[u8]) -> bool {
    links_into(memory_md, GLOBAL_DIR)
}

/// The same question about the machine directory.
///
/// A separate function rather than a parameter on the last one, because both
/// call sites want to name the scope they are asking about — but one body, so
/// the two answers are produced the same way.
pub fn machine_is_linked(memory_md: &[u8]) -> bool {
    links_into(memory_md, MACHINE_DIR)
}

fn links_into(memory_md: &[u8], dir: &str) -> bool {
    String::from_utf8_lossy(memory_md).contains(&format!("]({dir}/"))
}

/// One linkable file: its path inside the reserved directory, and the
/// front-matter description that becomes the gloss.
///
/// Named because clippy is right that the bare tuple had stopped being
/// readable once it gained a second level — and the gloss is the part the
/// model sees when deciding what to open, which is worth having a word for.
type Entry = (String, Option<String>);

/// Replaces every Recall-owned line with the current set, keeping everything
/// else exactly as it was — bar a line linking `vacated`, which is dropped.
fn rewrite(existing: &str, sets: &[(&str, Vec<Entry>)], vacated: Option<&str>) -> String {
    let markers: Vec<String> = sets.iter().map(|(dir, _)| format!("]({dir}/")).collect();
    let mut out = String::new();
    for line in existing.lines() {
        if markers.iter().any(|m| line.contains(m.as_str())) {
            continue;
        }
        if vacated.is_some_and(|path| links_to(line, path)) {
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

    // In RESERVED_DIRS order, so the file is stable rather than dependent on
    // which scopes happen to have files. The directory is visible in every
    // link, which is the only signal the model gets that "8 GB of RAM"
    // describes this machine and not the project — so the prefix is carried
    // rather than stripped for tidiness.
    for (dir, entries) in sets {
        for (path, description) in entries {
            let title = title_of(path);
            match description {
                Some(d) => out.push_str(&format!("- [{title}]({dir}/{path}) — {d}\n")),
                None => out.push_str(&format!("- [{title}]({dir}/{path})\n")),
            }
        }
    }
    out
}

/// Whether `text` carries a markdown link to `path`.
///
/// The two spellings such a link actually takes, rather than a markdown
/// parser: this is a one-line job, and a link written some third way survives
/// as a dead link — which is what it was before any of this existed.
fn links_to(text: &str, path: &str) -> bool {
    text.contains(&format!("]({path})")) || text.contains(&format!("](./{path})"))
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

    /// The machine scope shipped syncing files that Claude Code would never
    /// open, because this index only ever knew about `global/`. A file that
    /// nothing links is memory that exists and is never read — which is worse
    /// than the effort of syncing it suggests, because everything about it
    /// looks like it is working.
    #[test]
    fn a_machine_file_is_linked_too() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(
            &mem.join("global/editor.md"),
            "---\ndescription: tabs\n---\nx\n",
        );
        write(
            &mem.join("machine/ram.md"),
            "---\ndescription: 8 GB\n---\ny\n",
        );

        refresh(mem).unwrap();

        let memory_md = read(&mem.join("MEMORY.md"));
        assert!(
            memory_md.contains("](machine/ram.md)"),
            "a machine memory has to be reachable from MEMORY.md: {memory_md}"
        );
        assert!(memory_md.contains("](global/editor.md)"), "{memory_md}");
        // The prefix is the only signal the model gets that this fact is
        // about the machine rather than the project, so it stays in the link.
        assert!(memory_md.contains("— 8 GB"), "{memory_md}");
    }

    /// Each reserved directory owns only its own lines: refreshing after a
    /// machine file goes away must not take the global links with it, and the
    /// reverse.
    #[test]
    fn the_two_scopes_do_not_rewrite_each_others_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        write(&mem.join("global/editor.md"), "x\n");
        write(&mem.join("machine/ram.md"), "y\n");
        write(&mem.join("machine/jdk.md"), "z\n");
        refresh(mem).unwrap();

        fs::remove_file(mem.join("machine/jdk.md")).unwrap();
        refresh(mem).unwrap();

        let memory_md = read(&mem.join("MEMORY.md"));
        assert!(!memory_md.contains("machine/jdk.md"), "{memory_md}");
        assert!(memory_md.contains("machine/ram.md"), "{memory_md}");
        assert!(memory_md.contains("global/editor.md"), "{memory_md}");
    }

    /// `recall status` asks these two separately, and has to get two answers.
    #[test]
    fn linkedness_is_answered_per_scope() {
        let only_global = b"- [E](global/editor.md)\n";
        assert!(is_linked(only_global));
        assert!(!machine_is_linked(only_global));

        let only_machine = b"- [R](machine/ram.md)\n";
        assert!(!is_linked(only_machine));
        assert!(machine_is_linked(only_machine));

        assert!(!is_linked(b"- [N](note.md)\n"));
        assert!(!machine_is_linked(b"- [N](note.md)\n"));
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
