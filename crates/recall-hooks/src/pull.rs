//! `recall pull` — the `SessionStart` half.

use std::fs;
use std::io;

use crate::atomic;
use crate::context::{Context, Error};
use crate::index;
use crate::path::is_under;
use crate::push::{send_pending, Patience, Pending};
use crate::state;

/// What a pull changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullOutcome {
    /// Files written to the local memory directory, relative to it — so a
    /// global one reads as `global/editor.md`.
    pub written: Vec<String>,
    /// Local files removed because their scope holds a tombstone for them.
    pub removed: Vec<String>,
    /// Local changes sent before fetching, because the hooks never saw
    /// them: files edited or created outside `Edit` and `Write`.
    pub sent: Vec<String>,
    /// Local deletes sent before fetching, for the same reason.
    pub sent_deletes: Vec<String>,
}

impl PullOutcome {
    /// A short line for the hook's stderr, which is where Claude Code shows
    /// hook output to the user.
    pub fn describe(&self, project_key: &str) -> String {
        let mut line = format!(
            "recall-pull: synced {} memory file(s), removed {} deleted file(s) for {}",
            self.written.len(),
            self.removed.len(),
            project_key
        );
        if !self.sent.is_empty() || !self.sent_deletes.is_empty() {
            line.push_str(&format!(
                "; first sent {} local change(s) and {} local delete(s) no hook had seen",
                self.sent.len(),
                self.sent_deletes.len()
            ));
        }
        line
    }
}

/// Fetches every configured scope and makes the local memory directory match,
/// then refreshes the baseline so a machine that only ever pulls still has an
/// accurate one — otherwise its first local delete would go unnoticed.
///
/// First, though, it sends what this machine changed since its last sync
/// and no hook saw (the same sweep the push hook makes), and then
/// leaves those files as they are rather than overwriting them with what it
/// fetched. A pull runs at every session start, resume and compaction, and
/// it used to write the server's copy over every file: a note changed
/// through the shell mid-session was simply gone after the next compaction.
/// What it fetched for such a file is the stored result of that send, a
/// merge when another machine had moved it, and the next pull brings it
/// down, since by then the file is back in step with its base.
///
/// A scope that fails is fatal, deliberately: a half-applied pull is worse
/// than none, and the caller turns any error into "leaving local memory
/// untouched" rather than a failed session.
pub async fn pull(ctx: &Context) -> Result<PullOutcome, Error> {
    let mut res = PullOutcome::default();
    let mut patience = Patience::default();
    loop {
        let Err(err) = attempt(ctx, &mut res).await else {
            return Ok(res);
        };
        if !err.rate_limited() || !patience.wait().await {
            return Err(err);
        }
    }
}

/// One go at everything [`pull`] does, from a fresh look at the baseline and
/// the directory: see [`Patience`] for why a wait starts again. `res` carries
/// what an earlier attempt sent, which is still the newer of the two. However
/// it ends, what went out is recorded.
async fn attempt(ctx: &Context, res: &mut PullOutcome) -> Result<(), Error> {
    let mut pending = Pending::default();
    let mut written = Vec::new();
    let done = sweep_and_fetch(ctx, res, &mut pending, &mut written).await;
    if let Err(err) = done {
        if err.server_error().is_some()
            || pending.went_out()
            || !written.is_empty()
            || !res.removed.is_empty()
        {
            // What the fetch wrote or removed is the server's doing, and
            // left out of the baseline it reads as an edit made here, which
            // the next sweep would send back. Best effort: the failure is
            // the answer either way.
            let synced: Vec<_> = pending.sent.iter().chain(&written).cloned().collect();
            let gone: Vec<_> = pending
                .deleted
                .iter()
                .chain(&res.removed)
                .cloned()
                .collect();
            let _ = ctx.keep_what_was_sent(&synced, &gone, None);
        }
        return Err(err);
    }
    Ok(())
}

/// The sweep, then every scope's fetch, leaving in `pending` what went out
/// and in `written` what came in, so that a run stopped partway still knows.
async fn sweep_and_fetch(
    ctx: &Context,
    res: &mut PullOutcome,
    pending: &mut Pending,
    written: &mut Vec<(String, String)>,
) -> Result<(), Error> {
    let mut any_files = false;

    // Never on a first run, for the reason the push hook gives: with no
    // baseline there is no "since", and an empty directory would read as
    // everything deleted.
    if let Some(prev) = state::load(&ctx.state_file)? {
        let swept = send_pending(ctx, &prev, None, pending).await;
        res.sent
            .extend(pending.sent.iter().map(|(rel, _)| rel.clone()));
        res.sent_deletes.extend(pending.deleted.iter().cloned());
        swept?;
        any_files = pending.went_out();
    }

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
            // Just sent: the local file (or its absence) is the newer of
            // the two, whatever the server made of it.
            if res.sent.contains(&rel) || res.sent_deletes.contains(&rel) {
                continue;
            }

            if file.deleted {
                match fs::remove_file(&dest) {
                    Ok(()) => {
                        // Two scopes can name one local path, through a row
                        // from before the global scope.
                        written.retain(|(path, _)| *path != rel);
                        res.removed.push(rel);
                    }
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
            // What is on disk now is exactly what the server holds, so it is
            // the base of whatever this machine edits next.
            written.push((rel.clone(), recall_wire::content_sha256(content)));
            // An attempt after a refusal writes the first scope again.
            if !res.written.contains(&rel) {
                res.written.push(rel);
            }
        }
    }

    if !any_files {
        // Nothing anywhere. Writing a baseline here would only invent one out
        // of whatever happens to be on disk.
        return Ok(());
    }

    // After the writes, not before: the index lists what is now there.
    if ctx.has_reserved_scope() {
        index::refresh(&ctx.memory_dir)?;
    }
    let synced: Vec<_> = pending.sent.iter().chain(written.iter()).cloned().collect();
    ctx.refresh_state_with(&synced)?;
    Ok(())
}
