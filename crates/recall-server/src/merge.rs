//! The semantic merge, which now lives in `recall-worker`.
//!
//! A server with a worker enrolled queues a stale push as a job instead of
//! merging it here (see `server/jobs.rs`). One without a worker still
//! merges inline, exactly as before the queue existed, and does it with the
//! worker's own merge code, built without the worker's HTTP client, so the
//! prompt and the flags that keep a merge near $0.01 exist once.

pub use recall_worker::merge::{prompt, Error, Merger, Status, SYSTEM_PROMPT};
