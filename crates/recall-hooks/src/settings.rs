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
/// names on the fly. See `docs/history/phase-0-findings.md` §4.
pub const PUSH_MATCHER: &str = "Edit|Write";
/// The command `recall init` writes for the push hook.
///
/// Guarded rather than a bare `recall push`, and the guard is load-bearing.
/// This string is *committed*, and it travels to every machine that clones
/// the project — including the ephemeral cloud session that is the whole
/// reason the project commits it rather than wiring hooks user-side. On a
/// machine without Recall installed, a bare command exits 127 on every Edit
/// and Write in the session.
///
/// The bash implementation this replaced was self-contained in the
/// repository, so a fresh clone worked with nothing installed. A binary
/// cannot be, so it earns the same property back here: not installed is a
/// silent no-op, and only a Recall that *is* present gets to report
/// anything.
pub const PUSH_COMMAND: &str = "if command -v recall >/dev/null 2>&1; then recall push; fi";
/// Likewise for the session-start pull.
pub const PULL_COMMAND: &str = "if command -v recall >/dev/null 2>&1; then recall pull; fi";

/// What makes a hook command Recall's, for detection.
///
/// Matched as a substring rather than comparing whole commands, so a project
/// wired before the guard was added still reads as wired — and does not get
/// a second, duplicate hook appended the next time `recall init` runs.
const PUSH_MARKER: &str = "recall push";
const PULL_MARKER: &str = "recall pull";

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

    let push_added = add_matcher_hook(
        &mut doc,
        POST_TOOL_USE,
        PUSH_MATCHER,
        PUSH_COMMAND,
        PUSH_MARKER,
    );
    let pull_added = add_session_start_hook(&mut doc, PULL_COMMAND, PULL_MARKER);

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
            .any(|entry| entry_has_command(entry, PUSH_MARKER))
    })
}

/// What one settings document's `env` block declares.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnvBlock {
    /// Name and value, in the order the file writes them.
    pub vars: Vec<(String, String)>,
    /// Names whose value was not a string, and so could not be turned into
    /// an environment variable. Kept rather than discarded — see
    /// [`env_block`].
    pub ignored: Vec<String>,
}

/// The `env` block of one settings document, in the order it is written.
///
/// Claude Code writes every entry here into the environment of the processes
/// it spawns — the push and pull hooks among them — and an entry *replaces*
/// whatever the shell exported. That is the whole reason this function
/// exists: without it `recall status`, typed into a shell, reports values
/// that the hooks it is diagnosing never see.
///
/// Only string values are returned. The settings schema says `env` maps
/// names to strings, and a number or an object has no defensible spelling as
/// an environment variable, so guessing one would be inventing behaviour
/// Claude Code does not have. Their names come back in
/// [`EnvBlock::ignored`] instead of being dropped: somebody wrote
/// `"RECALL_PROJECT_KEY": 12345` on purpose, and a report that answers with
/// the derived key and no remark at all is confidently wrong about the one
/// thing it was asked.
///
/// An empty string is kept rather than dropped. It is a real declaration
/// with a real effect — the variable is set, to nothing — and it is exactly
/// the case where reporting "unset" would send someone looking in the wrong
/// place.
///
/// # Errors
///
/// [`Error::InvalidJson`] when the file is not JSON, or is JSON that is not
/// an object. A document with no `env` key is not an error: it is the
/// ordinary case, and it returns an empty list.
pub fn env_block(src: &[u8]) -> Result<EnvBlock, Error> {
    if src.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(EnvBlock::default());
    }
    let doc: Value = serde_json::from_slice(src).map_err(|_| Error::InvalidJson)?;
    let Value::Object(doc) = doc else {
        return Err(Error::InvalidJson);
    };

    let Some(Value::Object(env)) = doc.get("env") else {
        return Ok(EnvBlock::default());
    };

    let mut block = EnvBlock::default();
    for (name, value) in env {
        match value {
            Value::String(value) => block.vars.push((name.clone(), value.clone())),
            _ => block.ignored.push(name.clone()),
        }
    }
    Ok(block)
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
fn add_matcher_hook(
    doc: &mut Value,
    event: &str,
    matcher: &str,
    command: &str,
    marker: &str,
) -> bool {
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
        if list.iter().any(|h| has_command(h, marker)) {
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
fn add_session_start_hook(doc: &mut Value, command: &str, marker: &str) -> bool {
    let entries = event_entries_mut(doc, SESSION_START);
    if entries.iter().any(|e| entry_has_command(e, marker)) {
        return false;
    }
    entries.push(json!({ "hooks": [hook_entry(command)] }));
    true
}

fn hook_entry(command: &str) -> Value {
    json!({ "type": "command", "command": command })
}

fn has_command(hook: &Value, marker: &str) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|cmd| cmd.contains(marker))
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
    /// The committed command runs on machines that have never heard of
    /// Recall. A bare `recall push` there is 127 on every Edit and Write.
    #[test]
    fn the_wired_command_is_a_no_op_where_recall_is_not_installed() {
        use std::process::Command;

        for command in [PUSH_COMMAND, PULL_COMMAND] {
            // /bin/sh by absolute path, because the whole point is to run
            // it with a PATH on which nothing — including `recall` — can be
            // found.
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .env("PATH", "/nonexistent")
                .status()
                .expect("the hook command should at least run");
            assert!(
                status.success(),
                "{command:?} exited {status:?} with recall absent — that is a \
                 hook error on every edit in the session"
            );
        }
    }

    /// The `env` block is the one part of this file Recall *reads* rather
    /// than writes, and the two failure modes have to stay apart: a value it
    /// cannot use is a finding about one variable, while a document it
    /// cannot parse is a finding about the whole file.
    #[test]
    fn an_env_block_separates_what_it_read_from_what_it_could_not_use() {
        let block =
            env_block(br#"{"env":{"A":"one","B":2,"C":{"d":1},"E":"","F":null},"hooks":{}}"#)
                .unwrap();
        assert_eq!(
            block.vars,
            vec![
                ("A".to_string(), "one".to_string()),
                ("E".to_string(), String::new())
            ],
            "an empty string is a declaration with an effect, not a missing one"
        );
        assert_eq!(block.ignored, vec!["B", "C", "F"]);

        // No `env` key, an empty document, and whitespace are the ordinary
        // cases on almost every machine — none of them is a finding.
        for src in [&b"{\"hooks\":{}}"[..], b"{}", b"   \n", b""] {
            let block = env_block(src).unwrap();
            assert!(block.vars.is_empty() && block.ignored.is_empty());
        }

        // A document Claude Code cannot read either.
        assert!(env_block(b"{ not json").is_err());
        assert!(
            env_block(b"[]").is_err(),
            "a JSON array is not a settings file"
        );
    }

    /// A project wired before the guard existed must still read as wired, or
    /// `recall status` lies and `recall init` appends a duplicate.
    #[test]
    fn a_project_wired_with_the_old_bare_command_is_still_recognised() {
        let old = br#"{"hooks":{
            "PostToolUse":[{"matcher":"Edit|Write",
                "hooks":[{"type":"command","command":"recall push"}]}],
            "SessionStart":[
                {"hooks":[{"type":"command","command":"recall pull"}]}]}}"#;
        assert!(is_wired(old));

        let (out, changed) = wire(old).unwrap();
        assert!(!changed, "an already-wired project was rewritten");

        let text = String::from_utf8_lossy(&out);
        assert_eq!(text.matches("recall push").count(), 1, "push duplicated");
        assert_eq!(text.matches("recall pull").count(), 1, "pull duplicated");
    }

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
