//! Where things live, and what a project is called.
//!
//! Three derivations that every other crate depends on and none of them
//! should re-implement:
//!
//! - [`claude`] — the paths Claude Code itself uses for auto-memory. Its
//!   rules are someone else's implementation detail, tracked here so there
//!   is one place to fix when the CLI changes.
//! - [`project`] — the key Recall syncs a project under. Derived from the
//!   git remote so two machines that have never met agree on it.
//! - [`config`] — the environment both halves of the binary read.
//!
//! The first two are deliberately different derivations of "which project is
//! this", and are meant to disagree: one answers "where does this machine
//! keep the files", the other "whose history is this". Conflating them was
//! the bug that made the shell version sync into the void.
//!
//! # On the missing re-exports
//!
//! [`claude::Env`] and [`project::key`] are reached through their modules on
//! purpose. Flattened to the crate root they became `Env` and `key`, which
//! say nothing at a call site and collided with other crates' types — the
//! CLI had already resorted to `use recall_paths::Env as ClaudeEnv`, which
//! is the codebase telling you the name was wrong. `claude::Env` and
//! `project::key` read correctly wherever they appear.
//!
//! [`ClientConfig`] is re-exported, because there is nothing ambiguous about
//! it.

#![deny(missing_docs)]

pub mod claude;
pub mod config;
pub mod project;

pub use config::{ClientConfig, ConfigError};
