//! `recall-worker`: the process that merges, so the one facing the internet
//! does not have to.
//!
//! It is an enrolled device with the `worker` scope, running beside
//! `recall-server` (in the same compose file, as a second service) with no
//! inbound port. It long-polls the server's merge queue, reconciles each
//! job's two versions with the local `claude` CLI, and posts the result
//! back. The `claude` login lives on the worker's own volume, so a
//! compromise of the API process no longer reaches it. See
//! `docs/design/part5-plan.md`, "The worker", and the "Jobs" section of
//! `docs/reference/api.md`.
//!
//! No Anthropic API key appears anywhere here, as anywhere in Recall: the
//! merge rides whatever account the CLI on this machine is logged in to.
//!
//! The crate has two halves. [`merge`] is the merge itself, the prompt and
//! the flags that keep it cheap; `recall-server` depends on it for the
//! inline merge a deployment without a worker still runs. Everything else
//! is behind the `client` feature (on by default): enrolling, signing
//! requests, and the job loop, with the HTTP client they need. The server
//! turns it off.

#![deny(missing_docs)]

pub mod merge;

#[cfg(feature = "client")]
pub mod api;
#[cfg(feature = "client")]
pub mod config;
#[cfg(feature = "client")]
pub mod identity;
#[cfg(feature = "client")]
pub mod worker;

pub use merge::Merger;

use time::OffsetDateTime;

/// A timestamp in the format every Recall API answer uses: JavaScript's
/// `Date.toISOString()`, millisecond precision with a `Z`, such as
/// `2026-09-03T21:49:55.191Z`. The same function as `recall_server::now`,
/// kept here so the merge does not depend on the server.
pub fn now() -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    OffsetDateTime::now_utc()
        .format(&fmt)
        .expect("the timestamp format is a compile-time constant")
}

/// What `recall-worker` sends as its `User-Agent`, and so what `/health`
/// shows as the worker's agent: `recall-worker/0.4.2 (linux-x86_64)`.
pub fn user_agent() -> String {
    format!(
        "recall-worker/{} ({}-{})",
        recall_wire::discovery::version(),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_match_javascripts_toisostring() {
        let t = now();
        assert_eq!(t.len(), 24, "got {t}");
        assert!(t.ends_with('Z') && &t[10..11] == "T" && &t[19..20] == ".");
    }

    #[test]
    fn the_agent_names_the_worker() {
        assert!(
            user_agent().starts_with("recall-worker/"),
            "{}",
            user_agent()
        );
    }
}
