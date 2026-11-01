//! `recall push` — the `PostToolUse` half.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::scope::{route, Scope};
use recall_wire::PushRequest;

use crate::context::{Context, Error};
use crate::index;
use crate::path::{is_under, relative_slash};
use crate::state;

/// How long a run keeps trying once the server's rate limit has refused it:
/// the server's one-minute window and a little, so a refusal at any point in
/// a window is outlasted by it. A hook waiting is better than one that drops
/// a push, since nothing else may run for this file for a long time. Claude
/// Code's hook timeout is ten minutes; an older one of sixty seconds ends the
/// wait early, which leaves the file unsent, the same as giving up.
#[cfg(not(test))]
const RATE_LIMIT_PATIENCE: Duration = Duration::from_secs(65);

/// How long apart it tries. A refused request costs nothing: the window is
/// fixed, so asking does not move its end.
#[cfg(not(test))]
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(5);

/// Less in this crate's own tests, where only the order matters.
#[cfg(test)]
const RATE_LIMIT_PATIENCE: Duration = Duration::from_millis(300);
#[cfg(test)]
const RATE_LIMIT_WAIT: Duration = Duration::from_millis(10);

/// One run's patience with the server's rate limit.
///
/// A run waits by starting again rather than by asking again for one file.
/// The wait is a minute long, and what was read before it may not be true
/// after it: a file changed, come back or gone in that minute would be sent
/// as it was, deleted though it is there, or dropped from the baseline with
/// its delete never sent. A fresh attempt reads the baseline and the
/// directory again, so none of that lasts longer than a request does.
#[derive(Debug, Default)]
pub(crate) struct Patience {
    refused_at: Option<Instant>,
}

impl Patience {
    /// Waits before the next attempt, or answers false when the wait has
    /// run out. It runs from the first refusal, so one run waits once,
    /// however many files it holds.
    pub(crate) async fn wait(&mut self) -> bool {
        if self.refused_at.get_or_insert_with(Instant::now).elapsed() >= RATE_LIMIT_PATIENCE {
            return false;
        }
        tokio::time::sleep(RATE_LIMIT_WAIT).await;
        true
    }
}

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

    let mut patience = Patience::default();
    loop {
        let Err(err) = attempt(ctx, triggered_path, &mut res).await else {
            return Ok(res);
        };
        if !err.rate_limited() || !patience.wait().await {
            return Err(err);
        }
    }
}

/// One go at everything [`push`] does, from a fresh look at the baseline and
/// the directory. However it ends, what went out is recorded.
async fn attempt(ctx: &Context, triggered_path: &Path, res: &mut PushOutcome) -> Result<(), Error> {
    let baseline = state::load(&ctx.state_file)?;
    let triggered_rel = relative_slash(&ctx.memory_dir, triggered_path);

    let mut run = Pending::default();
    let mut done = send_all(
        ctx,
        baseline.as_ref(),
        triggered_path,
        triggered_rel.as_deref(),
        &mut run,
        res,
    )
    .await;
    if done.is_ok() && ctx.has_reserved_scope() {
        done = index::refresh(&ctx.memory_dir).map_err(Error::from);
    }
    res.deleted.extend(run.deleted.iter().cloned());
    if let Err(err) = done {
        // A failure before anything went out, and not the server's, says
        // nothing about what the server has.
        if err.server_error().is_some() || run.went_out() {
            // Best effort: the failure is the answer either way.
            let _ = ctx.keep_what_was_sent(&run.sent, &run.deleted, triggered_rel.as_deref());
        }
        return Err(err);
    }
    ctx.refresh_state_with(&run.sent)?;
    Ok(())
}

/// Everything [`push`] sends, in order, leaving in `run` what went out, so
/// that a run stopped partway still knows.
async fn send_all(
    ctx: &Context,
    baseline: Option<&state::State>,
    triggered_path: &Path,
    triggered_rel: Option<&str>,
    run: &mut Pending,
    res: &mut PushOutcome,
) -> Result<(), Error> {
    // Everything else this machine changed since the last sync, deletes
    // included: see [`send_pending`]. Never on the very first run for a
    // project. With no baseline, an empty or partial memory directory would
    // otherwise read as "everything was deleted" and tombstone the project's
    // whole history on the server. `None` here means "nothing has ever
    // synced", which is why load() distinguishes it from an empty baseline.
    if let Some(prev) = baseline {
        let swept = send_pending(ctx, prev, triggered_rel, run).await;
        res.swept
            .extend(run.sent.iter().map(|(rel, _)| rel.clone()));
        swept?;
    }

    if fs::metadata(triggered_path).is_ok_and(|m| !m.is_dir()) {
        let rel = triggered_rel.expect("containment was just checked");

        // A file in no scope — the global directory while global sync is
        // off — is left alone rather than swept into this project.
        if let Some((scope, path)) = route(&ctx.scopes, rel) {
            // Exact bytes. The shell version used command substitution,
            // which strips every trailing newline, so a file with none or
            // with two came back from a round trip with exactly one —
            // content silently altered. Reading raw and handing the bytes
            // straight to the serializer is what prevents that.
            let content = fs::read(triggered_path)?;
            let content = String::from_utf8(content).map_err(|_| Error::NotUtf8 {
                path: rel.to_string(),
            })?;

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
                base_sha256: baseline.and_then(|s| s.bases.get(rel)).cloned(),
            };
            ctx.client.push(&req).await.map_err(|source| Error::Push {
                path: rel.to_string(),
                source,
            })?;
            // The base for the next edit is what was sent, not what the
            // server stored: after a merge the two differ, the local file
            // still lacks the other side's changes until the next pull, and
            // naming the merged version would let the next push overwrite
            // them.
            run.sent.push((rel.to_string(), sent));
            res.pushed = Some(rel.to_string());
        }
    }
    Ok(())
}

/// What a run has sent so far.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    /// `(path, content_sha256)` for each file sent, as the new base.
    pub(crate) sent: Vec<(String, String)>,
    /// Paths sent as deletes.
    pub(crate) deleted: Vec<String>,
}

impl Pending {
    /// Whether anything has gone out.
    pub(crate) fn went_out(&self) -> bool {
        !self.sent.is_empty() || !self.deleted.is_empty()
    }
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
///
/// What went out is left in `out` whether or not all of it did.
pub(crate) async fn send_pending(
    ctx: &Context,
    prev: &state::State,
    skip: Option<&str>,
    out: &mut Pending,
) -> Result<(), Error> {
    for rel in vanished(&ctx.memory_dir, &ctx.scopes, prev) {
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

    for (rel, content) in changed(&ctx.memory_dir, &ctx.scopes, prev, skip)? {
        let Some((scope, path)) = route(&ctx.scopes, &rel) else {
            continue;
        };
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
    Ok(())
}

/// The files in the baseline that are gone from disk, in a scope that would
/// take their delete.
fn vanished<'a>(
    memory_dir: &'a Path,
    scopes: &'a [Scope],
    prev: &'a state::State,
) -> impl Iterator<Item = &'a String> {
    prev.files.iter().filter(move |rel| {
        !state::join_relative(memory_dir, rel).exists() && route(scopes, rel).is_some()
    })
}

/// The files that differ from how the last sync left them, each with its
/// content, except `skip`.
fn changed(
    memory_dir: &Path,
    scopes: &[Scope],
    prev: &state::State,
    skip: Option<&str>,
) -> Result<Vec<(String, String)>, std::io::Error> {
    let mut out = Vec::new();
    for rel in state::list_memory_files(memory_dir)? {
        if Some(rel.as_str()) == skip || state::is_internal(&rel) {
            continue;
        }
        // A file in no scope is left alone, as a push leaves it.
        if route(scopes, &rel).is_none() {
            continue;
        }
        // Not memory Claude Code could have written, and not something to
        // fail every hook over: left where it is, unsent.
        let Ok(content) = fs::read_to_string(state::join_relative(memory_dir, &rel)) else {
            continue;
        };
        if prev.changed_since_sync(&rel, &content) {
            out.push((rel, content));
        }
    }
    Ok(out)
}

/// What the next push or pull would send: every memory file changed here
/// since the last sync, and every delete, sorted. Empty before the first
/// sync, which has no "since".
///
/// `recall doctor`'s question. A pull from another folder, or another
/// machine, cannot tell that this one holds something it has not had.
pub fn unsent(
    memory_dir: &Path,
    state_file: &Path,
    scopes: &[Scope],
) -> Result<Vec<String>, std::io::Error> {
    let Some(prev) = state::load(state_file)? else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = vanished(memory_dir, scopes, &prev).cloned().collect();
    out.extend(
        changed(memory_dir, scopes, &prev, None)?
            .into_iter()
            .map(|(rel, _)| rel),
    );
    out.sort();
    Ok(out)
}
