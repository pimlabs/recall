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
//! alone and reported: that is a real disagreement, and nothing here is
//! entitled to pick the winner. A file the server has tombstoned is left
//! alone too — it was deleted somewhere, and re-sending it from this disk
//! would be a resurrection rather than a sync.
//!
//! That guarantee is as of the moment it asked. The frozen HTTP surface has
//! no conditional write, so a file another machine pushes *during* a long
//! run is not in the snapshot and can still be overwritten. Nothing here can
//! close that window; saying so is the honest alternative to implying it is
//! closed.
//!
//! # Why one refusal does not end the run, and another does
//!
//! Every file is its own request; the HTTP surface is frozen and has no
//! batch. Two very different things can go wrong, and treating them alike
//! was a bug: a file the server will *never* accept — a name its validator
//! rejects, a body over the size limit — must not stop the files behind it,
//! or one bad name makes everything sorted after it permanently unsendable.
//! Those are recorded and skipped. A refusal about the *run* — the rate
//! limit, a bad token, a server error — ends it, because continuing only
//! burns a budget shared with the push and pull hooks in the session this
//! was typed into. Re-running then resumes: what was sent is on the server,
//! and the second pass finds it there.

use std::collections::HashMap;
use std::fs;
use std::io;

use crate::scope;
use recall_wire::PushRequest;

use crate::context::{Context, Error};
use crate::{index, state};

/// What became of one file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// stray image in the memory directory ends up skipped.
    NotUtf8,
    /// The server, or this client's own validation, refused this particular
    /// file and would refuse it again. Skipped so that the files behind it
    /// still go.
    Refused,
    /// It is on disk but could not be read — permissions, a symlink to a
    /// directory, a bad block. Reported rather than dropped: a file missing
    /// from both the sync and the report is the failure this whole command
    /// exists to end.
    Ignored,
    /// One of Recall's own half-written files, which `atomic::write` creates
    /// in the destination directory and removes on drop.
    Internal,
}

/// One file and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The path, relative to the memory directory.
    pub path: String,
    /// What happened to it.
    pub disposition: Disposition,
    /// Why, when the disposition alone does not say.
    pub detail: Option<String>,
}

/// What one backfill did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Every file reached, in the order they were walked, each with what
    /// became of it.
    pub entries: Vec<Entry>,
    /// Why the run ended before the end of the directory, if it did.
    pub stopped: Option<String>,
    /// How many files were never reached because the run ended early.
    pub not_reached: usize,
    /// Why the baseline was not written, when it was not. It is skipped
    /// after an early stop on purpose: the baseline is a claim about a
    /// finished comparison, and half of one is not that.
    pub baseline: Option<String>,
}

impl Outcome {
    /// How many files fell into one disposition.
    pub fn count(&self, what: Disposition) -> usize {
        self.entries
            .iter()
            .filter(|e| e.disposition == what)
            .count()
    }

    fn push(&mut self, path: String, disposition: Disposition, detail: Option<String>) {
        self.entries.push(Entry {
            path,
            disposition,
            detail,
        });
    }
}

/// Whether a failed push should end the run, or only this file.
enum Refusal {
    ThisFile(String),
    TheRun(String),
}

/// Sends every memory file the server does not already have.
///
/// # Errors
///
/// Only before anything is sent: the round trip that asks what the server
/// holds, and the local reads that precede the loop. Without the server's
/// answer there is no safe way to continue, because sending blind is exactly
/// the overwrite this command exists to avoid. Everything that can go wrong
/// once sending has begun is reported in the [`Outcome`] instead, so a
/// partial run can still say what it did.
pub async fn backfill(ctx: &Context) -> Result<Outcome, Error> {
    // Asked before anything local is touched. `index::refresh` below rewrites
    // `MEMORY.md`, and a command whose contract is "ask first, then send"
    // must not have already edited a memory file on the path where it sends
    // nothing at all. This is the ordering `promote` argues for.
    let known = ask_what_the_server_has(ctx).await?;

    // `MEMORY.md` is a memory file and is about to be sent, and its global
    // links are regenerated rather than synced. Refreshing first costs
    // nothing — the rewrite is byte-idempotent — and skipping it would put a
    // stale index on every other machine.
    if ctx.has_reserved_scope() {
        index::refresh(&ctx.memory_dir)?;
    }

    let files = state::list_memory_files(&ctx.memory_dir)?;
    let mut out = Outcome::default();

    for (i, rel) in files.iter().enumerate() {
        let rel = rel.clone();

        if is_temp(&rel) {
            out.push(rel, Disposition::Internal, None);
            continue;
        }

        let Some((scope, path)) = scope::route(&ctx.scopes, &rel) else {
            // Every cause of Unroutable needs a different action from the
            // reader, so each file carries its own remedy. The heading used
            // to name RECALL_GLOBAL_KEY for all of them, which is the wrong
            // advice for a path under `machine/` and no advice at all for a
            // directory that is merely miscased.
            let detail = unroutable_detail(&rel);
            out.push(rel, Disposition::Unroutable, detail);
            continue;
        };

        let content = match fs::read(state::join_relative(&ctx.memory_dir, &rel)) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => content,
                Err(_) => {
                    out.push(rel, Disposition::NotUtf8, None);
                    continue;
                }
            },
            // Gone between the walk and the read is nothing at all — the next
            // push tombstones it. Anything else is a file that exists and
            // cannot be read, which has to be said out loud.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                out.push(rel, Disposition::Ignored, Some(e.to_string()));
                continue;
            }
        };

        match known.get(&scope.key).and_then(|files| files.get(&path)) {
            Some(None) => {
                out.push(rel, Disposition::Deleted, None);
                continue;
            }
            Some(Some(theirs)) => {
                let same = *theirs == content;
                let what = if same {
                    Disposition::Matches
                } else {
                    Disposition::Held
                };
                out.push(rel, what, None);
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
            // Backfill only sends what the server does not have, so there
            // is nothing stored for a base to be compared with.
            base_sha256: None,
        };
        match ctx.client.push(&req).await {
            Ok(_) => out.push(rel, Disposition::Sent, None),
            Err(err) => match classify(&err) {
                Refusal::ThisFile(why) => out.push(rel, Disposition::Refused, Some(why)),
                Refusal::TheRun(why) => {
                    out.stopped = Some(format!("at {rel}: {why}"));
                    out.not_reached = files.len() - i - 1;
                    break;
                }
            },
        }
    }

    write_baseline(ctx, &mut out);
    Ok(out)
}

/// The baseline is what lets a later `push` tell a deleted file from one
/// that was never there — and a machine whose memory predates Recall is
/// exactly the machine that has none.
///
/// It is not written after an early stop. `pull` declines for the same
/// reason in its own empty case: a baseline invented from whatever happens
/// to be on disk, when the comparison against the server never finished, is
/// a claim this run has not earned.
fn write_baseline(ctx: &Context, out: &mut Outcome) {
    if out.stopped.is_some() {
        out.baseline = Some(
            "the run stopped early, so the delete baseline was left as it was — \
             finish the run to write one"
                .to_string(),
        );
        return;
    }
    if let Err(err) = ctx.refresh_state() {
        out.baseline = Some(format!(
            "everything above still happened, but the delete baseline could not be \
             written ({err}), so the next push cannot detect a deleted file"
        ));
    }
}

/// Whether this refusal is about the file or about the run.
///
/// The distinction is the difference between skipping one file and making
/// every file sorted after it permanently unsendable.
fn classify(err: &crate::client::Error) -> Refusal {
    match err {
        // Caught by this client before sending, by the same rules the server
        // applies. Retrying changes nothing.
        crate::client::Error::Invalid(e) => Refusal::ThisFile(e.to_string()),
        // The server's judgement of this body: a name it will not take, or
        // one too large for it. Also permanent.
        crate::client::Error::Status { code, body } if *code == 400 || *code == 413 => {
            Refusal::ThisFile(format!("the server refused it ({code}: {body})"))
        }
        crate::client::Error::Status { code, .. } if *code == 429 => Refusal::TheRun(
            "the server's rate limit was reached — it allows 60 requests a minute per \
             address by default, and that budget is shared with the hooks in your \
             session. What was sent is on the server; run this again in a minute to \
             carry on"
                .to_string(),
        ),
        other => Refusal::TheRun(other.to_string()),
    }
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

/// Whether this is one of `atomic::write`'s temporary files rather than a
/// memory file.
/// Why a path belongs to no scope, phrased as what to do about it.
///
/// [`scope::route`] returns only "nowhere", which is all it needs to decide;
/// the reader needs to know which of several different situations they are
/// in. Derived from the path rather than passed down, so a scope added later
/// gets an answer here or none — never another scope's answer.
fn unroutable_detail(rel: &str) -> Option<String> {
    if let Some((found, reserved)) = scope::miscased_reserved_dir(rel) {
        return Some(format!(
            "'{found}/' is not '{reserved}/' — rename it and run this again"
        ));
    }
    let head = rel.split('/').next()?;
    match head {
        scope::GLOBAL_DIR => Some("set RECALL_GLOBAL_KEY to sync this".into()),
        scope::MACHINE_DIR => Some("set RECALL_MACHINE_KEY to sync this".into()),
        _ => None,
    }
}

fn is_temp(rel: &str) -> bool {
    rel.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with(".recall-"))
}
