//! How a hook is allowed to fail.
//!
//! The failure policy matters as much as the logic. These run inside
//! someone's editing session: a push hook that errors noisily on every
//! unrelated edit, or a pull hook that stops a session from starting because
//! the server is down, is worse than one that does nothing. So the default
//! is [`OK`], and only the two cases a user can actually act on are loud.

/// Success, or a deliberate no-op — an edit to a file that isn't a memory
/// file, or a pull that couldn't reach the server.
pub const OK: i32 = 0;

/// A misconfiguration only the user can fix: no token, not a git
/// repository. Worth surfacing.
pub const CONFIG: i32 = 1;

/// The server rejected the request.
pub const SERVER: i32 = 2;
