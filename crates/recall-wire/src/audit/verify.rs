//! Checking an exported audit log offline: the same checks as
//! `scripts/audit-verify.py`, in Rust, so `recall audit verify` needs
//! neither Python nor the network.
//!
//! An **export** is a first line holding the checkpoint it was taken at,
//! `<tree_size> <root_hash>` exactly as the `Recall-Audit-Checkpoint` header
//! carries it, then one leaf per line, its own compact JSON as the server
//! stored and hashed it, in `seq` order from 0. `recall audit export` writes
//! one; so does paging through `GET /v1/audit/checkpoint` and
//! `GET /v1/audit/entries` by hand. The Python script and this module read
//! the same file and are meant to agree on every one: the server's
//! integration tests run both over each honest log and each forgery they
//! build, and fail on any verdict the two do not share.
//!
//! What [`verify_export`] checks, in the script's order and words:
//!
//! 1. Every leaf is one JSON object, with no key given twice, of the shape
//!    leaf version 1 defines for its action and actor; `seq` is the integer
//!    position it is at; `at` never goes back.
//! 2. The root over every leaf is the checkpoint on the first line, and for
//!    every saved checkpoint given, the root over that many leaves is still
//!    that checkpoint's: a log that no longer extends one saved earlier has
//!    been rewritten.
//! 3. Every leaf a device acted in carries the request it signed, and the
//!    signature verifies over `signature_base` with the public key of that
//!    device's own `approve` or `enroll` leaf, an earlier one, of a device
//!    neither revoked nor swept before it: the log's own record, never a
//!    live `devices` table. The base's `keyid` is the actor, no `(keyid,
//!    nonce)` appears twice, its `content-digest` is `body_sha256`, its
//!    method, path and query are the action's, and a kept body hashes to
//!    `body_sha256` and asks for what `subject` says was done. A leaf
//!    nobody signed carries no request.
//! 4. What the server enforces holds, so a log it did not write fails: see
//!    `apply` below, one rule per refusal the server makes.
//!
//! The one place the two can differ is an authkey's tag sent with its
//! letters decomposed (a letter and its accent as two characters): the
//! server stores it composed (NFC), the script composes the body's before
//! comparing, and this module, which has no Unicode tables, compares bytes
//! and so refuses a log the script accepts. `recall authkey create` sends
//! what was typed, which a keyboard composes; the difference is written
//! down rather than paid for with those tables in every client.
//!
//! Two more, both where this module is the stricter and the server never
//! writes what they turn on: `"seq":-0`, which Python reads as the integer
//! 0 and `serde_json` as a float, so this refuses the leaf as having no
//! integer `seq`; and a lone surrogate escape such as `"\ud800"`, which
//! Python's `json` decodes into a string and `serde_json` refuses as not
//! JSON. Each is refused here and accepted there, never the other way
//! round.
//!
//! Every check is pinned by a test that fails without it: see
//! `verify_tests.rs`, and the server's `tests/audit.rs`, which runs this
//! beside the script over every forgery it knows.

use std::collections::{BTreeMap, HashSet};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use super::merkle::{self, Hash, Tree};
use crate::devices::{normalize_user_code, DEFAULT_MAX_DEVICES};
use crate::signature;

/// What [`verify_export`] found. The log checks out when
/// [`Verdict::problems`] is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// The export's first line, as it was written: the checkpoint it claims
    /// to have been taken at. Empty when there was none to read.
    pub checkpoint: String,
    /// How many leaves followed it.
    pub leaves: u64,
    /// How many leaves a device signed, each signature checked.
    pub signed: u64,
    /// Every problem, one line each, in the order found: a leaf's by its
    /// position, then the tree's.
    pub problems: Vec<String>,
}

impl Verdict {
    /// Whether everything checked out.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Reads a saved checkpoint given as `SIZE:ROOT`, the form
/// `scripts/audit-verify.py --checkpoint` takes. [`None`] if it is not one:
/// a size in decimal without leading zeros, and 32 bytes of standard,
/// padded base64.
pub fn parse_checkpoint_arg(text: &str) -> Option<(u64, Hash)> {
    let (size, root) = text.split_once(':')?;
    Some((decimal(size)?, root_hash(root)?))
}

/// A tree hash from its standard base64, as the API and a checkpoint carry
/// one; [`None`] unless it is exactly the canonical encoding of 32 bytes.
pub fn root_hash(text: &str) -> Option<Hash> {
    b64_strict(text, "", false, Some(32)).ok()?.try_into().ok()
}

/// `0` or a decimal without leading zeros, as a size: what the script's
/// `(0|[1-9][0-9]*)` reads.
fn decimal(text: &str) -> Option<u64> {
    let ok = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    ok.then(|| text.parse().ok()).flatten()
}

/// Checks an export, given every checkpoint saved from this server
/// earlier as `(tree_size, root)`. Never fails: anything wrong with the
/// export, its first line included, is a problem in the answer.
pub fn verify_export(export: &[u8], saved: &[(u64, Hash)]) -> Verdict {
    let mut verdict = Verdict::default();
    let mut lines: Vec<&[u8]> = export.split(|&b| b == b'\n').collect();
    // One trailing newline ends the last line; a second is an empty leaf.
    if lines.last() == Some(&&b""[..]) {
        lines.pop();
    }
    let Some((first, leaves)) = lines.split_first() else {
        verdict
            .problems
            .push("the export is empty: no checkpoint line".to_string());
        return verdict;
    };
    verdict.checkpoint = String::from_utf8_lossy(first).into_owned();
    let (size, root) = match read_checkpoint_line(first) {
        Ok(checkpoint) => checkpoint,
        Err(problem) => {
            verdict.problems.push(problem);
            return verdict;
        }
    };
    verdict.leaves = leaves.len() as u64;

    let mut state = State::default();
    for (position, raw) in leaves.iter().enumerate() {
        match check_leaf(raw, position as u64, &mut state) {
            Ok(signed) => verdict.signed += u64::from(signed),
            Err(problem) => verdict.problems.push(format!("leaf {position}: {problem}")),
        }
    }

    let tree = Tree::rebuild(leaves.iter().map(|raw| merkle::hash_leaf(raw)));
    let held = tree.size();
    if held != size {
        verdict.problems.push(format!(
            "the export holds {held} leaves but its checkpoint says tree_size {size}"
        ));
    } else if tree.root() != root {
        verdict.problems.push(format!(
            "the root over all {size} leaves is {}, the checkpoint says {} (a leaf changed, \
             removed or reordered)",
            STANDARD.encode(tree.root()),
            STANDARD.encode(root)
        ));
    }
    for (want_size, want_root) in saved {
        if *want_size > held {
            verdict.problems.push(format!(
                "a saved checkpoint at size {want_size} is past the end of this export ({held} \
                 leaves): the log does not extend it"
            ));
        } else if tree.root_at(*want_size) != *want_root {
            verdict.problems.push(format!(
                "the root over the first {want_size} leaves is {}, the saved checkpoint says {}: \
                 the log does not extend it",
                STANDARD.encode(tree.root_at(*want_size)),
                STANDARD.encode(want_root)
            ));
        }
    }
    verdict
}

/// The first line: `<tree_size> <root_hash>`.
fn read_checkpoint_line(line: &[u8]) -> Check<(u64, Hash)> {
    let text = std::str::from_utf8(line).ok().filter(|t| t.is_ascii());
    let parsed = text.and_then(|t| {
        let (size, root) = t.split_once(' ')?;
        let well_formed = !root.is_empty() && !root.bytes().any(|b| b.is_ascii_whitespace());
        let size = decimal(size)?;
        well_formed.then_some((size, root))
    });
    let Some((size, root)) = parsed else {
        return bad(format!(
            "the first line is not a checkpoint (\"<tree_size> <root_hash>\"): {:?}",
            String::from_utf8_lossy(line)
        ));
    };
    let root = b64_strict(root, "the checkpoint's root", false, Some(32))?;
    Ok((size, root.try_into().expect("32 bytes")))
}

// ---------------------------------------------------------------------------
// one leaf
// ---------------------------------------------------------------------------

type Check<T = ()> = Result<T, String>;

fn bad<T>(problem: impl Into<String>) -> Check<T> {
    Err(problem.into())
}

/// Checks the leaf at `position` against everything before it and records
/// what it changes. `Ok(true)` for a leaf a device signed.
///
/// The order is the script's, and matters where a check has a side effect:
/// `at` is taken as the newest before the leaf's request is looked at, and
/// a nonce is remembered before anything after it can fail, so a leaf
/// refused later still spends it. A leaf refused anywhere changes nothing
/// else: none of `apply`.
fn check_leaf(raw: &[u8], position: u64, state: &mut State) -> Check<bool> {
    let text = std::str::from_utf8(raw).map_err(|_| "the leaf is not UTF-8".to_string())?;
    let leaf = strict_json(text).map_err(|e| format!("the leaf is not JSON: {e}"))?;
    let leaf = Leaf::read(&leaf, position)?;
    if leaf.at < state.last_at.as_str() {
        return bad(format!("at {} is earlier than the leaf before it", leaf.at));
    }
    state.last_at = leaf.at.to_string();
    let signed = match leaf.kind {
        "device" => {
            check_request(&leaf, state)?;
            true
        }
        "session" => {
            check_session(&leaf, state)?;
            false
        }
        _ => false,
    };
    apply(&leaf, state)?;
    Ok(signed)
}

/// A leaf that has passed [`Leaf::read`]: version 1, at its position, of
/// its action's shape.
struct Leaf<'a> {
    at: &'a str,
    action: &'a str,
    kind: &'a str,
    actor: &'a Map<String, Value>,
    subject: &'a Map<String, Value>,
    request: &'a Value,
}

const LEAF_KEYS: &[&str] = &["v", "seq", "at", "action", "actor", "subject", "request"];
const FILE: &[&str] = &[
    "project_key",
    "file_path",
    "deleted",
    "stored_sha256",
    "base_sha256",
    "merged",
    "merge_job",
];
const DEVICE: &[&str] = &[
    "device_id",
    "name",
    "scope",
    "public_key",
    "fingerprint",
    "ephemeral",
    "authkey_id",
    "user_code",
];
const REQUEST_KEYS: &[&str] = &["body_sha256", "signature_base", "signature", "body"];
const SCOPES: &[&str] = &["sync", "admin", "worker"];

/// The keys `actor` has for each kind, in order.
fn actor_keys(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "device" => &["kind", "id", "name", "agent"],
        "operator" => &["kind"],
        "authkey" => &["kind", "id", "tag"],
        "session" => &["kind", "credential_id"],
        "server" => &["kind"],
        "host" => &["kind"],
        _ => return None,
    })
}

/// The keys `subject` has for each action, in order: [`None`] for an
/// action leaf version 1 does not have.
fn subject_keys(action: &str) -> Option<&'static [&'static str]> {
    Some(match action {
        "push" | "delete" => FILE,
        "pull" => &["project_key"],
        "approve" | "enroll" => DEVICE,
        "deny" => &["user_code", "name"],
        "revoke" | "sweep" => &["device_id", "name"],
        "authkey_create" => &["authkey_id", "tag", "ephemeral", "max_devices"],
        "authkey_revoke" => &["authkey_id", "revoke_devices", "revoked_devices"],
        "start" => &["version"],
        "job_claim" => &[
            "job_id",
            "kind",
            "attempt",
            "lease_expires_at",
            "project_key",
            "file_path",
        ],
        "job_result" => &[
            "job_id",
            "project_key",
            "file_path",
            "state",
            "stored_sha256",
            "follow_up",
        ],
        "job_retry" => &["job_id", "kind", "project_key", "file_path"],
        "passkey_add" => &["credential_id", "name", "first"],
        "passkey_remove" => &["credential_id", "name"],
        "sessions_end" => &["ended"],
        "bootstrap_code" => &["expires_at"],
        "passkey_reset" => &["passkeys_removed", "expires_at"],
        "admin_rename" => &["from", "to", "rows", "jobs_closed", "backup"],
        "admin_remove" => &["project_key", "rows", "jobs_closed", "backup"],
        "admin_restore" => &[
            "project_key",
            "source",
            "added",
            "overwritten",
            "deleted",
            "jobs_closed",
            "backup",
        ],
        _ => return None,
    })
}

/// Who may do what: a device or the operator through the API, the admin
/// page's passkey session on the routes that manage devices, an authkey
/// enrolling the one device it approves, the server on its own, the host
/// through `recall-server reset-passkeys` and `recall-server admin`.
fn may(kind: &str, action: &str) -> bool {
    let actors: &[&str] = match action {
        "push" | "delete" | "pull" => &["device", "operator"],
        "approve" | "deny" | "revoke" | "authkey_create" | "authkey_revoke" => {
            &["device", "operator", "session"]
        }
        "enroll" => &["authkey"],
        "sweep" | "start" | "bootstrap_code" => &["server"],
        // A worker, or the server merging without one; its own
        // housekeeping (a lease run out, a job failed for want of anything
        // to merge it) is a result too.
        "job_claim" | "job_result" => &["device", "server"],
        "job_retry" => &["device", "operator"],
        // The first passkey takes RECALL_TOKEN (and the bootstrap code);
        // every other passkey action, a session.
        "passkey_add" => &["operator", "session"],
        "passkey_remove" | "sessions_end" => &["session"],
        "passkey_reset" | "admin_rename" | "admin_remove" | "admin_restore" => &["host"],
        _ => &[],
    };
    actors.contains(&kind)
}

/// The actions whose request body the leaf keeps.
const KEEPS_BODY: &[&str] = &[
    "approve",
    "deny",
    "revoke",
    "authkey_create",
    "authkey_revoke",
];

/// The actions only an admin device may sign: the ones that keep their
/// body, and a job's retry.
fn admin_action(action: &str) -> bool {
    KEEPS_BODY.contains(&action) || action == "job_retry"
}

/// The actions a worker signs, and the only ones it may.
fn worker_action(action: &str) -> bool {
    matches!(action, "job_claim" | "job_result")
}

/// The host's changes to stored memory, run beside the server on its file.
fn host_action(action: &str) -> bool {
    matches!(action, "admin_rename" | "admin_remove" | "admin_restore")
}

impl<'a> Leaf<'a> {
    /// The leaf is version 1, at its position, of its action's shape.
    fn read(leaf: &'a Value, position: u64) -> Check<Leaf<'a>> {
        let obj = expect_keys(leaf, LEAF_KEYS, "the leaf")?;
        if !is_int(&obj["v"]) || obj["v"].as_u64() != Some(1) {
            return bad(format!("v is {}, not 1", obj["v"]));
        }
        if !is_int(&obj["seq"]) || obj["seq"].as_u64() != Some(position) {
            return bad(format!(
                "seq is {} at position {position} (a gap, a repeat, or a reordering)",
                obj["seq"]
            ));
        }
        let at = match obj["at"].as_str() {
            Some(at) if is_timestamp(at) => at,
            _ => return bad(format!("at is {}, not a timestamp", obj["at"])),
        };
        let action = match obj["action"].as_str() {
            Some(action) if subject_keys(action).is_some() => action,
            _ => return bad(format!("no action {} exists", obj["action"])),
        };
        let actor_value = &obj["actor"];
        let kind = actor_value
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| actor_keys(k).is_some());
        let Some(kind) = kind else {
            return bad(format!("the actor is {actor_value}"));
        };
        let keys = actor_keys(kind).expect("a known kind");
        let actor = expect_keys(actor_value, keys, "the actor")?;
        for key in &keys[1..] {
            expect_str(&actor[*key], &format!("actor.{key}"))?;
        }
        if !may(kind, action) {
            return bad(format!("a {kind} cannot {action}"));
        }

        let subject_keys = subject_keys(action).expect("a known action");
        let subject = expect_keys(&obj["subject"], subject_keys, "the subject")?;
        check_subject(action, kind, actor, subject)?;

        let request = &obj["request"];
        if kind == "device" {
            let r = expect_keys(request, REQUEST_KEYS, "a device's request")?;
            for key in ["body_sha256", "signature_base", "signature"] {
                expect_str(&r[key], &format!("request.{key}"))?;
            }
            if KEEPS_BODY.contains(&action) {
                expect_str(&r["body"], "request.body")?;
            } else if !r["body"].is_null() {
                return bad(format!("a {action} keeps its body"));
            }
        } else if !request.is_null() {
            return bad(format!(
                "the {kind} signs nothing, but the leaf carries a request"
            ));
        }
        Ok(Leaf {
            at,
            action,
            kind,
            actor,
            subject,
            request,
        })
    }

    fn subject_str(&self, key: &str) -> &'a str {
        self.subject[key].as_str().unwrap_or_default()
    }

    fn actor_id(&self) -> &'a str {
        self.actor
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    fn request_field(&self, key: &str) -> &'a Value {
        &self.request[key]
    }
}

/// The subject's own rules, by action.
fn check_subject(
    action: &str,
    kind: &str,
    actor: &Map<String, Value>,
    subject: &Map<String, Value>,
) -> Check {
    match action {
        "push" | "delete" => {
            for key in ["project_key", "file_path"] {
                expect_str(&subject[key], &format!("subject.{key}"))?;
            }
            if subject["deleted"] != Value::Bool(action == "delete") {
                return bad(format!(
                    "subject.deleted is {} on a {action}",
                    subject["deleted"]
                ));
            }
            if !subject["stored_sha256"].as_str().is_some_and(is_hex64) {
                return bad("subject.stored_sha256 is not a SHA-256");
            }
            let base = &subject["base_sha256"];
            if !base.is_null() && !base.as_str().is_some_and(is_hex64) {
                return bad("subject.base_sha256 is not a SHA-256");
            }
            expect_bool(&subject["merged"], "subject.merged")?;
            let merged = subject["merged"] == Value::Bool(true);
            if action == "delete" && merged {
                return bad("a delete says it merged");
            }
            let job = &subject["merge_job"];
            if !job.is_null() {
                expect_str(job, "subject.merge_job")?;
                if action == "delete" || merged {
                    return bad(format!(
                        "a {} queued a merge job",
                        if action == "delete" {
                            "delete"
                        } else {
                            "merged push"
                        }
                    ));
                }
            }
        }
        "approve" | "enroll" => {
            for key in ["device_id", "name", "scope", "fingerprint"] {
                expect_str(&subject[key], &format!("subject.{key}"))?;
            }
            let Some(public_key) = subject["public_key"].as_str() else {
                return bad("subject.public_key is not a string");
            };
            let key = b64_strict(public_key, "subject.public_key", true, Some(32))?;
            let fingerprint = format!(
                "SHA256:{}",
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&key))
            );
            if subject["fingerprint"].as_str() != Some(fingerprint.as_str()) {
                return bad("subject.fingerprint is not the public key's");
            }
            let scope = subject["scope"].as_str().unwrap_or_default();
            if !SCOPES.contains(&scope) {
                return bad(format!("subject.scope is {}", subject["scope"]));
            }
            expect_bool(&subject["ephemeral"], "subject.ephemeral")?;
            if action == "approve" {
                if subject["ephemeral"] == Value::Bool(true) || !subject["authkey_id"].is_null() {
                    return bad("an approved device names an authkey, or is ephemeral");
                }
                // The script's `normalize_user_code(x) != x`: a code already
                // in its one spelling, or a null, which normalizes to null.
                let code = &subject["user_code"];
                let normal = code.as_str().and_then(normalize_user_code);
                let same = match (normal, code) {
                    (None, Value::Null) => true,
                    (Some(n), Value::String(s)) => n == *s,
                    _ => false,
                };
                if !same {
                    return bad("subject.user_code is not a user code");
                }
            } else {
                if subject["authkey_id"] != actor["id"] || !subject["user_code"].is_null() {
                    return bad("an enrolled device does not name the authkey that enrolled it");
                }
                if scope != "sync" {
                    return bad(format!(
                        "an authkey enrolled a device with the {scope} scope"
                    ));
                }
            }
        }
        "authkey_create" => {
            expect_str(&subject["authkey_id"], "subject.authkey_id")?;
            expect_str(&subject["tag"], "subject.tag")?;
            expect_bool(&subject["ephemeral"], "subject.ephemeral")?;
            if !is_int(&subject["max_devices"]) || !count_at_least(&subject["max_devices"], 1) {
                return bad("subject.max_devices is not a count");
            }
        }
        "authkey_revoke" => {
            expect_str(&subject["authkey_id"], "subject.authkey_id")?;
            expect_bool(&subject["revoke_devices"], "subject.revoke_devices")?;
            let Some(revoked) = string_list(&subject["revoked_devices"]) else {
                return bad("subject.revoked_devices is not a list of ids");
            };
            if !revoked.is_empty() && subject["revoke_devices"] != Value::Bool(true) {
                return bad("devices were revoked though revoke_devices is false");
            }
        }
        "job_claim" => {
            expect_str(&subject["job_id"], "subject.job_id")?;
            expect_str(&subject["kind"], "subject.kind")?;
            if !is_int(&subject["attempt"]) || !count_at_least(&subject["attempt"], 1) {
                return bad("subject.attempt is not an attempt");
            }
            if !subject["lease_expires_at"]
                .as_str()
                .is_some_and(is_timestamp)
            {
                return bad("subject.lease_expires_at is not a timestamp");
            }
            if subject["kind"] == "merge" {
                expect_str(&subject["project_key"], "subject.project_key")?;
                expect_str(&subject["file_path"], "subject.file_path")?;
            }
        }
        "job_result" => {
            for key in ["job_id", "project_key", "file_path"] {
                expect_str(&subject[key], &format!("subject.{key}"))?;
            }
            let state = subject["state"].as_str().unwrap_or_default();
            if !matches!(state, "queued" | "done" | "failed") {
                return bad(format!("subject.state is {}", subject["state"]));
            }
            let stored = &subject["stored_sha256"];
            if !stored.is_null() {
                if !stored.as_str().is_some_and(is_hex64) {
                    return bad("subject.stored_sha256 is not a SHA-256");
                }
                if state != "done" || !subject["follow_up"].is_null() {
                    return bad(
                        "a result wrote the file but the job is not done, or has a follow-up",
                    );
                }
            }
            if !subject["follow_up"].is_null() {
                expect_str(&subject["follow_up"], "subject.follow_up")?;
            }
        }
        "passkey_add" | "passkey_remove" => {
            expect_str(&subject["credential_id"], "subject.credential_id")?;
            expect_str(&subject["name"], "subject.name")?;
            if action == "passkey_add" {
                expect_bool(&subject["first"], "subject.first")?;
                if subject["first"] != Value::Bool(kind == "operator") {
                    return bad("the first passkey is the operator's to add, and only the first");
                }
            }
        }
        "sessions_end" => {
            if !is_int(&subject["ended"]) || !count_at_least(&subject["ended"], 1) {
                return bad("subject.ended is not a count of sessions");
            }
        }
        "bootstrap_code" | "passkey_reset" => {
            if !subject["expires_at"].as_str().is_some_and(is_timestamp) {
                return bad("subject.expires_at is not a timestamp");
            }
            if action == "passkey_reset"
                && (!is_int(&subject["passkeys_removed"])
                    || !count_at_least(&subject["passkeys_removed"], 0))
            {
                return bad("subject.passkeys_removed is not a count");
            }
        }
        _ if host_action(action) => check_host_subject(action, subject)?,
        _ => {
            for (key, value) in subject {
                expect_str(value, &format!("subject.{key}"))?;
            }
        }
    }
    Ok(())
}

/// A rename's, remove's or restore's subject: its keys, counts that say
/// it changed something, the jobs it closed, and its backup by name.
fn check_host_subject(action: &str, subject: &Map<String, Value>) -> Check {
    if action == "admin_rename" {
        for key in ["from", "to"] {
            expect_str(&subject[key], &format!("subject.{key}"))?;
            if subject[key] == "" {
                return bad(format!("subject.{key} is empty"));
            }
        }
        if subject["from"] == subject["to"] {
            return bad("a rename onto the key it renames");
        }
    } else {
        expect_str(&subject["project_key"], "subject.project_key")?;
        if subject["project_key"] == "" {
            return bad("subject.project_key is empty");
        }
    }
    let changed = if action == "admin_restore" {
        expect_file_name(&subject["source"], "subject.source")?;
        let mut changed = 0u128;
        for key in ["added", "overwritten", "deleted"] {
            changed += u128::from(expect_count(&subject[key], &format!("subject.{key}"))?);
        }
        changed
    } else {
        u128::from(expect_count(&subject["rows"], "subject.rows")?)
    };
    if changed == 0 {
        return bad(format!(
            "an {action} that changed no row (one that has nothing to change is not made)"
        ));
    }
    let closed = subject["jobs_closed"].as_array().filter(|jobs| {
        jobs.iter()
            .all(|j| j.as_str().is_some_and(|s| !s.is_empty()))
    });
    let Some(closed) = closed else {
        return bad("subject.jobs_closed is not a list of job ids");
    };
    let distinct: HashSet<&str> = closed.iter().filter_map(Value::as_str).collect();
    if distinct.len() != closed.len() {
        return bad("subject.jobs_closed names a job twice");
    }
    expect_file_name(&subject["backup"], "subject.backup")
}

// ---------------------------------------------------------------------------
// a device's request
// ---------------------------------------------------------------------------

/// What the log has established by some point: its devices and keys, its
/// jobs and passkeys, and every nonce spent.
#[derive(Default)]
struct State {
    devices: BTreeMap<String, DeviceState>,
    /// Authkeys by id: whether revoked.
    authkeys: BTreeMap<String, bool>,
    jobs: BTreeMap<String, JobState>,
    /// The admin page's passkeys, by credential id.
    passkeys: HashSet<String>,
    /// Whether a bootstrap code is outstanding.
    bootstrap: bool,
    nonces: HashSet<(String, String)>,
    last_at: String,
}

struct DeviceState {
    public_key: [u8; 32],
    scope: String,
    ephemeral: bool,
    authkey_id: Option<String>,
    gone: bool,
}

struct JobState {
    file: (String, String),
    state: String,
    /// A worker's device id, or `server`.
    holder: Option<String>,
    attempt: u64,
}

/// The signature parameters the base names: integers and strings, the only
/// kinds Recall's `@signature-params` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Param {
    Int(i64),
    Str(String),
}

/// A device's leaf: its request is its device's, for this action.
fn check_request(leaf: &Leaf<'_>, state: &mut State) -> Check {
    let actor_id = leaf.actor_id();
    let base = leaf
        .request_field("signature_base")
        .as_str()
        .unwrap_or_default();
    let (values, params) = parse_signature_base(base)?;
    let keyid = params.get("keyid");
    if keyid != Some(&Param::Str(actor_id.to_string())) {
        return bad(format!(
            "the signature's keyid is {}, not the actor {actor_id:?}",
            show_param(keyid)
        ));
    }
    match params.get("alg") {
        None => {}
        Some(Param::Str(alg)) if alg == signature::ALGORITHM => {}
        other => return bad(format!("the signature's alg is {}", show_param(other))),
    }
    let nonce = match (params.get("nonce"), params.get("created")) {
        (Some(Param::Str(nonce)), Some(Param::Int(_)))
            if !nonce.is_empty() && nonce.chars().count() <= signature::MAX_NONCE_LEN =>
        {
            nonce
        }
        _ => return bad("the signature has no nonce or no created time"),
    };
    if !state
        .nonces
        .insert((actor_id.to_string(), nonce.to_string()))
    {
        return bad(format!(
            "the signed request with nonce {nonce:?} is in the log already (a replay)"
        ));
    }

    let body_sha256_text = leaf
        .request_field("body_sha256")
        .as_str()
        .unwrap_or_default();
    let body_sha256 = b64_strict(body_sha256_text, "request.body_sha256", false, Some(32))?;
    let sha256: Vec<&str> = values["content-digest"]
        .split(',')
        .map(py_strip)
        .filter_map(|m| m.strip_prefix("sha-256="))
        .collect();
    if sha256.len() != 1 || sha256[0] != format!(":{body_sha256_text}:") {
        return bad("the signed content-digest is not request.body_sha256");
    }
    if let Some(body) = leaf.request_field("body").as_str() {
        if Sha256::digest(body.as_bytes()).as_slice() != body_sha256.as_slice() {
            return bad("request.body does not hash to request.body_sha256");
        }
        check_body(leaf.action, leaf.subject, body)?;
    }

    let (method, path) = route_of(leaf);
    if values["@method"] != method || values["@path"] != path {
        return bad(format!(
            "the signed request was {} {}, not a {}",
            values["@method"], values["@path"], leaf.action
        ));
    }
    if leaf.action == "pull" {
        let query = values["@query"].as_str();
        let keys: Vec<String> = match query.strip_prefix('?') {
            Some(q) => parse_qsl(q)
                .into_iter()
                .filter(|(k, _)| k == "project_key")
                .map(|(_, v)| v)
                .collect(),
            None => Vec::new(),
        };
        if keys != [leaf.subject_str("project_key")] {
            return bad("the signed query does not name the pulled project");
        }
    }

    let Some(device) = state.devices.get(actor_id) else {
        return bad(format!(
            "signed by {actor_id}, which no earlier approve or enroll leaf made"
        ));
    };
    if device.gone {
        return bad(format!(
            "signed by {actor_id}, which was revoked or swept before it"
        ));
    }
    if admin_action(leaf.action) && device.scope != "admin" {
        return bad(format!(
            "a {} signed by {actor_id}, which does not have the admin scope",
            leaf.action
        ));
    }
    if worker_action(leaf.action) != (device.scope == "worker") {
        return bad(format!(
            "a {} signed by {actor_id}, whose scope is {}",
            leaf.action, device.scope
        ));
    }
    let sig = b64_strict(
        leaf.request_field("signature").as_str().unwrap_or_default(),
        "request.signature",
        false,
        Some(64),
    )?;
    let verified = signature::VerifyingKey::from_bytes(&device.public_key)
        .ok()
        .is_some_and(|key| signature::verify(&key, base, &sig).is_ok());
    if !verified {
        return bad(format!(
            "the signature does not verify with {actor_id}'s key"
        ));
    }
    Ok(())
}

fn show_param(param: Option<&Param>) -> String {
    match param {
        None => "missing".to_string(),
        Some(Param::Int(n)) => n.to_string(),
        Some(Param::Str(s)) => format!("{s:?}"),
    }
}

/// A signature base read: each covered component's value by name, and the
/// signature's parameters by name.
type SignatureBase = (BTreeMap<String, String>, BTreeMap<String, Param>);

/// The last line of a base read: the covered components in order, and each
/// parameter's name and unparsed value in order.
type ParamsLine = (Vec<String>, Vec<(String, String)>);

/// `{component: value}` and the `@signature-params` parameters, reading the
/// base the way RFC 9421 §2.5 builds it, and no other way: one line per
/// covered component, each `"<name>": <value>`, in the order the last line
/// lists them, and that last line.
fn parse_signature_base(base: &str) -> Check<SignatureBase> {
    if !base.is_ascii() {
        return bad("the signature base is not ASCII");
    }
    let mut lines: Vec<&str> = base.split('\n').collect();
    let last = lines.pop().unwrap_or_default();
    let Some(list) = last.strip_prefix("\"@signature-params\": ") else {
        return bad("the signature base does not end with @signature-params");
    };
    let Some((components, raw_params)) = parse_params_line(list) else {
        return bad("the signature parameters do not parse");
    };
    let mut params = BTreeMap::new();
    for (name, value) in raw_params {
        if params.contains_key(&name) {
            return bad(format!("the signature parameter {name} appears twice"));
        }
        let parsed = if is_int_param(&value) {
            Param::Int(value.parse().expect("at most 15 digits"))
        } else if value.len() >= 2
            && value.starts_with('"')
            && value.ends_with('"')
            && !value[1..value.len() - 1].contains(['"', '\\'])
        {
            Param::Str(value[1..value.len() - 1].to_string())
        } else {
            return bad(format!("the signature parameter {name} does not parse"));
        };
        params.insert(name, parsed);
    }
    let distinct: HashSet<&String> = components.iter().collect();
    if lines.len() != components.len() || distinct.len() != components.len() {
        return bad("the signature base's lines are not its covered components");
    }
    let mut values = BTreeMap::new();
    for (name, line) in components.iter().zip(&lines) {
        let head = format!("\"{name}\": ");
        let Some(value) = line.strip_prefix(&head) else {
            return bad(format!(
                "the signature base's line for {name} is out of place"
            ));
        };
        values.insert(name.clone(), value.to_string());
    }
    let missing: Vec<&str> = signature::COVERED_COMPONENTS
        .iter()
        .copied()
        .filter(|c| !values.contains_key(*c))
        .collect();
    if !missing.is_empty() {
        return bad(format!(
            "the signature does not cover {}",
            missing.join(", ")
        ));
    }
    Ok((values, params))
}

/// `("a" "b");k=v;k2="w"`: the covered components, a quoted name each
/// with no quote or backslash inside, one space apart, and then any number
/// of `;name=value` parameters, the name `[a-z*][a-z0-9_.*-]*` and the
/// value anything up to the next `;`. The script's regular expression,
/// read by hand.
fn parse_params_line(text: &str) -> Option<ParamsLine> {
    let mut rest = text.strip_prefix('(')?;
    let mut components = Vec::new();
    if let Some(after) = rest.strip_prefix(')') {
        rest = after;
    } else {
        loop {
            let inner = rest.strip_prefix('"')?;
            let end = inner.find(['"', '\\'])?;
            if inner.as_bytes()[end] != b'"' {
                return None;
            }
            components.push(inner[..end].to_string());
            rest = &inner[end + 1..];
            if let Some(after) = rest.strip_prefix(')') {
                rest = after;
                break;
            }
            rest = rest.strip_prefix(' ')?;
        }
    }
    let mut params = Vec::new();
    while !rest.is_empty() {
        let part = rest.strip_prefix(';')?;
        let end = part.find(';').unwrap_or(part.len());
        let (name, value) = part[..end].split_once('=')?;
        let mut chars = name.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '*');
        let rest_ok = chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '*' | '-')
        });
        if !(first_ok && rest_ok) {
            return None;
        }
        params.push((name.to_string(), value.to_string()));
        rest = &part[end..];
    }
    Some((components, params))
}

/// `-?[0-9]{1,15}`.
fn is_int_param(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    (1..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// The method and path an action's request is sent to.
fn route_of(leaf: &Leaf<'_>) -> (&'static str, String) {
    let id = |key: &str| quote(leaf.subject_str(key));
    match leaf.action {
        "revoke" => ("POST", format!("/v1/devices/{}/revoke", id("device_id"))),
        "authkey_revoke" => ("POST", format!("/v1/authkeys/{}/revoke", id("authkey_id"))),
        "job_result" => ("POST", format!("/v1/jobs/{}/result", id("job_id"))),
        "job_retry" => ("POST", format!("/v1/jobs/{}/retry", id("job_id"))),
        "push" | "delete" => ("POST", "/sync".to_string()),
        "pull" => ("GET", "/sync".to_string()),
        "approve" => ("POST", "/v1/devices/approve".to_string()),
        "deny" => ("POST", "/v1/devices/deny".to_string()),
        "authkey_create" => ("POST", "/v1/authkeys".to_string()),
        "job_claim" => ("POST", "/v1/jobs/claim".to_string()),
        // Nothing else is signed: `may` lets no device do it.
        _ => ("", String::new()),
    }
}

/// Python's `urllib.parse.quote(text, safe='')`: every byte but ASCII
/// letters, digits and `_.-~` percent-encoded, in upper case.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Python's `urllib.parse.parse_qsl(query, keep_blank_values=True)`: pairs
/// split on `&`, each at its first `=` (a pair without one has an empty
/// value), `+` read as a space and then percent-decoded, with bytes that
/// are not UTF-8 replaced.
fn parse_qsl(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (unquote_plus(k), unquote_plus(v))
        })
        .collect()
}

fn unquote_plus(text: &str) -> String {
    let bytes = text.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the kept body asked for is what the subject says was done.
fn check_body(action: &str, subject: &Map<String, Value>, body: &str) -> Check {
    if action == "revoke" {
        // Its id is in the path.
        return Ok(());
    }
    let empty = Map::new();
    let parsed;
    let asked: &Map<String, Value> =
        if action == "authkey_revoke" && body.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
            &empty
        } else {
            parsed = strict_json(body).map_err(|e| format!("the request body is not JSON: {e}"))?;
            match &parsed {
                Value::Object(map) => map,
                _ => return bad("the request body is not an object"),
            }
        };
    if matches!(action, "approve" | "deny") {
        let asked_code = asked
            .get("user_code")
            .and_then(Value::as_str)
            .and_then(normalize_user_code);
        let same = match (asked_code, &subject["user_code"]) {
            (None, Value::Null) => true,
            (Some(code), Value::String(s)) => code == *s,
            _ => false,
        };
        if !same {
            return bad("the body names another user code");
        }
    }
    match action {
        "approve" => {
            let scope = asked
                .get("scope")
                .cloned()
                .unwrap_or_else(|| Value::from("sync"));
            if scope != subject["scope"] {
                return bad("the body asked for another scope");
            }
            match asked.get("fingerprint") {
                None | Some(Value::Null) => {}
                Some(Value::String(f)) if py_strip(f) == subject["fingerprint"] => {}
                Some(_) => return bad("the body names another fingerprint"),
            }
        }
        "authkey_create" => {
            let tag = asked.get("tag").cloned().unwrap_or_else(|| Value::from(""));
            // The server stores a tag trimmed and composed (NFC); see the
            // module documentation for why this compares bytes.
            if tag.as_str().map(py_strip) != subject["tag"].as_str() {
                return bad("the body asked for another tag");
            }
            let ephemeral = asked.get("ephemeral").unwrap_or(&Value::Bool(true));
            if *ephemeral != subject["ephemeral"] || !ephemeral.is_boolean() {
                return bad("the body asked for another ephemeral");
            }
            // The script's `(asked.get("max_devices") or 25) != subject`,
            // with Python's truthiness and its numbers: `true` is 1, and
            // 3.0 is 3.
            let max = match asked.get("max_devices") {
                None => Value::from(DEFAULT_MAX_DEVICES),
                Some(v) if !py_truthy(v) => Value::from(DEFAULT_MAX_DEVICES),
                Some(v) => v.clone(),
            };
            if !py_number_eq(&max, &subject["max_devices"]) {
                return bad("the body asked for another max_devices");
            }
        }
        "authkey_revoke" => {
            let asked_revoke = asked.get("revoke_devices").unwrap_or(&Value::Bool(false));
            if !asked_revoke.is_boolean() || *asked_revoke != subject["revoke_devices"] {
                return bad("the body asked for another revoke_devices");
            }
        }
        _ => {}
    }
    Ok(())
}

/// Python's `str.strip()` for ASCII text: spaces, tabs, line and page
/// breaks, and the four separators below 0x20 it also counts as space.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// Python's truthiness, for what a JSON body can hold.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `==` between what a body asked for and the subject's integer,
/// exactly: an integer by its value, never through a float, so 2^53 + 1 is
/// not 2^53; a float only when it is a whole number a `u64` can hold, as
/// Python compares `3.0 == 3`; a boolean as 0 or 1; anything else unequal.
/// A JSON integer past `u64` reaches here as a float, and never equals a
/// subject, which is at most `u64::MAX`.
fn py_number_eq(asked: &Value, subject: &Value) -> bool {
    let Some(s) = subject.as_u64() else {
        return false;
    };
    match asked {
        Value::Bool(b) => u64::from(*b) == s,
        Value::Number(n) => match (n.as_u64(), n.as_i64(), n.as_f64()) {
            (Some(a), _, _) => a == s,
            // A negative integer: no subject is one.
            (None, Some(_), _) => false,
            (None, None, Some(f)) => {
                f.fract() == 0.0
                    && (0.0..18_446_744_073_709_551_616.0).contains(&f)
                    && f as u64 == s
            }
            _ => false,
        },
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// what each leaf changes
// ---------------------------------------------------------------------------

/// An admin session acts with a passkey the log added and still holds.
fn check_session(leaf: &Leaf<'_>, state: &State) -> Check {
    let credential = leaf.actor["credential_id"].as_str().unwrap_or_default();
    if !state.passkeys.contains(credential) {
        return bad(format!(
            "an admin session with passkey {credential}, which the log never added or removed \
             since"
        ));
    }
    Ok(())
}

/// What the leaf changes about the devices, keys, jobs and passkeys that
/// later leaves are checked against, refusing what the server refuses.
fn apply(leaf: &Leaf<'_>, state: &mut State) -> Check {
    let subject = leaf.subject;
    match leaf.action {
        "approve" | "enroll" => {
            let device_id = leaf.subject_str("device_id");
            if state.devices.contains_key(device_id) {
                return bad(format!("{device_id} was approved or enrolled before"));
            }
            if leaf.action == "enroll" {
                let authkey = leaf.actor_id();
                if state.authkeys.get(authkey) != Some(&false) {
                    return bad(format!(
                        "enrolled by {authkey}, which no leaf created, or which was revoked"
                    ));
                }
            }
            let key = b64_strict(
                leaf.subject_str("public_key"),
                "subject.public_key",
                true,
                Some(32),
            )?;
            state.devices.insert(
                device_id.to_string(),
                DeviceState {
                    public_key: key.try_into().expect("32 bytes"),
                    scope: leaf.subject_str("scope").to_string(),
                    ephemeral: subject["ephemeral"] == Value::Bool(true),
                    authkey_id: subject["authkey_id"].as_str().map(str::to_string),
                    gone: false,
                },
            );
        }
        "revoke" | "sweep" => {
            let device_id = leaf.subject_str("device_id");
            let Some(device) = state.devices.get_mut(device_id) else {
                return bad(format!(
                    "a {} of {device_id}, which no leaf made",
                    leaf.action
                ));
            };
            if leaf.action == "revoke" && device.gone {
                return bad(format!("{device_id} was revoked before"));
            }
            if leaf.action == "sweep" && !device.ephemeral {
                return bad(format!("a sweep of {device_id}, which is not ephemeral"));
            }
            device.gone = true;
        }
        "authkey_create" => {
            let id = leaf.subject_str("authkey_id");
            if state.authkeys.contains_key(id) {
                return bad(format!("{id} was created before"));
            }
            state.authkeys.insert(id.to_string(), false);
        }
        "authkey_revoke" => {
            let id = leaf.subject_str("authkey_id");
            let revoked = string_list(&subject["revoked_devices"]).unwrap_or_default();
            let Some(was_revoked) = state.authkeys.get_mut(id) else {
                return bad(format!("a revoke of {id}, which no leaf created"));
            };
            if *was_revoked && revoked.is_empty() {
                return bad(format!("{id} was revoked before, and nothing else changed"));
            }
            *was_revoked = true;
            for device_id in revoked {
                let live = state
                    .devices
                    .get_mut(device_id)
                    .filter(|d| d.authkey_id.as_deref() == Some(id) && !d.gone);
                let Some(device) = live else {
                    return bad(format!("{device_id} is not a live device {id} enrolled"));
                };
                device.gone = true;
            }
        }
        "push" | "delete" => {
            let file = (
                leaf.subject_str("project_key").to_string(),
                leaf.subject_str("file_path").to_string(),
            );
            if leaf.action == "delete" {
                // A delete closes the file's open jobs in its own
                // transaction.
                for job in state.jobs.values_mut() {
                    if job.file == file && is_open(&job.state) {
                        job.state = "done".to_string();
                        job.holder = None;
                    }
                }
            } else if let Some(job_id) = subject["merge_job"].as_str() {
                queue(state, job_id, file)?;
            }
        }
        "job_claim" | "job_result" | "job_retry" => apply_job(leaf, state)?,
        "passkey_add" | "passkey_remove" | "sessions_end" | "bootstrap_code" | "passkey_reset" => {
            apply_passkey(leaf, state)?
        }
        action if host_action(action) => apply_host(leaf, state)?,
        _ => {}
    }
    Ok(())
}

fn is_open(state: &str) -> bool {
    matches!(state, "queued" | "leased")
}

fn queue(state: &mut State, job_id: &str, file: (String, String)) -> Check {
    if state.jobs.contains_key(job_id) {
        return bad(format!("{job_id} was queued before"));
    }
    state.jobs.insert(
        job_id.to_string(),
        JobState {
            file,
            state: "queued".to_string(),
            holder: None,
            attempt: 0,
        },
    );
    Ok(())
}

/// A job's leaf follows from what the log says of that job so far.
fn apply_job(leaf: &Leaf<'_>, state: &mut State) -> Check {
    let job_id = leaf.subject_str("job_id");
    let action = leaf.action;
    // The server's own claims and results are held by "server".
    let who = if leaf.kind == "device" {
        leaf.actor_id().to_string()
    } else {
        "server".to_string()
    };
    let gone = |state: &State, holder: &str| state.devices.get(holder).is_some_and(|d| d.gone);
    let Some(job) = state.jobs.get(job_id) else {
        return bad(format!(
            "a {action} of {job_id}, which no push or result queued"
        ));
    };
    let named = (
        leaf.subject_str("project_key").to_string(),
        leaf.subject_str("file_path").to_string(),
    );
    if named != job.file {
        return bad(format!(
            "a {action} of {job_id} names another file than the one it was queued for"
        ));
    }
    match action {
        "job_claim" => {
            if job.state == "leased" {
                // The server frees a revoked worker's leases, and its own
                // after a restart, before it claims them again; nothing
                // frees a live worker's.
                let holder = job.holder.clone().unwrap_or_default();
                if holder != "server" && !gone(state, &holder) {
                    return bad(format!("{job_id} was claimed while {holder} held it"));
                }
            } else if job.state != "queued" {
                return bad(format!("{job_id} was claimed once it was {}", job.state));
            }
            let attempt = leaf.subject["attempt"].as_u64().unwrap_or_default();
            if attempt != job.attempt + 1 {
                return bad(format!(
                    "{job_id} was claimed at attempt {attempt}, not {}",
                    job.attempt + 1
                ));
            }
            let job = state.jobs.get_mut(job_id).expect("looked up above");
            job.state = "leased".to_string();
            job.holder = Some(who);
            job.attempt = attempt;
        }
        "job_result" => {
            if leaf.kind == "device"
                && (job.state != "leased" || job.holder.as_deref() != Some(who.as_str()))
            {
                return bad(format!(
                    "a result for {job_id} from {who}, which does not hold it"
                ));
            }
            if !is_open(&job.state) {
                return bad(format!("a result for {job_id} once it was {}", job.state));
            }
            let file = job.file.clone();
            let job = state.jobs.get_mut(job_id).expect("looked up above");
            job.state = leaf.subject_str("state").to_string();
            job.holder = None;
            if let Some(follow_up) = leaf.subject["follow_up"].as_str() {
                queue(state, follow_up, file)?;
            }
        }
        _ => {
            if job.state != "failed" {
                return bad(format!("{job_id} was retried while {}", job.state));
            }
            let job = state.jobs.get_mut(job_id).expect("looked up above");
            job.state = "queued".to_string();
            job.holder = None;
            job.attempt = 0;
        }
    }
    Ok(())
}

/// The admin page's passkeys, as the log has added and removed them.
fn apply_passkey(leaf: &Leaf<'_>, state: &mut State) -> Check {
    let subject = leaf.subject;
    match leaf.action {
        "bootstrap_code" => {
            if !state.passkeys.is_empty() {
                return bad("a bootstrap code was issued with a passkey registered");
            }
            state.bootstrap = true;
        }
        "passkey_reset" => {
            let removed = subject["passkeys_removed"].as_u64().unwrap_or_default();
            if removed != state.passkeys.len() as u64 {
                return bad(format!(
                    "a reset says it removed {removed} passkeys, but {} were registered",
                    state.passkeys.len()
                ));
            }
            state.passkeys.clear();
            state.bootstrap = true;
        }
        "passkey_add" => {
            let credential = leaf.subject_str("credential_id");
            if state.passkeys.contains(credential) {
                return bad(format!("passkey {credential} was added twice"));
            }
            if subject["first"] == Value::Bool(true) {
                if !state.passkeys.is_empty() || !state.bootstrap {
                    return bad(
                        "a first passkey added beside another, or with no bootstrap code \
                         outstanding",
                    );
                }
                state.bootstrap = false;
            }
            state.passkeys.insert(credential.to_string());
        }
        "passkey_remove" => {
            let credential = leaf.subject_str("credential_id");
            if !state.passkeys.contains(credential) {
                return bad(format!(
                    "a removal of passkey {credential}, which is not registered"
                ));
            }
            if state.passkeys.len() == 1 {
                return bad("the last passkey was removed");
            }
            state.passkeys.remove(credential);
        }
        _ => {}
    }
    Ok(())
}

/// A rename, remove or restore closes, in its own transaction, the jobs
/// open for the rows it touched: each one it names must be open and of its
/// key, and a rename or a remove, which touch every row of the key, leave
/// none of that key open. A restore touches only the paths it writes,
/// which the leaf does not list, so the jobs it leaves open are not
/// checked.
fn apply_host(leaf: &Leaf<'_>, state: &mut State) -> Check {
    let action = leaf.action;
    let key = if action == "admin_rename" {
        leaf.subject_str("from")
    } else {
        leaf.subject_str("project_key")
    };
    let closed: Vec<&str> = leaf.subject["jobs_closed"]
        .as_array()
        .map(|jobs| jobs.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for job_id in &closed {
        let Some(job) = state.jobs.get_mut(*job_id) else {
            return bad(format!(
                "an {action} closed {job_id}, which no push or result queued"
            ));
        };
        if !is_open(&job.state) {
            return bad(format!(
                "an {action} closed {job_id} once it was {}",
                job.state
            ));
        }
        if job.file.0 != key {
            return bad(format!(
                "an {action} of {key:?} closed {job_id}, a job of {:?}",
                job.file.0
            ));
        }
        job.state = "done".to_string();
        job.holder = None;
    }
    if action != "admin_restore" {
        let left = state.jobs.iter().find(|(id, job)| {
            job.file.0 == key && is_open(&job.state) && !closed.contains(&id.as_str())
        });
        if let Some((job_id, _)) = left {
            return bad(format!("an {action} of {key:?} left {job_id} open"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// reading JSON and base64 as strictly as the script does
// ---------------------------------------------------------------------------

/// JSON, refusing an object that gives a key twice: readers resolve that
/// differently (the first wins in one, the last in another), so a leaf
/// that does it could say one thing to this verifier and another to the
/// next. Object keys keep their order, which the shape checks read.
fn strict_json(text: &str) -> Result<Value, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_str(text);
    let value = <StrictValue as serde::Deserialize>::deserialize(&mut de)?;
    de.end()?;
    Ok(value.0)
}

struct StrictValue(Value);

impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("a number that is not finite"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(StrictValue(v)) = seq.next_element()? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "the key {key:?} appears twice in one object"
                )));
            }
            let StrictValue(value) = map.next_value()?;
            out.insert(key, value);
        }
        Ok(Value::Object(out))
    }
}

/// Base64 that is exactly the canonical encoding of what it decodes to:
/// standard and padded, or URL-safe and unpadded, and no other spelling of
/// the same bytes, so one value has one form. `length`, when given, is
/// how many bytes it must be.
fn b64_strict(text: &str, what: &str, urlsafe: bool, length: Option<usize>) -> Check<Vec<u8>> {
    let decoded = if urlsafe {
        URL_SAFE_NO_PAD.decode(text)
    } else {
        STANDARD.decode(text)
    };
    let raw = match decoded {
        Ok(raw) => raw,
        Err(e) => return bad(format!("{what} is not base64: {e}")),
    };
    let again = if urlsafe {
        URL_SAFE_NO_PAD.encode(&raw)
    } else {
        STANDARD.encode(&raw)
    };
    if again != text {
        return bad(format!("{what} is not base64: not canonical"));
    }
    if let Some(n) = length {
        if raw.len() != n {
            return bad(format!("{what} is {} bytes, not {n}", raw.len()));
        }
    }
    Ok(raw)
}

// ---------------------------------------------------------------------------
// small shape checks
// ---------------------------------------------------------------------------

/// An integer, as the script's `type(value) is int`: not a boolean, and
/// not `3.0`.
fn is_int(value: &Value) -> bool {
    value.is_i64() || value.is_u64()
}

/// An integer of at least `min`.
fn count_at_least(value: &Value, min: i64) -> bool {
    match (value.as_i64(), value.as_u64()) {
        (Some(n), _) => n >= min,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn expect_keys<'a>(value: &'a Value, keys: &[&str], what: &str) -> Check<&'a Map<String, Value>> {
    let Some(obj) = value.as_object() else {
        return bad(format!("{what} is not an object"));
    };
    if !obj.keys().map(String::as_str).eq(keys.iter().copied()) {
        let have: Vec<&str> = obj.keys().map(String::as_str).collect();
        return bad(format!("{what} has the keys {have:?}, not {keys:?}"));
    }
    Ok(obj)
}

fn expect_str(value: &Value, what: &str) -> Check {
    if value.is_string() {
        Ok(())
    } else {
        bad(format!("{what} is not a string"))
    }
}

fn expect_bool(value: &Value, what: &str) -> Check {
    if value.is_boolean() {
        Ok(())
    } else {
        bad(format!("{what} is not true or false"))
    }
}

fn expect_count(value: &Value, what: &str) -> Check<u64> {
    match value.as_u64() {
        Some(n) if is_int(value) => Ok(n),
        _ => bad(format!("{what} is not a count")),
    }
}

/// A file's name alone: the leaf names a backup, never where it is.
fn expect_file_name(value: &Value, what: &str) -> Check {
    match value.as_str() {
        Some(name) if !name.is_empty() && !name.contains('/') && name != "." && name != ".." => {
            Ok(())
        }
        _ => bad(format!("{what} is not a file name")),
    }
}

/// A list whose every item is a string.
fn string_list(value: &Value) -> Option<Vec<&str>> {
    value.as_array()?.iter().map(Value::as_str).collect()
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the one timestamp format the API writes.
fn is_timestamp(text: &str) -> bool {
    const SHAPE: &[u8] = b"0000-00-00T00:00:00.000Z";
    text.len() == SHAPE.len()
        && text.bytes().zip(SHAPE).all(|(b, &s)| match s {
            b'0' => b.is_ascii_digit(),
            other => b == other,
        })
}

/// A SHA-256 as lowercase hex.
fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
