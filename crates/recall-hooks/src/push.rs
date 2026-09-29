//! `recall push` — the `PostToolUse` half.

use std::fs;
use std::path::Path;

use crate::scope::route;
use recall_wire::PushRequest;

use crate::context::{Context, Error};
use crate::index;
use crate::path::{is_under, relative_slash};
use crate::state;

/// What a push actually did, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushOutcome {
    /// The memory-file path that was sent, if the triggering file was one.
    /// Relative to the memory directory, so a global file reads as
    /// `global/editor.md`.
    pub pushed: Option<String>,
    /// Paths reported as deleted by reconciliation.
    pub deleted: Vec<String>,
    /// Other memory files sent because they had changed since the last
    /// sync without a hook seeing it: an edit through the shell, say.
    pub swept: Vec<String>,
    /// The triggering file wasn't a memory file, so nothing happened at all.
    pub skipped: bool,
}

/// Handles one `PostToolUse` invocation.
///
/// Two things happen here, and only one of them is about the file that
/// triggered the hook. The triggering file is pushed if it is a memory file.
/// Separately — and regardless of what triggered this run — the memory
/// directory is reconciled against the last known baseline, and anything
/// that vanished is reported as a delete. That reconciliation is the *only*
/// mechanism that catches deletes at all: Claude Code has no delete event,
/// and an `rm` through the Bash tool wouldn't match an `Edit|Write` matcher
/// even if it did.
///
/// Each file goes to the scope that owns it, so an edit under `global/` is
/// stored against the global key rather than this repository's.
pub async fn push(ctx: &Context, triggered_path: &Path) -> Result<PushOutcome, Error> {
    let mut res = PushOutcome::default();

    if triggered_path.as_os_str().is_empty() || !is_under(&ctx.memory_dir, triggered_path) {
        // Not a memory file. Nothing to push, and — importantly — no
        // reconciliation either: this hook fires on every Edit and Write in
        // the session, and a directory walk plus a state write on each one
        // would be pure waste. Deletes still propagate on the next edit that
        // does touch a memory file.
        res.skipped = true;
        return Ok(res);
    }

    let baseline = state::load(&ctx.state_file)?;
    let triggered_rel = relative_slash(&ctx.memory_dir, triggered_path);

    // Everything else this machine changed since the last sync, deletes
    // included: see [`send_pending`]. Never on the very first run for a
    // project. With no baseline, an empty or partial memory directory would
    // otherwise read as "everything was deleted" and tombstone the project's
    // whole history on the server. `None` here means "nothing has ever
    // synced", which is why load() distinguishes it from an empty baseline.
    let mut synced = Vec::new();
    if let Some(prev) = &baseline {
        let pending = send_pending(ctx, prev, triggered_rel.as_deref()).await?;
        res.deleted = pending.deleted;
        res.swept = pending.sent.iter().map(|(rel, _)| rel.clone()).collect();
        synced = pending.sent;
    }

    if fs::metadata(triggered_path).is_ok_and(|m| !m.is_dir()) {
        let rel = triggered_rel.expect("containment was just checked");

        // A file in no scope — the global directory while global sync is
        // off — is left alone rather than swept into this project.
        if let Some((scope, path)) = route(&ctx.scopes, &rel) {
            // Exact bytes. The shell version used command substitution,
            // which strips every trailing newline, so a file with none or
            // with two came back from a round trip with exactly one —
            // content silently altered. Reading raw and handing the bytes
            // straight to the serializer is what prevents that.
            let content = fs::read(triggered_path)?;
            let content =
                String::from_utf8(content).map_err(|_| Error::NotUtf8 { path: rel.clone() })?;

            let sent = recall_wire::content_sha256(&content);
            let req = PushRequest {
                project_key: scope.key.clone(),
                file_path: path,
                // Some, even when the file is empty: None means "this is
                // a delete", and the server rejects a non-delete push
                // with no content field at all.
                content: Some(content),
                source_env: ctx.source_env.clone(),
                deleted: false,
                // What this edit started from. The server merges only when
                // what it holds is something else — a concurrent edit —
                // and otherwise lets this replace it, deletions included.
                base_sha256: baseline.as_ref().and_then(|s| s.bases.get(&rel)).cloned(),
            };
            ctx.client.push(&req).await.map_err(|source| Error::Push {
                path: rel.clone(),
                source,
            })?;
            // The base for the next edit is what was sent, not what the
            // server stored: after a merge the two differ, the local file
            // still lacks the other side's changes until the next pull, and
            // naming the merged version would let the next push overwrite
            // them.
            synced.push((rel.clone(), sent));
            res.pushed = Some(rel);
        }
    }

    if ctx.has_reserved_scope() {
        index::refresh(&ctx.memory_dir)?;
    }
    ctx.refresh_state_with(&synced)?;
    Ok(res)
}

/// What [`send_pending`] sent.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    /// `(path, content_sha256)` for each file sent, as the new base.
    pub(crate) sent: Vec<(String, String)>,
    /// Paths sent as deletes.
    pub(crate) deleted: Vec<String>,
}

/// Sends every change this machine made since `prev` that the server has
/// not had: files in the baseline that are gone (as deletes), and files
/// that differ from how the last sync left them (see
/// [`State::changed_since_sync`](state::State::changed_since_sync)),
/// except `skip`, which the caller sends itself.
///
/// The hooks see only what goes through Claude Code's `Edit` and `Write`.
/// A note written, appended to or removed through the shell fires nothing,
/// and until this existed such a change reached the server only if a later
/// `Edit` touched the same file. Worse, the next pull, which runs at every
/// session start, resume and compaction, overwrote it with the server's
/// copy, so the change was lost without a word. Both the push hook and a
/// pull call this first, so neither can happen.
///
/// Each file is sent with its base, so the server replaces its copy when
/// that is still the version this edit started from and merges when
/// another machine moved it in between: the same rule as any other push.
pub(crate) async fn send_pending(
    ctx: &Context,
    prev: &state::State,
    skip: Option<&str>,
) -> Result<Pending, Error> {
    let mut out = Pending::default();

    for rel in &prev.files {
        if state::join_relative(&ctx.memory_dir, rel).exists() {
            continue;
        }
        // A vanished file goes to the scope that owned it, which is not
        // necessarily this project's.
        let Some((scope, path)) = route(&ctx.scopes, rel) else {
            continue;
        };
        let req = PushRequest {
            project_key: scope.key.clone(),
            file_path: path,
            deleted: true,
            source_env: ctx.source_env.clone(),
            ..Default::default()
        };
        ctx.client
            .push(&req)
            .await
            .map_err(|source| Error::PushDelete {
                path: rel.clone(),
                source,
            })?;
        out.deleted.push(rel.clone());
    }

    for rel in state::list_memory_files(&ctx.memory_dir)? {
        if Some(rel.as_str()) == skip || state::is_internal(&rel) {
            continue;
        }
        // A file in no scope is left alone, as a push leaves it.
        let Some((scope, path)) = route(&ctx.scopes, &rel) else {
            continue;
        };
        // Not memory Claude Code could have written, and not something to
        // fail every hook over: left where it is, unsent.
        let Ok(content) = fs::read_to_string(state::join_relative(&ctx.memory_dir, &rel)) else {
            continue;
        };
        if !prev.changed_since_sync(&rel, &content) {
            continue;
        }
        let sent = recall_wire::content_sha256(&content);
        let req = PushRequest {
            project_key: scope.key.clone(),
            file_path: path,
            content: Some(content),
            source_env: ctx.source_env.clone(),
            deleted: false,
            base_sha256: prev.bases.get(&rel).cloned(),
        };
        ctx.client.push(&req).await.map_err(|source| Error::Push {
            path: rel.clone(),
            source,
        })?;
        out.sent.push((rel, sent));
    }
    Ok(out)
}
