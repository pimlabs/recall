//! Editing a project's own `.claude/settings.json` — the file `recall init`
//! writes and the user then commits.
//!
//! Hook config for a synced project lives in *that project's* settings, never
//! user-level, which is exactly what makes Recall work from a fresh cloud
//! session that has never seen this machine's home directory.
//!
//! Because that file is committed and reviewed, edits are surgical. The
//! workspace pins `serde_json` with `preserve_order`, so a
//! [`serde_json::Value`] map keeps insertion order through a decode/encode
//! round-trip; without it, re-encoding would silently re-sort every key in
//! the user's file and turn a two-line change into a whole-file diff. The
//! `preserves_key_order` test is there to fail loudly if that feature ever
//! gets dropped.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::atomic;

/// Matched against the *tool name*, not the file path — Claude Code offers
/// no path matching, which is precisely why this catches topic files Claude
/// names on the fly. See `docs/phase-0-findings.md` §4.
pub const PUSH_MATCHER: &str = "Edit|Write";
/// Resolved through `PATH`, so a project commits only this reference and
/// never a copy of the implementation.
pub const PUSH_COMMAND: &str = "recall push";
/// Likewise for the session-start pull.
pub const PULL_COMMAND: &str = "recall pull";

const POST_TOOL_USE: &str = "PostToolUse";
const SESSION_START: &str = "SessionStart";

/// Why the settings file could not be wired.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file exists but isn't JSON. Refusing is the point: this is a file
    /// the user hand-edits and commits, and rewriting it from scratch would
    /// throw away whatever they meant to keep.
    #[error("settings file exists but is not valid JSON")]
    InvalidJson,
    /// Reading or writing the settings file failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Adds Recall's hooks to the settings document in `src`, returning the new
/// document and whether anything actually changed.
///
/// Idempotent, and additive: an existing `Edit|Write` matcher belonging to
/// some other tool gets Recall's hook appended to it rather than being
/// replaced, and unrelated keys are left untouched.
///
/// The `changed` flag exists so callers can say "already wired" instead of
/// implying work happened.
pub fn wire(src: &[u8]) -> Result<(Vec<u8>, bool), Error> {
    let mut doc: Value = if src.iter().all(u8::is_ascii_whitespace) {
        Value::Object(Map::new())
    } else {
        serde_json::from_slice(src).map_err(|_| Error::InvalidJson)?
    };
    // A settings file whose root isn't an object is as unusable as one that
    // doesn't parse, and overwriting it would destroy whatever is there.
    if !doc.is_object() {
        return Err(Error::InvalidJson);
    }

    let push_added = add_matcher_hook(&mut doc, POST_TOOL_USE, PUSH_MATCHER, PUSH_COMMAND);
    let pull_added = add_session_start_hook(&mut doc, PULL_COMMAND);

    if !push_added && !pull_added {
        // Byte-identical, not merely equivalent: a no-op run must not
        // reformat a file the user has already committed.
        return Ok((src.to_vec(), false));
    }

    let mut out = serde_json::to_vec_pretty(&doc).map_err(|e| Error::Io(io::Error::other(e)))?;
    out.push(b'\n');
    Ok((out, true))
}

/// Reports whether a settings document already references Recall's push
/// hook — what `recall status` uses to answer "is this project opted in".
pub fn is_wired(src: &[u8]) -> bool {
    let Ok(doc) = serde_json::from_slice::<Value>(src) else {
        return false;
    };
    event_entries(&doc, POST_TOOL_USE).is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| entry_has_command(entry, PUSH_COMMAND))
    })
}

/// Applies [`wire`] to a `settings.json` on disk, creating it and its parent
/// directory if absent.
///
/// The write is atomic so an interrupted run can't leave the user with a
/// truncated settings file in their repository.
pub fn wire_file(path: &Path) -> Result<bool, Error> {
    let src = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };

    let (out, changed) = wire(&src)?;
    if !changed {
        return Ok(false);
    }
    atomic::write(path, ".settings-", ".json", &out)?;
    Ok(true)
}

/// Handles the `PostToolUse` shape, where entries are keyed by a matcher and
/// each holds its own list of hooks.
fn add_matcher_hook(doc: &mut Value, event: &str, matcher: &str, command: &str) -> bool {
    let entries = event_entries_mut(doc, event);

    for entry in entries.iter_mut() {
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        if entry.get("matcher").and_then(Value::as_str) != Some(matcher) {
            continue;
        }
        // The matcher is already here, most likely another tool's. Append
        // alongside rather than replacing, unless we're already in the list.
        let list = entry
            .entry("hooks")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !list.is_array() {
            *list = Value::Array(Vec::new());
        }
        let list = list.as_array_mut().expect("just ensured it is an array");
        if list.iter().any(|h| has_command(h, command)) {
            return false;
        }
        list.push(hook_entry(command));
        return true;
    }

    entries.push(json!({ "matcher": matcher, "hooks": [hook_entry(command)] }));
    true
}

/// Handles the `SessionStart` shape, which has no matcher — entries are just
/// groups of hooks.
fn add_session_start_hook(doc: &mut Value, command: &str) -> bool {
    let entries = event_entries_mut(doc, SESSION_START);
    if entries.iter().any(|e| entry_has_command(e, command)) {
        return false;
    }
    entries.push(json!({ "hooks": [hook_entry(command)] }));
    true
}

fn hook_entry(command: &str) -> Value {
    json!({ "type": "command", "command": command })
}

fn has_command(hook: &Value, command: &str) -> bool {
    hook.get("command").and_then(Value::as_str) == Some(command)
}

fn entry_has_command(entry: &Value, command: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|h| has_command(h, command)))
}

fn event_entries<'a>(doc: &'a Value, event: &str) -> Option<&'a Vec<Value>> {
    doc.get("hooks")?.get(event)?.as_array()
}

/// Walks down to `hooks.<event>`, creating the objects and array on the way
/// if they're missing. New keys land at the end, so existing ones keep their
/// position in the user's file.
fn event_entries_mut<'a>(doc: &'a mut Value, event: &str) -> &'a mut Vec<Value> {
    let root = doc
        .as_object_mut()
        .expect("caller checked the root is an object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks = hooks.as_object_mut().expect("just ensured it is an object");
    let entries = hooks
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entries.is_array() {
        *entries = Value::Array(Vec::new());
    }
    entries.as_array_mut().expect("just ensured it is an array")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts every `"command": <cmd>` anywhere in the document, so a
    /// duplicate can't hide in a shape the test didn't anticipate.
    fn count_command(doc: &[u8]) -> impl Fn(&str) -> usize {
        let parsed: Value = serde_json::from_slice(doc).expect("valid JSON");
        move |command| {
            fn walk(v: &Value, command: &str, n: &mut usize) {
                match v {
                    Value::Object(map) => {
                        if map.get("command").and_then(Value::as_str) == Some(command) {
                            *n += 1;
                        }
                        for value in map.values() {
                            walk(value, command, n);
                        }
                    }
                    Value::Array(items) => {
                        for item in items {
                            walk(item, command, n);
                        }
                    }
                    _ => {}
                }
            }
            let mut n = 0;
            walk(&parsed, command, &mut n);
            n
        }
    }

    #[test]
    fn wires_from_nothing() {
        let (out, changed) = wire(b"").unwrap();
        assert!(changed, "starting from an empty document is a change");

        let count = count_command(&out);
        assert_eq!(count(PUSH_COMMAND), 1);
        assert_eq!(count(PULL_COMMAND), 1);

        let doc: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            doc["hooks"][POST_TOOL_USE][0]["matcher"].as_str(),
            Some(PUSH_MATCHER)
        );
        assert_eq!(
            doc["hooks"][POST_TOOL_USE][0]["hooks"][0]["type"].as_str(),
            Some("command")
        );
        assert_eq!(
            doc["hooks"][SESSION_START][0]["hooks"][0]["command"].as_str(),
            Some(PULL_COMMAND)
        );
    }

    #[test]
    fn is_idempotent() {
        let (first, _) = wire(b"").unwrap();
        let (second, changed) = wire(&first).unwrap();
        assert!(
            !changed,
            "second run reported a change; wiring must be idempotent"
        );
        assert_eq!(first, second, "a no-op run must not rewrite the file");

        let count = count_command(&second);
        assert_eq!(count(PUSH_COMMAND), 1, "push hook duplicated");
        assert_eq!(count(PULL_COMMAND), 1, "pull hook duplicated");
    }

    /// The file belongs to the user's project and may already carry another
    /// tool's hooks. Recall adds itself alongside, never replaces.
    #[test]
    fn preserves_other_tools_and_unrelated_keys() {
        let src = br#"{
  "permissions": { "allow": ["Bash(npm test)"] },
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "prettier --write" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "audit-log" }] }
    ],
    "SessionStart": [ { "hooks": [{ "type": "command", "command": "some-other-tool" }] } ]
  }
}"#;

        let (out, changed) = wire(src).unwrap();
        assert!(changed);

        let doc: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            doc["permissions"]["allow"][0].as_str(),
            Some("Bash(npm test)"),
            "unrelated key lost"
        );

        let count = count_command(&out);
        for cmd in ["prettier --write", "audit-log", "some-other-tool"] {
            assert_eq!(count(cmd), 1, "pre-existing hook {cmd:?} was disturbed");
        }
        assert_eq!(count(PUSH_COMMAND), 1);
        assert_eq!(count(PULL_COMMAND), 1);

        // Appended to the existing Edit|Write entry rather than creating a
        // second entry carrying the same matcher.
        let entries = doc["hooks"][POST_TOOL_USE].as_array().unwrap();
        let matching = entries
            .iter()
            .filter(|e| e["matcher"].as_str() == Some(PUSH_MATCHER))
            .count();
        assert_eq!(matching, 1, "duplicate Edit|Write matcher entries");
        assert_eq!(
            entries[0]["hooks"].as_array().unwrap().len(),
            2,
            "Recall's hook should sit next to prettier's"
        );
    }

    /// A map without insertion order would silently re-sort a file the user
    /// commits and reviews. `serde_json/preserve_order` is what stops that,
    /// and this test fails if the feature is ever dropped.
    #[test]
    fn preserves_key_order() {
        let src = br#"{"zebra": 1, "alpha": 2, "middle": 3}"#;
        let (out, _) = wire(src).unwrap();
        let s = String::from_utf8(out).unwrap();

        let zebra = s.find("zebra").expect("zebra survives");
        let alpha = s.find("alpha").expect("alpha survives");
        let middle = s.find("middle").expect("middle survives");
        assert!(
            zebra < alpha && alpha < middle,
            "key order not preserved:\n{s}"
        );
    }

    /// Nested maps too — the hook events themselves are a map the user reads.
    #[test]
    fn preserves_nested_key_order() {
        let src = br#"{"hooks": {"Stop": [], "PreToolUse": [], "Notification": []}}"#;
        let (out, _) = wire(src).unwrap();
        let s = String::from_utf8(out).unwrap();
        let stop = s.find("Stop").unwrap();
        let pre = s.find("PreToolUse").unwrap();
        let notification = s.find("Notification").unwrap();
        assert!(stop < pre && pre < notification, "nested order lost:\n{s}");
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(wire(b"{not json"), Err(Error::InvalidJson)));
        assert!(matches!(wire(b"[1, 2, 3]"), Err(Error::InvalidJson)));
    }

    #[test]
    fn wire_file_creates_and_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude").join("settings.json");

        assert!(
            wire_file(&path).unwrap(),
            "first run should report a change"
        );

        let written = fs::read(&path).unwrap();
        assert!(is_wired(&written), "written file is not detected as wired");

        assert!(!wire_file(&path).unwrap(), "second run reported a change");

        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".settings-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn wire_file_leaves_an_unrelated_file_byte_identical_on_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        wire_file(&path).unwrap();
        let first = fs::read(&path).unwrap();

        assert!(!wire_file(&path).unwrap());
        assert_eq!(first, fs::read(&path).unwrap());
    }

    #[test]
    fn detects_wiring() {
        assert!(!is_wired(b"{}"), "empty settings reported as wired");
        assert!(
            !is_wired(b"{not json"),
            "invalid settings reported as wired"
        );
        assert!(
            !is_wired(br#"{"hooks":{"PostToolUse":[{"matcher":"Edit|Write","hooks":[{"type":"command","command":"prettier"}]}]}}"#),
            "another tool's hook reported as Recall's"
        );
        let (out, _) = wire(b"").unwrap();
        assert!(
            is_wired(&out),
            "freshly wired settings not reported as wired"
        );
    }
}
