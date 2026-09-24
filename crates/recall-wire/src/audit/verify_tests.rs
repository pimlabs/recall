//! The offline verifier against two logs: the 0.4.2 capture of a real
//! server (`fixtures/wire/0.4.2/audit_entries_response.json`), and one
//! built here with real signatures, so every kind of signed leaf a device
//! makes is in it.
//!
//! Each test is named for what it refuses, and was checked the other way
//! too: with the check it pins deleted, the test fails. The broader
//! corpus, every forgery the server's review found, runs in
//! `crates/recall-server/tests/audit.rs`, through this verifier and
//! `scripts/audit-verify.py` side by side.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Value};

use super::*;
use crate::signature::{SignatureInput, SigningKey};
use crate::{content_sha256, AuditCheckpoint, AuditEntriesResponse};

/// The captured page of a real server's log: ten leaves, one push signed
/// by the device an earlier leaf approved.
fn captured() -> Vec<String> {
    let page: AuditEntriesResponse = serde_json::from_str(include_str!(
        "../../fixtures/wire/0.4.2/audit_entries_response.json"
    ))
    .unwrap();
    page.entries
}

/// An export of `leaves` whose first line is the checkpoint over them, as
/// a server that wrote them would have answered.
fn export_of(leaves: &[String]) -> Vec<u8> {
    let (size, root) = checkpoint_of(leaves);
    let mut out = format!("{size} {}\n", STANDARD.encode(root));
    for leaf in leaves {
        out.push_str(leaf);
        out.push('\n');
    }
    out.into_bytes()
}

fn checkpoint_of(leaves: &[String]) -> (u64, Hash) {
    let hashes: Vec<Hash> = leaves
        .iter()
        .map(|l| merkle::hash_leaf(l.as_bytes()))
        .collect();
    (leaves.len() as u64, merkle::root(&hashes))
}

fn verify(export: &[u8], saved: &[(u64, Hash)]) -> Verdict {
    verify_export(export, saved)
}

/// That `verdict` refused, and said `want` somewhere.
#[track_caller]
fn refused(verdict: &Verdict, want: &str) {
    assert!(!verdict.ok(), "accepted: {verdict:?}");
    assert!(
        verdict.problems.iter().any(|p| p.contains(want)),
        "wanted {want:?} in {:#?}",
        verdict.problems
    );
}

/// That `verdict` refused leaf `at` itself, whatever else it said.
#[track_caller]
fn refused_leaf(verdict: &Verdict, at: usize, want: &str) {
    let prefix = format!("leaf {at}: ");
    assert!(
        verdict
            .problems
            .iter()
            .any(|p| p.starts_with(&prefix) && p.contains(want)),
        "wanted leaf {at} refused with {want:?} in {:#?}",
        verdict.problems
    );
}

// ---------------------------------------------------------------------------
// a log with every kind of signed leaf, built with real keys
// ---------------------------------------------------------------------------

const AUTHORITY: &str = "recall.test";
const CREATED: i64 = 1_790_000_000;

struct Machine {
    key: SigningKey,
    id: String,
    name: String,
}

impl Machine {
    fn new(seed: u8, id: &str, name: &str) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn public_key(&self) -> String {
        signature::encode_public_key(&self.key.verifying_key())
    }

    fn fingerprint(&self) -> String {
        signature::fingerprint(&self.key.verifying_key())
    }

    fn actor(&self) -> Value {
        json!({"kind": "device", "id": self.id, "name": self.name, "agent": "recall/0.4.2 (test)"})
    }

    /// The `request` a leaf keeps for a request this machine signed, the
    /// way the server records one: the digest, the exact base, the
    /// signature, and the body where the action keeps it.
    fn signed(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: &str,
        keep: bool,
    ) -> Value {
        let nonce = format!(
            "nonce-{}-{method}-{path}-{}",
            self.id,
            query.unwrap_or(body)
        );
        let nonce: String = STANDARD
            .encode(Sha256::digest(nonce.as_bytes()))
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(22)
            .collect();
        let digest = signature::content_digest(body.as_bytes());
        let input = SignatureInput::recall(CREATED, &self.id, &nonce);
        let base = input
            .signature_base(|name| match name {
                "@method" => Some(method.to_string()),
                "@authority" => Some(AUTHORITY.to_string()),
                "@path" => Some(path.to_string()),
                "@query" => Some(format!("?{}", query.unwrap_or(""))),
                "content-digest" => Some(digest.clone()),
                "recall-protocol" => Some("1".to_string()),
                _ => None,
            })
            .unwrap();
        json!({
            "body_sha256": STANDARD.encode(Sha256::digest(body.as_bytes())),
            "signature_base": base,
            "signature": STANDARD.encode(signature::sign(&self.key, &base)),
            "body": keep.then(|| body.to_string()),
        })
    }
}

fn leaf(seq: usize, action: &str, actor: Value, subject: Value, request: Value) -> Value {
    json!({
        "v": 1,
        "seq": seq,
        "at": format!("2026-10-02T09:14:{:02}.000Z", seq),
        "action": action,
        "actor": actor,
        "subject": subject,
        "request": request,
    })
}

fn device_subject(
    m: &Machine,
    scope: &str,
    ephemeral: bool,
    authkey: Option<&str>,
    code: Option<&str>,
) -> Value {
    json!({
        "device_id": m.id, "name": m.name, "scope": scope, "public_key": m.public_key(),
        "fingerprint": m.fingerprint(), "ephemeral": ephemeral, "authkey_id": authkey,
        "user_code": code,
    })
}

fn file_subject(file: &str, content: &str) -> Value {
    json!({
        "project_key": "acme/app", "file_path": file, "deleted": false,
        "stored_sha256": content_sha256(content), "base_sha256": null, "merged": false,
        "merge_job": null,
    })
}

/// Positions in [`built`]'s log, by what is there.
mod at {
    pub const APPROVE_LAPTOP: usize = 1;
    pub const PUSH: usize = 2;
    pub const PULL: usize = 3;
    pub const AUTHKEY_CREATE: usize = 4;
    pub const ENROLL: usize = 5;
    pub const CLOUD_PUSH: usize = 6;
    pub const APPROVE_PHONE: usize = 7;
    pub const REVOKE_PHONE: usize = 8;
    pub const AUTHKEY_REVOKE: usize = 9;
}

fn laptop() -> Machine {
    Machine::new(21, "dev_laptop", "laptop")
}
fn cloud() -> Machine {
    Machine::new(22, "dev_cloud", "cloud-dev_clou")
}
fn phone() -> Machine {
    Machine::new(23, "dev_phone", "phone")
}

/// A log as an honest server writes one: the laptop approved by the
/// operator as an admin device, pushing, pulling, making an authkey that
/// a cloud session enrols with and pushes as, approving a phone and
/// revoking it, and revoking the authkey with the device it enrolled.
fn built() -> Vec<Value> {
    let (laptop, cloud, phone) = (laptop(), cloud(), phone());
    let push_body = r#"{"project_key":"acme/app","file_path":"a.md","content":"hi"}"#;
    let cloud_body = r#"{"project_key":"acme/app","file_path":"c.md","content":"from the cloud"}"#;
    let key_body = r#"{"tag":"cloud","expires_in_days":90,"max_devices":3}"#;
    let approve_body = r#"{"user_code":"cdfg-hjkl","scope":"sync"}"#;
    let revoke_key_body = r#"{"revoke_devices":true}"#;
    vec![
        leaf(
            0,
            "start",
            json!({"kind": "server"}),
            json!({"version": "0.4.2"}),
            Value::Null,
        ),
        leaf(
            at::APPROVE_LAPTOP,
            "approve",
            json!({"kind": "operator"}),
            device_subject(&laptop, "admin", false, None, Some("BCDF-GHJK")),
            Value::Null,
        ),
        leaf(
            at::PUSH,
            "push",
            laptop.actor(),
            file_subject("a.md", "hi"),
            laptop.signed("POST", "/sync", None, push_body, false),
        ),
        leaf(
            at::PULL,
            "pull",
            laptop.actor(),
            json!({"project_key": "acme/app"}),
            laptop.signed("GET", "/sync", Some("project_key=acme%2Fapp"), "", false),
        ),
        leaf(
            at::AUTHKEY_CREATE,
            "authkey_create",
            laptop.actor(),
            json!({"authkey_id": "ak_1", "tag": "cloud", "ephemeral": true, "max_devices": 3}),
            laptop.signed("POST", "/v1/authkeys", None, key_body, true),
        ),
        leaf(
            at::ENROLL,
            "enroll",
            json!({"kind": "authkey", "id": "ak_1", "tag": "cloud"}),
            device_subject(&cloud, "sync", true, Some("ak_1"), None),
            Value::Null,
        ),
        leaf(
            at::CLOUD_PUSH,
            "push",
            cloud.actor(),
            file_subject("c.md", "from the cloud"),
            cloud.signed("POST", "/sync", None, cloud_body, false),
        ),
        leaf(
            at::APPROVE_PHONE,
            "approve",
            laptop.actor(),
            device_subject(&phone, "sync", false, None, Some("CDFG-HJKL")),
            laptop.signed("POST", "/v1/devices/approve", None, approve_body, true),
        ),
        leaf(
            at::REVOKE_PHONE,
            "revoke",
            laptop.actor(),
            json!({"device_id": "dev_phone", "name": "phone"}),
            laptop.signed("POST", "/v1/devices/dev_phone/revoke", None, "{}", true),
        ),
        leaf(
            at::AUTHKEY_REVOKE,
            "authkey_revoke",
            laptop.actor(),
            json!({"authkey_id": "ak_1", "revoke_devices": true, "revoked_devices": ["dev_cloud"]}),
            laptop.signed(
                "POST",
                "/v1/authkeys/ak_1/revoke",
                None,
                revoke_key_body,
                true,
            ),
        ),
    ]
}

fn strings(leaves: &[Value]) -> Vec<String> {
    leaves
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect()
}

/// `leaves` renumbered from 0 in their new order, and dated as the last
/// one before them is, so only what the test changed is wrong.
fn renumbered(mut leaves: Vec<Value>) -> Vec<String> {
    for (i, leaf) in leaves.iter_mut().enumerate() {
        leaf["seq"] = json!(i);
        leaf["at"] = json!("2026-10-02T09:14:00.000Z");
    }
    strings(&leaves)
}

/// The honest log with `extra` appended after it: a server that went on
/// writing and never rewrote, so every checkpoint saved from the honest
/// log still holds.
fn appended(extra: Vec<Value>) -> Vec<String> {
    let mut leaves = built();
    for mut leaf in extra {
        leaf["seq"] = json!(leaves.len());
        leaf["at"] = leaves.last().unwrap()["at"].clone();
        leaves.push(leaf);
    }
    strings(&leaves)
}

/// The honest log with the leaf at `at` edited in place.
fn edited(at: usize, edit: impl FnOnce(&mut Value)) -> Vec<String> {
    let mut leaves = built();
    edit(&mut leaves[at]);
    strings(&leaves)
}

fn saved_honest() -> Vec<(u64, Hash)> {
    vec![checkpoint_of(&strings(&built()))]
}

// ---------------------------------------------------------------------------
// honest logs
// ---------------------------------------------------------------------------

#[test]
fn the_captured_log_checks_out() {
    let leaves = captured();
    let verdict = verify(&export_of(&leaves), &[]);
    assert!(verdict.ok(), "{verdict:#?}");
    assert_eq!((verdict.leaves, verdict.signed), (10, 1));
    let cp: AuditCheckpoint = serde_json::from_str(include_str!(
        "../../fixtures/wire/0.4.2/audit_checkpoint_response.json"
    ))
    .unwrap();
    assert_eq!(
        verdict.checkpoint,
        cp.to_header_value(),
        "the server's own checkpoint"
    );
}

#[test]
fn the_built_log_checks_out_with_every_prefix_saved() {
    let leaves = strings(&built());
    let saved: Vec<(u64, Hash)> = (0..=leaves.len())
        .map(|n| checkpoint_of(&leaves[..n]))
        .collect();
    let verdict = verify(&export_of(&leaves), &saved);
    assert!(verdict.ok(), "{verdict:#?}");
    assert_eq!(verdict.signed, 7, "every device leaf's signature checked");
}

/// No trailing newline reads the same; a second one is an empty leaf.
#[test]
fn one_trailing_newline_and_no_more() {
    let export = export_of(&captured());
    assert!(verify(&export[..export.len() - 1], &[]).ok());
    let mut two = export.clone();
    two.push(b'\n');
    refused(&verify(&two, &[]), "leaf 10: the leaf is not JSON");
}

// ---------------------------------------------------------------------------
// the tree: the plan's "a changed byte, a missing leaf, two swapped leaves,
// or a saved checkpoint the log does not extend"
// ---------------------------------------------------------------------------

#[test]
fn a_changed_byte_is_refused() {
    let mut leaves = captured();
    leaves[2] = leaves[2].replacen("gone.md", "gone.me", 1);
    let (size, _) = checkpoint_of(&leaves);
    let honest = checkpoint_of(&captured());
    // The honest checkpoint on the first line, the changed leaf below it.
    let mut export = format!("{size} {}\n", STANDARD.encode(honest.1));
    for l in &leaves {
        export.push_str(l);
        export.push('\n');
    }
    refused(
        &verify(export.as_bytes(), &[]),
        "a leaf changed, removed or reordered",
    );
}

#[test]
fn a_missing_leaf_is_refused() {
    let mut leaves = captured();
    leaves.remove(4);
    // Its own checkpoint recomputed, so only the gap gives it away...
    let verdict = verify(&export_of(&leaves), &[]);
    refused_leaf(&verdict, 4, "seq is 5 at position 4");
    // ...and against the checkpoint saved before, the tree does too.
    let saved = [checkpoint_of(&captured())];
    refused(
        &verify(&export_of(&leaves), &saved),
        "past the end of this export",
    );
    let saved = [checkpoint_of(&captured()[..6])];
    refused(
        &verify(&export_of(&leaves), &saved),
        "the log does not extend it",
    );
}

#[test]
fn two_swapped_leaves_are_refused() {
    let mut leaves = captured();
    leaves.swap(3, 4);
    let verdict = verify(&export_of(&leaves), &[]);
    refused_leaf(&verdict, 3, "seq is 4 at position 3");
    // Renumbered to hide it, the saved checkpoint still catches it.
    let mut values: Vec<Value> = leaves
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for (i, v) in values.iter_mut().enumerate() {
        v["seq"] = json!(i);
    }
    let hidden = strings(&values);
    let saved = [checkpoint_of(&captured()[..5])];
    refused(
        &verify(&export_of(&hidden), &saved),
        "the log does not extend it",
    );
}

/// Mutation: check only the latest root. A log rewritten early and
/// re-rooted passes its own checkpoint and fails only an earlier one.
#[test]
fn a_saved_checkpoint_the_log_does_not_extend_is_refused() {
    let honest = captured();
    let early = checkpoint_of(&honest[..3]);
    let mut rewritten = honest.clone();
    rewritten[1] = rewritten[1].replacen("note.md", "nope.md", 1);
    let export = export_of(&rewritten);
    assert!(verify(&export, &[]).ok(), "consistent with itself");
    refused(
        &verify(&export, &[early]),
        "the root over the first 3 leaves",
    );
    let later = checkpoint_of(&honest);
    refused(
        &verify(&export, &[later]),
        "the root over the first 10 leaves",
    );
    assert!(verify(&export_of(&honest), &[early, later]).ok());
}

#[test]
fn a_checkpoint_line_that_is_not_the_count_is_refused() {
    let leaves = captured();
    let (_, root) = checkpoint_of(&leaves);
    let mut export = format!("11 {}\n", STANDARD.encode(root));
    for l in &leaves {
        export.push_str(l);
        export.push('\n');
    }
    refused(
        &verify(export.as_bytes(), &[]),
        "its checkpoint says tree_size 11",
    );
    refused(&verify(b"", &[]), "the export is empty");
    refused(&verify(b"10 not-base64\n", &[]), "not base64");
    refused(&verify(b"010 AAAA\n", &[]), "not a checkpoint");
}

// ---------------------------------------------------------------------------
// a forged export that keeps every checkpoint: the plan's "a device's leaf
// with no request, a signature replayed or moved onto another action or
// subject, a digest or keyid that is not the leaf's, a request signed
// before its device existed"; mutation "check only the tree"
// ---------------------------------------------------------------------------

#[test]
fn a_device_leaf_with_no_request_is_refused() {
    let mut forged = built()[at::PUSH].clone();
    forged["action"] = json!("delete");
    forged["subject"]["deleted"] = json!(true);
    forged["request"] = Value::Null;
    let verdict = verify(&export_of(&appended(vec![forged])), &saved_honest());
    refused_leaf(&verdict, 10, "a device's request is not an object");
}

#[test]
fn a_replayed_signature_is_refused() {
    let push = built()[at::PUSH].clone();
    let verdict = verify(&export_of(&appended(vec![push])), &saved_honest());
    refused_leaf(&verdict, 10, "(a replay)");
}

#[test]
fn a_signature_moved_onto_another_subject_is_refused() {
    let mut moved = built()[at::PUSH].clone();
    moved["subject"]["file_path"] = json!("topics/secret.md");
    let verdict = verify(&export_of(&appended(vec![moved])), &saved_honest());
    refused_leaf(&verdict, 10, "(a replay)");
}

#[test]
fn a_signature_moved_onto_another_action_is_refused() {
    let pull_request = built()[at::PULL]["request"].clone();
    let leaves = edited(at::PUSH, |l| l["request"] = pull_request);
    refused_leaf(&verify(&export_of(&leaves), &[]), at::PUSH, "not a push");
    // A pull's signature says which project: another one's is refused.
    let leaves = edited(at::PULL, |l| {
        l["subject"]["project_key"] = json!("someone/else")
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::PULL,
        "does not name the pulled project",
    );
    // A revoke's says which device, in its path.
    let leaves = edited(at::REVOKE_PHONE, |l| {
        l["subject"]["device_id"] = json!("dev_cloud")
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::REVOKE_PHONE,
        "not a revoke",
    );
}

#[test]
fn a_digest_that_is_not_the_leafs_is_refused() {
    let leaves = edited(at::PUSH, |l| {
        l["request"]["body_sha256"] = json!(STANDARD.encode([0u8; 32]))
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::PUSH,
        "content-digest is not",
    );
}

#[test]
fn a_keyid_that_is_not_the_actor_is_refused() {
    let leaves = edited(at::PUSH, |l| l["actor"]["id"] = json!("dev_cloud"));
    refused_leaf(&verify(&export_of(&leaves), &[]), at::PUSH, "not the actor");
}

#[test]
fn a_request_signed_before_its_device_existed_is_refused() {
    let mut leaves = built();
    leaves.swap(at::APPROVE_LAPTOP, at::PUSH);
    let verdict = verify(&export_of(&renumbered(leaves)), &[]);
    refused_leaf(&verdict, 1, "which no earlier approve or enroll leaf made");
}

/// Mutation: take keys from any approve leaf, the latest winning. A second
/// approve of the laptop's id under a key the server holds would then make
/// every signature it forges after it check out.
#[test]
fn a_device_approved_twice_keeps_its_first_key() {
    let forger = Machine::new(99, "dev_laptop", "laptop");
    let mut again = built()[at::APPROVE_LAPTOP].clone();
    again["subject"] = device_subject(&forger, "admin", false, None, Some("DFGH-JKLM"));
    let mut push = built()[at::PUSH].clone();
    push["subject"] = file_subject("b.md", "forged");
    push["request"] = forger.signed(
        "POST",
        "/sync",
        None,
        r#"{"project_key":"acme/app","file_path":"b.md","content":"forged"}"#,
        false,
    );
    let verdict = verify(&export_of(&appended(vec![again, push])), &saved_honest());
    refused_leaf(&verdict, 10, "approved or enrolled before");
    refused_leaf(&verdict, 11, "does not verify with dev_laptop's key");
}

#[test]
fn a_changed_signature_is_refused() {
    let leaves = edited(at::CLOUD_PUSH, |l| {
        let sig = l["request"]["signature"].as_str().unwrap().to_string();
        let flipped = if sig.starts_with('A') { "B" } else { "A" };
        l["request"]["signature"] = json!(format!("{flipped}{}", &sig[1..]));
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::CLOUD_PUSH,
        "does not verify",
    );
}

#[test]
fn a_revoked_device_signs_nothing_more() {
    let mut late = built()[at::CLOUD_PUSH].clone();
    late["request"] = cloud().signed("POST", "/sync", None, r#"{"late":true}"#, false);
    let verdict = verify(&export_of(&appended(vec![late])), &saved_honest());
    refused_leaf(&verdict, 10, "revoked or swept before it");
}

#[test]
fn a_kept_body_must_ask_for_what_was_done() {
    let leaves = edited(at::APPROVE_PHONE, |l| {
        l["subject"]["scope"] = json!("admin")
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::APPROVE_PHONE,
        "another scope",
    );
    let leaves = edited(at::AUTHKEY_CREATE, |l| {
        l["subject"]["max_devices"] = json!(30)
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::AUTHKEY_CREATE,
        "another max_devices",
    );
    let leaves = edited(at::AUTHKEY_CREATE, |l| l["subject"]["tag"] = json!("other"));
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::AUTHKEY_CREATE,
        "another tag",
    );
    let leaves = edited(at::AUTHKEY_CREATE, |l| {
        l["subject"]["ephemeral"] = json!(false)
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::AUTHKEY_CREATE,
        "another ephemeral",
    );
    let leaves = edited(at::APPROVE_PHONE, |l| {
        l["subject"]["user_code"] = json!("DFGH-JKLM")
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::APPROVE_PHONE,
        "another user code",
    );
    let leaves = edited(at::AUTHKEY_REVOKE, |l| {
        l["subject"]["revoke_devices"] = json!(false);
        l["subject"]["revoked_devices"] = json!([]);
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::AUTHKEY_REVOKE,
        "another revoke_devices",
    );
    let leaves = edited(at::APPROVE_PHONE, |l| {
        l["request"]["body"] = json!(r#"{"user_code":"cdfg-hjkl","scope":"sync","x":1}"#)
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::APPROVE_PHONE,
        "does not hash",
    );
}

#[test]
fn only_an_admin_device_manages_devices() {
    let mut leaves = built();
    leaves[at::APPROVE_LAPTOP]["subject"]["scope"] = json!("sync");
    let verdict = verify(&export_of(&strings(&leaves)), &[]);
    refused_leaf(
        &verdict,
        at::AUTHKEY_CREATE,
        "does not have the admin scope",
    );
}

#[test]
fn a_leaf_nobody_signed_carries_no_request() {
    let push_request = built()[at::PUSH]["request"].clone();
    let leaves = edited(at::APPROVE_LAPTOP, |l| l["request"] = push_request);
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::APPROVE_LAPTOP,
        "signs nothing",
    );
}

// ---------------------------------------------------------------------------
// the rest of what the server enforces
// ---------------------------------------------------------------------------

#[test]
fn an_enrolment_needs_a_live_authkey() {
    let mut leaves = built();
    leaves.remove(at::AUTHKEY_CREATE);
    let verdict = verify(&export_of(&renumbered(leaves)), &[]);
    refused_leaf(&verdict, at::ENROLL - 1, "which no leaf created");
    // One enrolled after its key was revoked.
    let mut enroll = built()[at::ENROLL].clone();
    enroll["subject"] = device_subject(
        &Machine::new(24, "dev_late", "late"),
        "sync",
        true,
        Some("ak_1"),
        None,
    );
    let verdict = verify(&export_of(&appended(vec![enroll])), &saved_honest());
    refused_leaf(&verdict, 10, "or which was revoked");
}

#[test]
fn a_device_is_revoked_once() {
    let mut again = built()[at::REVOKE_PHONE].clone();
    again["actor"] = json!({"kind": "operator"});
    again["request"] = Value::Null;
    let verdict = verify(&export_of(&appended(vec![again])), &saved_honest());
    refused_leaf(&verdict, 10, "was revoked before");
}

#[test]
fn a_sweep_is_of_an_ephemeral_device() {
    let sweep = leaf(
        0,
        "sweep",
        json!({"kind": "server"}),
        json!({"device_id": "dev_laptop", "name": "laptop"}),
        Value::Null,
    );
    let verdict = verify(&export_of(&appended(vec![sweep])), &saved_honest());
    refused_leaf(&verdict, 10, "which is not ephemeral");
}

#[test]
fn the_leaf_itself_is_read_strictly() {
    let honest = captured();
    for (from, to, want) in [
        ("\"seq\":1,", "\"seq\":true,", "seq is true"),
        ("\"seq\":3,", "\"seq\":3.0,", "seq is 3.0"),
        (
            "\"action\":\"pull\"",
            "\"action\":\"pull\",\"action\":\"push\"",
            "appears twice",
        ),
        ("\"v\":1,\"seq\":2,", "\"seq\":2,\"v\":1,", "has the keys"),
        ("\"action\":\"deny\"", "\"action\":\"shred\"", "no action"),
        (
            "\"kind\":\"operator\"},\"subject\":{\"project_key\":\"acme/app\"}",
            "\"kind\":\"server\"},\"subject\":{\"project_key\":\"acme/app\"}",
            "a server cannot pull",
        ),
    ] {
        let mut leaves = honest.clone();
        let i = leaves
            .iter()
            .position(|l| l.contains(from))
            .unwrap_or_else(|| panic!("{from}"));
        leaves[i] = leaves[i].replacen(from, to, 1);
        refused_leaf(&verify(&export_of(&leaves), &[]), i, want);
    }
}

#[test]
fn at_never_goes_back() {
    let leaves = edited(at::REVOKE_PHONE, |l| {
        l["at"] = json!("2000-01-01T00:00:00.000Z")
    });
    refused_leaf(
        &verify(&export_of(&leaves), &[]),
        at::REVOKE_PHONE,
        "earlier than the leaf before it",
    );
}

#[test]
fn a_job_follows_from_the_leaves_before_it() {
    let worker = Machine::new(25, "dev_worker", "worker");
    let mut log = built();
    let mut approve = log[at::APPROVE_PHONE].clone();
    approve["subject"] = device_subject(&worker, "worker", false, None, Some("FGHJ-KLMN"));
    approve["request"] = laptop().signed(
        "POST",
        "/v1/devices/approve",
        None,
        r#"{"user_code":"FGHJ-KLMN","scope":"worker"}"#,
        true,
    );
    let mut queued = log[at::PUSH].clone();
    queued["subject"]["merge_job"] = json!("job_1");
    queued["request"] = laptop().signed("POST", "/sync", None, r#"{"queued":1}"#, false);
    let claim = leaf(
        0,
        "job_claim",
        worker.actor(),
        json!({"job_id": "job_1", "kind": "merge", "attempt": 1,
               "lease_expires_at": "2026-10-02T09:20:00.000Z", "project_key": "acme/app", "file_path": "a.md"}),
        worker.signed(
            "POST",
            "/v1/jobs/claim",
            None,
            r#"{"kinds":["merge"]}"#,
            false,
        ),
    );
    let result = leaf(
        0,
        "job_result",
        worker.actor(),
        json!({"job_id": "job_1", "project_key": "acme/app", "file_path": "a.md", "state": "done",
               "stored_sha256": content_sha256("merged"), "follow_up": null}),
        worker.signed(
            "POST",
            "/v1/jobs/job_1/result",
            None,
            r#"{"lease_id":"l"}"#,
            false,
        ),
    );
    for extra in [approve, queued, claim.clone(), result.clone()] {
        log.push(extra);
    }
    let n = log.len();
    let fix = |mut leaves: Vec<Value>| -> Vec<String> {
        for (i, l) in leaves.iter_mut().enumerate().skip(10) {
            l["seq"] = json!(i);
            l["at"] = json!("2026-10-02T09:15:00.000Z");
        }
        strings(&leaves)
    };
    let honest = fix(log.clone());
    assert!(
        verify(&export_of(&honest), &[]).ok(),
        "{:#?}",
        verify(&export_of(&honest), &[])
    );

    // A second claim of a job its live worker holds.
    let mut twice = log.clone();
    twice.insert(n - 1, claim.clone());
    twice[n - 1]["request"] =
        worker.signed("POST", "/v1/jobs/claim", None, r#"{"again":1}"#, false);
    refused_leaf(
        &verify(&export_of(&fix(twice)), &[]),
        n - 1,
        "while dev_worker held it",
    );

    // A result from a worker that does not hold it: the laptop is no
    // worker at all.
    let mut other = log.clone();
    other[n - 1]["actor"] = laptop().actor();
    other[n - 1]["request"] = laptop().signed(
        "POST",
        "/v1/jobs/job_1/result",
        None,
        r#"{"lease_id":"l"}"#,
        false,
    );
    refused_leaf(
        &verify(&export_of(&fix(other)), &[]),
        n - 1,
        "whose scope is admin",
    );

    // A job nothing queued.
    let mut unqueued = log.clone();
    unqueued[n - 3]["subject"]["merge_job"] = Value::Null;
    refused_leaf(
        &verify(&export_of(&fix(unqueued)), &[]),
        n - 2,
        "which no push or result queued",
    );
}

#[test]
fn a_session_acts_with_a_passkey_the_log_added() {
    let bootstrap = leaf(
        0,
        "bootstrap_code",
        json!({"kind": "server"}),
        json!({"expires_at": "2026-10-02T10:00:00.000Z"}),
        Value::Null,
    );
    let add = leaf(
        0,
        "passkey_add",
        json!({"kind": "operator"}),
        json!({"credential_id": "pk_1", "name": "phone", "first": true}),
        Value::Null,
    );
    let deny = leaf(
        0,
        "deny",
        json!({"kind": "session", "credential_id": "pk_1"}),
        json!({"user_code": "GHJK-LMNP", "name": "x"}),
        Value::Null,
    );
    let honest = appended(vec![bootstrap.clone(), add.clone(), deny.clone()]);
    assert!(verify(&export_of(&honest), &[]).ok());
    let verdict = verify(
        &export_of(&appended(vec![bootstrap.clone(), deny.clone()])),
        &[],
    );
    refused_leaf(&verdict, 11, "which the log never added");
    let verdict = verify(&export_of(&appended(vec![add, deny])), &[]);
    refused_leaf(&verdict, 10, "no bootstrap code outstanding");
}

#[test]
fn a_hosts_change_closes_only_open_jobs_of_its_key() {
    let mut queued = built()[at::PUSH].clone();
    queued["subject"]["merge_job"] = json!("job_9");
    queued["request"] = laptop().signed("POST", "/sync", None, r#"{"q":9}"#, false);
    let remove = |closed: Value| {
        leaf(
            0,
            "admin_remove",
            json!({"kind": "host"}),
            json!({
                "project_key": "acme/app", "rows": 2, "jobs_closed": closed,
                "backup": "recall-admin.db"
            }),
            Value::Null,
        )
    };
    assert!(verify(
        &export_of(&appended(vec![queued.clone(), remove(json!(["job_9"]))])),
        &[]
    )
    .ok());
    let verdict = verify(
        &export_of(&appended(vec![queued.clone(), remove(json!([]))])),
        &[],
    );
    refused_leaf(&verdict, 11, "left job_9 open");
    // A second remove of the same key, claiming the job the first closed.
    let verdict = verify(
        &export_of(&appended(vec![
            queued,
            remove(json!(["job_9"])),
            remove(json!(["job_9"])),
        ])),
        &[],
    );
    refused_leaf(&verdict, 12, "closed job_9 once it was done");
}

// ---------------------------------------------------------------------------
// the pieces
// ---------------------------------------------------------------------------

#[test]
fn a_saved_checkpoint_argument_reads_as_the_script_reads_it() {
    let root = STANDARD.encode([7u8; 32]);
    assert_eq!(
        parse_checkpoint_arg(&format!("12:{root}")),
        Some((12, [7u8; 32]))
    );
    assert_eq!(parse_checkpoint_arg(&format!("012:{root}")), None);
    assert_eq!(parse_checkpoint_arg(&format!("12 {root}")), None);
    assert_eq!(parse_checkpoint_arg("12:AAAA"), None);
    assert_eq!(
        parse_checkpoint_arg(&format!("12:{}", &root[..root.len() - 1])),
        None
    );
}

#[test]
fn base64_has_one_spelling() {
    // The same 32 bytes, with the padding's spare bits set: decodes
    // leniently elsewhere, refused here.
    let canonical = STANDARD.encode([0u8; 32]);
    let spare = format!("{}B=", &canonical[..canonical.len() - 2]);
    assert!(root_hash(&canonical).is_some());
    assert!(root_hash(&spare).is_none());
}

#[test]
fn the_query_is_read_as_python_reads_it() {
    assert_eq!(
        parse_qsl("project_key=acme%2Fapp&x=a+b&flag"),
        vec![
            ("project_key".to_string(), "acme/app".to_string()),
            ("x".to_string(), "a b".to_string()),
            ("flag".to_string(), String::new()),
        ]
    );
    assert_eq!(
        parse_qsl("a=%zz%4"),
        vec![("a".to_string(), "%zz%4".to_string())]
    );
    assert_eq!(quote("dev_a b/é"), "dev_a%20b%2F%C3%A9");
}

#[test]
fn the_signature_parameters_are_read_as_the_script_reads_them() {
    let (components, params) =
        parse_params_line(r#"("@method" "a)b");created=1;keyid="dev_x";alg="ed25519""#).unwrap();
    assert_eq!(components, ["@method", "a)b"]);
    assert_eq!(params.len(), 3);
    assert!(
        parse_params_line(r#"("@method"  "x")"#).is_none(),
        "two spaces"
    );
    assert!(parse_params_line(r#"("a\b")"#).is_none(), "a backslash");
    assert!(
        parse_params_line(r#"();Keyid="x""#).is_none(),
        "an upper-case name"
    );
    assert!(parse_params_line(r#"();keyid"#).is_none(), "no value");
    assert_eq!(parse_params_line("()").unwrap(), (vec![], vec![]));
}
