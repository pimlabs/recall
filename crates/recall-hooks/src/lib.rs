//! The client half of Recall: the hooks Claude Code runs, and everything
//! they need to do their job.
//!
//! Every module here runs *inside someone's editing session*, and that
//! shapes the design more than anything else. The push hook fires on every
//! Edit and Write, so it has to be cheap and silent when nothing concerns
//! it. The pull hook runs at session start, so it must not be able to leave
//! a half-written memory file behind for the session that is about to read
//! it. And the delete reconciliation in [`hooks`] can tombstone a project's
//! entire history if its baseline is misread, which is why [`state::load`]
//! distinguishes "no baseline" from "an empty baseline" at the type level.

mod atomic;

pub mod hookio;
pub mod hooks;
pub mod settings;
pub mod state;
pub mod syncclient;

#[cfg(test)]
mod testserver;
