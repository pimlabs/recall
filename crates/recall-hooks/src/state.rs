//! The baseline that lets a delete be noticed at all.
//!
//! Claude Code has no delete event, and a delete via the Bash tool wouldn't
//! match the `Edit|Write` hook matcher even if it did. So the push hook
//! reconciles instead: it compares what is on disk now against what was
//! there last time. This file is that "last time".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic;

/// The on-disk format: the memory-file paths, relative to the memory
/// directory and slash-separated, that were present at the last push or
/// pull.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The paths that were present, sorted, so two runs on the same
    /// directory produce byte-identical files.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Reads the baseline, returning `None` when there isn't one yet.
///
/// `None` versus `Some(State { files: [] })` is the whole point of this
/// signature: it decides whether an empty memory directory means "nothing
/// has ever synced here" or "everything was just deleted". Getting that
/// wrong on a first run would tombstone the project's entire history on the
/// server, so the distinction is carried in the type rather than in a
/// convention a caller could forget.
///
/// A corrupt baseline reads as absent rather than fatal: the cost is one
/// missed delete propagation, versus a hook that fails on every edit until
/// someone deletes the file by hand.
pub fn load(path: &Path) -> io::Result<Option<State>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Writes the baseline atomically, so two hooks racing on adjacent edits
/// can't leave a truncated file behind.
pub fn save(path: &Path, files: &[String]) -> io::Result<()> {
    let state = State {
        files: files.to_vec(),
    };
    let body = serde_json::to_vec(&state).map_err(io::Error::other)?;
    atomic::write(path, ".recall-state-", ".json", &body)
}

/// Returns the memory files currently on disk, as sorted slash-separated
/// paths relative to `dir`.
///
/// A missing directory is not an error — it just hasn't been created yet,
/// which is the normal state of affairs before the first pull.
pub fn list_memory_files(dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    walk(dir, "", &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rel = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        // file_type() comes off the directory entry and does not follow
        // symlinks, so a symlinked directory is listed as a file rather than
        // descended into — which also means no cycles.
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// Joins a slash-separated relative path onto a directory, on any platform.
pub(crate) fn join_relative(dir: &Path, rel: &str) -> PathBuf {
    let mut p = dir.to_path_buf();
    for segment in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(segment);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// The distinction the first-run guard in `hooks::push` depends on.
    #[test]
    fn a_missing_baseline_is_not_an_empty_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".recall-state.json");

        assert_eq!(load(&path).unwrap(), None, "no file yet");

        save(&path, &[]).unwrap();
        assert_eq!(
            load(&path).unwrap(),
            Some(State { files: vec![] }),
            "an empty baseline is still a baseline"
        );
    }

    #[test]
    fn round_trips_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".recall-state.json");
        save(&path, &["MEMORY.md".into(), "topics/auth.md".into()]).unwrap();

        let got = load(&path).unwrap().unwrap();
        assert_eq!(got.files, vec!["MEMORY.md", "topics/auth.md"]);
        // Parent directories are created rather than erroring.
        assert!(path.exists());
    }

    /// Better one missed delete than a hook that fails on every edit.
    #[test]
    fn a_corrupt_baseline_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".recall-state.json");
        write(&path, "{ this is not json");
        assert_eq!(load(&path).unwrap(), None);

        write(&path, r#"{"files": "not a list"}"#);
        assert_eq!(load(&path).unwrap(), None);
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".recall-state.json");
        save(&path, &["MEMORY.md".into()]).unwrap();
        save(&path, &["MEMORY.md".into(), "b.md".into()]).unwrap();

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".recall-state-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn lists_memory_files_sorted_with_forward_slashes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        write(&root.join("MEMORY.md"), "a");
        write(&root.join("topics").join("auth.md"), "b");
        write(&root.join("topics").join("deep").join("tokens.md"), "c");

        assert_eq!(
            list_memory_files(&root).unwrap(),
            vec!["MEMORY.md", "topics/auth.md", "topics/deep/tokens.md"]
        );
    }

    #[test]
    fn a_missing_memory_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let files = list_memory_files(&dir.path().join("never-created")).unwrap();
        assert!(files.is_empty());
    }
}
