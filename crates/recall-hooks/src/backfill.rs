//! The first sync: memory that was already on disk when Recall arrived.
//!
//! `push` sends the file a hook names and nothing else, so a memory
//! directory that already held files when Recall was installed keeps holding
//! them — each one reaches the server only if Claude happens to edit it, and
//! a `touch` from the shell fires no hook at all. Until this existed, Recall
//! could only protect memory written after it was installed, and catching up
//! meant one `curl` per file.
//!
//! # Why this sends less than "everything on disk"
//!
//! `POST /sync` overwrites in place. It compares no timestamps, reads no
//! `source_env`, and has no conflict status: whatever arrives last is what
//! the server holds. So a bulk push of the local directory is not a way to
//! rescue memory — on any machine that is not the first one, it is a way to
//! delete another machine's newer work.
//!
//! This asks the server what it already has, once per scope, and sends only
//! what is missing. A file the server holds with different bytes is left
//! alone and reported: that is a real disagreement, and the path for it is
//! `pull` and the server's merge, not a backfill that overwrites. A file the
//! server has tombstoned is left alone too — it was deleted somewhere, and
//! re-sending it from this disk would be a resurrection rather than a sync.
//!
//! # Why it stops rather than pushes through
//!
//! Every file is its own request; the HTTP surface is frozen and has no
//! batch. The server allows 60 requests a minute per IP by default, before
//! authentication, and that budget is shared with the push and pull hooks
//! running in the session this was typed into. So the first refusal ends the
//! run and says what is left. Re-running resumes by construction: what was
//! sent is now on the server, and the second run will not send it again.

use std::collections::HashMap;
use std::fs;

use recall_paths::scope;
use recall_wire::PushRequest;

use crate::context::{Context, Error};
use crate::{index, state};

/// What became of one file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// It was missing from the server, and was sent.
    Sent,
    /// The server already holds it, byte for byte.
    Matches,
    /// The server holds it with *different* bytes, so it was left alone.
    /// Overwriting is what the server does with whatever arrives last, and
    /// this is not the command that should decide the winner.
    Held,
    /// The server holds a tombstone for it: it was deleted somewhere else,
    /// and this disk has not caught up. Sending it would undo that delete.
    Deleted,
    /// It belongs to no scope. In practice this is the global directory with
    /// global sync switched off — pushing someone's global notes into one
    /// repository's history is the one outcome worth refusing.
    Unroutable,
    /// Not UTF-8, so it cannot be sent at all. Also how a `.DS_Store` or a
    /// stray image in the memory directory ends up quietly skipped.
    NotUtf8,
}

/// One file and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path, relative to the memory directory.
    pub path: String,
    /// What happened to it.
    pub disposition: Disposition,
}

/// What one backfill did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Every file considered, in the order they were walked, each with what
    /// became of it. Files after an early stop are absent rather than
    /// recorded as untouched — they were never reached.
    pub entries: Vec<Entry>,
    /// Why the run ended before the end of the directory, if it did. The
    /// files already sent stay sent, so a re-run picks up where this left
    /// off.
    pub stopped: Option<String>,
}

impl Outcome {
    /// How many files fell into one disposition.
    pub fn count(&self, what: &Disposition) -> usize {
        self.entries
            .iter()
            .filter(|e| &e.disposition == what)
            .count()
    }
}

/// Sends every memory file the server does not already have.
///
/// # Errors
///
/// Only for the round trip that asks what the server holds. Without that
/// answer there is no safe way to continue: sending blind is exactly the
/// overwrite this command exists to avoid. Failures of individual pushes end
/// the run and are reported in [`Outcome::stopped`] instead.
pub async fn backfill(ctx: &Context) -> Result<Outcome, Error> {
    // `MEMORY.md` is a memory file and is about to be sent, and its global
    // links are regenerated rather than synced. Refreshing first costs
    // nothing — the rewrite is byte-idempotent — and skipping it would put a
    // stale index on every other machine.
    if ctx.global().is_some() {
        index::refresh(&ctx.memory_dir)?;
    }

    let known = ask_what_the_server_has(ctx).await?;
    let mut out = Outcome::default();

    for rel in state::list_memory_files(&ctx.memory_dir)? {
        // Recall's own half-written files. `atomic::write` creates them in
        // the destination directory and removes them on drop, so one is only
        // ever visible to a walk that races a write — but a backfill is the
        // first thing here to walk a directory it did not write.
        if is_temp(&rel) {
            continue;
        }

        let Some((scope, path)) = scope::route(&ctx.scopes, &rel) else {
            out.push(rel, Disposition::Unroutable);
            continue;
        };

        let content = match fs::read(state::join_relative(&ctx.memory_dir, &rel)) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => content,
                Err(_) => {
                    out.push(rel, Disposition::NotUtf8);
                    continue;
                }
            },
            // Vanished between the walk and the read. Not a finding: the
            // next push will tombstone it.
            Err(_) => continue,
        };

        match known.get(&scope.key).and_then(|files| files.get(&path)) {
            Some(None) => {
                out.push(rel, Disposition::Deleted);
                continue;
            }
            Some(Some(theirs)) => {
                let same = *theirs == content;
                out.push(
                    rel,
                    if same {
                        Disposition::Matches
                    } else {
                        Disposition::Held
                    },
                );
                continue;
            }
            None => {}
        }

        let req = PushRequest {
            project_key: scope.key.clone(),
            file_path: path,
            content: Some(content),
            source_env: ctx.source_env.clone(),
            deleted: false,
        };
        match ctx.client.push(&req).await {
            Ok(_) => out.push(rel, Disposition::Sent),
            Err(err) => {
                out.stopped = Some(why_it_stopped(&rel, &err));
                break;
            }
        }
    }

    // Written whether or not the run finished. The baseline records what is
    // on disk, not what was sent, and a machine that has never had one is
    // exactly the machine a backfill runs on — until it has one, `push`
    // cannot detect a delete at all.
    ctx.refresh_state()?;
    Ok(out)
}

/// Every file the server holds, per scope key: the content, or [`None`] for
/// a tombstone.
async fn ask_what_the_server_has(
    ctx: &Context,
) -> Result<HashMap<String, HashMap<String, Option<String>>>, Error> {
    let mut known = HashMap::new();
    for scope in &ctx.scopes {
        let resp = ctx
            .client
            .pull(&scope.key)
            .await
            .map_err(|source| Error::Pull {
                project_key: scope.key.clone(),
                source,
            })?;
        let files = resp
            .files
            .into_iter()
            .map(|file| {
                let content = if file.deleted { None } else { file.content };
                (file.file_path, content)
            })
            .collect();
        known.insert(scope.key.clone(), files);
    }
    Ok(known)
}

/// The sentence a user reads when the run ends early. It has to say what to
/// do next, because "43 of 60 sent" with no verb is just an alarm.
fn why_it_stopped(path: &str, err: &crate::client::Error) -> String {
    if let crate::client::Error::Status { code: 429, .. } = err {
        return format!(
            "the server's rate limit was reached at {path} — it allows 60 requests a \
             minute per address by default, shared with the hooks in your session. \
             Everything sent so far is on the server; run this again in a minute to \
             continue"
        );
    }
    format!("{path} could not be sent ({err}) — nothing after it was tried")
}

/// Whether this is one of `atomic::write`'s temporary files rather than a
/// memory file.
fn is_temp(rel: &str) -> bool {
    rel.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with(".recall-"))
}

impl Outcome {
    fn push(&mut self, path: String, disposition: Disposition) {
        self.entries.push(Entry { path, disposition });
    }
}
