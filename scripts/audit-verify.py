#!/usr/bin/env python3
"""Verifies an exported audit log offline.

    recall audit export > audit.jsonl
    python3 scripts/audit-verify.py audit.jsonl

Reads the export `recall audit export` (or a raw `GET /v1/audit/entries`
walk written out the same way) produces: a first line holding the
checkpoint the export was taken at, `<tree_size> <root_hash>` exactly as
the `Recall-Audit-Checkpoint` header carries it, then one leaf per line,
its own compact JSON, in `seq` order from 0.

This is a *second* implementation of the tree hash in
`crates/recall-server/src/audit/merkle.rs` — RFC 9162 Section 2.1, SHA-256,
`SHA-256(0x00 || leaf)` for a leaf and `SHA-256(0x01 || left || right)` for
a node, splitting at the largest power of two below a tree's size — using
nothing but the standard library's `hashlib`, so a bug shared by both
implementations is far less likely than a bug in one. It checks:

1. `seq` runs from 0 without gaps or repeats.
2. The root recomputed over every leaf matches the checkpoint on the
   export's first line, and, for every earlier checkpoint given with
   `--checkpoint`, the root over that many leaves still matches it — the
   property that makes a saved checkpoint worth anything: a log that no
   longer extends one you trusted has been rewritten.
3. Every signed leaf's `signature` verifies, over its own
   `signature_base`, with the device's public key — taken from that
   device's own `approve` leaf, never from a live `devices` table, since a
   device the sweep already removed still signed real requests while it
   existed. This is what "a device's actions being forged" actually means
   to check: the Merkle tree alone proves the log was not rewritten, not
   that what it records is genuine.

Exits 0 and prints "OK" when everything checks out; otherwise prints what
failed and exits 1. Ed25519 verification needs the `cryptography` package
(``pip install cryptography``); without it, step 3 is skipped with a
warning printed to stderr — steps 1 and 2, the tree itself, still run in
full and still use only the standard library.
"""

import argparse
import base64
import hashlib
import json
import sys

LEAF_PREFIX = b"\x00"
NODE_PREFIX = b"\x01"


def hash_leaf(leaf_bytes: bytes) -> bytes:
    return hashlib.sha256(LEAF_PREFIX + leaf_bytes).digest()


def hash_children(left: bytes, right: bytes) -> bytes:
    return hashlib.sha256(NODE_PREFIX + left + right).digest()


def empty_root() -> bytes:
    return hashlib.sha256(b"").digest()


def split_point(n: int) -> int:
    """The largest power of two strictly smaller than n (n >= 2)."""
    k = 1
    while k * 2 < n:
        k *= 2
    return k


def root(hashes: list) -> bytes:
    n = len(hashes)
    if n == 0:
        return empty_root()
    if n == 1:
        return hashes[0]
    k = split_point(n)
    return hash_children(root(hashes[:k]), root(hashes[k:]))


def b64(s: str) -> bytes:
    return base64.b64decode(s)


def load(path: str):
    """Reads the export: (checkpoint_size, checkpoint_root_b64, [(seq, raw_line, parsed)])."""
    with open(path, "rb") as f:
        lines = f.read().split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    if not lines:
        sys.exit("empty export: no checkpoint line")
    first = lines[0].decode("utf-8")
    try:
        size_s, root_b64 = first.split(" ", 1)
        checkpoint_size = int(size_s)
    except ValueError:
        sys.exit(f"first line is not a checkpoint (\"<tree_size> <root_hash>\"): {first!r}")

    entries = []
    for raw in lines[1:]:
        try:
            parsed = json.loads(raw)
        except json.JSONDecodeError as e:
            sys.exit(f"leaf is not valid JSON: {e}: {raw!r}")
        entries.append((parsed.get("seq"), raw, parsed))
    return checkpoint_size, root_b64, entries


def check_seq(entries) -> list:
    problems = []
    for i, (seq, _raw, _parsed) in enumerate(entries):
        if seq != i:
            problems.append(f"entry at position {i} has seq {seq!r}, wanted {i} (gap, repeat, or reordering)")
    return problems


def check_roots(entries, checkpoint_size, checkpoint_root_b64, extra_checkpoints) -> list:
    problems = []
    hashes = [hash_leaf(raw) for _seq, raw, _parsed in entries]

    if len(entries) != checkpoint_size:
        problems.append(
            f"the export holds {len(entries)} leaves but its checkpoint says tree_size {checkpoint_size}"
        )
    else:
        got = base64.b64encode(root(hashes)).decode("ascii")
        if got != checkpoint_root_b64:
            problems.append(
                f"root over all {len(hashes)} leaves is {got}, checkpoint says {checkpoint_root_b64} "
                "(a leaf was changed, removed, or reordered)"
            )

    for size, want_root_b64 in extra_checkpoints:
        if size > len(hashes):
            problems.append(
                f"a saved checkpoint at size {size} is past the end of this export ({len(hashes)} leaves): "
                "the log does not extend it"
            )
            continue
        got = base64.b64encode(root(hashes[:size])).decode("ascii")
        if got != want_root_b64:
            problems.append(
                f"root over the first {size} leaves is {got}, the saved checkpoint says {want_root_b64}: "
                "the log does not extend that checkpoint"
            )
    return problems


def check_signatures(entries) -> list:
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
        from cryptography.exceptions import InvalidSignature
    except BaseException as e:  # noqa: BLE001 - a broken install (even a Rust-extension
        # panic, which pyo3 raises as a BaseException rather than an Exception, precisely
        # so naive `except Exception` handlers do not swallow it) must not crash the tree
        # check, which is the property this script exists to guarantee.
        print(
            f"audit-verify: `cryptography` is not usable here ({e}), so signatures were not "
            "checked (only the tree itself was); `pip install cryptography` to check them too",
            file=sys.stderr,
        )
        return []

    problems = []
    # A device's public key comes from its own `approve` leaf: the log
    # verifies itself, with no live `devices` table needed, even for a
    # device the ephemeral sweep has since removed.
    keys = {}
    for seq, _raw, parsed in entries:
        if parsed.get("action") == "approve":
            subject = parsed.get("subject") or {}
            device_id = subject.get("device_id")
            public_key = subject.get("public_key")
            if device_id and public_key:
                keys[device_id] = public_key

    for seq, _raw, parsed in entries:
        request = parsed.get("request")
        if not request:
            continue
        actor = parsed.get("actor") or {}
        device_id = actor.get("id")
        public_key_b64url = keys.get(device_id)
        if not public_key_b64url:
            problems.append(f"seq {seq}: signed by {device_id!r}, but no approve leaf carries its key")
            continue
        pad = "=" * (-len(public_key_b64url) % 4)
        raw_key = base64.urlsafe_b64decode(public_key_b64url + pad)
        try:
            key = Ed25519PublicKey.from_public_bytes(raw_key)
            key.verify(b64(request["signature"]), request["signature_base"].encode("ascii"))
        except (InvalidSignature, ValueError, KeyError) as e:
            problems.append(f"seq {seq}: signature does not verify: {e}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("export", help="the file `recall audit export` (or an equivalent walk) wrote")
    parser.add_argument(
        "--checkpoint",
        action="append",
        default=[],
        metavar="SIZE:ROOT_B64",
        help="an earlier checkpoint (e.g. from ~/.recall/audit.json) the log must still extend; "
        "may be given more than once",
    )
    args = parser.parse_args()

    checkpoint_size, checkpoint_root_b64, entries = load(args.export)

    extra_checkpoints = []
    for c in args.checkpoint:
        try:
            size_s, root_b64 = c.split(":", 1)
            extra_checkpoints.append((int(size_s), root_b64))
        except ValueError:
            sys.exit(f"--checkpoint must be SIZE:ROOT_B64, got {c!r}")

    problems = []
    problems += check_seq(entries)
    problems += check_roots(entries, checkpoint_size, checkpoint_root_b64, extra_checkpoints)
    problems += check_signatures(entries)

    if problems:
        for p in problems:
            print(f"FAIL: {p}", file=sys.stderr)
        return 1

    print(f"OK: {len(entries)} leaves, tree_size {checkpoint_size}, root {checkpoint_root_b64}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
