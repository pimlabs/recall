//! Every request and response shape that has shipped, kept as the bytes a
//! real release sent, and read by today's types.
//!
//! This is the check behind the promise in `docs/reference/api.md` and
//! `docs/design/handshake.md`: a newer server keeps reading what older
//! clients send, and a newer client keeps reading what older servers
//! answer. `fixtures/wire/<version>/` holds what that version put on the
//! wire. Responses were captured from the released binary; requests follow
//! recall-wire's `PushRequest` at that tag, field order and skip rules
//! included. See `fixtures/wire/README.md`.
//!
//! Two checks run over them:
//!
//! 1. **Old shapes still read.** Every fixture, of every version, parses
//!    into today's type and, for a request, passes today's validation.
//! 2. **Nothing was removed.** For each kind, the newest fixture parsed and
//!    serialized again still carries every key the fixture had, at every
//!    depth. A field renamed or dropped fails here, before any client in
//!    the field does.

use std::fs;
use std::path::{Path, PathBuf};

use recall_wire::{
    AdminStats, Discovery, ErrorResponse, Health, PushRequest, PushResponse, SyncResponse,
};
use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/wire")
}

/// Every `(version, kind, path)` under the fixture root, oldest version
/// first. The kind is the file name without its extension.
fn fixtures() -> Vec<(recall_wire::discovery::Version, String, PathBuf)> {
    let mut out = Vec::new();
    for dir in fs::read_dir(root()).expect("fixtures/wire exists") {
        let dir = dir.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let version = recall_wire::discovery::Version::parse(&name)
            .unwrap_or_else(|| panic!("fixtures/wire/{name} is not a version"));
        for file in fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            let kind = file.file_stem().unwrap().to_string_lossy().to_string();
            out.push((version.clone(), kind, file));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    assert!(!out.is_empty(), "no fixtures found");
    out
}

/// Parses `bytes` as the type `kind` names, and serializes it back.
/// A kind nobody taught this function is a failure, so a fixture cannot be
/// added and then silently never read.
fn round_trip(kind: &str, bytes: &[u8]) -> Result<Value, String> {
    fn go<T: serde::de::DeserializeOwned + serde::Serialize>(b: &[u8]) -> Result<Value, String> {
        let parsed: T = serde_json::from_slice(b).map_err(|e| e.to_string())?;
        serde_json::to_value(parsed).map_err(|e| e.to_string())
    }
    match kind {
        "push_request" | "push_request_delete" => {
            let req: PushRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            req.validate().map_err(|e| e.to_string())?;
            serde_json::to_value(req).map_err(|e| e.to_string())
        }
        "push_response" | "push_response_delete" => go::<PushResponse>(bytes),
        "sync_response" => go::<SyncResponse>(bytes),
        "health" => go::<Health>(bytes),
        "admin_stats" => go::<AdminStats>(bytes),
        "error" => go::<ErrorResponse>(bytes),
        "discovery" => go::<Discovery>(bytes),
        other => Err(format!("no type is known for the fixture kind {other:?}")),
    }
}

#[test]
fn every_shape_that_ever_shipped_still_reads() {
    let mut failures = Vec::new();
    for (_, kind, path) in fixtures() {
        let bytes = fs::read(&path).unwrap();
        if let Err(e) = round_trip(&kind, &bytes) {
            failures.push(format!("{} {kind}: {e}", path.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every key path in `fixture` that `now` no longer has.
fn missing_keys(fixture: &Value, now: &Value, at: &str, out: &mut Vec<String>) {
    match (fixture, now) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in old {
                let path = format!("{at}.{key}");
                match new.get(key) {
                    Some(next) => missing_keys(value, next, &path, out),
                    None => out.push(path),
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            for (i, (a, b)) in old.iter().zip(new).enumerate() {
                missing_keys(a, b, &format!("{at}[{i}]"), out);
            }
        }
        _ => {}
    }
}

#[test]
fn the_newest_shape_of_each_kind_has_lost_no_field() {
    let all = fixtures();
    let mut kinds: Vec<&String> = all.iter().map(|(_, k, _)| k).collect();
    kinds.sort();
    kinds.dedup();
    let mut failures = Vec::new();
    for kind in kinds {
        // Sorted oldest first, so the first found from the end is the newest.
        let (_, _, path) = all.iter().rev().find(|(_, k, _)| k == kind).unwrap();
        let bytes = fs::read(path).unwrap();
        let fixture: Value = serde_json::from_slice(&bytes).unwrap();
        let now = round_trip(kind, &bytes).unwrap();
        let mut missing = Vec::new();
        missing_keys(&fixture, &now, "", &mut missing);
        for m in missing {
            failures.push(format!("{}: {m} is gone", path.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The fixtures cover every kind the API has. A new response type with no
/// fixture would otherwise be the one shape nothing protects.
#[test]
fn every_kind_has_a_fixture() {
    let all = fixtures();
    for kind in [
        "push_request",
        "push_request_delete",
        "push_response",
        "push_response_delete",
        "sync_response",
        "health",
        "admin_stats",
        "error",
        "discovery",
    ] {
        assert!(
            all.iter().any(|(_, k, _)| k == kind),
            "no fixture for {kind}"
        );
    }
}
