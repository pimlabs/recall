//! `GET /health` — deliberately unauthenticated, so uptime tooling can poll
//! it without holding the token.
//!
//! The `merge` object exists for one specific reason: every merge failure
//! degrades to last-write-wins rather than rejecting the sync, which is the
//! right behavior and also means a broken merge step is otherwise invisible.
//! These fields are how that degraded state becomes observable from outside.

use serde::{Deserialize, Serialize};

/// Whether the server can actually perform a semantic merge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeCliStatus {
    /// When the check last ran, in the crate's frozen timestamp format.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checked_at: String,
    /// Whether the `claude` binary was found and runnable. [`None`] means
    /// the check has not run yet.
    pub available: Option<bool>,
    /// Whether that binary has a usable login. [`None`] means unknown.
    pub logged_in: Option<bool>,
    /// Why the check failed, when it did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// The most recent failed merge attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeError {
    /// The failure, already truncated to a sane length for display.
    pub message: String,
    /// When it happened, in the crate's frozen timestamp format.
    pub at: String,
}

/// The `merge` object inside [`Health`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MergeStatus {
    /// Whether merging is turned on at all (`RECALL_MERGE_ENABLED`).
    pub enabled: bool,
    /// Whether the local `claude` CLI can actually serve a merge.
    pub claude_cli: ClaudeCliStatus,
    /// When a merge last succeeded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_merge_at: String,
    /// The last failure, if there has been one since startup.
    pub last_merge_error: Option<MergeError>,
    /// The worker that merges queued jobs. Omitted while none is enrolled,
    /// which is also when merges still run inside the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerStatus>,
    /// The merge queue. Omitted while no worker is enrolled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueStatus>,
}

/// The `worker` object inside [`MergeStatus`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkerStatus {
    /// When a worker last asked for a job, since this server started.
    pub last_claim_at: Option<String>,
    /// Its `User-Agent`, such as `recall-worker/0.4.2 (linux-x86_64)`.
    pub agent: String,
}

/// The `queue` object inside [`MergeStatus`]: what is waiting, so a worker
/// that stopped is noticed rather than silently leaving merges undone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueStatus {
    /// Jobs waiting for a worker.
    pub queued: u64,
    /// Jobs a worker holds now.
    pub leased: u64,
    /// Jobs out of attempts, kept until retried.
    pub failed: u64,
    /// When the oldest waiting job was queued; `null` when none waits.
    pub oldest_queued_at: Option<String>,
}

/// Body returned by `GET /health`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// `"ok"` whenever the server can answer at all.
    pub status: String,
    /// The commit the running binary was built from, so a deploy can be
    /// confirmed from outside.
    pub git_commit: String,
    /// When this process started, in the crate's frozen timestamp format.
    pub started_at: String,
    /// When any project last synced.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_sync_at: String,
    /// When the database was last backed up.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_backup_at: String,
    /// When a copy last reached somewhere the loss of this machine does not
    /// reach, per the stamp `deploy/backup-offbox.sh` leaves behind.
    ///
    /// Reported separately from [`Health::last_backup_at`] because they
    /// protect against different things, and only one of them survives the
    /// disk. Empty when nothing has ever written the stamp, which is also
    /// what "no off-box backup is configured" looks like — the two are
    /// indistinguishable from here, and neither is an error.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_offbox_at: String,
    /// Whether semantic merge is working, or silently degraded.
    pub merge: MergeStatus,
}
