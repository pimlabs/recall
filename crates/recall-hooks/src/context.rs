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

    /// Rewrites the baseline to match what is on disk right now.
    ///
    /// Called at the end of both a push and a pull. A machine that only ever
    /// pulls still needs an accurate one, or its first local delete would go
    /// unnoticed.
    pub(crate) fn refresh_state(&self) -> Result<(), Error> {
        self.refresh_state_with(&[])
    }

    /// The same, recording `synced` — `(path, content_sha256)` pairs for
    /// files this run pulled or pushed — as each file's new base.
    ///
    /// Bases for files that are no longer on disk are dropped, so a file
    /// deleted and later recreated starts without one rather than naming a
    /// version it was never edited from.
    pub(crate) fn refresh_state_with(&self, synced: &[(String, String)]) -> Result<(), Error> {
        let files = state::list_memory_files(&self.memory_dir)?;
        let mut bases = state::load(&self.state_file)
            .ok()
            .flatten()
            .map(|s| s.bases)
            .unwrap_or_default();
        for (rel, hash) in synced {
            bases.insert(rel.clone(), hash.clone());
        }
        bases.retain(|rel, _| files.binary_search(rel).is_ok());
        state::save(&self.state_file, &files, &bases)?;
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
    /// Whether the server refused because it no longer knows the device
    /// this machine signs as: revoked, or swept away after sitting idle.
    /// The one refusal a hook can do something about, by enrolling again.
    pub fn device_gone(&self) -> bool {
        match self {
            Error::Push { source, .. }
            | Error::PushDelete { source, .. }
            | Error::Pull { source, .. } => source.device_gone(),
            _ => false,
        }
    }
}
