//! Recall's server half: SQLite persistence, LLM-assisted merge, and the
//! HTTP API.
//!
//! The routes, status codes, JSON shapes and auth behavior are deliberately
//! identical to the Node implementation this replaces, because during a
//! migration a machine still on the old client and one already on this
//! binary talk to the same deployment.

#![deny(missing_docs)]

pub mod config;
pub mod merge;
pub mod server;
pub mod store;

pub use config::{Config, ConfigError};
pub use merge::Merger;
pub use server::Server;
pub use store::Store;

use time::OffsetDateTime;

/// A timestamp in the format every stored row and every API response uses:
/// JavaScript's `Date.toISOString()` — millisecond precision, `Z` suffix,
/// e.g. `2026-09-03T21:49:55.191Z`.
///
/// Rows written by the Node server are already in the database in this
/// shape, and clients and the admin page display them, so the format is
/// part of the compatibility surface rather than a style choice.
pub fn now() -> String {
    format_timestamp(OffsetDateTime::now_utc())
}

fn format_timestamp(at: OffsetDateTime) -> String {
    // `[subsecond digits:3]` is load-bearing: it must render actual
    // milliseconds, not three literal zeroes.
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    at.to_offset(time::UtcOffset::UTC)
        .format(&fmt)
        .expect("the timestamp format is a compile-time constant")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_match_javascripts_toisostring() {
        let t = now();
        assert_eq!(t.len(), 24, "got {t}");
        assert!(t.ends_with('Z'), "got {t}");
        assert_eq!(&t[10..11], "T", "got {t}");
        assert_eq!(&t[19..20], ".", "got {t}");
        assert!(
            t[20..23].chars().all(|c| c.is_ascii_digit()),
            "milliseconds must be digits, got {t}"
        );
    }

    /// The Go port had a bug where the layout rendered three literal zeroes
    /// instead of milliseconds. Any real sub-second value must survive.
    #[test]
    fn renders_real_milliseconds_not_literal_zeroes() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_788_472_195_191_000_000).unwrap();
        assert_eq!(format_timestamp(at), "2026-09-03T21:49:55.191Z");
    }
}
