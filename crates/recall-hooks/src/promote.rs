//! Moving a note out of one project and into a scope that outlives it.
//!
//! The global scope shipped with the mechanism to *carry* a note into every
//! project but no way to put one there: a user had to read their memory
//! directory out of `recall status`, `mkdir global/` by hand, and then get
//! the push hook to fire on it. This is the way in, and it is shaped around
//! how such a note actually comes to exist — Claude writes something about
//! the *user* while working in one repository (it labels these `type: user`
//! in the file's own front matter), and the user then wants it everywhere.
//!
//! The machine scope arrived later and reproduced that gap exactly, which is
//! why the destination is a parameter now rather than a constant. A note
//! saying which JDK wins on this box is written the same way and wanted in
//! the same shape — just somewhere narrower.
//!
//! So this is a move, not a copy. A note left in both scopes would be pulled
//! twice into every future session of this project, and the two copies would
//! drift apart the first time either was edited.

use std::fs;
use std::io;
use std::path::Path;

use crate::scope::route;
use recall_wire::PushRequest;

use crate::atomic;
use crate::context::Context;
use crate::index;
use crate::path::relative_slash;
use crate::state;

/// The project's index, which is generated into rather than promoted out of.
/// Kept in step with `index`, which owns the name.
const INDEX_FILE: &str = "MEMORY.md";

/// What a promotion did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteOutcome {
    /// Where the note was, relative to the memory directory. This path is
    /// now tombstoned under the project key.
    pub from: String,
    /// Where it is now, relative to the memory directory — so it reads as
    /// `global/user.md`, the same way [`crate::PushOutcome`] names files.
    pub to: String,
    /// Whether this run only finished a move an earlier one had started.
    ///
    /// True when the global copy was already on disk with identical bytes,
    /// which is what an interrupted promotion leaves behind. Reported rather
    /// than hidden so a caller can say "already promoted; tidied up" instead
    /// of claiming to have done the whole thing.
    pub resumed: bool,
}

/// Where a note is being promoted to.
///
/// Only the scopes with a directory of their own: the project scope is where
/// notes come *from*, and promoting into it would be a demotion nobody has
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Follows the user into every project they sync.
    Global,
    /// Stays with the machine that declared [`crate::scope::machine_key`].
    Machine,
}

impl Target {
    /// The reserved directory this target owns.
    pub fn dir(self) -> &'static str {
        match self {
            Target::Global => crate::scope::GLOBAL_DIR,
            Target::Machine => crate::scope::MACHINE_DIR,
        }
    }

    /// The variable that turns this scope on, named in the refusal when it
    /// is off — a refusal that says only "not configured" leaves the reader
    /// to guess which of two variables it meant.
    pub fn variable(self) -> &'static str {
        match self {
            Target::Global => "RECALL_GLOBAL_KEY",
            Target::Machine => "RECALL_MACHINE_KEY",
        }
    }
}

/// Why a note could not be promoted.
///
/// Separate from [`crate::Error`] because every variant here is a refusal
/// decided before anything moves — whether this promotion makes sense at all
/// — while that type is about a push, a pull or the disk failing mid-flight.
/// Those failures still happen here, and arrive unchanged through
/// [`Error::Sync`] so a caller formats them exactly once.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The scope asked for is not configured here, so there is nowhere to
    /// promote to. A refusal rather than a silent no-op: the user asked for
    /// a note to go somewhere, and it would not.
    #[error(
        "the {scope} scope is not configured on this machine, so there is nowhere to \
         promote to (set {variable}, then run this again)"
    )]
    ScopeNotConfigured {
        /// The scope's directory name, as the user would type it.
        scope: &'static str,
        /// The variable that would turn it on.
        variable: &'static str,
    },
    /// The path is not a file inside this project's memory directory.
    #[error("{path} is not a memory file of this project")]
    NotAMemoryFile {
        /// The path as the caller gave it.
        path: String,
    },
    /// The note is already in the scope it was asked to go to.
    #[error("{path} is already in the {scope} scope")]
    AlreadyThere {
        /// The offending path, relative to the memory directory.
        path: String,
        /// The scope it is already in.
        scope: &'static str,
    },
    /// The note is in one reserved directory and was asked to go to another.
    ///
    /// Refused rather than implemented, and the reason is one line deeper
    /// than it looks: `MEMORY.md` lives at the memory root and belongs to the
    /// project scope, but the index push at the end of a promotion sends it
    /// under the *source* scope's key. That is correct only while the source
    /// is always the project. Moving `global/x.md` to `machine/` would file
    /// this project's index into the global scope's history, on a code path
    /// nothing else exercises. Move the file by hand and let the next push
    /// reconcile it.
    #[error(
        "{path} is in the {from} scope already; promote moves notes out of this project, \
         not between scopes — move the file yourself and the next push will follow it"
    )]
    CrossScope {
        /// The path, relative to the memory directory.
        path: String,
        /// The reserved directory it is currently in.
        from: String,
    },
    /// Nothing is at that path.
    #[error("{path} does not exist")]
    Missing {
        /// The path that was asked for, relative to the memory directory.
        path: String,
    },
    /// `MEMORY.md` is the project's index, not a note.
    ///
    /// Refused rather than merely discouraged because the move would appear
    /// to work and then quietly undo itself: `index` regenerates `MEMORY.md`
    /// at the end of every push and pull, so the project would get its index
    /// back a moment after this tombstoned it — while a copy of one repo's
    /// index sat in every other project the user opens.
    #[error("{INDEX_FILE} is this project's index, not a note that can be promoted")]
    IsTheIndex,
    /// A *different* note already holds that name in the global scope.
    ///
    /// Refused, not overwritten. The three candidate behaviours are not
    /// symmetric: overwriting destroys a note the user deliberately chose to
    /// carry into every project, silently and with no local copy left to
    /// recover from; auto-renaming to `user-2.md` keeps both but leaves two
    /// near-identical notes linked side by side from `MEMORY.md`, which is
    /// exactly what the model would then have to choose between; refusing
    /// costs the user one rename and loses nothing. Only the first is
    /// unrecoverable, so it is the one ruled out.
    ///
    /// The case that looks like a collision but is not — the same name
    /// holding *identical bytes* — is finished rather than refused: that is
    /// an interrupted promotion. See [`PromoteOutcome::resumed`].
    #[error(
        "{path} already exists and holds a different note; rename this one and promote it again"
    )]
    NameTaken {
        /// The occupied path, relative to the memory directory.
        path: String,
    },
    /// A push, a pull or the local disk failed, as it can in any hook.
    #[error(transparent)]
    Sync(#[from] crate::context::Error),
}

/// An I/O failure travels as the shared error rather than as a variant of
/// its own, so a failed write reads the same here as it does from a pull.
impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Sync(crate::context::Error::Io(err))
    }
}

/// Moves one of this project's memory files into the global scope, so it
/// follows the user into every other project.
///
/// The note is stored under the global key, tombstoned under the project
/// key, moved on disk into `global/`, and linked from `MEMORY.md` — without
/// that last step the file is on disk and Claude Code never reads it, which
/// is the whole failure mode the index exists to prevent.
///
/// It keeps only its file name: a project's own folders (`topics/auth.md` is
/// auth *in this repository*) mean nothing in a scope that has no project,
/// and the global scope is a small hand-curated set where a flat listing is
/// the readable one. Names that collide are the price, and
/// [`Error::NameTaken`] is where it is paid.
///
/// # Ordering
///
/// The invariant is that no failure may leave the note at neither path.
/// Three orderings follow from it:
///
/// 1. **Store globally before touching the disk.** The likely failure — the
///    server is down, the token has expired — then changes nothing at all:
///    the note is still a project note, still synced, and the user retries.
/// 2. **Write the new copy before removing the old one.** A crash in between
///    leaves the note in both places, which is visible and is repaired by
///    running this again; the other order has a window in which it is in
///    neither, and a crash there loses it for good.
/// 3. **Refresh the baseline last.** Until it is rewritten the baseline still
///    lists the old path, so a tombstone that failed to send is re-sent by
///    the next push hook's delete reconciliation instead of being forgotten.
///
/// The index is rewritten on disk as soon as the note moves — the project's
/// link to its old path is dead from that moment — and, because `MEMORY.md`
/// is itself a synced project file, that removal is pushed rather than left
/// local; otherwise the stale line comes back with the next pull, here and on
/// every other machine. That push comes after the tombstone, being the one
/// step whose failure costs nothing worse than a dead link.
pub async fn promote(ctx: &Context, file: &Path, target: Target) -> Result<PromoteOutcome, Error> {
    let Some(dest) = ctx.reserved(target.dir()) else {
        return Err(Error::ScopeNotConfigured {
            scope: target.dir(),
            variable: target.variable(),
        });
    };

    // Lexical, like every other containment check here: the memory directory
    // may hold symlinks, and resolving them would let one inside it decide
    // what counts as being inside it.
    let Some(rel) = relative_slash(&ctx.memory_dir, file) else {
        return Err(Error::NotAMemoryFile {
            path: file.display().to_string(),
        });
    };
    let Some((scope, source_path)) = route(&ctx.scopes, &rel) else {
        // Several paths route nowhere now — a reserved directory itself, one
        // whose scope is switched off, one whose name is a miscased reserved
        // word. None of them is a note this command can move, and the
        // distinction is not one the caller can act on differently.
        return Err(Error::NotAMemoryFile { path: rel });
    };
    if let Some(from) = &scope.prefix {
        return Err(if from == target.dir() {
            Error::AlreadyThere {
                path: rel,
                scope: target.dir(),
            }
        } else {
            Error::CrossScope {
                path: rel,
                from: from.clone(),
            }
        });
    }
    if rel == INDEX_FILE {
        return Err(Error::IsTheIndex);
    }

    let source_abs = state::join_relative(&ctx.memory_dir, &rel);
    match fs::metadata(&source_abs) {
        Ok(meta) if meta.is_dir() => return Err(Error::NotAMemoryFile { path: rel }),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::Missing { path: rel }),
        Err(e) => return Err(e.into()),
    }

    // Read as bytes and checked here, before anything moves: `content` is a
    // JSON string on the wire, so a note that is not UTF-8 cannot be stored
    // globally at all. Discovering that after the tombstone had gone out
    // would take it out of the project scope and put it in no other.
    let content = fs::read(&source_abs)?;
    let content = String::from_utf8(content)
        .map_err(|_| crate::context::Error::NotUtf8 { path: rel.clone() })?;

    let name = rel.rsplit('/').next().unwrap_or(rel.as_str()).to_string();
    let dest_rel = dest.local_path(&name);
    let dest_abs = state::join_relative(&ctx.memory_dir, &dest_rel);

    let resumed = match fs::read(&dest_abs) {
        Ok(existing) => {
            if existing.as_slice() != content.as_bytes() {
                return Err(Error::NameTaken { path: dest_rel });
            }
            // Same name, same bytes: a promotion that died between writing
            // the copy and removing the original. Finishing it is what was
            // asked for the first time, and re-sending the content costs one
            // idempotent push in exchange for working wherever that run
            // stopped.
            true
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };

    let stored = PushRequest {
        project_key: dest.key.clone(),
        file_path: name,
        content: Some(content.clone()),
        source_env: ctx.source_env.clone(),
        deleted: false,
    };
    ctx.client
        .push(&stored)
        .await
        .map_err(|source| crate::context::Error::Push {
            path: dest_rel.clone(),
            source,
        })?;

    atomic::write(&dest_abs, ".recall-", ".tmp", content.as_bytes())?;
    fs::remove_file(&source_abs)?;

    // Local, so it cannot fail on the network the tombstone still has to
    // cross: whatever happens below, the note is reachable from MEMORY.md.
    // `Some(&rel)` additionally drops the project's own link to the path just
    // emptied, which is the one index line a promotion invalidates.
    let dropped_dead_link = index::refresh_forgetting(&ctx.memory_dir, Some(&rel))?;

    let tombstone = PushRequest {
        project_key: scope.key.clone(),
        file_path: source_path,
        deleted: true,
        source_env: ctx.source_env.clone(),
        ..Default::default()
    };
    ctx.client
        .push(&tombstone)
        .await
        .map_err(|source| crate::context::Error::PushDelete {
            path: rel.clone(),
            source,
        })?;

    // Only when a link was actually removed. The global links in the same
    // file changed too, and are not owed a push: every machine regenerates
    // those for itself after a pull.
    if dropped_dead_link {
        push_index(ctx, scope).await?;
    }

    ctx.refresh_state()?;

    Ok(PromoteOutcome {
        from: rel,
        to: dest_rel,
        resumed,
    })
}

/// Sends the rewritten `MEMORY.md` under the project's key.
///
/// Only the link removal makes this necessary: a line taken away is content,
/// and content taken away only on this disk is put back by the next pull —
/// here, and on every other machine, which never saw the removal at all.
async fn push_index(ctx: &Context, scope: &crate::scope::Scope) -> Result<(), Error> {
    // `scope` is the note's source, and this is deliberately not it. MEMORY.md
    // sits at the memory root, which belongs to the project scope, so that is
    // the key it has to go under. The two are the same today only because the
    // source is always the project — an invariant the type system does not
    // hold, and which a future cross-scope promotion would break silently.
    debug_assert!(
        scope.prefix.is_none(),
        "the index is pushed under the project key; the source was {:?}",
        scope.prefix
    );
    let body = fs::read_to_string(ctx.memory_dir.join(INDEX_FILE))?;
    let req = PushRequest {
        project_key: ctx.project_key().to_string(),
        file_path: INDEX_FILE.to_string(),
        content: Some(body),
        source_env: ctx.source_env.clone(),
        deleted: false,
    };
    ctx.client
        .push(&req)
        .await
        .map_err(|source| crate::context::Error::Push {
            path: INDEX_FILE.to_string(),
            source,
        })?;
    Ok(())
}
