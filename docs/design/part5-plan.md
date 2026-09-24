# Plan: Part 5, encrypted storage and a worker

Status: **plan**, not built. Part 5 of [`handshake.md`](handshake.md) is the
design the owner approved on 2026-09-23. This file turns it into pull
requests that can be built and merged one at a time. Where the design leaves
something open, the plan decides it and says why; what only the owner can
decide is under [Open decisions](#open-decisions). As with every other part,
once a pull request here is built, [`../reference/api.md`](../reference/api.md)
is the authority for what it shipped.

## What it builds on

| Prerequisite | State on 2026-09-23 | Needed by |
|---|---|---|
| Part 2a, the server half of devices (PR #100): enrolment, approval, revocation, RFC 9421 signatures | Open, not merged | PR 1, PR 2 |
| Part 2b, the client half: `recall connect` enrols, the keychain, signed requests from `recall-hooks` | Not built | PR 3 onwards |
| The admin page's Devices tab and passkeys | Not built | Nothing here; see open decision 1 |

Two details of Part 2a shape this plan. `devices.scope` is declared with
`CHECK (scope IN ('sync', 'admin'))`, so a third scope means rebuilding the
table. And the ephemeral sweep deletes device rows outright, so anything that
must still verify later, the audit log above all, carries public keys itself
instead of pointing at `devices`.

## The pull requests

| # | Pull request | Depends on | Version | Size |
|---|---|---|---|---|
| 1 | **Audit log**: a Merkle tree over every authenticated action, a checkpoint on every pull, export and offline verification | #100 | patch: new routes, tables and an optional header | M, ~1,500 lines |
| 2 | **Merge queue and `recall-worker`**: a `worker` scope, jobs with leases, the worker binary and its compose service | #100; parallel with 1 | patch, see below | L, ~3,000 |
| 3 | **Content keys**: device encryption keys, signed grants, the recovery key, the unwrap half of cloud enrolment keys, rotation | 1, 2, Part 2b | patch: new routes and optional fields | L, ~3,000 |
| 4 | **Sealed sync**: clients encrypt file bodies (XChaCha20-Poly1305), the server stores ciphertext beside legacy plaintext, the worker merges sealed files | 3 | patch: optional fields, and nothing changes until the owner runs `recall keys init` | L, ~2,500 |
| 5 | **Ciphertext only**: protocol 2, plaintext refused, legacy rows migrated, merge and the `claude` CLI gone from the API image | 4 | **breaking**, three rules below | M, ~1,500 |
| 6 | **Evaluation reports** by the worker | 4; before or after 5 | patch | M, ~1,500 |
| 7 | **Strict end-to-end**: revoke the worker, rotate and re-encrypt, merge on the clients | 5 | patch: opt-in | L, ~2,500 |

Sizes count code, tests, fixtures and docs, against Part 2a's server half,
which came to about 6,200 lines. PR 2 may split the way Part 2 did, server
first and binary second.

Key management (3) is its own pull request, ahead of the first encrypted
byte (4). It is the part with the most ways to be subtly wrong, and this way
it is reviewed and tested before anything depends on it.

Where two pull requests are in flight at once (1 and 2), whichever merges
second adds the audit leaves for the other's actions.

### Which one is breaking, and why only that one

PR 5, under three items of [`releasing.md`](../reference/releasing.md#versioning):

- *"a previously accepted request refused"*: a push carrying plaintext
  `content` is refused, and so is every protocol 1 request.
- *"An environment variable is removed"*: `RECALL_CLAUDE_BIN`,
  `RECALL_MERGE_TIMEOUT_MS` and `RECALL_CLAUDE_STATUS_INTERVAL_MS` stop
  meaning anything to `recall-server`. They move to `recall-worker` under the
  same names. `RECALL_MERGE_ENABLED` keeps its meaning: whether a stale push
  is queued for merging at all.
- *"The HTTP API stops working between an old client and a new server"*: an
  old client can neither read nor write memory there.

The design says moving merge behind a queue *"changes the push contract, so
it ships in a minor release"*. Under the rules it does not, provided PR 2
keeps answering a push with `200` and the same fields. A queued merge reads
as `merged: false` now and a merged file on a later pull, which is exactly
what a degraded merge looks like today. A deployment with no worker keeps
merging inline, so no variable changes meaning. PR 2 is a patch.

PR 4 is a patch as well: it adds optional fields, and nothing changes until
the owner runs `recall keys init`. That command is the moment an old client
starts missing files written by new ones, so it refuses to run while
`recall devices list` shows a device whose agent predates PR 4 (unless
forced), names the machines that would go quiet, and warns that any machine
still on the shared token is among them. For that list to be right, the
server refreshes a device's `agent` whenever it records `last_seen`; Part 2a
records it only at enrolment.

### Keeping old clients working through the transition

Honestly, it cannot be done once memory is encrypted. A client without the
key cannot read, and no shim can read for it without putting the key back on
the server. What the plan can do is make the order safe and the break loud:

1. **Clients first.** From PR 5 the client speaks protocols 1 and 2 and picks
   whichever the server's discovery document supports. Every machine can be
   upgraded while the server still runs the previous release; nothing breaks
   until the server is upgraded.
2. **Loud, not silent.** A PR 5 server supports only protocol 2 and raises
   `min_client`. An old client's first request at session start gets the
   refusal Part 1 built for exactly this, naming the upgrade, and the hook
   prints it as one line.
3. **Cloud sessions** install the current client every time and are not
   affected.
4. **Machines on the shared token** have no device key, so they can hold no
   grant; after PR 5 they cannot sync at all. That makes PR 5's minor release
   the natural place for Part 2's third step, removing `bearer` from the sync
   routes. The operator keeps it for approving the first device.

## Wire contracts

Everything below follows [`api.md`](../reference/api.md): every non-2xx is
`{"error": "…"}`, timestamps have the `Date.toISOString()` shape, an absent
value is `null` rather than omitted unless a section says otherwise, and a new
field goes after the existing ones, because field order is part of the frozen
surface. In the Auth columns, "yes" is either credential, "device" is a
signed request from any device, "admin device" a signed request from an
`admin` device, and "worker" a signed request from a `worker` device.

**Fixtures, in every pull request.** The shapes a pull request adds or
changes are captured into a new directory,
`crates/recall-wire/fixtures/wire/<release>/`, named for the release that
ships it, and never into an existing one. Each new kind gets an arm in
`round_trip` and an entry in `every_kind_has_a_fixture` in
`crates/recall-wire/tests/golden.rs`. A kind whose shape changes gets a new
file in the new directory; the old file stays as proof that today's types
still read it. The only rewrite the fixtures README allows, replacing a dev
capture with the release capture, stays the only one.

### PR 1: audit

| Route | Auth | Purpose |
|---|:---:|---|
| `GET /v1/audit/checkpoint` | yes | The tree's size and root |
| `GET /v1/audit/entries?start=&end=` | admin | Leaves `start` to `end - 1`, at most 1,000 and 2 MiB |
| `GET /v1/audit/consistency?first=&second=` | yes | The RFC 9162 §2.1.4 proof that `second` extends `first` |

The leaves name every project, file, device and authkey, which the device
list and `/admin/stats` already keep to the `admin` scope, so `entries` is
admin too; the checkpoint and the proofs are hashes, for any credential.

```json
{ "tree_size": 1042, "root_hash": "CsUYapGGPo4dkMgIAUqom/Xajj7h2fB2MPA3j2jxq2I=" }
```

```json
{
  "start": 1000,
  "end": 1002,
  "tree_size": 1042,
  "entries": [
    "{\"v\":1,\"seq\":1000,\"at\":\"2026-10-02T09:14:03.118Z\",\"action\":\"pull\",…}",
    "{\"v\":1,\"seq\":1001,\"at\":\"2026-10-02T09:14:05.402Z\",\"action\":\"push\",…}"
  ]
}
```

```json
{ "first": 900, "second": 1042, "proof": ["t8Qm…=", "Hc0v…="] }
```

Each entry is a leaf exactly as stored; the string's UTF-8 bytes are what is
hashed, and a client never re-serializes one. Tree hashes are standard
base64, as in C2SP checkpoints; file hashes stay lowercase hex, as
`base_sha256` is today. `end` beyond `tree_size`, or a page over 1,000, is a
`400`; a page whose leaves come to more than 2 MiB stops early, its `end`
saying where. Reading the audit routes appends nothing.

`GET /sync` answers also carry `Recall-Audit-Checkpoint: 1042 CsUY…=`, so
every pull leaves the client a checkpoint without another request.

**The leaf, version 1.** One per authenticated push, pull and change to a
device or authkey (an authkey enrolling a device included), one per merge
job leased, settled or retried, and one per action the server takes
itself. Unauthenticated routes and refused requests
append nothing, so the internet cannot grow the log. Shown indented; stored
on one line, compact, in this field order:

```json
{"v":1,"seq":1001,"at":"2026-10-02T09:14:05.402Z","action":"push",
 "actor":{"kind":"device","id":"dev_eerivjyffuwecbgzybcesz5hwi","name":"laptop","agent":"recall/0.4.5 (macos-aarch64)"},
 "subject":{"project_key":"acme/app","file_path":"topics/auth.md","deleted":false,
            "stored_sha256":"4b1f…","base_sha256":"9f2c…","merged":false,"merge_job":null},
 "request":{"body_sha256":"47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "signature_base":"\"@method\": POST\n\"@authority\": recall-server.pimlabs.id\n…","signature":"…",
            "body":null}}
```

| Field | Meaning |
|---|---|
| `v` | Leaf format, `1`. |
| `seq` | Its index in the tree, from 0. |
| `at` | Taken under the store's lock with `seq`, and never below the leaf before it. |
| `action` | `push`, `delete`, `pull`, `approve`, `enroll`, `deny`, `revoke`, `sweep`, `authkey_create`, `authkey_revoke`, `start`, and, with PR 2's queue, `job_claim`, `job_result` and `job_retry`. Later pull requests add `key_create`, `key_grant`, `seal`, `evaluate` and `strict`. |
| `actor.kind` | `device` (a signed request), `operator` (`RECALL_TOKEN`), `authkey` (by id and tag, for the device it enrols) or `server` (its own sweeps; `start`, which records the version it started as; and the jobs it settles itself: a merge with no worker left, a lease run out, a job failed for want of anything to merge it). |
| `subject` | Per action. `approve` and `enroll` leaves carry the device's `public_key`, and from PR 3 its `encryption_key`, so the log verifies without the `devices` table. A push says whether the server `merged` it inline, or names the `merge_job` it queued; that job's `job_result` says what the file became, by hash. |
| `request` | For a signed request: the SHA-256 of its body in base64, as its `Content-Digest` carries it; the RFC 9421 signature base the server verified; the signature; and the body itself for the device-management actions, whose bodies are small and hold no secret, so what they asked for is bound to the subject. `null` otherwise. A push's body, the file, is not kept: its signature proves the device pushed at that moment, not which file the server says it was. |

Discovery gains `"audit": { "leaf_version": 1, "max_page": 1000,
"max_page_bytes": 2097152 }`.

Fixtures, captured from a server with a push `openssl` signed:
`audit_checkpoint_response`, `audit_entries_response`,
`audit_consistency_response`, `audit_leaf_push`, `audit_leaf_approve`,
`audit_leaf_enroll`, `discovery`.

### PR 2: jobs and the worker

| Route | Auth | Purpose |
|---|:---:|---|
| `POST /v1/jobs/claim` | worker | Wait up to `wait_seconds` for a job, and lease it |
| `POST /v1/jobs/{id}/result` | worker | Hand back a result or an error, under the current lease |
| `GET /v1/jobs?state=` | admin | Jobs, newest first, at most 200, without file content |
| `POST /v1/jobs/{id}/retry` | admin | Queue a failed job again |

`POST /v1/devices/approve` accepts `"scope": "worker"`, and a device's
`scope` may be `worker`. A worker may use the job routes, the audit routes
and, from PR 6, `GET /sync` and `GET /admin/stats`; nothing else. A `sync` or
`admin` device on a worker route gets `403`, as a worker does on theirs.

The claim:

```json
{
  "kinds": ["merge"],
  "wait_seconds": 25,
  "lease_seconds": 120,
  "claude_cli": { "checked_at": "2026-10-02T09:13:40.002Z", "available": true, "logged_in": true, "error": "" }
}
```

`wait_seconds` is 0 to 30 and `lease_seconds` 30 to 600. `claude_cli` is how
`/health` keeps reporting the CLI once it has left the API process.

```json
{
  "job": {
    "id": "job_3m5k7q2x9w4r8t6y",
    "kind": "merge",
    "lease_id": "lse_q8w2e4r6t8y0u2i4o6p8a0s2d4",
    "lease_expires_at": "2026-10-02T09:16:03.118Z",
    "attempt": 1,
    "merge": {
      "project_key": "acme/app",
      "file_path": "topics/auth.md",
      "stored": {
        "sha256": "9f2c…",
        "content": "# Auth\n- tokens live in 1Password\n",
        "source_env": "laptop",
        "updated_at": "2026-10-02T09:10:11.020Z"
      },
      "incoming": {
        "sha256": "4b1f…",
        "content": "# Auth\n- rotate them monthly\n",
        "source_env": "cloud",
        "updated_at": "2026-10-02T09:14:02.991Z"
      }
    }
  }
}
```

`{ "job": null }` when nothing arrived within `wait_seconds`. `stored` is the
version the push displaced, `incoming` the one it stored.

The result, or an error:

```json
{ "lease_id": "lse_q8w2e4r6t8y0u2i4o6p8a0s2d4", "merge": { "content": "# Auth\n- tokens live in 1Password\n- rotate them monthly\n" } }
```

```json
{ "lease_id": "lse_q8w2e4r6t8y0u2i4o6p8a0s2d4", "error": "claude merge timed out after 45s" }
```

```json
{ "id": "job_3m5k7q2x9w4r8t6y", "state": "done", "applied": true, "follow_up": null }
```

| Code | When |
|:---:|---|
| `200` | Recorded. `applied: false` with a `follow_up` job id when the file changed while the job ran (see [Jobs and leases](#jobs-and-leases)). |
| `400` | Neither `merge` nor `error`, or both. |
| `403` | Not a worker. |
| `404` | `{"error":"no job has that id"}` |
| `409` | `{"error":"this lease has ended; the job was handed out again"}` |

The listing is `{ "jobs": [ … ] }`, each with `id`, `kind`, `state`
(`queued`, `leased`, `done`, `failed`), `project_key`, `file_path`,
`attempt`, `created_at`, `updated_at`, `error` and `follow_up`. Retry answers
with the job as listed.

`POST /sync`'s response gains `merge_job`, a job id or `null`:

```json
{ "ok": true, "project_key": "acme/app", "file_path": "topics/auth.md", "deleted": false, "merged": false, "updated_at": "2026-10-02T09:14:02.991Z", "merge_job": "job_3m5k7q2x9w4r8t6y" }
```

`/health`'s `merge` object gains two members, omitted while no worker is
enrolled; `claude_cli` keeps its place and is now the worker's report:

```json
"worker": { "last_claim_at": "2026-10-02T09:14:03.118Z", "agent": "recall-worker/0.4.3 (linux-x86_64)" },
"queue": { "queued": 0, "leased": 1, "failed": 0, "oldest_queued_at": null }
```

Discovery gains `"merge_queue": {}`.

Fixtures: `job_claim_request`, `job_claim_response`,
`job_claim_response_empty`, `job_result_request`, `job_result_request_error`,
`job_result_response`, `job_list_response`, `push_response_queued`,
`device_approve_request_worker`, `health`, `discovery`.

### PR 3: keys

| Route | Auth | Purpose |
|---|:---:|---|
| `PUT /v1/devices/self/encryption-key` | device | Register this device's X25519 key, once |
| `GET /v1/keys` | device | Key versions, and the wraps addressed to the caller |
| `POST /v1/keys` | admin device | Create the next key version, with its wraps |
| `POST /v1/keys/wraps` | admin device | Add wraps for existing versions |
| `GET /v1/keys/recovery` | admin device | The recovery public key and its wraps |

A grant must be attributable to a device, so `RECALL_TOKEN` gets `403` on
every key route.

The register route's body is below. `EnrollRequest` gains the same two
fields, optional, and `PendingEnrollment` and `Device` gain them as well,
`null` when absent.

```json
{
  "encryption_key": "hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo",
  "encryption_key_binding": "0Ag9…"
}
```

| Field | Meaning |
|---|---|
| `encryption_key` | An X25519 public key: 32 bytes, base64url, no padding. |
| `encryption_key_binding` | The device's Ed25519 signature, base64url, over `"recall/encryption-key/v1" \|\| ed25519_public_key \|\| encryption_key`. |

`ApproveRequest` gains `fingerprint`, the key fingerprint the approver
compared. When present, the server approves only if it matches the pending
enrolment's key, else `409` `{"error":"that code now belongs to a different
key; start the enrolment again"}`. Part 2a's signed approval names only the
code, so without this field the audit log cannot show which key an admin
device vouched for; grants rely on it (see [Grants](#grants-and-why-a-device-can-trust-one)).

`GET /v1/keys`:

```json
{
  "current": 2,
  "versions": [
    { "version": 1, "ck_id": "KD7Q-M2XP-4RNC-8TWB", "created_at": "2026-10-09T08:00:12.504Z", "created_by": "dev_eerivjyffuwecbgzybcesz5hwi", "retired_at": null },
    { "version": 2, "ck_id": "P3HZ-9WQD-T6LM-2KXV", "created_at": "2026-11-20T17:41:55.010Z", "created_by": "dev_eerivjyffuwecbgzybcesz5hwi", "retired_at": null }
  ],
  "wraps": [
    { "version": 1, "wrap": "Qx7n…", "grantor": "dev_eerivjyffuwecbgzybcesz5hwi", "grantor_public_key": "JrQLj5P_…", "grantor_signature": "m1Vd…", "created_at": "2026-10-09T08:00:12.504Z" }
  ],
  "recovery_public_key": "Yk3s…"
}
```

`POST /v1/keys`, where `version` must be `current + 1` (else `409`) and the
first version must include a wrap to the caller and one to recovery:

```json
{
  "version": 1,
  "ck_id": "KD7Q-M2XP-4RNC-8TWB",
  "recovery_public_key": "Yk3s…",
  "wraps": [
    { "recipient_kind": "device", "recipient_id": "dev_eerivjyffuwecbgzybcesz5hwi", "wrap": "Qx7n…", "grantor_signature": "m1Vd…" },
    { "recipient_kind": "recovery", "recipient_id": "recovery", "wrap": "Ze0p…", "grantor_signature": "c9Tr…" }
  ]
}
```

It answers as `GET /v1/keys` does for the caller. `POST /v1/keys/wraps` takes
`{ "wraps": [ … ] }`, each as above plus `version`, and answers
`{ "added": 1 }`; a wrap that already exists for that version and recipient
is a `409`.

**Enrolment keys.** `POST /v1/enroll-keys` accepts
`"content_key_wraps": [ { "version": 1, "wrap": "…" } ]`, from an admin
device only. `EnrollKey` gains `"unwrap_half": true` when it has wraps, and
the approved-with-a-key enrolment answer gains `"content_key_wraps"` (`[]`
when there are none). An `enroll_key` containing a `.` is refused with `400`
before anything is hashed or stored: the part after the dot is the half the
server must never see.

Discovery gains `"content_keys": { "wrap": "hpke-x25519-sha256-chacha20poly1305", "unwrap_half": "hkdf-sha256-xchacha20poly1305" }`.

Fixtures: `enroll_request_with_encryption_key`,
`device_encryption_key_request`, `device_approve_request_with_fingerprint`,
`keys_response`, `keys_create_request`,
`keys_wraps_request`, `keys_wraps_response`, `keys_recovery_response`,
`enroll_key_create_request_with_wraps`, `enroll_response_approved_with_wraps`,
`device_pending_response`, `device_list_response`, `discovery`.

### PR 4: sealed sync

`PushRequest` gains `sealed`, after `base_sha256`:

```json
{
  "project_key": "acme/app",
  "file_path": "topics/auth.md",
  "source_env": "laptop",
  "base_sha256": "4b1f…",
  "sealed": "v1.2.W2xR8…"
}
```

A write carries exactly one of `content` and `sealed`, else `400`. A delete
may carry `sealed`: a sealed tombstone, described under [Keys](#the-content-key).
`sealed` must look like `v1.<version>.<base64url>` and decode to at least 40
bytes, which is all the server can check.

`File` in `GET /sync` gains `sealed`, last, `null` for a plaintext row:

```json
{ "file_path": "topics/auth.md", "content": null, "source_env": "laptop", "updated_at": "2026-10-02T09:14:02.991Z", "deleted": false, "sealed": "v1.2.W2xR8…" }
```

An old client reads that as a file with no content that is not deleted, and
its pull already treats a missing `content` as nothing to write
(`recall_hooks::pull`): it writes no ciphertext to disk and deletes nothing.

`base_sha256` keeps its name and widens its meaning: the SHA-256 of the exact
string last pulled or pushed for the file, `content` for a plaintext row and
`sealed` for a sealed one. The server hashes whichever it holds the same way,
so the `If-Match` comparison (RFC 9110 §13.1.1) works on ciphertext unchanged.

Jobs: `stored` and `incoming` gain `sealed` (with `content: null` for a
sealed version), and a merge result may be `{ "merge": { "sealed": "…" } }`.
`/health` gains `"storage": { "plaintext_files": 12, "sealed_files": 240 }`.
Discovery gains `"sealed_sync": { "formats": ["v1"] }`.

Fixtures: `push_request_sealed`, `push_request_delete_sealed`,
`sync_response_sealed` (a sealed file, a plaintext file and both kinds of
tombstone), `job_claim_response_sealed`, `job_result_request_sealed`,
`health`, `discovery`.

### PR 5: ciphertext only

- Discovery: `"protocol": { "current": 2, "supported": [2] }`, and
  `min_client` is this release. Protocol 2 is protocol 1 with `content` never
  accepted, and sent only for a legacy row the migration has not sealed yet.
- A protocol 1 request, which includes one with no `Recall-Protocol` header,
  gets the existing refusal: `{"error":"this server speaks Recall protocol 2,
  and the request asked for 1. Upgrade whichever side is older; GET
  /.well-known/recall says what this server supports"}`.
- A protocol 2 push with `content` is a `400`: `{"error":"this server stores
  only encrypted memory; send sealed, not content"}`.
- On a deployment where nobody has run `recall keys init`, every push is a
  `409`: `{"error":"no content key exists yet; run recall keys init on an
  admin device"}`. Such a deployment cannot sync until it does, and the
  changelog says so first.
- `File.content` is `null` for every row once the migration is done. The
  field stays, so the type does not change.
- Job kind `seal`, for the migration. The claim carries `"seal": {
  "project_key", "file_path", "sha256", "content", "deleted", "source_env",
  "updated_at" }`; the result is `{ "seal": { "sealed": "…", "retained": null } }`,
  `retained` being a legacy tombstone's kept content, sealed.

The golden-fixture rule meets its first breaking release here, and no
fixture file changes. `every_shape_that_ever_shipped_still_reads` keeps
passing, since today's types still parse every old shape.
`every_push_a_released_client_sent_is_still_accepted` changes its expectation
for releases below `min_client`: each of their request fixtures must now get
the documented protocol refusal, which pins that an old client is told, not
ignored.

Fixtures: `discovery`, `error_protocol`, `sync_response`,
`job_claim_response_seal`, `job_result_request_seal`.

### PR 6: evaluation

| Route | Auth | Purpose |
|---|:---:|---|
| `POST /v1/evaluations` | admin | Ask for a run, of every project or some |
| `GET /v1/evaluations` | admin | Runs, newest first, with counts |
| `GET /v1/evaluations/{id}` | admin | One run: its findings, and its sealed details |

```json
{ "projects": ["acme/app"] }
```

```json
{ "id": "eval_7c2kq9", "state": "queued", "job": "job_8d1xk2m4q6w8e0r2" }
```

```json
{
  "id": "eval_7c2kq9",
  "state": "done",
  "created_at": "2026-12-01T06:00:00.004Z",
  "finished_at": "2026-12-01T06:01:12.880Z",
  "findings": [
    { "id": "f1", "kind": "secret", "severity": "high", "project_key": "acme/app", "file_path": "topics/deploy.md", "lines": [12, 12], "related": [] },
    { "id": "f2", "kind": "contradiction", "severity": "medium", "project_key": "acme/app", "file_path": "topics/auth.md", "lines": [3, 5],
      "related": [ { "project_key": "global:eko", "file_path": "tools.md" } ] }
  ],
  "details": "v1.2.8LkQ…"
}
```

`kind` is one of `duplicate`, `stale`, `contradiction`, `secret`,
`wrong_scope`, `dead_link`, and `severity` one of `low`, `medium`, `high`.
A finding holds only those enums, paths and line numbers, and the server
refuses a result with any other key: that is what keeps note text out of the
one part of a report the API can read. `details` is sealed with the content
key and holds the excerpts, the reasoning and a suggested edit per finding.

The listing is `{ "evaluations": [ { "id", "state", "created_at",
"finished_at", "counts": { "secret": 1, "contradiction": 1 } } ] }`. Job kind
`evaluate`: the claim carries `{ "evaluation_id", "projects" }`, the result
`{ "evaluate": { "findings": [ … ], "details": "…" } }`. Discovery gains
`"evaluation": {}`.

Fixtures: `evaluation_request`, `evaluation_created_response`,
`evaluation_list_response`, `evaluation_response`,
`job_claim_response_evaluate`, `job_result_request_evaluate`, `discovery`.

### PR 7: strict

| Route | Auth | Purpose |
|---|:---:|---|
| `POST /v1/strict` | admin device | Turn strict mode on or off |
| `GET /v1/versions/{sha256}` | yes | A retained sealed version, for a three-way merge on the client |
| `POST /v1/keys/{version}/retire` | admin device | Stop granting a key version to new devices |

```json
{ "enabled": true, "key_version": 3 }
```

Turning it on is a `409` unless `key_version` is current and no worker is
unrevoked. While strict: no merge job is made, approving a `worker` is a
`409`, `merge_queue` leaves discovery and `"strict": { "since_key_version": 3 }`
joins it.

A stale push is stored as a failed merge is today, and the answer gains two
fields so a new client can merge by itself. Outside strict mode they are
`false` and `null`:

```json
{ "ok": true, "project_key": "acme/app", "file_path": "topics/auth.md", "deleted": false, "merged": false, "updated_at": "2026-12-03T10:02:44.310Z", "merge_job": null, "stale": true, "displaced_sha256": "9f2c…" }
```

```json
{ "project_key": "acme/app", "file_path": "topics/auth.md", "sha256": "9f2c…", "sealed": "v1.3.Tq0w…", "created_at": "2026-12-03T09:58:01.774Z" }
```

A version past its retention is a `404`, and the client falls back to a
conflict copy.

Fixtures: `strict_request`, `strict_response`, `version_response`,
`push_response_stale`, `discovery`.

## Storage

Every table is created with `CREATE TABLE IF NOT EXISTS` when the store
opens, as Part 2a's are. `memory_files` is never altered and never dropped;
after PR 5 it is simply empty. That keeps the rollback property the store
was built around: an older server started on a newer database ignores tables
it does not know and serves what is in `memory_files`, which is never
ciphertext and never an empty stand-in. A file sealed since then is absent to
it, and an old client does not delete a file for being absent.

| PR | Table | Columns, abridged |
|---|---|---|
| 1 | `audit_log` | `seq INTEGER PRIMARY KEY`, `leaf BLOB NOT NULL`, `leaf_hash BLOB NOT NULL`; triggers abort any `UPDATE`, `DELETE`, or insert that is not the next `seq` (which also stops `INSERT OR REPLACE`) |
| 2 | `jobs` | `id`, `kind` (`merge`; `seal` and `evaluate` arrive later, so no `CHECK` on it), `state`, `project_key`, `file_path`, `payload` (JSON), `lease_id`, `lease_expires_at`, `attempt`, `not_before`, `parent_id`, `error`, `created_at`, `updated_at` |
| 2 | `devices`, rebuilt | the same columns with `scope IN ('sync', 'admin', 'worker')`. SQLite cannot alter a `CHECK`, so the rows are copied into a new table in one transaction, guarded by `PRAGMA user_version`. An older server reads a `worker` row as a device without admin rights, which can sync: acceptable for a device the owner approved |
| 3 | `devices`, `device_enrollments` | add `encryption_key`, `encryption_key_binding` |
| 3 | `content_keys` | `version INTEGER PRIMARY KEY`, `ck_id`, `created_at`, `created_by`, `retired_at` |
| 3 | `content_key_wraps` | `version`, `recipient_kind` (`device`, `recovery`), `recipient_id`, `wrap`, `grantor`, `grantor_signature`, `created_at`; key `(version, recipient_kind, recipient_id)` |
| 3 | `recovery` | one row: `public_key`, `created_at` |
| 3 | `enroll_key_wraps` | `enroll_key_id`, `version`, `wrap` |
| 4 | `memory_sealed` | `project_key`, `file_path`, `sealed`, `stored_sha256`, `key_version`, `source_env`, `device_id`, `updated_at`, `deleted`; key `(project_key, file_path)` |
| 4 | `memory_versions` | `stored_sha256 PRIMARY KEY`, `project_key`, `file_path`, `body BLOB` (the exact signed request body), `created_at`; pruned after `RECALL_VERSION_RETENTION_DAYS`, 90 by default |
| 6 | `evaluations` | `id`, `state`, `projects`, `findings` (JSON), `details` (sealed), `created_at`, `finished_at` |
| 7 | `settings` | `key`, `value`; holds `strict` |

Invariants, each pinned by a test:

- A file is in at most one of `memory_files` and `memory_sealed`. A sealed
  write deletes the plaintext row in the same transaction.
- A state change and its audit leaf commit in one transaction. The store has
  no transactions today; PR 1 adds them.
- Nothing retained is plaintext. `memory_versions` holds sealed pushes only,
  and a plaintext push's leaf records the hash of its body, not the body.

**PR 5's migration.** At start, if a worker holding the key is enrolled, a
PR 5 server queues one `seal` job per plaintext row, tombstones included. The
worker seals each and posts it back.
If the row still has the hash the job saw, the server inserts the sealed row
with the original `updated_at` and `source_env`, so the migration does not
read as an edit, and deletes the plaintext row under `PRAGMA secure_delete =
ON`, which makes SQLite overwrite the freed content with zeros. Without a
worker, `recall keys migrate` does the same from a device holding the key:
pull, seal, push with the plaintext's hash as the base. Plaintext rows are
served read-only until sealed. When `plaintext_files` reaches 0 the server
runs `VACUUM`. Snapshots taken before that still hold plaintext: they age out
after `RECALL_BACKUP_KEEP` runs, and off-box copies after whatever retention
the remote keeps, which `deploy/README.md` has to say plainly.

### The audit chain

**What is hashed.** The leaf, as the bytes the server wrote when it appended
it: compact JSON from `recall-wire`'s serializer, fields in declaration
order, UTF-8. It is stored and exported as those bytes and never
re-serialized, so no canonical JSON scheme is needed: a verifier hashes what
it was given.

**How.** RFC 9162 §2.1.1 with SHA-256. A leaf hashes as
`SHA-256(0x00 || leaf)`, an inner node as `SHA-256(0x01 || left || right)`,
and a tree of `n` leaves splits at the largest power of two below `n`. The
two prefixes are the domain separation the RFC says *"is required to give
second preimage resistance"*. The server keeps the hash of every complete
subtree in memory, 64 bytes a leaf (64 MB at a million), rebuilt at start
from the leaves themselves, each rehashed and compared with its stored
`leaf_hash` (8.5 s at a million on a small VM, against 1.8 s to read the
stored hashes alone; a log that fails to match stops the start). An append costs O(log n) hashes, a root O(log n), and a consistency
proof (§2.1.4) O(log² n), a few hundred, with no read of the database. The
first version recomputed each proof from every `leaf_hash` under the
store's one lock, about 2 s at a million leaves, during which no push or
pull could run: any credential could stall the server that way.

**How a proof is checked.** A verifier rebuilds both roots from the proof —
the old tree's and the new one's — walking `SUBPROOF`'s recursion, and
accepts only when the old one is the checkpoint it trusts and the new one
the root it was given. The first version rebuilt only the new root, and
looked at the old one only when the first size was a power of two, so a
proof cut from a tree with its early leaves rewritten verified against the
honest checkpoint. Every probe in transparency-dev/merkle's
`testdata/consistency` now gets its verdict.

**Who witnesses.** A Merkle tree proves something only to someone holding an
earlier root. Every pull stores the `Recall-Audit-Checkpoint` it received in
`~/.recall/audit.json`; `recall doctor` asks for a consistency proof from it
to the current tree and fails loudly without one, and the worker does the
same on each claim. A client stores each checkpoint as the three lines of a
C2SP tlog-checkpoint note, with the server's address as the origin and no
signature. The owner's devices fetch it over TLS from the very server they
are checking, so a server signature adds nothing for one owner. Sigstore Rekor and Certificate
Transparency sign theirs because their verifiers are strangers; a signature
can be added if a third-party witness is ever wanted.

**Verifying offline.** `recall audit export > audit.jsonl` writes one leaf per
line, after a first line holding the checkpoint it exported at.
`recall audit verify audit.jsonl` then needs no network:

1. `seq` runs from 0 without gaps.
2. The root recomputed with §2.1.2 matches the export's checkpoint, and, for
   each checkpoint in `~/.recall/audit.json` at size `m`, the root over the
   first `m` leaves matches it.
3. Device keys come from the log's own `approve` and `enroll` leaves: the
   one before each signed leaf, of a device not revoked or swept since, and
   never two for one id. Each signed leaf's `signature` verifies over its
   `signature_base` with that key, the base's `keyid` is the actor, no
   `(keyid, nonce)` appears twice, its `content-digest` line equals
   `request.body_sha256`, its method, path and query are the action's, and
   a kept body hashes to `body_sha256` and asks for what `subject` records.
   A device's leaf without a request, or anyone else's with one, fails.
4. Where `memory_versions` still holds a body (from PR 4), the body hashes to
   `body_sha256` and carries the sealed string whose hash is
   `subject.stored_sha256`.

`scripts/audit-verify.py` does all but 4 with the standard library alone,
a second implementation of the tree and of Ed25519 (the `cryptography`
package, when it works, only speeds the second up); it never skips the
signatures unless told to. The integration tests run it over a log they
produce, and over every forgery the review found a server could build from
it.

## Keys

### The content key

One per owner and version: 32 bytes from the OS CSPRNG, made by
`recall keys init` on an admin device, which also shows the recovery key and
offers to replace existing enrolment keys with two-part ones. Versions count
from 1. A client encrypts with the highest version it holds, whatever the
server calls current, so a server cannot push a client back to a key a
revoked device knows. `ck_id` is the first 10 bytes of
HKDF-SHA256(key, info `"recall/ck-id/v1"`) in base32, shown as
`KD7Q-M2XP-4RNC-8TWB` by `recall keys status` on every device: 80 bits, too
many to grind a lookalike key for.

**Files** are sealed with XChaCha20-Poly1305 and a fresh random 24-byte nonce
each time. libsodium: *"its large nonce size (192-bit) allows random nonces
to be safely used"*; the CFRG draft puts a 2^-32 chance of collision at 2^80
messages under one key. The associated data binds a ciphertext to its place,
so the server cannot move a sealed body to another file, project, version or
tombstone state:

```
AD     = "recall/file/v1" || u16 len || owner_id || u16 len || project_key
                          || u16 len || file_path || u32 version || u8 deleted
sealed = "v1." version "." base64url(nonce || ciphertext || tag)
```

`owner_id` is the Part 3 reservation, always `owner` today. A tombstone is
sealed too, over an empty plaintext with `deleted = 1`. From PR 5, when every
writer seals, a device honours only a sealed tombstone, so a compromised
server cannot make the owner's machines delete notes; during PR 4's mixed
period it still honours an old client's unsealed one, as today. And a client
does not push a file whose plaintext is unchanged since it last synced it:
with random nonces the server can no longer spot an unchanged re-push, and
each one would queue a merge. `.recall-state.json` gains a `plain_sha256` map
beside `bases` for this, a format change Recall migrates itself.

### Device encryption keys: separate, not converted

A device's Part 2 key is Ed25519, a signing key. There are two ways to
encrypt to a device:

- **Convert it** to X25519, as libsodium's
  `crypto_sign_ed25519_pk_to_curve25519` and ed25519-dalek's
  `VerifyingKey::to_montgomery` do. Nothing new to store or register, and
  existing devices could be granted a key at once. Both sources advise
  against it. libsodium: *"If you can afford it, using distinct keys for
  signing and for encryption is still highly recommended."* ed25519-dalek,
  already in this workspace: *"We do NOT recommend this usage of a
  signing/verifying key. … If you can help it, use a separate key for
  encryption."* The analysis both cite (Thormarker, IACR ePrint 2021/509)
  covers one construction, not reuse in general. age converts SSH Ed25519
  keys only because it must accept keys that already exist, and its own
  documentation says those recipient types *"should only be used for
  compatibility with existing keys, and native keys should be preferred
  otherwise."*
- **A separate X25519 key per device**, generated beside the Ed25519 key and
  bound to it by an Ed25519 signature, `encryption_key_binding`. One more
  keychain item and one more field.

Recall is designing its protocol now, so it can afford the second, and takes
it. The binding matters as much as the key: without it, the server could hand
a grantor its own X25519 key under a real device's name.

**Wrapping** is HPKE (RFC 9180) single-shot `SealBase` (§6.1) with
DHKEM(X25519, HKDF-SHA256), HKDF-SHA256 and ChaCha20Poly1305, identifiers
`0x0020`, `0x0001` and `0x0003` (§7), with `info = "recall/content-key/v1"`
and `aad = u32 version || recipient_kind || 0x00 || recipient_id`. A wrap is
`base64url(enc || ciphertext)`, 80 bytes. HPKE rather than a hand-built
ECIES: it is a standard with test vectors, Appendix A.2 being this exact
suite, and age's newer recipient types are defined as the same `SealBase`
call with the same KDF and AEAD.

**Libraries**: `hpke` 0.13, `x25519-dalek` 2, `chacha20poly1305` 0.10 and
`hkdf` 0.12, the generation that matches `ed25519-dalek` 2 and `sha2` 0.10
already in the workspace and its declared Rust 1.82. `hpke` 0.14 and the
dalek 3 crates need Rust 1.85; that move is one later pull request for the
whole set.

### Grants, and why a device can trust one

HPKE's base mode does not authenticate the sender: RFC 9180 §9.1 lists sender
authentication for the PSK, Auth and AuthPSK modes only. Anyone who knows a
device's public key can wrap *a* key to it, a compromised server wrapping one
it knows included. So:

- **Only a holder grants.** A wrap is made on a device that holds the key,
  never by the server and never by the worker. Each carries the grantor's
  Ed25519 signature over `"recall/grant/v1" || u32 version || recipient_kind
  || 0x00 || recipient_id || 0x00 || SHA-256(wrap) || ck_id`.
- **A grantor wraps only to a device it can vouch for**: one it approves
  now, after the owner compares fingerprints (Part 2's check), or one whose
  `approve` leaf in the audit log is signed by an admin device the grantor
  already trusts and names the device's `fingerprint`. A device the operator
  approved with `RECALL_TOKEN` has an unsigned approval and needs the
  interactive check. The grantor also checks the recipient's
  `encryption_key_binding`.
- **A new device trusts its first grant on first use**, as SSH trusts a
  host's first key. It pins the grantor and shows `content key KD7Q-… granted
  by laptop (SHA256:…)`, and `recall keys status` shows the same `ck_id`
  everywhere. In practice a substituted key is also noticed at once: the
  device cannot open a single existing file.

### The recovery key

32 random bytes, shown once by `recall keys init` as `recall-rk-` and 52
base32 characters, and never stored by Recall. It is an HPKE identity:
`DeriveKeyPair` (RFC 9180 §7.1.3) turns it into an X25519 key pair, the
server keeps the public half, and every key version is wrapped to it like a
device. Being wrapped to a public key, it never has to be typed in for a
rotation. `recall keys recover`, on a freshly approved admin device, asks for
it, opens every version and wraps them to that device.

### The two-part cloud enrolment key

```
recall-ek-<E: 52 base32 characters>.<U: 52 base32 characters>
```

`recall-ek-<E>` is exactly a Part 2a enrolment key: the client sends it, the
server stores its SHA-256, and enrolment is unchanged. `U` is 32 random bytes
the client never sends. When the key is made, the admin device derives
`K_U = HKDF-SHA256(U, info "recall/enroll-unwrap/v1")` and uploads each live
key version sealed under it with XChaCha20-Poly1305, `aad = "recall/enroll-wrap/v1"
|| u32 version`. A cloud session enrolling with `E` receives those wraps and
caches them in `~/.recall`; each hook re-derives `K_U` from
`RECALL_ENROLL_KEY` to open them, so the content key is never written to the
container's disk.

This half is symmetric on purpose. The server never learns `U`, so it cannot
make a wrap that opens, and the substitution problem above does not arise
here. The cost is that only whoever held `U` could wrap a new version for it;
see rotation.

Part 2b's client must parse the key before this exists: split at the dot and
send only `recall-ek-<E>`. The server refuses any key containing a dot, but
by then `U` has already crossed the wire.

### Rotation and revocation

| Event | What happens to memory |
|---|---|
| `recall devices revoke laptop` | Its requests are refused at once (Part 2). It still holds every key version it had, which matters only if it later obtains ciphertext: a leaked backup, or a compromised server. The command asks whether to rotate, and recommends it for a lost or stolen machine. |
| `recall keys rotate` | Version `n + 1`, wrapped to the recovery key, to the worker unless strict, and to every unrevoked device the grantor can vouch for. New writes use it at once; older versions stay readable to current devices, which already hold them. |
| `recall keys rotate --reencrypt` | Also re-seals every row under `n + 1`, by the worker in standard mode and by the CLI in strict, then retires the older versions so no new device is granted them. Strict mode needs it; otherwise a revoked device already had the chance to read those rows before it was revoked. |
| Enrolment keys, at any rotation | Nobody holds their `U`, so no new version can be wrapped for them. Rotation revokes every enrolment key with an unwrap half and prints replacements, which the owner pastes into the cloud environments. |
| An ephemeral cloud device is swept | Nothing to rotate: it held the key only through `U`, which stays with the enrolment key. |
| An enrolment key leaks | Whoever has both halves can enrol and open the key: revoke the enrolment key, then rotate. |

### Where each secret lives

| Holder | Secret | Where |
|---|---|---|
| Laptop | Ed25519 and X25519 device keys | The OS keychain, or the `0600` fallback file Part 2 describes |
| Laptop | Content key versions | Nowhere at rest: each hook opens the cached wraps in `~/.recall/keys.json` |
| Cloud session | `U` | The `RECALL_ENROLL_KEY` variable; wraps cached in `~/.recall` |
| Worker | Ed25519 and X25519 device keys | Its own volume, `0600`, never mounted into the API container |
| Worker | Content key versions | Process memory only |
| Owner | Recovery key | Offline, wherever the owner keeps such things |
| API server | Wraps, public keys, `ck_id` | The database, holding nothing it can open |

## The worker

### Identity and scope

`recall-worker` is its own crate and binary. It depends on `recall-wire` and
an HTTP client, never on `axum` or `rusqlite`. On first start it generates its
keys in `/data`, enrols like any device, and prints its user code and
fingerprint to its log. The owner approves it with
`recall devices approve <code> --worker`, which from PR 3 also grants it
the content key, or with `RECALL_TOKEN` and `curl` from the host, which grants
nothing until a device runs `recall keys grant worker`. It signs every
request, and it opens no port.

The merge code (`merge.rs`: the prompt, and the flags that keep a merge near
$0.01) moves into `recall-worker`. Until PR 5 removes the inline path,
`recall-server` uses it from there with the worker's HTTP client feature
switched off, so the internet-facing crate gains no client code.

### Jobs and leases

- A push whose base is stale, while an unrevoked worker is enrolled, stores
  the incoming version at once (last-write-wins, which is what a failed merge
  does today), creates a `merge` job holding both versions, and answers `200`
  with `merge_job`. With no worker enrolled, the inline merge runs exactly as
  now.
- A claim takes the oldest `queued` job whose `not_before` has passed, marks
  it `leased` with a random `lease_id` and the expiry the worker asked for,
  and wakes as soon as a job arrives, so a merge starts within a second of
  the push.
- A result counts only under the current `lease_id`. An expired lease puts
  the job back in `queued` with `attempt + 1`, and the old holder's late
  result gets `409`: a fencing token, so a worker that stalled past its lease
  cannot overwrite the one that took over.
- **Applying a merge** is a compare-and-swap. If the file still has the hash
  the job stored, the merged content replaces it, attributed to the worker.
  If another push landed meanwhile, the merged content becomes the `stored`
  side of a follow-up job against the new version (`applied: false`,
  `follow_up`). After three links the chain stops, the newest push stands, and
  the unapplied result stays in the failed job, recoverable.
- An error, or an expired lease, retries after 1, 5 and 30 minutes, then
  marks the job `failed` and sets `merge.last_merge_error` in `/health`.
- Identical inputs never reach `claude`: the worker compares the (decrypted)
  bodies first.
- `MEMORY.md` is merged as it is today; see open decision 3.

### While it is down

Pushes keep landing as last-write-wins and jobs wait; no hook ever waits on
the worker. `/health` shows `queue.queued` and `oldest_queued_at`, and
`recall status` and `recall doctor` warn once the oldest job is an hour old,
the same way today's degraded merge is made visible rather than silent. When
the worker returns it drains the queue, and every result goes through the
compare-and-swap above. Sealed files with no worker holding the key are
last-write-wins until one does.

### Where it runs

Recommended: the same compose file, as a second service.

```yaml
  recall-worker:
    profiles: ["worker"]           # opt-in: COMPOSE_PROFILES=worker in deploy/.env
    build: { context: .., dockerfile: deploy/Dockerfile, target: worker }
    restart: unless-stopped
    init: true
    environment:
      RECALL_WORKER_SERVER: http://recall-server:8787   # never the client's RECALL_URL
    volumes:
      - recall-worker-data:/data   # its keys and the claude login; not the server's volume
    networks:
      - backend                    # shared with recall-server only
    # no ports, no expose, no Traefik labels
```

`recall-server` joins `backend` too. The worker reaches the API directly
rather than through Traefik, which suits both checks on the way in: it signs
the `recall-server:8787` authority it actually sends, and with no trusted IP
header on its requests the rate limiter falls back to its container address,
a bucket of its own. The long-poll claim costs about three requests a minute.

The Dockerfile gains a `worker` target carrying Node, the `claude` CLI and
`recall-worker`; after PR 5 the API image carries neither Node nor the CLI.
`claude setup-token` moves to
`docker compose exec -it -u node recall-worker claude setup-token`.
`scripts/compose-check.py` asserts, as `wrangler-check.py` does for the
installer's config, that the worker has no `ports`, `expose` or labels, is on
no ingress network, and shares no volume with the server.

The stronger option is another host, with `RECALL_WORKER_SERVER` set to the
public address and requests going through Traefik like any client's. That takes the
key off the machine the internet reaches, at the cost of a second machine; see
open decision 2.

A release builds `recall-worker` for Linux amd64 and arm64, static against
musl, into the same `checksums.txt` as `recall-server`. The crate name was
free on crates.io on 2026-09-23 and needs the one-time trusted-publishing
step in `releasing.md`.

### Evaluation reports

A run is an `evaluate` job, queued from `recall eval run` or the admin page,
and on a schedule when `RECALL_EVAL_INTERVAL_HOURS` is set (see open
decision 5). The worker reads each project and the global scope, decrypts,
and runs:

| Check | How |
|---|---|
| `secret` | Patterns for private keys, cloud and GitHub tokens, `recall-ek-` and `recall-rk-` strings, long hex secrets. No `claude` call. |
| `duplicate` | The same normalized paragraph in two files or two scopes. No `claude` call. |
| `dead_link` | A `MEMORY.md` link to a file that is not there. No `claude` call. |
| `wrong_scope` | A project file whose front matter says `type: user`, the case `recall promote` exists for. No `claude` call. |
| `stale` | Unchanged for `RECALL_EVAL_STALE_DAYS`, naming a path or command, for the owner to confirm. No `claude` call. |
| `contradiction` | One `claude -p` call per project, with the merge's cost-keeping flags, over the project's files and the global scope. |

The findings are the plain summary shown under the wire contract; everything
that quotes a note goes into `details`, sealed. The admin page lists runs and
findings (kind, file, lines) and says `recall eval show eval_7c2kq9` for the
rest. `recall eval apply f2` writes the suggested edit into the local file and
pushes it, the way `recall promote` does. Nothing changes unless the owner
runs that.

## Threat model, by pull request

Never hidden, at any stage: project keys, file paths, sizes (a sealed body is
the file plus 40 bytes, then base64), timings, which device wrote what and
when, key versions, when files conflict (merge jobs), and which kinds of
finding a file has. secsync states the same limit for its relay: its protocol
*"doesn't hide meta data from the server"*. The audit log makes that metadata
durable; that is its job.

| PR | Protects against | Does not protect against |
|---|---|---|
| 1 Audit | A server quietly removing or rewriting history someone holds a checkpoint for (until the client keeps them, the owner saving one by hand); a device's requests being forged, since they are signed | Rewrites before anyone saved a checkpoint; entries by the operator or the server, which are unsigned; what the server did with a signed push, which the log states but the signature does not bind; `recall-server admin` changes on the host, which append nothing; reading anything |
| 2 Worker | A compromise of the API process reaching the `claude` login, which moves to the worker's volume; slow merges holding a hook | Root on the host, which reaches both volumes. Merges still see plaintext |
| 3 Keys | Nothing new for notes, since none is encrypted yet. It sets up grants a compromised server cannot forge unnoticed | Key substitution for a device enrolling while an attacker holds the API, beyond what trust on first use and `ck_id` catch |
| 4 Sealed sync | A leaked database, backup or `sqlite-web` view exposing sealed files; the server moving a body between files or versions | Files still in plaintext; forged deletes, while old clients' unsealed tombstones are still honoured; a server serving an older version of a file (visible in the audit log, not prevented); a server withholding files |
| 5 Ciphertext only | The internet-facing process and its backups holding any note, once the migration is done and older snapshots have aged out; forged deletes; the API image carrying the CLI or Node | The worker: whoever takes it takes the key. Root on a shared host. Off-box copies made before the migration |
| 6 Evaluation | Secrets and contradictions sitting unnoticed in memory | It tells the API server which kinds of finding each file has |
| 7 Strict | Any server-side process reading what is written after the switch; old ciphertext being readable with the worker's key, once re-encrypted | The owner's own devices: a compromised laptop reads everything. Server-side merge and evaluation stop |

## Tests

Each row is a property a test pins, and a change to the code that must make
the test fail. A property that no mutation fails is not pinned.

**PR 1**

| Property | Mutation that must fail it |
|---|---|
| Every authenticated state change appends exactly one leaf, in the same transaction as the change, an authkey enrolment included; the store has no way to change a file, device or authkey without one | Drop the append from revoke; append after the commit and inject a failing write; a write method that takes no leaf |
| Unauthenticated routes, refused requests, the audit routes, and a revoke that changes nothing append nothing | Append in the rate limiter, or on a `401`; append on a second revoke |
| Roots match transparency-dev/merkle's vectors (`testonly/constants.go`; RFC 9162 keeps RFC 6962's tree hash), every consistency proof between sizes up to 64 verifies, and every one of its `testdata/consistency` probes gets its verdict | Swap the `0x00` and `0x01` prefixes; split at `n / 2`; an off-by-one in proof generation |
| A proof cut from a tree with any one leaf of the first `first` rewritten fails against the honest checkpoint, for every pair of sizes up to 64 | Check only the second root |
| A proof costs O(log² n) hashes and reads no table | Hash every leaf per request |
| Leaves are exported byte for byte | Re-serialize on export, pretty-printed or with keys sorted |
| `recall audit verify` fails on a changed byte, a missing leaf, two swapped leaves, or a saved checkpoint the log does not extend | Check only the latest root |
| It fails on a forged export that keeps every checkpoint: a device's leaf with no request, a signature replayed or moved onto another action or subject, a digest or keyid that is not the leaf's, a request signed before its device existed | Check only the tree; take keys from any approve leaf, the latest winning |
| A signed leaf verifies with nothing but the log, after its device has been swept | Take the key from `devices` |
| `UPDATE`, `DELETE`, `INSERT OR REPLACE` and an out-of-order insert on `audit_log` abort; opening refuses a log whose leaves no longer hash as stored | Drop the triggers; trust `leaf_hash` at open |

**PR 2**

| Property | Mutation that must fail it |
|---|---|
| With a worker enrolled, a stale push answers within a second while a fake `claude` sleeps 30 seconds, stores the incoming version and makes one job | Merge inline |
| With no worker, today's merge tests pass untouched | Queue regardless |
| A result under a superseded `lease_id` is a `409` and changes nothing | Accept any lease |
| A result applies only if the file still has the hash the job stored, and otherwise makes a follow-up | Write unconditionally: the push in between is lost |
| The same result posted twice changes nothing the second time | Apply it twice |
| A `sync` device cannot claim; a worker cannot push, approve or list jobs | Treat `worker` as `sync` or `admin` |
| End to end, server and worker in one test: a conflict's merged file arrives with the next pull | |
| `compose-check.py` fails a worker with a port, a label, the ingress network or the server's volume | |

**PR 3**

| Property | Mutation that must fail it |
|---|---|
| The wrap reproduces RFC 9180 Appendix A.2's base-mode vector | AES-128-GCM, or another `info` |
| A grantor refuses a recipient whose binding does not verify, and a device refuses a wrap whose grantor signature does not | Skip either check: the substitution test then wraps to the server's key |
| An approval naming a fingerprint is refused when the pending key differs, and a grantor does not vouch for an approval without one | Drop the comparison: a code swapped to another key is approved |
| After a full flow, the database file contains none of the content key, `U` or the recovery key, searched as raw bytes and in every encoding the code uses | Store any of them |
| The enrolment request never contains the unwrap half, and the server refuses a key with a dot without storing it | Send the whole key |
| The public key derived from the recovery key equals the stored one, and recovery opens every version | Change the `DeriveKeyPair` input |
| Rotation grants no revoked device, and revokes and replaces every enrolment key with an unwrap half | Include revoked devices |
| A client encrypts with its highest version even when the server says `current` is lower | Trust `current` |

**PR 4**

| Property | Mutation that must fail it |
|---|---|
| A sealed body moved to another path, project, version or tombstone state fails to open, and the client skips it with a warning | Empty associated data |
| 10,000 seals of one file give 10,000 distinct nonces | A fixed nonce, or a counter that restarts with each process |
| Round trips keep exact bytes: an empty file, no trailing newline, two trailing newlines | Trim or normalize anywhere |
| Today's pull code, run on `sync_response_sealed`, writes and deletes nothing for sealed rows | |
| An unchanged file is not pushed and makes no job | Remove the plaintext-hash check |
| A sealed write removes the plaintext row in the same transaction | Two transactions |
| After a sealed flow, the database holds none of the test's sentinel strings, the jobs table included | Keep a decrypted copy anywhere |

**PR 5**

| Property | Mutation that must fail it |
|---|---|
| Every request fixture below `min_client` gets the protocol refusal | Accept protocol 1 |
| A plaintext push in protocol 2 is a `400` | Accept it |
| A client ignores a tombstone that is not sealed, or whose seal does not open | Honour any tombstone: a forged delete removes a note |
| After migration, `memory_files` is empty, and the database file after `VACUUM` holds no sentinel string | Skip `secure_delete` or `VACUUM` |
| The previous release's server, started on a migrated database, serves no rows and no empty files (a `compat-check.sh` case) | Put ciphertext in `memory_files.content` |
| The client speaks protocol 1 to a PR 4 server and 2 to a PR 5 server | |
| The API image contains no `node` and no `claude` | |

**PR 6**

| Property | Mutation that must fail it |
|---|---|
| A report built from notes containing sentinel strings carries none of them outside `details` | Put an excerpt in a finding |
| The server refuses a finding with an unknown key | Accept free text |
| An evaluation changes no memory row | Apply a suggestion |
| The deterministic checks make no `claude` call, counted by a fake | |
| The secret check finds every planted token in a fixture set and nothing in a set of ordinary notes | |

**PR 7**

| Property | Mutation that must fail it |
|---|---|
| After `recall keys strict on`, the worker is revoked, the current version has no worker wrap, every row is sealed under it, and no job is made | Leave one row under the old version |
| The client's three-way merge (the `diffy` crate) agrees with `git merge-file` on a corpus of cases about which merge cleanly and what the clean results are, keeps a line deleted on one side, and writes a conflict copy plus `additionalContext` for overlapping edits | Merge two ways: the deleted line comes back |
| Approving a worker while strict is a `409` | |

## Open decisions

1. **Setting up cloud sessions from a phone.** Part 2 promises that creating
   an enrolment key on the admin page is one step, like pasting
   `RECALL_TOKEN`. From PR 4, a key without an unwrap half enrols a session
   that can read nothing, and only a device holding the content key can make
   the unwrap half. The admin page holds no key, and letting the worker wrap
   on the page's word would let a compromised API ask it to wrap to a `U` of
   its own choosing, since the page's script comes from the API.
   **Recommendation:** in Part 5 the unwrap half is made only by
   `recall devices enroll-key create`. The admin page makes one-part keys,
   says a session enrolled with one cannot read memory, and shows the command.
   Revisit if phone-only setup turns out to matter.
2. **Where the worker runs.** **Recommendation:** the same compose file and
   host. It matches how this deployment is run (docker compose, no hand
   configuration) and already takes the key out of the process that faces the
   internet. Move it to another host only if the VPS runs things the owner
   trusts less than Recall.
3. **Whether `MEMORY.md` is merged.** The design says it never is, being an
   index Recall regenerates. Today Recall regenerates only the lines that
   point into `global/` and `machine/`; the rest is Claude's own writing, and
   it is merged like any other file. Not merging it would drop an index line
   whenever two machines add one at once. **Recommendation:** keep merging it
   until Recall owns every line, and treat that as a change of its own.
4. **How the breaking release refuses old clients.** Protocol 2, as above, or
   protocol 1 kept with plaintext pushes refused. **Recommendation:**
   protocol 2, with the client speaking both from that release on and
   upgraded first. It is what Part 1's machinery is for, and it gives old
   clients a one-line upgrade message instead of files that quietly stop
   arriving. Ship Part 2's removal of `bearer` for sync in the same minor.
5. **Scheduled evaluation.** **Recommendation:** off by default. The
   deterministic checks cost nothing and could run daily, but the
   contradiction check spends the owner's Claude usage, so switching the
   schedule on is the owner's call.

## References

rfc-editor.org and ietf.org were not reachable while this was written, so the
RFC text was read from the drafts' sources on GitHub, and section numbers were
checked against them.

- RFC 9180, Hybrid Public Key Encryption: §6.1 single-shot APIs, §7
  identifiers, §7.1.3 `DeriveKeyPair`, §9.1 security properties, Appendix
  A.2 test vectors (source: `cfrg/draft-irtf-cfrg-hpke`)
- RFC 9162, Certificate Transparency 2.0: §2.1 Merkle hash trees, inclusion
  and consistency proofs (source: `google/certificate-transparency-rfcs`)
- draft-irtf-cfrg-xchacha-03, XChaCha20-Poly1305 (`bikeshedders/xchacha-rfc`)
- RFC 9110, HTTP Semantics, §13.1.1 `If-Match`
- RFC 9421, HTTP Message Signatures (the signature base an audit leaf keeps)
- libsodium documentation: XChaCha20-Poly1305 construction; Ed25519 to
  Curve25519
- ed25519-dalek 2.1.1: `VerifyingKey::to_montgomery`,
  `SigningKey::to_scalar_bytes`
- E. Thormarker, *On using the same key pair for Ed25519 and an X25519 based
  KEM*, IACR ePrint 2021/509
- age specification (C2SP `age.md`), and age's `agessh` package
- C2SP tlog-checkpoint
- Sigstore Rekor
- transparency-dev/merkle, `testonly/constants.go`
- secsync, "Meta Data"
- SQLite, `secure_delete` (`src/btree.c`)
- Crates: `hpke`, `x25519-dalek`, `chacha20poly1305`, `hkdf`, `diffy`
