//! The client half of Recall: the two hooks Claude Code runs, and
//! everything they need to do their job.
//!
//! Every module here runs *inside someone's editing session*, and that
//! shapes the design more than anything else. [`push`] fires on every Edit
//! and Write, so it has to be cheap and silent when nothing concerns it.
//! [`pull`] runs at session start, so it must not be able to leave a
//! half-written memory file behind for the session that is about to read it.
//! And the delete reconciliation inside [`push`] can tombstone a project's
//! entire history if its baseline is misread, which is why [`state::load`]
//! distinguishes "no baseline" from "an empty baseline" at the type level.
//!
//! # The whole client, in one example
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use std::path::PathBuf;
//! use recall_hooks::{client::Client, pull, push, Context};
//!
//! let ctx = Context {
//!     memory_dir: PathBuf::from("/home/me/.claude/projects/-home-me-app/memory"),
//!     state_file: PathBuf::from("/home/me/.claude/projects/-home-me-app/.recall-state.json"),
//!     project_key: "acme/app".to_string(),
//!     source_env: "laptop".to_string(),
//!     client: Client::new("https://recall.example.com", "token")?,
//! };
//!
//! // At session start: make local memory match the server.
//! let pulled = pull(&ctx).await?;
//! eprintln!("{}", pulled.describe(&ctx.project_key));
//!
//! // After an edit: send that file, and reconcile any deletes.
//! let pushed = push(&ctx, &ctx.memory_dir.join("MEMORY.md")).await?;
//! assert!(!pushed.skipped);
//! # Ok(())
//! # }
//! ```
//!
//! # Layout
//!
//! The two operations and the things they operate on are at the crate root:
//! [`push`], [`pull`], [`Context`], [`Error`], [`PushOutcome`],
//! [`PullOutcome`], [`is_memory_file`]. The modules below are the supporting
//! surface, kept separate because each is useful on its own — `recall
//! status` reads [`settings`] and [`state`] without ever pushing anything.
//!
//! | Module | What it holds |
//! |---|---|
//! | [`client`] | the HTTP client for the Recall API |
//! | [`payload`] | what Claude Code sends a hook on stdin |
//! | [`exit`] | how a hook is allowed to fail |
//! | [`path`] | whether a path is a memory file — the security boundary |
//! | [`state`] | the baseline that makes deletes detectable |
//! | [`settings`] | the idempotent `.claude/settings.json` merge |

#![deny(missing_docs)]

mod atomic;
mod context;
mod pull;
mod push;

pub mod client;
pub mod exit;
pub mod path;
pub mod payload;
pub mod settings;
pub mod state;

pub use context::{Context, Error};
pub use path::is_memory_file;
pub use pull::{pull, PullOutcome};
pub use push::{push, PushOutcome};

#[cfg(test)]
mod testserver;

#[cfg(test)]
mod tests;
