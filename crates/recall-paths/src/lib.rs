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

pub mod claude;
pub mod config;
pub mod project;

pub use claude::{slug, Env};
pub use config::{Client, ConfigError};
pub use project::{key, key_from_remote, local_key};
