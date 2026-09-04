//! `GET /admin/stats` — what the owner is storing, per project.
//!
//! Authenticated like `/sync`, and read-only: there is no admin *write*
//! surface, deliberately.

use serde::{Deserialize, Serialize};

/// One project's row in [`AdminStats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectStats {
    /// The key the project syncs under.
    pub project_key: String,
    /// Live files, tombstones excluded.
    pub file_count: i64,
    /// Tombstoned files, which still hold their last known content.
    pub deleted_count: i64,
    /// Every machine that has written to this project.
    pub sources: Vec<String>,
    /// The newest `updated_at` across the project's files.
    pub last_updated_at: String,
}

/// The aggregate line of [`AdminStats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminTotals {
    /// How many projects have ever synced.
    pub project_count: i64,
    /// Live files across every project.
    pub file_count: i64,
    /// Tombstones across every project.
    pub deleted_count: i64,
}

/// Body returned by `GET /admin/stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminStats {
    /// One row per project, newest activity first.
    pub projects: Vec<ProjectStats>,
    /// The same numbers, summed.
    pub totals: AdminTotals,
    /// The commit the running binary was built from.
    pub git_commit: String,
    /// When the database was last backed up.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_backup_at: String,
}
