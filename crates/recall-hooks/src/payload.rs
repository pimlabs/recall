//! What Claude Code feeds a hook on stdin.
//!
//! Parsing here is deliberately forgiving; the matching failure policy — and
//! the reasoning behind it — lives in [`crate::exit`].

use std::io::Read;

use serde::Deserialize;

/// The payload Claude Code sends after a tool runs.
///
/// Only the fields Recall acts on are declared; the real payload carries
/// more (`session_id`, `transcript_path`, `tool_response`, …) and is free to
/// grow, so unknown keys are ignored rather than rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PostToolUse {
    /// Always `"PostToolUse"` in practice; carried for completeness.
    #[serde(default)]
    pub hook_event_name: String,
    /// The tool that just ran — `Edit` or `Write`, given the matcher.
    #[serde(default)]
    pub tool_name: String,
    /// The arguments that tool was called with.
    #[serde(default)]
    pub tool_input: ToolInput,
}

/// The part of `tool_input` Recall reads. `file_path` is the only field
/// that decides anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ToolInput {
    /// The absolute path the tool wrote to. Empty for tools that don't write
    /// files at all, which callers read as "nothing to do".
    #[serde(default)]
    pub file_path: String,
}

/// Cap on how much stdin is read, so a runaway producer can't make the hook
/// allocate without bound.
const MAX_PAYLOAD: u64 = 8 << 20;

/// Reads a hook payload.
///
/// A malformed or empty payload yields a default value and **no error**: the
/// hook runs on every Edit and Write in the session, so anything
/// unrecognised should be quietly ignored rather than turned into noise the
/// user can't act on. Callers treat an empty `file_path` as "nothing to do".
pub fn parse_post_tool_use<R: Read>(reader: R) -> PostToolUse {
    let mut buf = Vec::new();
    if reader.take(MAX_PAYLOAD).read_to_end(&mut buf).is_err() || buf.is_empty() {
        return PostToolUse::default();
    }
    serde_json::from_slice(&buf).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> PostToolUse {
        parse_post_tool_use(s.as_bytes())
    }

    #[test]
    fn parses_a_real_payload() {
        let got = parse(
            r#"{"session_id":"abc","transcript_path":"/tmp/t.jsonl",
                "hook_event_name":"PostToolUse","tool_name":"Edit",
                "tool_input":{"file_path":"/home/u/.claude/memory/MEMORY.md",
                              "old_string":"a","new_string":"b"},
                "tool_response":{"success":true}}"#,
        );
        assert_eq!(got.hook_event_name, "PostToolUse");
        assert_eq!(got.tool_name, "Edit");
        assert_eq!(got.tool_input.file_path, "/home/u/.claude/memory/MEMORY.md");
    }

    /// Anything unrecognised is a silent no-op, never an error: this runs on
    /// every edit in the session.
    #[test]
    fn unrecognised_payloads_yield_an_empty_value_and_no_error() {
        for payload in [
            "",
            "   ",
            "not json at all",
            "{",
            "null",
            "[]",
            r#""a string""#,
            // tool_input present but the wrong shape.
            r#"{"tool_name":"Edit","tool_input":"oops"}"#,
            r#"{"tool_name":"Edit","tool_input":{"file_path":null}}"#,
        ] {
            assert_eq!(
                parse(payload),
                PostToolUse::default(),
                "payload {payload:?} should parse to an empty value"
            );
        }
    }

    /// A tool with no `file_path` (Bash, Read, …) is simply "nothing to do".
    #[test]
    fn a_payload_without_a_file_path_is_empty_not_an_error() {
        let got = parse(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#);
        assert_eq!(got.tool_name, "Bash");
        assert!(got.tool_input.file_path.is_empty());
    }

    #[test]
    fn oversized_input_is_truncated_rather_than_read_whole() {
        // Truncation makes the JSON invalid, which lands on the same silent
        // no-op path rather than allocating the whole stream.
        let huge = format!(
            r#"{{"tool_input":{{"file_path":"{}"}}}}"#,
            "x".repeat(9 << 20)
        );
        assert_eq!(parse(&huge), PostToolUse::default());
    }
}
