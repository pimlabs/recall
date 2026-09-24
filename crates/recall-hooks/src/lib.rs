//! The client half of Recall: the two hooks Claude Code runs, everything
//! they need to do their job, and the derivations that answer *where things
//! live*.
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
//!     scopes: recall_hooks::scope::scopes("acme/app".into(), None, None),
//!     source_env: "laptop".to_string(),
//!     client: Client::new("https://recall.example.com", "token")?,
//! };
//!
//! // At session start: make local memory match the server.
//! let pulled = pull(&ctx).await?;
//! eprintln!("{}", pulled.describe(ctx.project_key()));
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
//! [`PullOutcome`], [`is_memory_file`] — plus [`promote()`], which is no hook
//! at all but the one way a note gets *into* the global scope, and its own
//! [`PromoteOutcome`] and [`PromoteError`]. The modules below are the
//! supporting surface, kept separate because each is useful on its own —
//! `recall status` reads [`settings`] and [`state`] without ever pushing
//! anything.
//!
//! | Module | What it holds |
//! |---|---|
//! | [`client`] | the HTTP client for the Recall API |
//! | [`audit`] | this machine as a witness of the server's audit log: the checkpoints it saved, and checking them |
//! | [`device`] | this machine as an enrolled device: its key, and signing with it |
//! | [`payload`] | what Claude Code sends a hook on stdin |
//! | [`exit`] | how a hook is allowed to fail |
//! | [`path`] | whether a path is a memory file — the security boundary. Singular: it judges *one* path, where [`claude`] and [`project`] derive where paths come from |
//! | [`state`] | the baseline that makes deletes detectable |
//! | [`settings`] | the idempotent `.claude/settings.json` merge |
//! | [`claude`] | the paths Claude Code itself uses for auto-memory |
//! | [`project`] | the key Recall syncs a project under |
//! | [`config`] | the environment the client reads |
//! | [`scope`] | what is synced, and under which key |
//!
//! # Where things live
//!
//! The last four arrived here when `recall-paths` was folded in: nothing
//! outside this crate and the binary ever used them, and a crate boundary
//! that stops nothing is a published name for no reason. The boundary that
//! does the work is the one below — [`recall_wire`] is all `recall-server`
//! depends on, so the host half stays compile-time incapable of reaching
//! any of this.
//!
//! [`claude`] and [`project`] are deliberately different answers to "which
//! project is this", and are meant to disagree: one says where this machine
//! keeps the files, the other whose history it is. Conflating them was the
//! bug that made the shell version sync into the void.
//!
//! ## On the missing re-exports
//!
//! [`claude::Env`] and [`project::key`] are reached through their modules
//! on purpose. Flattened to the crate root they became `Env` and `key`,
//! which say nothing at a call site and collided with other types — the CLI
//! had already resorted to `use recall_paths::Env as ClaudeEnv`, which is
//! the codebase telling you the name was wrong. [`ClientConfig`] is
//! re-exported, because there is nothing ambiguous about it.

#![deny(missing_docs)]

mod atomic;
mod backfill;
mod context;
mod index;
mod promote;
mod pull;
mod push;

pub mod audit;
pub mod claude;
pub mod client;
pub mod config;
pub mod declared_env;
pub mod device;
pub mod exit;
pub mod home;
pub mod path;
pub mod payload;
pub mod project;
pub mod scope;
pub mod settings;
pub mod state;

pub use backfill::{backfill, Disposition, Entry, Outcome as BackfillOutcome};
pub use config::{ClientConfig, ConfigError};
pub use context::{Context, Error};
pub use index::is_linked as global_index_is_linked;
pub use index::machine_is_linked as machine_index_is_linked;
pub use path::{foreign_memory_slug, is_memory_file};
pub use promote::{promote, Error as PromoteError, PromoteOutcome, Target as PromoteTarget};
pub use pull::{pull, PullOutcome};
pub use push::{push, PushOutcome};
pub use scope::Scope;

#[cfg(test)]
mod testserver;

#[cfg(test)]
mod tests;
