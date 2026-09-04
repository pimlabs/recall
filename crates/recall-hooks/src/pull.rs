//! `recall pull` — the `SessionStart` half.

use std::fs;
use std::io;

use crate::atomic;
use crate::context::{Context, Error};
use crate::index;
use crate::path::is_under;
use crate::state;

/// What a pull changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullOutcome {
    /// Files written to the local memory directory, relative to it — so a
    /// global one reads as `global/editor.md`.
    pub written: Vec<String>,
    /// Local files removed because their scope holds a tombstone for them.
    pub removed: Vec<String>,
}

impl PullOutcome {
    /// A short line for the hook's stderr, which is where Claude Code shows
    /// hook output to the user.
    pub fn describe(&self, project_key: &str) -> String {
        format!(
            "recall-pull: synced {} memory file(s), removed {} deleted file(s) for {}",
            self.written.len(),
            self.removed.len(),
            project_key
        )
    }
}

/// Fetches every configured scope and makes the local memory directory match,
/// then refreshes the baseline so a machine that only ever pulls still has an
/// accurate one — otherwise its first local delete would go unnoticed.
///
/// A scope that fails is fatal, deliberately: a half-applied pull is worse
/// than none, and the caller turns any error into "leaving local memory
/// untouched" rather than a failed session.
pub async fn pull(ctx: &Context) -> Result<PullOutcome, Error> {
    let mut res = PullOutcome::default();
    let mut any_files = false;

    for scope in &ctx.scopes {
        let resp = ctx
            .client
            .pull(&scope.key)
            .await
            .map_err(|source| Error::Pull {
                project_key: scope.key.clone(),
                source,
            })?;

        if resp.files.is_empty() {
            // This scope has nothing yet. Nothing to write and nothing to
            // reconcile against.
            continue;
        }
        any_files = true;
        fs::create_dir_all(&ctx.memory_dir)?;

        for file in &resp.files {
            // The server validates this on the way in, and it is validated
            // again here on the way out: this is the moment a malicious or
            // buggy server's traversal path would become a write outside the
            // memory directory on *this* machine. A bad path is skipped
            // rather than failing the pull, so one poisoned row can't block
            // the rest.
            if recall_wire::validate_file_path(&file.file_path).is_err() {
                continue;
            }
            let rel = scope.local_path(&file.file_path);
            let dest = state::join_relative(&ctx.memory_dir, &rel);
            // Belt and braces: validation is the real guard, but the
            // containment check is cheap and this is the security boundary.
            if !is_under(&ctx.memory_dir, &dest) {
                continue;
            }

            if file.deleted {
                match fs::remove_file(&dest) {
                    Ok(()) => res.removed.push(rel),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                continue;
            }

            // A tombstone withholds content; an empty file legitimately has
            // content that happens to be empty. `Option` is what keeps those
            // apart, so `None` here means "nothing to write", not "write ''".
            let Some(content) = file.content.as_ref() else {
                continue;
            };

            atomic::write(&dest, ".recall-", ".tmp", content.as_bytes())?;
            res.written.push(rel);
        }
    }

    if !any_files {
        // Nothing anywhere. Writing a baseline here would only invent one out
        // of whatever happens to be on disk.
        return Ok(res);
    }

    // After the writes, not before: the index lists what is now there.
    if ctx.global().is_some() {
        index::refresh(&ctx.memory_dir)?;
    }
    ctx.refresh_state()?;
    Ok(res)
}
