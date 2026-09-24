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
//! Three checks run over them:
//!
//! 1. **Old shapes still read.** Every fixture, of every version, parses
//!    into today's type and, for a request, passes today's validation.
//! 2. **Nothing was removed.** For each kind, the newest fixture parsed and
//!    serialized again still carries every key the fixture had, at every
//!    depth. A field renamed or dropped fails here, before any client in
//!    the field does.
//! 3. **The audit fixtures are a real log.** Their leaves hash to their
//!    checkpoint, and the signed push verifies against the key its device's
//!    approve leaf carries.

use std::fs;
use std::path::{Path, PathBuf};

use recall_wire::{
    AdminStats, ApproveRequest, AuditCheckpoint, AuditConsistencyResponse, AuditEntriesResponse,
    Authkey, AuthkeyCreated, AuthkeyList, AuthkeyRequest, AuthkeyRevokeRequest, ClaimRequest,
    ClaimResponse, DenyRequest, DenyResponse, Device, DeviceIdentity, DeviceList, Discovery,
    EnrollApproved, EnrollPending, EnrollPollRequest, EnrollPollResponse, EnrollRequest,
    ErrorResponse, Health, JobList, PendingEnrollment, PushRequest, PushResponse, ResultRequest,
    ResultResponse, SyncResponse,
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
        "push_response" | "push_response_delete" | "push_response_queued" => {
            go::<PushResponse>(bytes)
        }
        "sync_response" => go::<SyncResponse>(bytes),
        "health" => go::<Health>(bytes),
        "admin_stats" => go::<AdminStats>(bytes),
        "error" => go::<ErrorResponse>(bytes),
        "discovery" => {
            // The devices capability is typed; a document that lists it
            // must list one this build can read.
            let doc: Discovery = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            if doc.can(recall_wire::discovery::CAPABILITY_DEVICES) && doc.devices().is_none() {
                return Err("the devices capability does not read".to_string());
            }
            serde_json::to_value(doc).map_err(|e| e.to_string())
        }
        "enroll_request" | "enroll_request_with_authkey" => {
            let req: EnrollRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            req.validate().map_err(|e| e.to_string())?;
            serde_json::to_value(req).map_err(|e| e.to_string())
        }
        "enroll_response_pending" => go::<EnrollPending>(bytes),
        "enroll_response_approved" => go::<EnrollApproved>(bytes),
        "enroll_poll_request" => go::<EnrollPollRequest>(bytes),
        "enroll_poll_response" => go::<EnrollPollResponse>(bytes),
        // RFC 8628's error codes travel in the one error shape.
        "enroll_poll_error" => go::<ErrorResponse>(bytes),
        "device_approve_request" | "device_approve_request_worker" => go::<ApproveRequest>(bytes),
        "device_approve_response" | "device_revoke_response" => go::<Device>(bytes),
        "device_pending_response" => go::<PendingEnrollment>(bytes),
        "device_me_response" => go::<DeviceIdentity>(bytes),
        "authkey_revoke_request" => go::<AuthkeyRevokeRequest>(bytes),
        "device_deny_request" => go::<DenyRequest>(bytes),
        "device_deny_response" => go::<DenyResponse>(bytes),
        "device_list_response" => go::<DeviceList>(bytes),
        "authkey_create_request" => go::<AuthkeyRequest>(bytes),
        "authkey_create_response" => go::<AuthkeyCreated>(bytes),
        "authkey_list_response" => go::<AuthkeyList>(bytes),
        "authkey_revoke_response" => go::<Authkey>(bytes),
        "audit_checkpoint_response" => go::<AuditCheckpoint>(bytes),
        "audit_entries_response" => go::<AuditEntriesResponse>(bytes),
        "audit_consistency_response" => go::<AuditConsistencyResponse>(bytes),
        // A leaf's own shape lives in recall_server::audit::leaf, not this
        // crate — it varies by `action`, and no consumer here re-serializes
        // it (see that module's docs). Here it is valid JSON, and every key
        // present in the oldest capture is still present: parsing to a
        // generic `Value` and serializing it back does that, the same way it
        // would for a typed shape. What the leaves say is checked by
        // `the_audit_fixtures_check_out_as_the_offline_verifier_checks_them`.
        "audit_leaf_push" | "audit_leaf_approve" | "audit_leaf_enroll" => go::<Value>(bytes),
        "job_claim_request" => go::<ClaimRequest>(bytes),
        "job_claim_response" | "job_claim_response_empty" => go::<ClaimResponse>(bytes),
        "job_result_request" | "job_result_request_error" => {
            let req: ResultRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            // What the server refuses with a 400: neither, or both.
            if req.merge.is_some() == req.error.is_some() {
                return Err("a result carries exactly one of merge and error".to_string());
            }
            serde_json::to_value(req).map_err(|e| e.to_string())
        }
        "job_result_response" => go::<ResultResponse>(bytes),
        "job_list_response" => go::<JobList>(bytes),
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
        "enroll_request",
        "enroll_request_with_authkey",
        "enroll_response_pending",
        "enroll_response_approved",
        "enroll_poll_request",
        "enroll_poll_response",
        "enroll_poll_error",
        "device_approve_request",
        "device_approve_response",
        "device_pending_response",
        "device_me_response",
        "authkey_revoke_request",
        "device_deny_request",
        "device_deny_response",
        "device_list_response",
        "device_revoke_response",
        "authkey_create_request",
        "authkey_create_response",
        "authkey_list_response",
        "authkey_revoke_response",
        "audit_checkpoint_response",
        "audit_entries_response",
        "audit_consistency_response",
        "audit_leaf_push",
        "audit_leaf_approve",
        "audit_leaf_enroll",
        "job_claim_request",
        "job_claim_response",
        "job_claim_response_empty",
        "job_result_request",
        "job_result_request_error",
        "job_result_response",
        "job_list_response",
        "push_response_queued",
        "device_approve_request_worker",
    ] {
        assert!(
            all.iter().any(|(_, k, _)| k == kind),
            "no fixture for {kind}"
        );
    }
}

/// RFC 9162's tree hash over `leaves`, a third time (after the server's and
/// `scripts/audit-verify.py`'s), for the check below.
fn merkle_root(leaves: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    match leaves.len() {
        0 => Sha256::digest([]).into(),
        1 => Sha256::new()
            .chain_update([0u8])
            .chain_update(leaves[0])
            .finalize()
            .into(),
        n => {
            let k = 1usize << (usize::BITS - 1 - (n - 1).leading_zeros());
            Sha256::new()
                .chain_update([1u8])
                .chain_update(merkle_root(&leaves[..k]))
                .chain_update(merkle_root(&leaves[k..]))
                .finalize()
                .into()
        }
    }
}

/// The audit fixtures are one capture of a real server, and check out the
/// way `scripts/audit-verify.py` checks an export: the page's leaves hash
/// to the checkpoint's root; each leaf fixture is one of them byte for
/// byte; each is leaf version 1 in field order; the push is signed by the
/// device the approve leaf made, over a base whose keyid is that device and
/// whose digest is the leaf's; and the enrolment names the authkey that
/// made it. Hand-built leaves could not pass this.
#[test]
fn the_audit_fixtures_check_out_as_the_offline_verifier_checks_them() {
    use base64::Engine;
    use recall_wire::signature;

    let all = fixtures();
    let newest = |kind: &str| -> Vec<u8> {
        let (_, _, path) = all.iter().rev().find(|(_, k, _)| k == kind).unwrap();
        fs::read(path).unwrap()
    };
    let checkpoint: AuditCheckpoint =
        serde_json::from_slice(&newest("audit_checkpoint_response")).unwrap();
    let page: AuditEntriesResponse =
        serde_json::from_slice(&newest("audit_entries_response")).unwrap();
    let proof: AuditConsistencyResponse =
        serde_json::from_slice(&newest("audit_consistency_response")).unwrap();

    assert_eq!((page.start, page.end), (0, checkpoint.tree_size));
    assert_eq!(page.entries.len() as u64, checkpoint.tree_size);
    let bytes: Vec<&[u8]> = page.entries.iter().map(|e| e.as_bytes()).collect();
    assert_eq!(
        base64::engine::general_purpose::STANDARD.encode(merkle_root(&bytes)),
        checkpoint.root_hash,
        "the page's leaves are the checkpoint's tree"
    );
    assert_eq!((proof.first, proof.second), (1, checkpoint.tree_size));
    assert!(!proof.proof.is_empty());

    let order = ["v", "seq", "at", "action", "actor", "subject", "request"];
    for (i, entry) in page.entries.iter().enumerate() {
        let leaf: Value = serde_json::from_str(entry).unwrap();
        let keys: Vec<&str> = leaf
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, order, "leaf {i}");
        assert_eq!(
            (leaf["v"].as_u64(), leaf["seq"].as_u64()),
            (Some(1), Some(i as u64))
        );
    }

    let leaf = |kind: &str| -> Value {
        let text = String::from_utf8(newest(kind)).unwrap();
        assert!(
            page.entries.contains(&text),
            "{kind} is not a leaf of the page"
        );
        serde_json::from_str(&text).unwrap()
    };
    let (push, approve, enroll) = (
        leaf("audit_leaf_push"),
        leaf("audit_leaf_approve"),
        leaf("audit_leaf_enroll"),
    );

    assert_eq!(approve["action"], "approve");
    let key =
        signature::parse_public_key(approve["subject"]["public_key"].as_str().unwrap()).unwrap();
    assert_eq!(
        approve["subject"]["fingerprint"].as_str().unwrap(),
        signature::fingerprint(&key)
    );

    assert_eq!(push["action"], "push");
    assert_eq!(push["actor"]["kind"], "device");
    assert_eq!(push["actor"]["id"], approve["subject"]["device_id"]);
    assert!(push["seq"].as_u64() > approve["seq"].as_u64());
    let request = &push["request"];
    let base = request["signature_base"].as_str().unwrap();
    assert!(base.starts_with("\"@method\": POST\n"), "{base}");
    assert!(base.contains("\n\"@path\": /sync\n"), "{base}");
    let keyid = format!("keyid=\"{}\"", push["actor"]["id"].as_str().unwrap());
    assert!(base.contains(&keyid), "{base}");
    let digest = format!(
        "\n\"content-digest\": sha-256=:{}:\n",
        request["body_sha256"].as_str().unwrap()
    );
    assert!(base.contains(&digest), "{base}");
    assert_eq!(
        request["body"],
        Value::Null,
        "a push does not keep its body"
    );
    let sig = base64::engine::general_purpose::STANDARD
        .decode(request["signature"].as_str().unwrap())
        .unwrap();
    signature::verify(&key, base, &sig).expect("the push is signed by the approved key");

    assert_eq!(enroll["action"], "enroll");
    assert_eq!(enroll["actor"]["kind"], "authkey");
    assert_eq!(enroll["subject"]["authkey_id"], enroll["actor"]["id"]);
    assert_eq!(enroll["subject"]["user_code"], Value::Null);
    assert_eq!(enroll["request"], Value::Null);
    signature::parse_public_key(enroll["subject"]["public_key"].as_str().unwrap()).unwrap();
}
