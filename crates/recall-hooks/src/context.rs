//! Where a hook is running, and what went wrong when it did.

use std::io;
use std::path::PathBuf;

use recall_paths::Scope;

use crate::client::Client;
use crate::state;

/// Everything the hooks need to know about where they're running.
///
/// Passed in rather than discovered inside, so the logic is testable without
/// a real home directory or a real git repository — and so path derivation
/// stays somebody else's problem (`recall_paths`).
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
    /// comes first — see [`recall_paths::scope`].
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
        self.scopes.iter().find(|s| s.is_global())
    }

    /// Rewrites the baseline to match what is on disk right now.
    ///
    /// Called at the end of both a push and a pull. A machine that only ever
    /// pulls still needs an accurate one, or its first local delete would go
    /// unnoticed.
    pub(crate) fn refresh_state(&self) -> Result<(), Error> {
        let files = state::list_memory_files(&self.memory_dir)?;
        state::save(&self.state_file, &files)?;
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
