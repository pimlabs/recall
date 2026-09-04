//! `recall push` — the `PostToolUse` half.

use std::fs;
use std::path::Path;

use recall_wire::PushRequest;

use crate::context::{Context, Error};
use crate::path::{is_under, relative_slash};
use crate::state;

/// What a push actually did, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushOutcome {
    /// The memory-file path that was sent, if the triggering file was one.
    pub pushed: Option<String>,
    /// Paths reported as deleted by reconciliation.
    pub deleted: Vec<String>,
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

    // Reconcile deletes, but never on the very first run for a project.
    // With no baseline, an empty or partial memory directory would otherwise
    // read as "everything was deleted" and tombstone the project's whole
    // history on the server. `None` here means "nothing has ever synced",
    // which is why load() distinguishes it from an empty baseline.
    if let Some(prev) = baseline {
        for rel in &prev.files {
            if state::join_relative(&ctx.memory_dir, rel).exists() {
                continue;
            }
            let req = PushRequest {
                project_key: ctx.project_key.clone(),
                file_path: rel.clone(),
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
            res.deleted.push(rel.clone());
        }
    }

    if fs::metadata(triggered_path).is_ok_and(|m| !m.is_dir()) {
        let rel =
            relative_slash(&ctx.memory_dir, triggered_path).expect("containment was just checked");

        // Exact bytes. The shell version used command substitution, which
        // strips every trailing newline, so a file with none or with two came
        // back from a round trip with exactly one — content silently altered.
        // Reading raw and handing the bytes straight to the serializer is
        // what prevents that.
        let content = fs::read(triggered_path)?;
        let content =
            String::from_utf8(content).map_err(|_| Error::NotUtf8 { path: rel.clone() })?;

        let req = PushRequest {
            project_key: ctx.project_key.clone(),
            file_path: rel.clone(),
            // Some, even when the file is empty: None means "this is a
            // delete", and the server rejects a non-delete push with no
            // content field at all.
            content: Some(content),
            source_env: ctx.source_env.clone(),
            deleted: false,
        };
        ctx.client.push(&req).await.map_err(|source| Error::Push {
            path: rel.clone(),
            source,
        })?;
        res.pushed = Some(rel);
    }

    ctx.refresh_state()?;
    Ok(res)
}
