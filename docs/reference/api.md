# HTTP API

Recall's server exposes six routes for memory and the deployment, and, since
0.4.1, ten under `/v1` for devices, and four for the merge queue. Two carry
memory files, two are for looking at the deployment, one says what the
server is and speaks, one is a browser page, the device routes enrol
machines and manage them, and the job routes are how a worker merges
conflicts away from the process that faces the internet.

This is a **frozen** surface: field names, field order, and the difference
between `null` and `""` are compatibility guarantees, not style. The shape was
set by the Node server this one replaced — the rows in production were written
against it — and it stayed frozen through that migration so a machine on the
old client and one on the new binary could talk to the same deployment. The
Rust definition of every shape below lives in
[`recall-wire`](../../crates/recall-wire/src/), which both halves share so they
cannot drift apart.

Everything on this page is asserted against a running server by
[`scripts/api-doc-check.sh`](../../scripts/api-doc-check.sh) — status codes,
error wording, field order, and the `null`-versus-`""` distinction. If a
handler changes and this document doesn't, that script fails. The one
exception is signed requests, which take a signing client rather than
`curl`: [`crates/recall-server/tests/devices.rs`](../../crates/recall-server/tests/devices.rs)
and, for the job routes a worker signs,
[`crates/recall-server/tests/jobs.rs`](../../crates/recall-server/tests/jobs.rs)
assert what this page says about them.

| Route | Auth | Purpose |
|---|:---:|---|
| [`POST /sync`](#post-sync) | yes | Send one memory file, or one delete |
| [`GET /sync`](#get-sync) | yes | Fetch every file held for one project |
| [`GET /health`](#get-health) | **no** | Liveness, and whether merge actually works |
| [`GET /.well-known/recall`](#get-well-knownrecall) | **no** | Which protocol and release this server is, and what it can do |
| [`GET /admin/stats`](#get-adminstats) | admin | What is stored, per project |
| [`GET /admin`](#get-admin) | **no** | An HTML page rendering the above |
| [`POST /v1/devices/enroll`](#post-v1devicesenroll) | **no** | Start enrolling a machine |
| [`POST /v1/devices/enroll/poll`](#post-v1devicesenrollpoll) | **no** | Ask whether it was approved |
| [`GET /v1/devices/me`](#get-v1devicesme) | device | Which device signed this request |
| [`GET /v1/devices/pending/{user_code}`](#get-v1devicespendinguser_code) | admin | What a code would approve, before approving it |
| [`POST /v1/devices/approve`](#post-v1devicesapprove-and-post-v1devicesdeny) | admin | Approve a machine by its code |
| [`POST /v1/devices/deny`](#post-v1devicesapprove-and-post-v1devicesdeny) | admin | Refuse one |
| [`GET /v1/devices`](#get-v1devices) | admin | Every device |
| [`POST /v1/devices/{id}/revoke`](#post-v1devicesidrevoke) | admin | Revoke one |
| [`POST /v1/enroll-keys`](#post-v1enroll-keys) | admin | Make an enrolment key, for cloud sessions |
| [`GET /v1/enroll-keys`](#get-v1enroll-keys-and-post-v1enroll-keysidrevoke) | admin | Every enrolment key |
| [`POST /v1/enroll-keys/{id}/revoke`](#get-v1enroll-keys-and-post-v1enroll-keysidrevoke) | admin | Stop one enrolling anything more |
| [`POST /v1/jobs/claim`](#post-v1jobsclaim) | worker | Wait for a merge job, and lease it |
| [`POST /v1/jobs/{id}/result`](#post-v1jobsidresult) | worker | Hand back a merge, or the error it met |
| [`GET /v1/jobs`](#get-v1jobs-and-post-v1jobsidretry) | admin | Jobs, newest first, without file content |
| [`POST /v1/jobs/{id}/retry`](#get-v1jobs-and-post-v1jobsidretry) | admin | Queue a failed job again |

"yes" is either credential below, except that a `worker` device may not
use them; "admin" is `RECALL_TOKEN` or a device approved with the `admin`
scope; "device" is any device's signature but a worker's; "worker" is a
device approved with the `worker` scope, and nothing else, not even the
token.
`/admin/stats` was "yes" until 0.4.1, and still is for the token: only a
`sync` device is refused there.

---

## Authentication

Two credentials are accepted on every authenticated route: the one bearer
token, and, from 0.4.1, a request signed by an enrolled device.

### The bearer token

```
Authorization: Bearer <RECALL_TOKEN>
```

The operator's secret, from `deploy/.env`. It works exactly as it always
has, on every route that needs auth, the device routes included: it is the
legacy path, kept while machines move to device keys (see
[`docs/design/handshake.md`](../design/handshake.md)). The comparison is
constant-time, so a wrong token takes the same time to reject whether the
first character was right or the first thirty were.

`GET /health` and `GET /admin` are deliberately unauthenticated so uptime
tooling can poll them without holding the token. `/health` reports no file
contents and no project keys.

### Device signatures

A machine enrolled as a device holds an Ed25519 key pair it generated, and
signs each request with it instead of sending a secret: [RFC 9421 HTTP
Message Signatures](https://www.rfc-editor.org/rfc/rfc9421) over an [RFC
9530 `Content-Digest`](https://www.rfc-editor.org/rfc/rfc9530) of the body.
A request copied out of a proxy log is useless once its minute is up, and
refused inside it.

```
Recall-Protocol: 1
Content-Digest: sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:
Signature-Input: sig1=("@method" "@authority" "@path" "@query" "content-digest" "recall-protocol");created=1790000000;keyid="dev_4k3jz…";nonce="Jq3v…";alg="ed25519"
Signature: sig1=:…:
```

| Part | What it is |
|---|---|
| `Content-Digest` | SHA-256 of the body, as a structured-field byte sequence. Always sent: a `GET` sends the digest of an empty body, the value above. |
| Covered components | All six, in any order: `@method`, `@authority` (host and port, lowercased, without `:443` or `:80`), `@path`, `@query` (`?` and the query as sent, or `?` alone), and the `Content-Digest` and `Recall-Protocol` headers. |
| `created` | UNIX seconds. Must be within **60 seconds** of the server's clock, either way. |
| `keyid` | The device id. |
| `nonce` | A random string, 1 to 128 characters, never reused. |
| `alg` | `"ed25519"`, or left out. |
| Label | `sig1`. It is the only label the server reads. |

The signature covers `@authority`, `@path` and `@query` rather than RFC
9421's `@target-uri` because TLS ends at the proxy in front of the server,
so the server never learns the scheme the client signed; it does see the
`Host` the client sent. The signature base is built as RFC 9421 §2.5
describes; the Rust implementation both halves use is
[`recall_wire::signature`](../../crates/recall-wire/src/signature.rs), and
its tests reproduce RFC 9421's own Ed25519 example (Appendix B.2.6) byte
for byte.

The server checks the headers first, in this order: the device exists and
is not revoked; `created` is inside the window; `created` is not earlier
than the moment the server started; `Content-Digest` has a sha-256 value;
the signature verifies with the device's key; the nonce has not been seen
from that device inside the window. The signature covers the
`Content-Digest` header rather than the body, so all of that is settled
before the body is read: the server reads a signed request's body only when
the device's own key signed it, and refuses one whose `Content-Length` is
over the limit before reading it. Then the body must match the digest, and
only then is the nonce recorded, so a forged or altered request cannot use
up a nonce the real device has yet to send. Last, the server notes the
device's `last_seen`, to within a minute.

The nonces are kept in memory, so a restart forgets them. That is why a
signature made before the server started is refused: the process before it
may already have accepted it. A request signed in the second or so a deploy
takes gets that answer, and is signed again. One device may have as many
nonces live at once as the rate limit lets one address send in two
minutes, the longest a nonce lives; a device over it is refused alone, and
every other device carries on.

A `sync` device may use every route but the admin ones and the job routes;
an `admin` device may use all of those but the job routes. A `worker`
device may use the two job routes it drains the queue with and nothing
else: it cannot pull or push memory, look itself up at `/v1/devices/me`, or
reach an admin route.

**The name belongs to the key.** A push a device signed is stored with that
device's name as its `source_env`, whatever the body's `source_env` says, so
one machine cannot write under another's name. A push with the bearer token
has no key to go by and is stored with the `source_env` it sent, as always.

A request carrying the right bearer token is the operator's, whatever else
it carries. A request with neither credential gets the same bare `401` as
always.

### Failures

| Failure | Status | Body |
|---|:---:|---|
| No credential at all, or a malformed `Authorization` | `401` | `{"error":"unauthorized"}` |
| Wrong token, and no signature | `401` | `{"error":"unauthorized"}` |
| Only one of `Signature-Input` and `Signature` | `401` | `{"error":"unauthorized: a signed request needs both signature-input and signature"}` |
| A `keyid` no device has | `401` | `{"error":"unauthorized: unknown device"}` |
| A revoked device | `401` | `{"error":"unauthorized: this device has been revoked"}` |
| `created` too far from the server's clock | `401` | `{"error":"unauthorized: signature created 75 seconds from the server's clock, more than the 60 allowed; check this machine's clock"}` |
| `created` earlier than the server's start | `401` | `{"error":"unauthorized: signature created before this server started; sign the request again"}` |
| The body is not what `Content-Digest` says | `401` | `{"error":"unauthorized: content-digest does not match the body"}` |
| The signature does not verify | `401` | `{"error":"unauthorized: the signature does not verify"}` |
| The same request a second time | `401` | `{"error":"unauthorized: this request was already received once"}` |
| A component missing from what is covered, or a header that does not parse | `401` | `{"error":"unauthorized: …"}`, naming what is wrong |
| A `sync` device on an admin route | `403` | `{"error":"forbidden: this needs RECALL_TOKEN or a device with the admin scope"}` |
| A `worker` device on `/sync` or `/v1/devices/me` | `403` | `{"error":"forbidden: a worker device may only claim jobs and post their results"}` |
| Anyone but a `worker` device on a job route a worker uses | `403` | `{"error":"forbidden: this needs a device with the worker scope"}` |
| A signed body declared or found over 5 MiB | `413` | `{"error":"request body too large"}` |
| One device signing more than its share of nonces | `429` | `{"error":"too many signed requests from this device, try again later"}` |
| Too many signed requests from every device together to remember their nonces | `503` | `{"error":"too many signed requests at once, try again later"}` |
| Too many requests | `429` | `{"error":"rate limit exceeded, try again later"}`, plus a `Retry-After` header |

### Rate limiting

Rate limiting is per client IP, defaulting to **60 requests per 60 seconds**
(`RECALL_RATE_LIMIT_MAX`, `RECALL_RATE_LIMIT_WINDOW_MS`), and runs *before*
the auth check — so a flood of invalid tokens is limited too, rather than
escaping the limiter by never reaching auth. The two enrolment routes, which
need no credential, are limited the same way and share the same bucket.

There is no batch endpoint: a client with many files sends one `POST /sync`
each, so a run longer than `RECALL_RATE_LIMIT_MAX` files in one window is cut
short. `Retry-After` carries the whole window in seconds — `60` by default —
rather than the time remaining in it. `recall backfill` is such a client, and
stops rather than retrying, so that the push and pull hooks running in the
same session keep their share of the same bucket.

The client's address is read from exactly one request header, named by
`RECALL_TRUSTED_IP_HEADER` — `cf-connecting-ip` behind a Cloudflare tunnel,
`x-real-ip` behind Traefik or nginx. One header, not a list: anything the
server is willing to read from an untrusted client is something that client
can choose, and choosing your own bucket defeats the limit. It is trustworthy
only because the container has no published port, so every request really
does arrive through that ingress.

## Protocol and client identity

Every request the `recall` client sends names the protocol it speaks and the
build that sent it:

```
Recall-Protocol: 1
User-Agent: recall/0.3.3 (macos-aarch64)
```

A request without `Recall-Protocol` is protocol 1, which is what every client
before the header spoke. On the authenticated routes, a protocol the server
does not speak is refused after the rate limit and before the token is
checked, because the fix is an upgrade, not a login:

| Failure | Status | Body |
|---|:---:|---|
| `Recall-Protocol` names a version the server does not speak | `400` | `{"error":"this server speaks Recall protocol 1, and the request asked for 2. Upgrade whichever side is older; GET /.well-known/recall says what this server supports"}` |

## Errors

Every non-2xx response, on every route, has the same shape:

```json
{ "error": "file_path must be relative, no traversal" }
```

Requests larger than **5 MiB** are rejected before they are parsed — `413`,
or `400` if the truncated body fails to parse first. Memory files are prose;
anything that size is a bug or an attack, not a note. The two enrolment
routes, which anyone may call, take **8 KiB**, and answer anything larger
with `413` and `{"error":"request body too large"}`.

---

## `POST /sync`

Stores one memory file, or tombstones one.

### Request

```json
{
  "project_key": "acme/app",
  "file_path": "topics/auth.md",
  "content": "# Auth\n\nTokens live in 1Password.\n",
  "source_env": "laptop",
  "deleted": false,
  "base_sha256": "3f9a…"
}
```

| Field | Type | Required | Notes |
|---|---|:---:|---|
| `project_key` | string | yes | How two machines agree they mean the same project. See [Project identity](../../ARCHITECTURE.md#project-identity). |
| `file_path` | string | yes | Relative to the memory directory, forward slashes. Validated — see below. |
| `content` | string | for a write | The file's **exact** bytes, trailing newlines included. |
| `source_env` | string | no | A display label for the machine. Nothing keys off it. On a push a device signed, the server records the device's name here instead, whatever the body says: see below. |
| `deleted` | bool | no | `true` makes this a delete; `content` is then omitted. |
| `base_sha256` | string | no | SHA-256, lowercase hex, of the content this edit started from — what the client last pulled or pushed for this file. Decides whether the push is merged; see below. |

**`content` and `deleted` are the subtle pair.** `content: ""` is a legitimate
empty file. A delete omits `content` entirely. A push that is neither a delete
nor carries a `content` field is malformed and gets a `400` — which is exactly
the bug that made an empty memory file unsyncable in two earlier
implementations, both of which "helpfully" omitted the field when the string
was empty.

`file_path` is rejected if it is absolute (including a `C:` drive prefix) or
contains a `..` **path segment**. Segment-wise, not by substring: `..config.md`
is a perfectly ordinary filename and is accepted. The client applies the same
rule before sending, so a bad path never leaves the machine.

### Response

```json
{
  "ok": true,
  "project_key": "acme/app",
  "file_path": "topics/auth.md",
  "deleted": false,
  "merged": true,
  "updated_at": "2026-09-03T21:49:55.191Z"
}
```

On a server with a [worker](#jobs) enrolled, a push that needs merging is
stored as sent and answered at once, with the job that will merge it:

```json
{
  "ok": true,
  "project_key": "acme/app",
  "file_path": "topics/auth.md",
  "deleted": false,
  "merged": false,
  "updated_at": "2026-10-02T09:14:02.991Z",
  "merge_job": "job_3m5k7q2x9w4r8t6y"
}
```

`merge_job` is there only when a job was queued, and is omitted otherwise,
so a server without a worker answers exactly as before the queue existed.
The merged file arrives with a later pull.

`merged: true` means the stored content is the result of a semantic merge
rather than the bytes you sent. That happens only when there was genuinely
something to reconcile — an existing, non-tombstoned row whose content differs
from the push, **and** which is not the version the push names as its base. A
new file, a revived tombstone, a re-push of unchanged content, and the next
edit of the stored version all skip straight to a write.

**The base is what separates the next edit from a concurrent one.** If
`base_sha256` matches the stored content, nothing was written in between, and
the push replaces it outright. If it does not match, another machine wrote
since this client last saw the file, and that is what the merge is for. A push
with no `base_sha256` is merged whenever it differs from what is stored, which
is how every push was handled before 0.3.1 — so older clients behave as they
always did.

That distinction is not an optimisation. The merge keeps every distinct fact
from both versions, which means it cannot express a deletion: a line removed
on purpose is a fact from the stored side, and the merge puts it back. The same
happened to a resolved `CONFLICT` marker, which came back on every push that
tried to remove it.

**Where the merge runs.** With no worker enrolled, the server runs it
inline with the `claude` CLI before answering, as it always has. With an
unrevoked `worker` device enrolled, it never does: the push is stored,
last-write-wins, a `merge` job is queued holding both versions, and the
answer carries `merged: false` and the job's id, which is exactly what a
merge that degraded to last-write-wins looks like. No push waits on a
worker, whether or not it is running. `RECALL_MERGE_ENABLED=false` turns
both off. A full queue (1,000 jobs waiting or held) stores the push without
a job, and says so in `merge.last_merge_error`.

**A failed merge still returns `200`.** Every failure mode — the `claude` CLI
missing, not logged in, timing out, returning malformed output, or returning
an empty result — degrades to last-write-wins rather than rejecting the sync,
because a broken merge step must never be able to take basic syncing down with
it. `merged` is then `false`, and the reason appears in
[`GET /health`](#get-health)'s `merge.last_merge_error`. That is the *only*
way a degraded merge is visible, which is why the field exists.

### Status codes

| Code | When |
|:---:|---|
| `200` | Stored. Check `merged` to see whether a merge happened, and `merge_job` for one queued. |
| `400` | Bad JSON, a missing required field, or a rejected `file_path`. |
| `401` | Bad or missing credentials; see [Authentication](#authentication). |
| `413` | Body over 5 MiB (`400` if it fails to parse first). |
| `429` | Rate limited. |
| `500` | The database write failed. |

### Example

```sh
curl -sS -X POST "$RECALL_URL/sync" \
  -H "Authorization: Bearer $RECALL_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"MEMORY.md","content":"# Memory\n","source_env":"laptop"}'
```

Deleting is the same call with `deleted` set and no `content`:

```sh
curl -sS -X POST "$RECALL_URL/sync" \
  -H "Authorization: Bearer $RECALL_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"stale.md","deleted":true,"source_env":"laptop"}'
```

---

## `GET /sync`

Returns every file held for one project, **tombstones included** — a puller
needs to see those to remove its local copies.

### Request

```
GET /sync?project_key=acme/app
```

`project_key` is required; without it the response is `400`.

### Response

```json
{
  "project_key": "acme/app",
  "files": [
    {
      "file_path": "MEMORY.md",
      "content": "# Memory\n",
      "source_env": "laptop",
      "updated_at": "2026-09-03T21:49:55.191Z",
      "deleted": false
    },
    {
      "file_path": "stale.md",
      "content": null,
      "source_env": "cloud",
      "updated_at": "2026-09-03T21:50:02.004Z",
      "deleted": true
    }
  ]
}
```

**A tombstone reports `content: null`, never `""`.** The server keeps the last
known content in the database — a delete sets a flag, it does not remove the
row — but withholds it here so a pull cannot resurrect a deleted file. `""`
would be indistinguishable from a genuinely empty file, so the two cases are
kept apart in the type, not by convention.

An unknown `project_key` is not an error: it returns an empty `files` array.
That is what a machine syncing a project for the first time sees.

### Status codes

| Code | When |
|:---:|---|
| `200` | Including for a project the server has never heard of. |
| `400` | No `project_key`. |
| `401` | Bad or missing credentials; see [Authentication](#authentication). |
| `429` | Rate limited. |

### Example

```sh
curl -sS -G "$RECALL_URL/sync" \
  -H "Authorization: Bearer $RECALL_TOKEN" \
  --data-urlencode "project_key=acme/app"
```

---

## `GET /.well-known/recall`

Unauthenticated, at the path RFC 8615 sets aside for a service describing
itself. A client asks it before anything else: whether it can talk to this
server at all, and what the server can do.

```json
{
  "protocol": { "current": 1, "supported": [1] },
  "server": {
    "version": "0.3.3",
    "build": {
      "channel": "release",
      "revision": "e100cfdd88e8a0e6659b357ad88979b381f1e548",
      "created": "2026-09-23T01:10:00Z"
    }
  },
  "min_client": "0.1.0",
  "auth": { "methods": ["bearer", "device-sig-v1"] },
  "capabilities": {
    "devices": {
      "enroll_path": "/v1/devices/enroll",
      "code_ttl_seconds": 900,
      "poll_interval_seconds": 5,
      "signature_window_seconds": 60
    },
    "limits": {
      "max_body_bytes": 5242880,
      "rate_limit": { "max": 60, "window_seconds": 60 }
    },
    "merge_base": {},
    "merge_queue": {},
    "scopes": { "kinds": ["project", "global", "machine"] }
  }
}
```

| Field | Meaning |
|---|---|
| `protocol.current` | The protocol this server's own client speaks. |
| `protocol.supported` | Every protocol it accepts requests in. |
| `server.version` | The server's identity, as SemVer. A release build reports its release. Any other build reports a `-dev` pre-release of the next patch with the commit as build metadata, e.g. `0.3.3-dev+ge100cfd`, which SemVer orders after `0.3.2` and before `0.3.3`. |
| `server.build.channel` | `release` for a build the release workflow made, `dev` for anything else. |
| `server.build.revision` | The commit it was built from. Provenance only: nothing is decided on it. Omitted when unknown. |
| `server.build.created` | When it was built, when the build recorded it. Omitted otherwise. |
| `min_client` | The oldest client version the server accepts. Every client released so far is accepted. |
| `auth.methods` | How a client may authenticate. `bearer` is the `RECALL_TOKEN` above; `device-sig-v1`, from 0.4.1, is a [device signature](#device-signatures). New methods are appended; none is removed within a protocol version. |
| `capabilities` | What the server can do, by name. Each is an object, so it can carry parameters later. |
| `capabilities.devices` | From 0.4.1: the server enrols devices and accepts their signatures. |
| `capabilities.devices.enroll_path` | Where enrolment starts, [`/v1/devices/enroll`](#post-v1devicesenroll). |
| `capabilities.devices.code_ttl_seconds` | How long a user code can be approved: 900. |
| `capabilities.devices.poll_interval_seconds` | How long to wait between polls: 5. |
| `capabilities.devices.signature_window_seconds` | How far a signature's `created` may be from the server's clock, either way: 60. |
| `capabilities.limits` | `max_body_bytes`, and `rate_limit`'s `max` requests per `window_seconds`. |
| `capabilities.merge_base` | The server reads `base_sha256` on a push. |
| `capabilities.merge_queue` | The server can queue a stale push for a [worker](#jobs), and has the job routes. Listed whether or not a worker is enrolled now. |
| `capabilities.scopes` | The memory scopes a client may sync, each under an ordinary `project_key`. |

The rules that keep this readable by clients that do not exist yet:

- **Unknown keys are ignored**, at any depth. A newer server may add fields
  and capabilities; an older client reads past them.
- **An absent capability is unsupported.**
- **`protocol` changes only for a breaking change**, which is a breaking
  release under [`releasing.md`](releasing.md). Capabilities only ever grow,
  and nothing is removed within a protocol version.

A server older than this document answers `404` here. It speaks protocol 1.

## `GET /health`

Unauthenticated. Safe to point uptime monitoring at.

```json
{
  "status": "ok",
  "git_commit": "a1b2c3d",
  "started_at": "2026-09-03T09:00:00.000Z",
  "last_sync_at": "2026-09-03T21:49:55.191Z",
  "last_backup_at": "2026-09-03T09:00:01.412Z",
  "last_offbox_at": "2026-09-03T04:17:02.000Z",
  "merge": {
    "enabled": true,
    "claude_cli": {
      "checked_at": "2026-09-03T21:30:00.000Z",
      "available": true,
      "logged_in": true,
      "error": ""
    },
    "last_merge_at": "2026-09-03T21:49:55.101Z",
    "last_merge_error": null,
    "worker": { "last_claim_at": "2026-10-02T09:14:03.118Z", "agent": "recall-worker/0.4.2 (linux-x86_64)" },
    "queue": { "queued": 0, "leased": 1, "failed": 0, "oldest_queued_at": null }
  }
}
```

The `merge` object is the reason this endpoint is worth reading rather than
just pinging. Because every merge failure degrades silently to
last-write-wins, a deployment where the `claude` CLI is missing or logged out
looks perfectly healthy from the outside — sync keeps working, conflicts just
stop being merged. These are the fields that make that state visible:

| Field | Watch for |
|---|---|
| `merge.enabled` | `false` means merging is switched off entirely (`RECALL_MERGE_ENABLED`). |
| `merge.claude_cli.available` | `false` means the binary isn't on the server's `PATH`. |
| `merge.claude_cli.logged_in` | `false` is the common one: run `claude setup-token` on the host. |
| `merge.last_merge_error` | Non-null means a real merge was attempted and failed. With a worker, it is set when a job runs out of attempts or its result could not be applied, or when the queue is full. |
| `merge.worker` | Present while a `worker` device is enrolled. `last_claim_at` is when it last asked for a job since the server started, `null` before it has; `agent` is its `User-Agent`. A worker that has stopped shows here as a `last_claim_at` that no longer moves. |
| `merge.queue` | Present while a worker is enrolled: jobs `queued`, `leased` and `failed`, and `oldest_queued_at`, `null` when nothing waits. An old `oldest_queued_at` means merges are waiting on a worker that is not taking them. |

With a worker enrolled, `merge.claude_cli` is the worker's own check of its
CLI, sent with each claim, rather than the server's: the merge runs there.
Until the worker's first claim it reads as unknown (`available` and
`logged_in` `null`).

`last_backup_at` and `last_offbox_at` answer different questions and only one
of them survives the disk. The first is the server's own snapshot, written by
`VACUUM INTO` beside the database it protects. The second is the stamp
`deploy/backup-offbox.sh` leaves after a copy has reached a remote *and* been
verified against it — so a stale `last_offbox_at` beside a fresh
`last_backup_at` is precisely the state where losing the machine loses
everything. It is read from disk per request rather than cached, because the
process that writes it is a cron job in another container.

Fields that would be empty are **omitted rather than sent empty**:
`last_sync_at` before anything has synced, `last_backup_at` when backups are
off, `last_offbox_at` when no off-box copy has ever succeeded — which is also
what "no off-box backup is configured" looks like, and neither is an error —
`merge.last_merge_at` before a merge has succeeded. `last_merge_error` is
the exception — it is `null` when there is nothing to report, because the
difference between "no failure" and "not checked" matters there.

`git_commit` is baked in at build time, so it is also how you confirm a deploy
actually landed.

```sh
curl -sS "$RECALL_URL/health" | jq '.merge.claude_cli'
```

`recall status` reads exactly these fields — it is usually the friendlier way
to ask.

---

## `GET /admin/stats`

Admin: `RECALL_TOKEN`, or a device with the `admin` scope. What the owner is
storing, per project.

```json
{
  "projects": [
    {
      "project_key": "acme/app",
      "file_count": 4,
      "deleted_count": 1,
      "sources": ["laptop", "cloud"],
      "last_updated_at": "2026-09-03T21:49:55.191Z"
    }
  ],
  "totals": { "project_count": 1, "file_count": 4, "deleted_count": 1 },
  "git_commit": "a1b2c3d",
  "last_backup_at": "2026-09-03T09:00:01.412Z"
}
```

`last_backup_at` is omitted when backups are off.

Read-only, and only `GET` is routed — a `POST` here is a `404`. There is no
admin *write* surface for memory at all, deliberately: nothing on this route
can delete a project or edit a note, so a leaked token cannot be used to
quietly destroy history through it. The device routes below do change
state, but only about devices; none of them reads or writes memory.

Until 0.4.1 there was one credential, and it could read this. It still
can, but a device needs the `admin` scope, since the list of every project
is more than a machine that syncs one needs. A device checks that the
server knows it with [`GET /v1/devices/me`](#get-v1devicesme).

## `GET /admin`

The same numbers as an HTML page, for a browser. Unauthenticated because it
ships no data of its own — it fetches `/admin/stats` from the browser, which
means the person looking at it still needs the token. Served under a strict
CSP that allows no external anything.

---

## Devices

From 0.4.1 a machine can be enrolled as a **device**: it generates an
Ed25519 key pair, keeps the private half, and signs every request (see
[Device signatures](#device-signatures)). Enrolment follows [RFC 8628, the
OAuth device authorization
grant](https://www.rfc-editor.org/rfc/rfc8628): the machine asks for a
short code, the owner approves the code from somewhere already trusted, and
the machine polls until it is approved. A cloud session, which cannot wait
for anyone, enrols with an [enrolment key](#post-v1enroll-keys) instead
and is approved at once.

A device has a **scope**: `sync` may use every route except the admin ones;
`admin` may also approve, list and revoke devices and enrolment keys;
`worker` may claim merge jobs and post their results and nothing else (see
[Jobs](#jobs)). The
operator's `RECALL_TOKEN` can do everything an `admin` device can, which is
how the first device is approved.

Every timestamp below has the [usual shape](#timestamps). A field that has
no value yet is `null`, never omitted.

## `POST /v1/devices/enroll`

Unauthenticated, and rate limited like every other route.

### Request

```json
{
  "name": "laptop",
  "public_key": "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs",
  "agent": "recall/0.4.1 (macos-aarch64)"
}
```

| Field | Type | Required | Notes |
|---|---|:---:|---|
| `name` | string | yes | What the owner sees the machine as. At most 64 characters, and none that hide what the name says: no control or format characters (Unicode categories Cc and Cf, which include the bidirectional overrides and the zero-width characters), no line or paragraph separators, no other invisible ones. No two unrevoked devices share a name, compared without case; a revoked device's name is free again. Ignored with `enroll_key`: see below. |
| `public_key` | string | yes | The Ed25519 public key: the raw 32 bytes, base64url, no padding. A key of small order is refused. |
| `agent` | string | no | The client's `User-Agent`, shown in the device list. At most 256 characters, under the same rules as `name`. |
| `enroll_key` | string | no | An [enrolment key](#post-v1enroll-keys). With a valid one the device is approved at once. |

### Response: waiting for approval

Without `enroll_key`, RFC 8628 §3.2's device authorization response, the
enrolment id standing where the RFC has its `device_code`:

```json
{
  "enrollment_id": "enr_kf4cyx5cramwoe34o5ku3iyrhy",
  "user_code": "TXLV-FNTC",
  "expires_in": 900,
  "interval": 5
}
```

| Field | Meaning |
|---|---|
| `enrollment_id` | What the machine polls with. A secret: keep it on the machine. |
| `user_code` | What the owner approves: eight characters of RFC 8628 §6.1's consonant alphabet, `BCDFGHJKLMNPQRSTVWXZ`, with a hyphen. When it is typed back, case, the hyphen and spaces do not matter. |
| `expires_in` | Seconds until the code can no longer be approved: fifteen minutes. |
| `interval` | Seconds to wait between polls. |

The machine should show the code, and its key's fingerprint, so the owner
can check the device list shows the same one. The fingerprint is `SHA256:`
and the unpadded base64 of the SHA-256 of the raw 32-byte key.

### Response: approved with an enrolment key

```json
{ "device_id": "dev_7e3jth4xgnksqm7hyx5z5j4quq", "name": "cloud-7e3jth4x", "scope": "sync", "ephemeral": true }
```

Always `sync` scope. Nobody looks at a device enrolled this way before it
exists, so it does not choose its name: the server names it after the key's
tag and the start of its id (`device-…` for a key with no tag), and the
`name` in the request is ignored. `ephemeral` is the key's: an ephemeral
device is removed once it has made no signed request for
`RECALL_EPHEMERAL_DEVICE_TTL_HOURS` hours, 24 by default.

### Status codes

| Code | When |
|:---:|---|
| `200` | Either response above. |
| `400` | Bad JSON, or a field that breaks the rules above, with the rule as the error. |
| `401` | `{"error":"unauthorized: this enrolment key is not one this server issued"}`, `…has expired` or `…has been revoked`. |
| `403` | The enrolment key already has as many unrevoked devices as its `max_devices`. |
| `409` | An unrevoked device already has the name: `{"error":"a device named laptop already exists; revoke it first, or enrol with another name"}`. |
| `413` | A body over 8 KiB. |
| `429` | Rate limited; or five enrolments from this address are already waiting: `{"error":"too many enrolments from this address are waiting for approval; approve or deny them, or let them expire"}`. The address is the one the rate limiter uses. |
| `503` | A thousand enrolments are already waiting for approval: `{"error":"too many enrolments are waiting for approval, try again later"}`. |

## `POST /v1/devices/enroll/poll`

Unauthenticated; the `enrollment_id` is the secret.

```json
{ "enrollment_id": "enr_kf4cyx5cramwoe34o5ku3iyrhy" }
```

Once approved, a `200`:

```json
{ "device_id": "dev_pxu4i2r2zc27mil6ufyw5rxtke", "scope": "sync" }
```

From then on the machine signs its requests with `keyid` set to
`device_id`. The same answer comes back to every poll until the enrolment is
swept away, an hour after its code expired, so a machine that lost the
first one can ask again.

Until then, a `400` whose `error` is one of RFC 8628 §3.5's codes, in the
shape every Recall error has:

| `error` | Meaning | What the machine does |
|---|---|---|
| `authorization_pending` | Nobody has approved it yet. | Wait `interval` seconds and poll again. |
| `slow_down` | Not approved, and polled sooner than `interval` after the last poll. | Add five seconds to the interval, for this and every later poll. |
| `expired_token` | Fifteen minutes passed with no approval. | Stop; start again with a new enrolment. |
| `access_denied` | The owner denied it, or approved it and then revoked the device. | Stop. |
| `invalid_grant` | No enrolment has that id (RFC 6749 §5.2), or it expired over an hour ago. | Stop. |

An approval is answered even if it arrives after the code expired, as long
as it was approved in time. A body with no `enrollment_id` is a `400`
saying so. Every successful answer from the enrolment routes, and every
poll answer, is sent with `Cache-Control: no-store`, as RFC 6749 §5.1 asks
of token responses.

## `GET /v1/devices/pending/{user_code}`

Admin. What approving the code would approve, so the owner can compare the
name and fingerprint with what the machine shows before deciding: RFC 8628
§5.4's defence against being talked into approving someone else's machine.
The code is read the way approving reads it: case, the hyphen and spaces do
not matter.

```json
{
  "user_code": "TXLV-FNTC",
  "name": "laptop",
  "agent": "recall/0.4.1 (linux-x86_64)",
  "fingerprint": "SHA256:sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI",
  "expires_in": 899
}
```

`user_code` comes back normalized; `expires_in` is the seconds left to
approve it. It answers only for a code still waiting: unexpired and
undecided. Looking decides nothing: the machine keeps polling
`authorization_pending`. The answers for a code that cannot be approved are
the ones approving it would give, `400`, `404`, `409` and `410`, listed
under approve below, and every answer is sent with `Cache-Control:
no-store`.

## `GET /v1/devices/me`

Any device's signature. The device that signed the request, as the server
knows it: how a machine checks it is enrolled and not revoked, without the
`admin` scope the device list needs.

```json
{ "device_id": "dev_pxu4i2r2zc27mil6ufyw5rxtke", "name": "laptop", "scope": "sync", "ephemeral": false }
```

A request with `RECALL_TOKEN` is from no device: `404` with
`{"error":"not a device: this request was authenticated with RECALL_TOKEN"}`.

## `POST /v1/devices/approve` and `POST /v1/devices/deny`

Admin. The owner, holding `RECALL_TOKEN` or an admin device, approves or
refuses the code a machine shows.

```json
{ "user_code": "TXLV-FNTC", "scope": "sync", "fingerprint": "SHA256:sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI" }
```

`scope` is `sync` when left out, and may be `admin` or `worker`. `fingerprint` is
optional: when given, it is the fingerprint the approver was shown, by the
machine or by the lookup above, and the code is approved only if its key
has exactly that fingerprint. That binds the approval to what the approver
actually saw. Deny takes only `user_code`.

Approve answers with the new [device](#get-v1devices); deny with what was
refused:

```json
{ "user_code": "LFLQ-TTHP", "name": "phone", "denied": true }
```

| Code | When |
|:---:|---|
| `200` | Approved, or denied. |
| `400` | Bad JSON, a `scope` other than `sync`, `admin` or `worker` (`{"error":"scope must be sync, admin or worker"}`), or a `user_code` that is not eight letters of the alphabet: `{"error":"user_code must be the 8 letters the device shows, such as WDJB-MJHT"}`. |
| `401`, `403` | See [Authentication](#authentication). |
| `404` | `{"error":"no enrolment is waiting with that code"}` |
| `409` | `{"error":"that code was already approved or denied"}`; or, with `fingerprint`, `{"error":"that code's key does not have the fingerprint given; nothing was approved"}`; or an unrevoked device already has the name the machine asked for. |
| `410` | `{"error":"that code has expired; start the enrolment again"}` |

## `GET /v1/devices`

Admin. Every device, newest first, revoked ones included.

```json
{
  "devices": [
    {
      "id": "dev_pxu4i2r2zc27mil6ufyw5rxtke",
      "name": "laptop",
      "scope": "sync",
      "ephemeral": false,
      "agent": "recall/0.4.1 (macos-aarch64)",
      "fingerprint": "SHA256:sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI",
      "public_key": "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs",
      "enroll_key_id": null,
      "created_at": "2026-09-23T12:04:54.311Z",
      "last_seen": "2026-09-23T12:31:02.118Z",
      "revoked_at": null
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `id` | `dev_` and 26 lowercase base32 characters, 128 random bits. The `keyid` it signs with. |
| `name` | What it enrolled as. |
| `scope` | `sync` or `admin`. |
| `ephemeral` | Whether it is removed once idle. |
| `agent` | The `agent` it enrolled with. |
| `fingerprint` | Its key's fingerprint, as the machine showed it. |
| `public_key` | Its key, base64url. |
| `enroll_key_id` | The enrolment key it came in with, or `null` when a person approved it. |
| `created_at` | When it was approved. |
| `last_seen` | Its latest signed request, to within a minute, or `null` before its first. |
| `revoked_at` | When it was revoked, or `null`. |

## `POST /v1/devices/{id}/revoke`

Admin. The device's requests are refused from now on. It stays in the list,
with `revoked_at` set, and the answer is the device as it now stands.
Revoking one already revoked keeps the first time. `404` with `{"error":"no
device has that id"}` for an id that is not there.

## `POST /v1/enroll-keys`

Admin. Makes an **enrolment key**: a credential for machines that cannot
wait for someone to approve a code, such as cloud sessions. It enrols
devices with `sync` scope and nothing else: it cannot read or write memory
itself.

```json
{ "tag": "cloud", "expires_in_days": 90, "ephemeral": true, "max_devices": 10 }
```

| Field | Type | Required | Notes |
|---|---|:---:|---|
| `tag` | string | no | A label, and the start of the name of every device the key enrols. At most 32 characters, under the rules for a device name. |
| `expires_in_days` | integer | yes | 1 to 365. There is no key that never expires. |
| `ephemeral` | bool | no | Whether the devices it enrols are ephemeral. `true` when left out: a key is for machines that come and go. |
| `max_devices` | integer | no | The most unrevoked devices it may have enrolled at once, 1 or more; no limit when left out. An ephemeral device swept for being idle frees its place. |

```json
{
  "id": "ek_ecfq6bc4luadka2i",
  "key": "recall-ek-tqvi2pktd7u6rpc7wwrq57cc5gjupt7mdqdupbcx3wijsjdmxmma",
  "tag": "cloud",
  "ephemeral": true,
  "max_devices": 10,
  "created_at": "2026-09-23T12:47:29.936Z",
  "expires_at": "2026-12-22T12:47:29.936Z"
}
```

`key` is shown this once: the server keeps only its SHA-256. It starts with
`recall-ek-` so one found in a log says what it is, followed by 256 random
bits in lowercase base32. The reply is sent with `Cache-Control: no-store`.

## `GET /v1/enroll-keys` and `POST /v1/enroll-keys/{id}/revoke`

Admin. The list is every enrolment key, newest first, expired and revoked
ones included, each as above without `key` and with `revoked_at` (`null`
until revoked):

```json
{ "enroll_keys": [ { "id": "ek_ecfq6bc4luadka2i", "tag": "cloud", "ephemeral": true, "max_devices": 10, "created_at": "2026-09-23T12:47:29.936Z", "expires_at": "2026-12-22T12:47:29.936Z", "revoked_at": null } ] }
```

Revoking one stops it enrolling anything more and answers with the key as it
now stands. Its body may be empty; with `{"revoke_devices": true}` every
device the key enrolled is revoked too, which is what to do when a key has
leaked. Without it they keep working. `404` with `{"error":"no enrolment
key has that id"}` for an id that is not there.

---

## Jobs

From 0.4.2 a server can hand its semantic merges to **`recall-worker`**, a
separate process with no inbound port, instead of running them in the
process that faces the internet. The worker is an enrolled device approved
with the `worker` scope; it long-polls for jobs, merges each with the
`claude` CLI it runs next to, and posts the result back. See
[`deploy/README.md`](../../deploy/README.md#the-merge-worker) for running
one.

A **job** holds one conflict: the version a push displaced (`stored`) and
the version it stored (`incoming`). Its `state` is `queued`, `leased` (a
worker holds it), `done` or `failed`. A claim **leases** a job for the
`lease_seconds` it asks for, under a `lease_id`; a result counts only under
the job's current, unexpired `lease_id`. A lease that runs out puts the job
back in the queue, so a job a crashed worker took is released, and the old
holder's late result gets `409`: a fencing token, so a worker that stalled
cannot overwrite the one that took over.

**Applying a merge is a compare-and-swap.** The result is written only if
the file still has the hash of `incoming`, and is then attributed to the
worker's device name. If another push landed while the job ran, the push
stands, and the merged content becomes the `stored` side of a follow-up job
against the newer version (`applied: false`, `follow_up`). After three
follow-ups the chase stops: the newest push stands, and the unapplied
result is kept in the failed job, where a retry picks it up again. A file
deleted while its job ran stays deleted.

**Retries.** A result carrying an `error`, or a lease that runs out, puts
the job back in the queue after 1, then 5, then 30 minutes; after its
fourth attempt it is `failed`, and `merge.last_merge_error` in
[`GET /health`](#get-health) says why. Nothing waits on any of it: pushes
keep landing as last-write-wins while jobs wait, and a worker that comes
back drains the queue through the same compare-and-swap. Finished jobs are
removed after 30 days; failed ones stay until retried.

## `POST /v1/jobs/claim`

Worker only. Waits up to `wait_seconds` for a queued job of one of `kinds`
and leases it, waking as soon as a push queues one.

```json
{
  "kinds": ["merge"],
  "wait_seconds": 25,
  "lease_seconds": 120,
  "claude_cli": { "checked_at": "2026-10-02T09:13:40.002Z", "available": true, "logged_in": true, "error": "" }
}
```

| Field | Notes |
|---|---|
| `kinds` | Required. `merge` is the only kind so far. Empty is allowed: the claim then waits and answers with no job, which is how a worker whose CLI cannot merge stays visible without taking jobs it would fail. |
| `wait_seconds` | 0 to 30; 0 when left out. |
| `lease_seconds` | 30 to 600; 120 when left out. |
| `claude_cli` | Optional. The worker's own check of its CLI, which `/health` then reports. |

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
      "stored": { "sha256": "9f2c…", "content": "# Auth\n- tokens live in 1Password\n", "source_env": "laptop", "updated_at": "2026-10-02T09:10:11.020Z" },
      "incoming": { "sha256": "4b1f…", "content": "# Auth\n- rotate them monthly\n", "source_env": "cloud", "updated_at": "2026-10-02T09:14:02.991Z" }
    }
  }
}
```

`{"job": null}` when nothing arrived within `wait_seconds`. `attempt`
counts from 1. `sha256` is the lowercase hex SHA-256 of `content`, as
`base_sha256` is.

| Code | When |
|:---:|---|
| `200` | A job, or `null`. |
| `400` | Bad JSON, `{"error":"wait_seconds must be 0 to 30"}` or `{"error":"lease_seconds must be 30 to 600"}`. |
| `401`, `403` | See [Authentication](#authentication). The operator's token is a `403` here. |

## `POST /v1/jobs/{id}/result`

Worker only. Hands back a merge, or the error the worker met, under the
lease the job was claimed with. Exactly one of `merge` and `error`:

```json
{ "lease_id": "lse_q8w2e4r6t8y0u2i4o6p8a0s2d4", "merge": { "content": "# Auth\n- tokens live in 1Password\n- rotate them monthly\n" } }
```

```json
{ "lease_id": "lse_q8w2e4r6t8y0u2i4o6p8a0s2d4", "error": "claude merge timed out after 45s" }
```

```json
{ "id": "job_3m5k7q2x9w4r8t6y", "state": "done", "applied": true, "follow_up": null }
```

`applied` is whether the merged content was written to the file. An error
answers with the job back in `queued`, or `failed` if that was its last
attempt. **Posting the same result again is safe**: under the lease that
settled the job, it changes nothing and gets the same answer, so a worker
whose first answer was lost can simply send it again.

| Code | When |
|:---:|---|
| `200` | Recorded. `applied: false` with a `follow_up` job id when the file changed while the job ran. |
| `400` | Bad JSON; `{"error":"a result carries exactly one of merge and error"}`. |
| `401`, `403` | See [Authentication](#authentication). |
| `404` | `{"error":"no job has that id"}` |
| `409` | `{"error":"this lease has ended; the job was handed out again"}`: the lease is not the job's current one, or has run out. Nothing was changed. |

## `GET /v1/jobs` and `POST /v1/jobs/{id}/retry`

Admin. The listing is newest first, at most 200, optionally one `?state=`,
and never carries file content:

```json
{
  "jobs": [
    {
      "id": "job_3m5k7q2x9w4r8t6y",
      "kind": "merge",
      "state": "done",
      "project_key": "acme/app",
      "file_path": "topics/auth.md",
      "attempt": 1,
      "created_at": "2026-10-02T09:14:02.992Z",
      "updated_at": "2026-10-02T09:14:07.310Z",
      "error": null,
      "follow_up": null
    }
  ]
}
```

`error` is the last error the job met, or why its result was not applied;
`follow_up` the job its result was queued into. A `state` other than the
four is `400`: `{"error":"state must be queued, leased, done or failed"}`.

Retry queues a failed job again with its attempts counted afresh, and
answers with the job as listed. A job that failed because the file kept
changing is queued as a merge of its kept result with the file as it is
now. `404` with `{"error":"no job has that id"}`; `409` for a job that has
not failed, such as `{"error":"only a failed job can be retried; this one is leased"}`.

---

## Timestamps

Every `updated_at`, `checked_at` and `last_*_at` field is JavaScript's
`Date.toISOString()`:

```
2026-09-03T21:49:55.191Z
```

Millisecond precision, `Z` suffix, 24 characters. Rows already in the database
carry this exact shape, so it is frozen along with everything else. The Go port
had a bug where its format string rendered three literal zeroes instead of real
milliseconds; there is a test pinning this specifically.

## Client-side rules worth knowing

The client applies two rules the server also applies, and the duplication is
intentional:

1. **`file_path` is validated before sending**, so a request the server would
   reject never leaves the machine and the user sees a real reason instead of
   a `400`.
2. **`file_path` is validated again on the way in from `GET /sync`**, because
   that is the moment a buggy or malicious server's traversal path would
   become a write outside the memory directory on *your* machine. A bad path
   is skipped rather than failing the whole pull, so one poisoned row cannot
   block the rest.

A memory file that is not valid UTF-8 is refused outright rather than being
sent, because `content` is a JSON string and there is no lossless way to carry
arbitrary bytes in one. Refusing is louder than the alternative — the
alternative is silent corruption. The push hook fails on such a file; a bulk
send skips it and names it, since one stray image should not stop the rest of
a directory from reaching the server.
