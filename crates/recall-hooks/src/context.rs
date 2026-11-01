//! Where a hook is running, and what went wrong when it did.

use std::io;
use std::path::PathBuf;

use crate::scope::Scope;

use crate::client::Client;
use crate::state;

/// Everything the hooks need to know about where they're running.
///
/// Passed in rather than discovered inside, so the logic is testable without
/// a real home directory or a real git repository — and so path derivation
/// stays out of the hooks themselves. [`crate::claude`], [`crate::project`]
/// and [`crate::scope`] work it out; a hook is only ever handed the answer.
#[derive(Debug)]
pub struct Context {
    /// The directory Claude Code keeps this project's auto-memory in, on
    /// this machine.
    pub memory_dir: PathBuf,
    /// Recall's own baseline of what was last synced, used to notice
    /// deletes. Deliberately beside the memory directory, never inside it.
    pub state_file: PathBuf,
    /// What this machine syncs, and under which keys.
    ///
    /// Always at least the project scope. A global scope, when configured,
    /// comes first — see [`crate::scope`].
    pub scopes: Vec<Scope>,
    /// The label writes from this machine are stamped with.
    pub source_env: String,
    /// The configured server connection.
    pub client: Client,
}

impl Context {
    /// The key of the scope that owns the memory directory itself — the
    /// repository. Used for messages, where "which project is this" is the
    /// question a person is actually asking.
    pub fn project_key(&self) -> &str {
        self.scopes
            .iter()
            .find(|s| s.prefix.is_none())
            .map(|s| s.key.as_str())
            .unwrap_or_default()
    }

    /// The global scope, when one is configured.
    pub fn global(&self) -> Option<&Scope> {
        self.reserved(crate::scope::GLOBAL_DIR)
    }

    /// The machine scope, when one is configured.
    pub fn machine(&self) -> Option<&Scope> {
        self.reserved(crate::scope::MACHINE_DIR)
    }

    /// The scope owning a reserved directory, when it is switched on.
    ///
    /// One lookup behind both of the above, so "is this scope configured" is
    /// answered the same way whichever scope is asking.
    pub fn reserved(&self, dir: &str) -> Option<&Scope> {
        self.scopes
            .iter()
            .find(|s| s.prefix.as_deref() == Some(dir))
    }

    /// Whether any reserved scope is switched on.
    ///
    /// The question the index gate has to ask, and the reason it is spelled
    /// out here rather than at each call site: `MEMORY.md` is maintained for
    /// every reserved directory at once, so asking about one of them is only
    /// ever right while there is only one. Asking about the global scope was
    /// correct until the machine scope arrived, and then quietly stopped
    /// being — a machine-only setup synced its files and indexed none of
    /// them.
    pub fn has_reserved_scope(&self) -> bool {
        self.scopes.iter().any(|s| s.prefix.is_some())
    }

    /// Rewrites the baseline to match what is on disk right now, every file
    /// included: what `recall backfill` leaves, having dealt with each.
    pub(crate) fn refresh_state(&self) -> Result<(), Error> {
        self.rewrite_state(&[], true)
    }

    /// The same, recording `synced` — `(path, content_sha256)` pairs for
    /// files this run pulled or pushed — as each file's new base.
    ///
    /// Called at the end of both a push and a pull. A machine that only ever
    /// pulls still needs an accurate one, or its first local delete would go
    /// unnoticed.
    ///
    /// Bases for files that are no longer on disk are dropped, so a file
    /// deleted and later recreated starts without one rather than naming a
    /// version it was never edited from.
    ///
    /// A new file this run did not send stays off the list. It is another
    /// run's, still under way or refused, and a listed file with no base
    /// reads as unchanged, so listing it here would be the last anyone
    /// heard of it. A first run has nothing to tell those from and lists
    /// everything, leaving what was already there to `recall backfill`.
    pub(crate) fn refresh_state_with(&self, synced: &[(String, String)]) -> Result<(), Error> {
        self.rewrite_state(synced, false)
    }

    fn rewrite_state(&self, synced: &[(String, String)], list_all: bool) -> Result<(), Error> {
        let files = state::list_memory_files(&self.memory_dir)?;
        let prev = state::load(&self.state_file).ok().flatten();
        let (list_all, prev) = (list_all || prev.is_none(), prev.unwrap_or_default());
        let known = prev.files;
        let (mut bases, mut disk) = (prev.bases, prev.disk);
        // Whatever lists everything has dealt with everything, and a file
        // it held back must not go out at the next sweep regardless.
        if list_all {
            disk.retain(|_, fingerprint| fingerprint != state::NEVER_SENT);
        }
        for (rel, hash) in synced {
            bases.insert(rel.clone(), hash.clone());
            // As it stands now, after the write or the send and after any
            // index rewrite, so the next run compares against exactly this.
            let path = state::join_relative(&self.memory_dir, rel);
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    disk.insert(rel.clone(), state::fingerprint(rel, &content));
                }
                Err(_) => {
                    disk.remove(rel);
                }
            }
        }
        bases.retain(|rel, _| files.binary_search(rel).is_ok());
        disk.retain(|rel, _| files.binary_search(rel).is_ok());
        let files: Vec<String> = files
            .into_iter()
            .filter(|rel| list_all || known.contains(rel) || bases.contains_key(rel))
            .collect();
        state::save(&self.state_file, &files, &bases, &disk)?;
        Ok(())
    }

    /// What a run stopped partway leaves behind: the new base of each file
    /// it did send, and each delete it did send off the list, base and all.
    /// Nothing else moves, so the next run finds whatever this one could
    /// not send, and a file it did send is not sent from its old base, which
    /// the server would take for a concurrent edit and merge.
    ///
    /// `refused` is the file the run was for, when that is not among `sent`.
    /// Any run but this one could miss it. With no baseline yet, this writes
    /// the first, without it: a first run sweeps nothing, and the next run's
    /// baseline would list it as though it had gone. With a baseline listing
    /// it and no base, which is how a first run leaves what was already on
    /// disk, it gets [`state::NEVER_SENT`] for a fingerprint: a listed file
    /// with no base reads as unchanged, edited or not, and taking it off the
    /// list instead would lose its delete, should it be removed next.
    pub(crate) fn keep_what_was_sent(
        &self,
        sent: &[(String, String)],
        deleted: &[String],
        refused: Option<&str>,
    ) -> Result<(), Error> {
        let refused = refused.filter(|rel| !sent.iter().any(|(s, _)| s == rel));
        let mut baseline = match state::load(&self.state_file)? {
            Some(mut baseline) => {
                if let Some(rel) = refused.filter(|rel| {
                    !baseline.bases.contains_key(*rel) && !baseline.disk.contains_key(*rel)
                }) {
                    baseline
                        .disk
                        .insert(rel.to_string(), state::NEVER_SENT.to_string());
                }
                baseline
            }
            None => state::State {
                files: state::list_memory_files(&self.memory_dir)?
                    .into_iter()
                    .filter(|rel| Some(rel.as_str()) != refused)
                    .collect(),
                ..Default::default()
            },
        };
        baseline.files.retain(|rel| !deleted.contains(rel));
        for rel in deleted {
            baseline.bases.remove(rel);
            baseline.disk.remove(rel);
        }
        for (rel, hash) in sent {
            baseline.bases.insert(rel.clone(), hash.clone());
            if let Ok(content) =
                std::fs::read_to_string(state::join_relative(&self.memory_dir, rel))
            {
                baseline
                    .disk
                    .insert(rel.clone(), state::fingerprint(rel, &content));
            }
            baseline.files.push(rel.clone());
        }
        baseline.files.sort();
        baseline.files.dedup();
        state::save(
            &self.state_file,
            &baseline.files,
            &baseline.bases,
            &baseline.disk,
        )?;
        Ok(())
    }
}

/// Why a push or a pull could not complete.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server refused, or could not be reached, while sending a file.
    #[error("pushing {path}: {source}")]
    Push {
        /// The memory file being sent.
        path: String,
        /// The underlying transport or status error.
        #[source]
        source: crate::client::Error,
    },
    /// The same, for a delete.
    #[error("pushing delete of {path}: {source}")]
    PushDelete {
        /// The memory file being tombstoned.
        path: String,
        /// The underlying transport or status error.
        #[source]
        source: crate::client::Error,
    },
    /// The server refused, or could not be reached, while fetching.
    #[error("pulling {project_key}: {source}")]
    Pull {
        /// The project being fetched.
        project_key: String,
        /// The underlying transport or status error.
        #[source]
        source: crate::client::Error,
    },
    /// A memory file that isn't valid UTF-8 can't cross the wire: `content`
    /// is a JSON string. Refusing is louder than the alternative, but the
    /// alternative is silent corruption — Go's `json.Marshal` replaces
    /// invalid bytes with U+FFFD and pushes the result as if it were the
    /// file.
    #[error("{path} is not valid UTF-8, so it cannot be synced as a memory file")]
    NotUtf8 {
        /// The offending memory file.
        path: String,
    },
    /// Reading or writing the local memory directory failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Error {
    /// The server's refusal, when the server is what failed: what a hook
    /// reads to tell a device the server no longer accepts from any other
    /// failure.
    pub fn server_error(&self) -> Option<&crate::client::Error> {
        match self {
            Error::Push { source, .. }
            | Error::PushDelete { source, .. }
            | Error::Pull { source, .. } => Some(source),
            _ => None,
        }
    }

    /// Whether the server's rate limit is what refused: the one answer that
    /// means "later", and that a hook waits out.
    pub(crate) fn rate_limited(&self) -> bool {
        self.server_error().is_some_and(|e| e.status() == Some(429))
    }

    /// Whether the server refused because it no longer accepts the device
    /// this machine signs as: revoked, or swept away after sitting idle.
    /// See [`crate::client::Error::device_gone`] for why a hook asks which.
    pub fn device_gone(&self) -> bool {
        self.server_error()
            .is_some_and(crate::client::Error::device_gone)
    }
}
