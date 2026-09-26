#!/usr/bin/env python3
"""Verifies an exported audit log offline.

    python3 scripts/audit-verify.py audit.jsonl --checkpoint 1042:CsUY...=

Reads an export: a first line holding the checkpoint it was taken at,
`<tree_size> <root_hash>` exactly as the `Recall-Audit-Checkpoint` header
carries it, then one leaf per line, its own compact JSON, in `seq` order
from 0 — what paging through `GET /v1/audit/checkpoint` and
`GET /v1/audit/entries` and writing each out gives.

This is a second implementation of the tree hash in
`crates/recall-server/src/audit/merkle.rs` (RFC 9162 Section 2.1, SHA-256,
`SHA-256(0x00 || leaf)` for a leaf and `SHA-256(0x01 || left || right)` for
a node, splitting at the largest power of two below a tree's size), using
nothing but the standard library, so a bug shared by both implementations
is far less likely than a bug in one. It checks:

1. Every leaf is one JSON object, with no key given twice, of the shape
   leaf version 1 defines for its action and actor; `seq` is the integer
   position it is at; `at` never goes back.
2. The root over every leaf is the checkpoint on the first line, and, for
   every checkpoint given with `--checkpoint`, the root over that many
   leaves is still that checkpoint's: a log that no longer extends one you
   saved has been rewritten.
3. Every leaf a device acted in carries the request it signed, and:
   - the signature verifies over `signature_base` with the public key of
     that device's `approve` or `enroll` leaf, an earlier one, from a
     device neither revoked nor swept before it — the log's own record,
     never a live `devices` table;
   - the base's `keyid` is the actor, no (keyid, nonce) appears twice, and
     its `content-digest` line is `body_sha256`;
   - its method, path and query are the action's route: a pull's
     `project_key` is the query's, a revoke's or a job's id the path's;
   - where the leaf keeps the body (the device-management actions), the
     body hashes to `body_sha256` and says what `subject` says.
   A leaf nobody signed (the operator's, an authkey's, an admin session's,
   the server's, the host's) must carry no request.
4. The rest the server enforces, so a log it did not write fails: an id
   approved or enrolled twice, a device-management action signed by a
   device without the admin scope, a device revoked twice, an enrolment
   by an authkey never created or already revoked; a job action signed by
   a device that is not a worker, or a worker signing anything else; a job
   no push, result or evaluation queued, claimed while a live worker holds it or once
   it is settled, settled by a worker that does not hold it, retried
   before it failed, or counted at the wrong attempt; an admin session
   acting with a passkey the log never added or already removed; a first
   passkey added with no bootstrap code outstanding or beside another, the
   last passkey removed, or a reset that miscounts the passkeys it removed;
   a host's rename, remove or restore (`recall-server admin`) that changed
   no row, closed a job no push queued, one already settled or one of
   another key, or, for a rename or a remove, left a job of its key open.

What a signature proves is that a device sent a request with that method,
path, query and body digest — not what the server did with it. A push's or
a delete's body is not in the log, so which file it changed is the server's
word; see "Audit" in docs/reference/api.md.

Signatures are checked with Ed25519 from the `cryptography` package when it
imports and passes a self-test, and otherwise with the implementation below
(RFC 8032, the standard library only, some milliseconds a signature).
`--ed25519` chooses one outright. They are never skipped unless
`--no-signatures` says so; `--self-test` checks the chosen one against RFC
8032's test vectors and exits.

Exit status: 0 and "OK" when everything checks out; 1, with each problem
on stderr, when something does not; 2 when the check could not be run as
asked (a usage error, an unreadable file, or no working Ed25519).
"""

import argparse
import base64
import binascii
import hashlib
import json
import re
import sys
import unicodedata
import urllib.parse

EXIT_OK, EXIT_FAILED, EXIT_UNUSABLE = 0, 1, 2


class Unusable(Exception):
    """The check cannot run as asked: exit 2, not a verdict on the log."""


# ---------------------------------------------------------------------------
# The tree: RFC 9162 Section 2.1
# ---------------------------------------------------------------------------


def hash_leaf(leaf: bytes) -> bytes:
    return hashlib.sha256(b"\x00" + leaf).digest()


def hash_children(left: bytes, right: bytes) -> bytes:
    return hashlib.sha256(b"\x01" + left + right).digest()


def roots_at(leaf_hashes, sizes):
    """{size: MTH of the first `size` leaves}, for each size asked, in one
    pass: the complete subtrees so far, folded smallest first whenever a
    size asked for is reached (RFC 9162 Section 2.1.2's stack)."""
    wanted = set(sizes)
    out = {}
    if 0 in wanted:
        out[0] = hashlib.sha256(b"").digest()
    stack = []  # (subtree size, hash), largest first
    for i, h in enumerate(leaf_hashes):
        node = (1, h)
        while stack and stack[-1][0] == node[0]:
            left = stack.pop()
            node = (left[0] * 2, hash_children(left[1], node[1]))
        stack.append(node)
        if i + 1 in wanted:
            acc = stack[-1][1]
            for _, left in reversed(stack[:-1]):
                acc = hash_children(left, acc)
            out[i + 1] = acc
    return out


# ---------------------------------------------------------------------------
# Ed25519: RFC 8032 Section 5.1.7, as strict as the server's verify_strict
# ---------------------------------------------------------------------------

P = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)
IDENTITY = (0, 1, 1, 0)


def _add(a, b):
    """Extended twisted Edwards coordinates, as RFC 8032 Section 6 adds."""
    x = (a[1] - a[0]) * (b[1] - b[0]) % P
    y = (a[1] + a[0]) * (b[1] + b[0]) % P
    c = 2 * a[3] * b[3] * D % P
    d = 2 * a[2] * b[2] % P
    e, f, g, h = y - x, d - c, d + c, y + x
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def _mul(s, point):
    q = IDENTITY
    while s:
        if s & 1:
            q = _add(q, point)
        point = _add(point, point)
        s >>= 1
    return q


def _same(a, b):
    return (a[0] * b[2] - b[0] * a[2]) % P == 0 and (a[1] * b[2] - b[1] * a[2]) % P == 0


def _decode_point(raw: bytes):
    """The point `raw` encodes, or None: RFC 8032 Section 5.1.3, which
    refuses y >= p and a negative zero x, so each point has one encoding."""
    y = int.from_bytes(raw, "little")
    sign, y = y >> 255, y & ((1 << 255) - 1)
    if y >= P:
        return None
    x2 = (y * y - 1) * pow(D * y * y + 1, P - 2, P) % P
    if x2 == 0:
        if sign:
            return None
        return (0, y, 1, 0)
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P:
        return None
    if x & 1 != sign:
        x = P - x
    return (x, y, 1, x * y % P)


_BASE_Y = 4 * pow(5, P - 2, P) % P
BASE = _decode_point(_BASE_Y.to_bytes(32, "little"))


def builtin_verify(public: bytes, message: bytes, signature: bytes) -> bool:
    """Ed25519 verification that refuses what ed25519-dalek's verify_strict
    refuses: a non-canonical point or scalar, a key or an R of small order
    (under which any signature verifies), and anything but the cofactorless
    equation [S]B = R + [k]A."""
    if len(public) != 32 or len(signature) != 64:
        return False
    a = _decode_point(public)
    r = _decode_point(signature[:32])
    if a is None or r is None:
        return False
    if _same(_mul(8, a), IDENTITY) or _same(_mul(8, r), IDENTITY):
        return False
    s = int.from_bytes(signature[32:], "little")
    if s >= L:
        return False
    k = int.from_bytes(hashlib.sha512(signature[:32] + public + message).digest(), "little") % L
    return _same(_mul(s, BASE), _add(r, _mul(k, a)))


# RFC 8032 Section 7.1, TEST 1 to 3: (public key, message, signature).
RFC8032_VECTORS = [
    (
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155"
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da"
        "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac"
        "18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
]


def self_test(verify) -> bool:
    """Every vector verifies, and none does with a bit of its signature,
    its message or its S (plus L, the malleable twin) changed."""
    for pk, msg, sig in RFC8032_VECTORS:
        pk, msg, sig = bytes.fromhex(pk), bytes.fromhex(msg), bytes.fromhex(sig)
        if not verify(pk, msg, sig):
            return False
        flipped = bytearray(sig)
        flipped[5] ^= 1
        if verify(pk, msg, bytes(flipped)) or verify(pk, msg + b"x", sig):
            return False
        s_plus_l = (int.from_bytes(sig[32:], "little") + L).to_bytes(32, "little")
        if verify(pk, msg, sig[:32] + s_plus_l):
            return False
    return True


def cryptography_verify():
    """Ed25519 from the `cryptography` package, or Unusable. A broken
    install can raise anything, a Rust panic included, which pyo3 raises as
    a BaseException precisely so `except Exception` does not swallow it."""
    try:
        from cryptography.exceptions import InvalidSignature
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
    except BaseException as e:  # noqa: BLE001
        raise Unusable(f"the cryptography package is not usable here ({type(e).__name__}: {e})")

    def verify(public: bytes, message: bytes, signature: bytes) -> bool:
        try:
            Ed25519PublicKey.from_public_bytes(public).verify(signature, message)
            return True
        except (InvalidSignature, ValueError):
            return False

    return verify


def choose_ed25519(choice: str):
    """(name, verify) for `--ed25519`; Unusable if the one asked for does
    not work. `auto` prefers `cryptography`, which is fast, and falls back
    to the built-in one, which is slow but always there."""
    if choice in ("auto", "cryptography"):
        try:
            verify = cryptography_verify()
            if self_test(verify):
                return "cryptography", verify
            problem = "the cryptography package failed the RFC 8032 self-test"
        except Unusable as e:
            problem = str(e)
        if choice == "cryptography":
            raise Unusable(problem)
        print(f"audit-verify: {problem}; using the built-in Ed25519, which is slower", file=sys.stderr)
    if not self_test(builtin_verify):
        raise Unusable("the built-in Ed25519 failed the RFC 8032 self-test")
    return "built-in", builtin_verify


# ---------------------------------------------------------------------------
# Reading the export
# ---------------------------------------------------------------------------


class Invalid(Exception):
    """A leaf, or the export, is not what the server writes."""


def _no_duplicate_keys(pairs):
    seen = {}
    for key, value in pairs:
        if key in seen:
            raise Invalid(f"the key {key!r} appears twice in one object")
        seen[key] = value
    return seen


def _no_constants(name):
    raise Invalid(f"{name} is not JSON")


def strict_json(text: str):
    return json.loads(text, object_pairs_hook=_no_duplicate_keys, parse_constant=_no_constants)


def b64_strict(text, what, urlsafe=False, length=None):
    if not isinstance(text, str):
        raise Invalid(f"{what} is not a string")
    try:
        if urlsafe:
            raw = base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))
            if base64.urlsafe_b64encode(raw).decode().rstrip("=") != text:
                raise ValueError("not canonical")
        else:
            raw = base64.b64decode(text, validate=True)
            if base64.b64encode(raw).decode() != text:
                raise ValueError("not canonical")
    except (binascii.Error, ValueError) as e:
        raise Invalid(f"{what} is not base64: {e}")
    if length is not None and len(raw) != length:
        raise Invalid(f"{what} is {len(raw)} bytes, not {length}")
    return raw


def load(path):
    """(checkpoint size, checkpoint root, [raw leaf bytes])."""
    try:
        with open(path, "rb") as f:
            lines = f.read().split(b"\n")
    except OSError as e:
        raise Unusable(f"cannot read {path}: {e}")
    if lines and lines[-1] == b"":
        lines.pop()
    if not lines:
        raise Invalid("the export is empty: no checkpoint line")
    first = lines[0].decode("ascii", "replace")
    match = re.fullmatch(r"(0|[1-9][0-9]*) (\S+)", first)
    if not match:
        raise Invalid(f'the first line is not a checkpoint ("<tree_size> <root_hash>"): {first!r}')
    root = b64_strict(match.group(2), "the checkpoint's root", length=32)
    return int(match.group(1)), root, lines[1:]


# ---------------------------------------------------------------------------
# What leaf version 1 is
# ---------------------------------------------------------------------------

LEAF_KEYS = ["v", "seq", "at", "action", "actor", "subject", "request"]
ACTOR_KEYS = {
    "device": ["kind", "id", "name", "agent"],
    "operator": ["kind"],
    "authkey": ["kind", "id", "tag"],
    "session": ["kind", "credential_id"],
    "server": ["kind"],
    "host": ["kind"],
}
FILE = ["project_key", "file_path", "deleted", "stored_sha256", "base_sha256", "merged", "merge_job"]
DEVICE = ["device_id", "name", "scope", "public_key", "fingerprint", "ephemeral", "authkey_id", "user_code"]
SUBJECT_KEYS = {
    "push": FILE,
    "delete": FILE,
    "pull": ["project_key"],
    "approve": DEVICE,
    "enroll": DEVICE,
    "deny": ["user_code", "name"],
    "revoke": ["device_id", "name"],
    "sweep": ["device_id", "name"],
    "authkey_create": ["authkey_id", "tag", "ephemeral", "max_devices"],
    "authkey_revoke": ["authkey_id", "revoke_devices", "revoked_devices"],
    "start": ["version"],
    "job_claim": ["job_id", "kind", "attempt", "lease_expires_at", "project_key", "file_path"],
    "job_result": ["job_id", "project_key", "file_path", "state", "stored_sha256", "follow_up"],
    "job_retry": ["job_id", "kind", "project_key", "file_path"],
    "evaluate": ["evaluation_id", "job_id", "projects", "contradictions"],
    "passkey_add": ["credential_id", "name", "first"],
    "passkey_remove": ["credential_id", "name"],
    "sessions_end": ["ended"],
    "bootstrap_code": ["expires_at"],
    "passkey_reset": ["passkeys_removed", "expires_at"],
    "admin_rename": ["from", "to", "rows", "jobs_closed", "backup"],
    "admin_remove": ["project_key", "rows", "jobs_closed", "backup"],
    "admin_restore": ["project_key", "source", "added", "overwritten", "deleted", "jobs_closed", "backup"],
}
# Who may do what: a device or the operator through the API, the admin
# page's passkey session on the routes that manage devices, an authkey
# enrolling the one device it approves, the server on its own, the host
# through `recall-server reset-passkeys` and `recall-server admin`.
ACTORS = {
    "push": {"device", "operator"},
    "delete": {"device", "operator"},
    "pull": {"device", "operator"},
    "approve": {"device", "operator", "session"},
    "deny": {"device", "operator", "session"},
    "revoke": {"device", "operator", "session"},
    "authkey_create": {"device", "operator", "session"},
    "authkey_revoke": {"device", "operator", "session"},
    "enroll": {"authkey"},
    "sweep": {"server"},
    "start": {"server"},
    # A worker, or the server merging without one; its own housekeeping
    # (a lease run out, a job failed for want of anything to merge it)
    # is a result too.
    "job_claim": {"device", "server"},
    "job_result": {"device", "server"},
    "job_retry": {"device", "operator"},
    # The owner, by any admin credential, or the server on the schedule.
    "evaluate": {"device", "operator", "session", "server"},
    # The first passkey takes RECALL_TOKEN (and the bootstrap code); every
    # other passkey action, a session.
    "passkey_add": {"operator", "session"},
    "passkey_remove": {"session"},
    "sessions_end": {"session"},
    "bootstrap_code": {"server"},
    "passkey_reset": {"host"},
    "admin_rename": {"host"},
    "admin_remove": {"host"},
    "admin_restore": {"host"},
}
# The actions whose request body the leaf keeps, and their routes.
KEEPS_BODY = {"approve", "deny", "revoke", "authkey_create", "authkey_revoke", "evaluate"}
ADMIN_ACTIONS = KEEPS_BODY | {"job_retry"}
# The actions a worker device signs, and the only ones it may.
WORKER_ACTIONS = {"job_claim", "job_result"}
# The host's changes to stored memory, run beside the server on its file.
HOST_ACTIONS = {"admin_rename", "admin_remove", "admin_restore"}
SCOPES = ("sync", "admin", "worker")
JOB_STATES = ("queued", "leased", "done", "failed")
COVERED = ["@method", "@authority", "@path", "@query", "content-digest", "recall-protocol"]
TIMESTAMP = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z")
HEX64 = re.compile(r"[0-9a-f]{64}")
USER_CODE_ALPHABET = "BCDFGHJKLMNPQRSTVWXZ"
DEFAULT_MAX_DEVICES = 25


def is_int(value):
    return type(value) is int  # bool is an int subclass, and 3.0 a float


def expect_keys(obj, keys, what):
    if not isinstance(obj, dict):
        raise Invalid(f"{what} is not an object")
    if list(obj) != keys:
        raise Invalid(f"{what} has the keys {list(obj)}, not {keys}")


def expect_str(value, what):
    if not isinstance(value, str):
        raise Invalid(f"{what} is not a string")


def expect_bool(value, what):
    if type(value) is not bool:
        raise Invalid(f"{what} is not true or false")


def normalize_user_code(text):
    """recall_wire::devices::normalize_user_code: the code's letters, any
    case, any separators, as XXXX-XXXX; None if not eight of them."""
    if not isinstance(text, str):
        return None
    letters = [c.upper() for c in text if c.upper() in USER_CODE_ALPHABET and c.isascii()]
    if len(letters) != 8:
        return None
    return "".join(letters[:4]) + "-" + "".join(letters[4:])


def check_shape(leaf, position):
    """The leaf is version 1, at its position, of its action's shape."""
    expect_keys(leaf, LEAF_KEYS, "the leaf")
    if not is_int(leaf["v"]) or leaf["v"] != 1:
        raise Invalid(f"v is {leaf['v']!r}, not 1")
    if not is_int(leaf["seq"]) or leaf["seq"] != position:
        raise Invalid(f"seq is {leaf['seq']!r} at position {position} (a gap, a repeat, or a reordering)")
    if not isinstance(leaf["at"], str) or not TIMESTAMP.fullmatch(leaf["at"]):
        raise Invalid(f"at is {leaf['at']!r}, not a timestamp")
    action = leaf["action"]
    if action not in SUBJECT_KEYS:
        raise Invalid(f"no action {action!r} exists")
    actor = leaf["actor"]
    kind = actor.get("kind") if isinstance(actor, dict) else None
    if kind not in ACTOR_KEYS:
        raise Invalid(f"the actor is {actor!r}")
    expect_keys(actor, ACTOR_KEYS[kind], "the actor")
    for key in ACTOR_KEYS[kind][1:]:
        expect_str(actor[key], f"actor.{key}")
    if kind not in ACTORS[action]:
        raise Invalid(f"a {kind} cannot {action}")

    subject = leaf["subject"]
    expect_keys(subject, SUBJECT_KEYS[action], "the subject")
    if action in ("push", "delete"):
        for key in ("project_key", "file_path"):
            expect_str(subject[key], f"subject.{key}")
        if subject["deleted"] is not (action == "delete"):
            raise Invalid(f"subject.deleted is {subject['deleted']!r} on a {action}")
        if not isinstance(subject["stored_sha256"], str) or not HEX64.fullmatch(subject["stored_sha256"]):
            raise Invalid("subject.stored_sha256 is not a SHA-256")
        base = subject["base_sha256"]
        if base is not None and (not isinstance(base, str) or not HEX64.fullmatch(base)):
            raise Invalid("subject.base_sha256 is not a SHA-256")
        expect_bool(subject["merged"], "subject.merged")
        if action == "delete" and subject["merged"]:
            raise Invalid("a delete says it merged")
        job = subject["merge_job"]
        if job is not None:
            expect_str(job, "subject.merge_job")
            if action == "delete" or subject["merged"]:
                raise Invalid(f"a {'delete' if action == 'delete' else 'merged push'} queued a merge job")
    elif action in ("approve", "enroll"):
        for key in ("device_id", "name", "scope", "fingerprint"):
            expect_str(subject[key], f"subject.{key}")
        key = b64_strict(subject["public_key"], "subject.public_key", urlsafe=True, length=32)
        fingerprint = "SHA256:" + base64.b64encode(hashlib.sha256(key).digest()).decode().rstrip("=")
        if subject["fingerprint"] != fingerprint:
            raise Invalid("subject.fingerprint is not the public key's")
        if subject["scope"] not in SCOPES:
            raise Invalid(f"subject.scope is {subject['scope']!r}")
        expect_bool(subject["ephemeral"], "subject.ephemeral")
        if action == "approve":
            if subject["ephemeral"] or subject["authkey_id"] is not None:
                raise Invalid("an approved device names an authkey, or is ephemeral")
            if normalize_user_code(subject["user_code"]) != subject["user_code"]:
                raise Invalid("subject.user_code is not a user code")
        else:
            if subject["authkey_id"] != actor["id"] or subject["user_code"] is not None:
                raise Invalid("an enrolled device does not name the authkey that enrolled it")
            if subject["scope"] != "sync":
                raise Invalid(f"an authkey enrolled a device with the {subject['scope']} scope")
    elif action == "authkey_create":
        expect_str(subject["authkey_id"], "subject.authkey_id")
        expect_str(subject["tag"], "subject.tag")
        expect_bool(subject["ephemeral"], "subject.ephemeral")
        if not is_int(subject["max_devices"]) or subject["max_devices"] < 1:
            raise Invalid("subject.max_devices is not a count")
    elif action == "authkey_revoke":
        expect_str(subject["authkey_id"], "subject.authkey_id")
        expect_bool(subject["revoke_devices"], "subject.revoke_devices")
        revoked = subject["revoked_devices"]
        if not isinstance(revoked, list) or not all(isinstance(d, str) for d in revoked):
            raise Invalid("subject.revoked_devices is not a list of ids")
        if revoked and not subject["revoke_devices"]:
            raise Invalid("devices were revoked though revoke_devices is false")
    elif action == "job_claim":
        expect_str(subject["job_id"], "subject.job_id")
        expect_str(subject["kind"], "subject.kind")
        if not is_int(subject["attempt"]) or subject["attempt"] < 1:
            raise Invalid("subject.attempt is not an attempt")
        if not isinstance(subject["lease_expires_at"], str) or not TIMESTAMP.fullmatch(subject["lease_expires_at"]):
            raise Invalid("subject.lease_expires_at is not a timestamp")
        if subject["kind"] == "merge":
            expect_str(subject["project_key"], "subject.project_key")
            expect_str(subject["file_path"], "subject.file_path")
    elif action == "job_result":
        for key in ("job_id", "project_key", "file_path"):
            expect_str(subject[key], f"subject.{key}")
        if subject["state"] not in ("queued", "done", "failed"):
            raise Invalid(f"subject.state is {subject['state']!r}")
        stored = subject["stored_sha256"]
        if stored is not None:
            if not isinstance(stored, str) or not HEX64.fullmatch(stored):
                raise Invalid("subject.stored_sha256 is not a SHA-256")
            if subject["state"] != "done" or subject["follow_up"] is not None:
                raise Invalid("a result wrote the file but the job is not done, or has a follow-up")
        if subject["follow_up"] is not None:
            expect_str(subject["follow_up"], "subject.follow_up")
    elif action == "evaluate":
        expect_str(subject["evaluation_id"], "subject.evaluation_id")
        expect_str(subject["job_id"], "subject.job_id")
        projects = subject["projects"]
        if not isinstance(projects, list) or not all(isinstance(p, str) for p in projects):
            raise Invalid("subject.projects is not a list of project keys")
        if len(set(projects)) != len(projects):
            raise Invalid("subject.projects names a project twice")
        expect_bool(subject["contradictions"], "subject.contradictions")
    elif action in ("passkey_add", "passkey_remove"):
        expect_str(subject["credential_id"], "subject.credential_id")
        expect_str(subject["name"], "subject.name")
        if action == "passkey_add":
            expect_bool(subject["first"], "subject.first")
            if subject["first"] is not (kind == "operator"):
                raise Invalid("the first passkey is the operator's to add, and only the first")
    elif action == "sessions_end":
        if not is_int(subject["ended"]) or subject["ended"] < 1:
            raise Invalid("subject.ended is not a count of sessions")
    elif action in ("bootstrap_code", "passkey_reset"):
        if not isinstance(subject["expires_at"], str) or not TIMESTAMP.fullmatch(subject["expires_at"]):
            raise Invalid("subject.expires_at is not a timestamp")
        if action == "passkey_reset" and (not is_int(subject["passkeys_removed"]) or subject["passkeys_removed"] < 0):
            raise Invalid("subject.passkeys_removed is not a count")
    elif action in HOST_ACTIONS:
        check_host_subject(action, subject)
    else:
        for key, value in subject.items():
            expect_str(value, f"subject.{key}")

    request = leaf["request"]
    if kind == "device":
        expect_keys(request, ["body_sha256", "signature_base", "signature", "body"], "a device's request")
        for key in ("body_sha256", "signature_base", "signature"):
            expect_str(request[key], f"request.{key}")
        if action in KEEPS_BODY:
            expect_str(request["body"], "request.body")
        elif request["body"] is not None:
            raise Invalid(f"a {action} keeps its body")
    elif request is not None:
        raise Invalid(f"the {kind} signs nothing, but the leaf carries a request")


def expect_count(value, what):
    if not is_int(value) or value < 0:
        raise Invalid(f"{what} is not a count")


def expect_file_name(value, what):
    """A file's name alone: the leaf names a backup, never where it is."""
    if not isinstance(value, str) or not value or "/" in value or value in (".", ".."):
        raise Invalid(f"{what} is not a file name")


def check_host_subject(action, subject):
    """A rename's, remove's or restore's subject: its keys, counts that say
    it changed something, the jobs it closed, and its backup by name."""
    if action == "admin_rename":
        for key in ("from", "to"):
            expect_str(subject[key], f"subject.{key}")
            if not subject[key]:
                raise Invalid(f"subject.{key} is empty")
        if subject["from"] == subject["to"]:
            raise Invalid("a rename onto the key it renames")
    else:
        expect_str(subject["project_key"], "subject.project_key")
        if not subject["project_key"]:
            raise Invalid("subject.project_key is empty")
    if action == "admin_restore":
        expect_file_name(subject["source"], "subject.source")
        for key in ("added", "overwritten", "deleted"):
            expect_count(subject[key], f"subject.{key}")
        changed = subject["added"] + subject["overwritten"] + subject["deleted"]
    else:
        expect_count(subject["rows"], "subject.rows")
        changed = subject["rows"]
    if changed == 0:
        raise Invalid(f"an {action} that changed no row (one that has nothing to change is not made)")
    closed = subject["jobs_closed"]
    if not isinstance(closed, list) or not all(isinstance(j, str) and j for j in closed):
        raise Invalid("subject.jobs_closed is not a list of job ids")
    if len(set(closed)) != len(closed):
        raise Invalid("subject.jobs_closed names a job twice")
    expect_file_name(subject["backup"], "subject.backup")


def parse_signature_base(base):
    """{component: value} and the @signature-params parameters, reading the
    base the way RFC 9421 Section 2.5 builds it, and no other way."""
    if not base.isascii():
        raise Invalid("the signature base is not ASCII")
    lines = base.split("\n")
    last = lines.pop()
    prefix = '"@signature-params": '
    if not last.startswith(prefix):
        raise Invalid("the signature base does not end with @signature-params")
    match = re.fullmatch(r"\(((?:\"[^\"\\]*\"(?: \"[^\"\\]*\")*)?)\)((?:;[a-z*][a-z0-9_.*-]*=[^;]*)*)", last[len(prefix):])
    if not match:
        raise Invalid("the signature parameters do not parse")
    components = re.findall(r"\"([^\"\\]*)\"", match.group(1))
    params = {}
    for part in match.group(2).split(";")[1:]:
        name, _, value = part.partition("=")
        if name in params:
            raise Invalid(f"the signature parameter {name} appears twice")
        if re.fullmatch(r"-?[0-9]{1,15}", value):
            params[name] = int(value)
        elif re.fullmatch(r"\"[^\"\\]*\"", value):
            params[name] = value[1:-1]
        else:
            raise Invalid(f"the signature parameter {name} does not parse")
    if len(lines) != len(components) or len(set(components)) != len(components):
        raise Invalid("the signature base's lines are not its covered components")
    values = {}
    for name, line in zip(components, lines):
        head = f'"{name}": '
        if not line.startswith(head):
            raise Invalid(f"the signature base's line for {name} is out of place")
        values[name] = line[len(head):]
    missing = [c for c in COVERED if c not in values]
    if missing:
        raise Invalid(f"the signature does not cover {', '.join(missing)}")
    return values, params


def route_of(action, subject):
    """The method and path an action's request is sent to."""
    if action == "revoke":
        return "POST", f"/v1/devices/{urllib.parse.quote(subject['device_id'], safe='')}/revoke"
    if action == "authkey_revoke":
        return "POST", f"/v1/authkeys/{urllib.parse.quote(subject['authkey_id'], safe='')}/revoke"
    if action == "job_result":
        return "POST", f"/v1/jobs/{urllib.parse.quote(subject['job_id'], safe='')}/result"
    if action == "job_retry":
        return "POST", f"/v1/jobs/{urllib.parse.quote(subject['job_id'], safe='')}/retry"
    return {
        "push": ("POST", "/sync"),
        "delete": ("POST", "/sync"),
        "pull": ("GET", "/sync"),
        "approve": ("POST", "/v1/devices/approve"),
        "deny": ("POST", "/v1/devices/deny"),
        "authkey_create": ("POST", "/v1/authkeys"),
        "job_claim": ("POST", "/v1/jobs/claim"),
        "evaluate": ("POST", "/v1/evaluations"),
    }[action]


def check_body(action, subject, body):
    """What the kept body asked for is what the subject says was done."""
    if action == "revoke":
        return  # its id is in the path
    if action in ("authkey_revoke", "evaluate") and body.strip(" \t\r\n") == "":
        asked = {}
    else:
        try:
            asked = strict_json(body)
        except (ValueError, Invalid) as e:
            raise Invalid(f"the request body is not JSON: {e}")
        if not isinstance(asked, dict):
            raise Invalid("the request body is not an object")
    if action in ("approve", "deny"):
        if normalize_user_code(asked.get("user_code")) != subject["user_code"]:
            raise Invalid("the body names another user code")
    if action == "approve":
        if asked.get("scope", "sync") != subject["scope"]:
            raise Invalid("the body asked for another scope")
        fingerprint = asked.get("fingerprint")
        if fingerprint is not None and (not isinstance(fingerprint, str) or fingerprint.strip() != subject["fingerprint"]):
            raise Invalid("the body names another fingerprint")
    elif action == "authkey_create":
        tag = asked.get("tag", "")
        if not isinstance(tag, str) or unicodedata.normalize("NFC", tag.strip()) != subject["tag"]:
            raise Invalid("the body asked for another tag")
        if asked.get("ephemeral", True) is not subject["ephemeral"]:
            raise Invalid("the body asked for another ephemeral")
        if (asked.get("max_devices") or DEFAULT_MAX_DEVICES) != subject["max_devices"]:
            raise Invalid("the body asked for another max_devices")
    elif action == "authkey_revoke":
        if asked.get("revoke_devices", False) is not subject["revoke_devices"]:
            raise Invalid("the body asked for another revoke_devices")
    elif action == "evaluate":
        projects = asked.get("projects", [])
        if isinstance(projects, list) and all(isinstance(p, str) for p in projects):
            # The server keeps the first of any project named twice.
            projects = list(dict.fromkeys(projects))
        if projects != subject["projects"]:
            raise Invalid("the body asked for other projects")
        if asked.get("contradictions", False) is not subject["contradictions"]:
            raise Invalid("the body asked for another contradictions")


class State:
    """What the log has established by some point: its devices and keys."""

    def __init__(self):
        self.devices = {}  # id -> {"public_key", "scope", "ephemeral", "authkey_id", "gone"}
        self.authkeys = {}  # id -> {"revoked": bool}
        self.jobs = {}  # id -> {"file", "state", "holder", "attempt"}
        self.passkeys = set()  # the admin page's passkeys, by credential id
        self.bootstrap = False  # a bootstrap code outstanding
        self.nonces = set()
        self.last_at = ""


def check_request(leaf, state, verify):
    """A device's leaf: its request is its device's, for this action."""
    action, actor, subject, request = leaf["action"], leaf["actor"], leaf["subject"], leaf["request"]
    values, params = parse_signature_base(request["signature_base"])
    if params.get("keyid") != actor["id"]:
        raise Invalid(f"the signature's keyid is {params.get('keyid')!r}, not the actor {actor['id']!r}")
    if params.get("alg", "ed25519") != "ed25519":
        raise Invalid(f"the signature's alg is {params.get('alg')!r}")
    nonce, created = params.get("nonce"), params.get("created")
    if not isinstance(nonce, str) or not 0 < len(nonce) <= 128 or not is_int(created):
        raise Invalid("the signature has no nonce or no created time")
    if (actor["id"], nonce) in state.nonces:
        raise Invalid(f"the signed request with nonce {nonce!r} is in the log already (a replay)")
    state.nonces.add((actor["id"], nonce))

    body_sha256 = b64_strict(request["body_sha256"], "request.body_sha256", length=32)
    digests = [m.strip() for m in values["content-digest"].split(",")]
    sha256 = [m[len("sha-256="):] for m in digests if m.startswith("sha-256=")]
    if len(sha256) != 1 or sha256[0] != f":{request['body_sha256']}:":
        raise Invalid("the signed content-digest is not request.body_sha256")
    if request["body"] is not None:
        if hashlib.sha256(request["body"].encode("utf-8")).digest() != body_sha256:
            raise Invalid("request.body does not hash to request.body_sha256")
        check_body(action, subject, request["body"])

    method, path = route_of(action, subject)
    if values["@method"] != method or values["@path"] != path:
        raise Invalid(f"the signed request was {values['@method']} {values['@path']}, not a {action}")
    if action == "pull":
        query = values["@query"]
        pairs = urllib.parse.parse_qsl(query[1:], keep_blank_values=True) if query.startswith("?") else None
        keys = [v for k, v in pairs or [] if k == "project_key"]
        if keys != [subject["project_key"]]:
            raise Invalid("the signed query does not name the pulled project")

    device = state.devices.get(actor["id"])
    if device is None:
        raise Invalid(f"signed by {actor['id']}, which no earlier approve or enroll leaf made")
    if device["gone"]:
        raise Invalid(f"signed by {actor['id']}, which was revoked or swept before it")
    if action in ADMIN_ACTIONS and device["scope"] != "admin":
        raise Invalid(f"a {action} signed by {actor['id']}, which does not have the admin scope")
    if (action in WORKER_ACTIONS) != (device["scope"] == "worker"):
        raise Invalid(f"a {action} signed by {actor['id']}, whose scope is {device['scope']}")
    if verify is not None:
        signature = b64_strict(request["signature"], "request.signature", length=64)
        if not verify(device["public_key"], request["signature_base"].encode("ascii"), signature):
            raise Invalid(f"the signature does not verify with {actor['id']}'s key")


def apply(leaf, state):
    """What the leaf changes about the devices and keys that sign later."""
    action, subject, actor = leaf["action"], leaf["subject"], leaf["actor"]
    if action in ("approve", "enroll"):
        if subject["device_id"] in state.devices:
            raise Invalid(f"{subject['device_id']} was approved or enrolled before")
        if action == "enroll":
            key = state.authkeys.get(actor["id"])
            if key is None or key["revoked"]:
                raise Invalid(f"enrolled by {actor['id']}, which no leaf created, or which was revoked")
        state.devices[subject["device_id"]] = {
            "public_key": b64_strict(subject["public_key"], "subject.public_key", urlsafe=True, length=32),
            "scope": subject["scope"],
            "ephemeral": subject["ephemeral"],
            "authkey_id": subject["authkey_id"],
            "gone": False,
        }
    elif action in ("revoke", "sweep"):
        device = state.devices.get(subject["device_id"])
        if device is None:
            raise Invalid(f"a {action} of {subject['device_id']}, which no leaf made")
        if action == "revoke" and device["gone"]:
            raise Invalid(f"{subject['device_id']} was revoked before")
        if action == "sweep" and not device["ephemeral"]:
            raise Invalid(f"a sweep of {subject['device_id']}, which is not ephemeral")
        device["gone"] = True
    elif action == "authkey_create":
        if subject["authkey_id"] in state.authkeys:
            raise Invalid(f"{subject['authkey_id']} was created before")
        state.authkeys[subject["authkey_id"]] = {"revoked": False}
    elif action == "authkey_revoke":
        key = state.authkeys.get(subject["authkey_id"])
        if key is None:
            raise Invalid(f"a revoke of {subject['authkey_id']}, which no leaf created")
        if key["revoked"] and not subject["revoked_devices"]:
            raise Invalid(f"{subject['authkey_id']} was revoked before, and nothing else changed")
        key["revoked"] = True
        for device_id in subject["revoked_devices"]:
            device = state.devices.get(device_id)
            if device is None or device["authkey_id"] != subject["authkey_id"] or device["gone"]:
                raise Invalid(f"{device_id} is not a live device {subject['authkey_id']} enrolled")
            device["gone"] = True
    elif action in ("push", "delete"):
        file = (subject["project_key"], subject["file_path"])
        if action == "delete":
            # A delete closes the file's open jobs in its own transaction.
            for job in state.jobs.values():
                if job["file"] == file and job["state"] in ("queued", "leased"):
                    job.update(state="done", holder=None)
        elif subject["merge_job"] is not None:
            queue(state, subject["merge_job"], file)
    elif action in ("job_claim", "job_result", "job_retry"):
        apply_job(leaf, state)
    elif action == "evaluate":
        # An evaluation's job names no file: it reads many.
        queue(state, subject["job_id"], ("", ""))
    elif action in PASSKEY_ACTIONS:
        apply_passkey(leaf, state)
    elif action in HOST_ACTIONS:
        apply_host(leaf, state)


PASSKEY_ACTIONS = {"passkey_add", "passkey_remove", "sessions_end", "bootstrap_code", "passkey_reset"}


def apply_passkey(leaf, state):
    """The admin page's passkeys, as the log has added and removed them."""
    action, subject = leaf["action"], leaf["subject"]
    if action == "bootstrap_code":
        if state.passkeys:
            raise Invalid("a bootstrap code was issued with a passkey registered")
        state.bootstrap = True
    elif action == "passkey_reset":
        if subject["passkeys_removed"] != len(state.passkeys):
            raise Invalid(
                f"a reset says it removed {subject['passkeys_removed']} passkeys, "
                f"but {len(state.passkeys)} were registered"
            )
        state.passkeys.clear()
        state.bootstrap = True
    elif action == "passkey_add":
        if subject["credential_id"] in state.passkeys:
            raise Invalid(f"passkey {subject['credential_id']} was added twice")
        if subject["first"]:
            if state.passkeys or not state.bootstrap:
                raise Invalid("a first passkey added beside another, or with no bootstrap code outstanding")
            state.bootstrap = False
        state.passkeys.add(subject["credential_id"])
    elif action == "passkey_remove":
        if subject["credential_id"] not in state.passkeys:
            raise Invalid(f"a removal of passkey {subject['credential_id']}, which is not registered")
        if len(state.passkeys) == 1:
            raise Invalid("the last passkey was removed")
        state.passkeys.discard(subject["credential_id"])


def apply_host(leaf, state):
    """A rename, remove or restore closes, in its own transaction, the jobs
    open for the rows it touched: each one it names must be open and of its
    key, and a rename or a remove, which touch every row of the key, leave
    none of that key open. A restore touches only the paths it writes, which
    the leaf does not list, so the jobs it leaves open are not checked."""
    action, subject = leaf["action"], leaf["subject"]
    key = subject["from"] if action == "admin_rename" else subject["project_key"]
    closed = set(subject["jobs_closed"])
    for job_id in subject["jobs_closed"]:
        job = state.jobs.get(job_id)
        if job is None:
            raise Invalid(f"an {action} closed {job_id}, which no push or result queued")
        if job["state"] not in ("queued", "leased"):
            raise Invalid(f"an {action} closed {job_id} once it was {job['state']}")
        if job["file"][0] != key:
            raise Invalid(f"an {action} of {key!r} closed {job_id}, a job of {job['file'][0]!r}")
        job.update(state="done", holder=None)
    if action != "admin_restore":
        for job_id, job in state.jobs.items():
            if job["file"][0] == key and job["state"] in ("queued", "leased") and job_id not in closed:
                raise Invalid(f"an {action} of {key!r} left {job_id} open")


def check_session(leaf, state):
    """An admin session acts with a passkey the log added and still holds."""
    credential = leaf["actor"]["credential_id"]
    if credential not in state.passkeys:
        raise Invalid(f"an admin session with passkey {credential}, which the log never added or removed since")


def queue(state, job_id, file):
    if job_id in state.jobs:
        raise Invalid(f"{job_id} was queued before")
    state.jobs[job_id] = {"file": file, "state": "queued", "holder": None, "attempt": 0}


def apply_job(leaf, state):
    """A job's leaf follows from what the log says of that job so far."""
    action, subject, actor = leaf["action"], leaf["subject"], leaf["actor"]
    job = state.jobs.get(subject["job_id"])
    if job is None:
        raise Invalid(f"a {action} of {subject['job_id']}, which no push or result queued")
    # A claim of a job with no file (an evaluation's) names none: null, read
    # as the empty key and path the job was queued with.
    if (subject["project_key"] or "", subject["file_path"] or "") != job["file"]:
        raise Invalid(f"a {action} of {subject['job_id']} names another file than the one it was queued for")
    who = actor.get("id", "server")
    if action == "job_claim":
        holder = job["holder"]
        if job["state"] == "leased":
            # The server frees a revoked worker's leases, and its own after a
            # restart, before it claims them again; nothing frees a live
            # worker's.
            if holder != "server" and not state.devices[holder]["gone"]:
                raise Invalid(f"{subject['job_id']} was claimed while {holder} held it")
        elif job["state"] != "queued":
            raise Invalid(f"{subject['job_id']} was claimed once it was {job['state']}")
        if subject["attempt"] != job["attempt"] + 1:
            raise Invalid(f"{subject['job_id']} was claimed at attempt {subject['attempt']}, not {job['attempt'] + 1}")
        job.update(state="leased", holder=who, attempt=subject["attempt"])
    elif action == "job_result":
        if actor["kind"] == "device" and (job["state"] != "leased" or job["holder"] != who):
            raise Invalid(f"a result for {subject['job_id']} from {who}, which does not hold it")
        if job["state"] not in ("queued", "leased"):
            raise Invalid(f"a result for {subject['job_id']} once it was {job['state']}")
        job.update(state=subject["state"], holder=None)
        if subject["follow_up"] is not None:
            queue(state, subject["follow_up"], job["file"])
    else:
        if job["state"] != "failed":
            raise Invalid(f"{subject['job_id']} was retried while {job['state']}")
        job.update(state="queued", holder=None, attempt=0)


# ---------------------------------------------------------------------------


def verify_export(path, checkpoints, verify):
    """Every problem found, as text; none means the export checks out."""
    problems = []
    try:
        size, root, lines = load(path)
    except Invalid as e:
        return [str(e)], 0
    state = State()
    signed = 0
    for position, raw in enumerate(lines):
        try:
            try:
                leaf = strict_json(raw.decode("utf-8"))
            except UnicodeDecodeError:
                raise Invalid("the leaf is not UTF-8")
            except ValueError as e:
                raise Invalid(f"the leaf is not JSON: {e}")
            check_shape(leaf, position)
            if leaf["at"] < state.last_at:
                raise Invalid(f"at {leaf['at']} is earlier than the leaf before it")
            state.last_at = leaf["at"]
            if leaf["actor"]["kind"] == "device":
                check_request(leaf, state, verify)
                signed += 1
            elif leaf["actor"]["kind"] == "session":
                check_session(leaf, state)
            apply(leaf, state)
        except Invalid as e:
            problems.append(f"leaf {position}: {e}")

    hashes = [hash_leaf(raw) for raw in lines]
    if len(hashes) != size:
        problems.append(f"the export holds {len(hashes)} leaves but its checkpoint says tree_size {size}")
    roots = roots_at(hashes, [size] + [s for s, _ in checkpoints if s <= len(hashes)])
    if size == len(hashes) and roots[size] != root:
        problems.append(
            f"the root over all {size} leaves is {base64.b64encode(roots[size]).decode()}, the "
            f"checkpoint says {base64.b64encode(root).decode()} (a leaf changed, removed or reordered)"
        )
    for want_size, want_root in checkpoints:
        if want_size > len(hashes):
            problems.append(
                f"a saved checkpoint at size {want_size} is past the end of this export "
                f"({len(hashes)} leaves): the log does not extend it"
            )
        elif roots[want_size] != want_root:
            problems.append(
                f"the root over the first {want_size} leaves is {base64.b64encode(roots[want_size]).decode()}, "
                f"the saved checkpoint says {base64.b64encode(want_root).decode()}: the log does not extend it"
            )
    return problems, signed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("export", nargs="?", help="the export: a checkpoint line, then one leaf per line")
    parser.add_argument(
        "--checkpoint",
        action="append",
        default=[],
        metavar="SIZE:ROOT_B64",
        help="a checkpoint saved earlier that the log must still extend; may be given more than once",
    )
    parser.add_argument(
        "--ed25519",
        choices=["auto", "cryptography", "builtin"],
        default="auto",
        help="which Ed25519 checks signatures (default: cryptography if it works, else the built-in one)",
    )
    parser.add_argument(
        "--no-signatures",
        action="store_true",
        help="check everything but the signatures themselves; the OK line says so",
    )
    parser.add_argument("--self-test", action="store_true", help="check the Ed25519 chosen against RFC 8032 and exit")
    args = parser.parse_args()

    try:
        verify, backend = None, None
        if not args.no_signatures or args.self_test:
            backend, verify = choose_ed25519(args.ed25519)
        if args.self_test:
            print(f"OK: the {backend} Ed25519 passes RFC 8032's test vectors")
            return EXIT_OK
        if args.export is None:
            raise Unusable("no export named")
        checkpoints = []
        for c in args.checkpoint:
            match = re.fullmatch(r"(0|[1-9][0-9]*):(\S+)", c)
            try:
                if not match:
                    raise Invalid("not SIZE:ROOT_B64")
                checkpoints.append((int(match.group(1)), b64_strict(match.group(2), "its root", length=32)))
            except Invalid as e:
                raise Unusable(f"--checkpoint {c!r}: {e}")
        problems, signed = verify_export(args.export, checkpoints, verify)
    except Unusable as e:
        print(f"audit-verify: {e}", file=sys.stderr)
        return EXIT_UNUSABLE

    if problems:
        for p in problems:
            print(f"FAIL: {p}", file=sys.stderr)
        return EXIT_FAILED
    with open(args.export, "rb") as f:
        first = f.readline().decode("ascii").strip()
    if verify is None:
        print(f"OK: checkpoint {first}; {signed} signed leaves, their signatures NOT checked (--no-signatures)")
    else:
        print(f"OK: checkpoint {first}; {signed} signed leaves, every signature checked ({backend} Ed25519)")
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
