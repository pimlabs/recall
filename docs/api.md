# HTTP API

Recall's server exposes five routes. Two carry memory files, two are for
looking at the deployment, and one is a browser page.

This is a **frozen** surface: field names, field order, and the difference
between `null` and `""` are compatibility guarantees, not style. The deployed
Node server speaks the same JSON, rows already in the database were written
against it, and during a migration a machine on the old client and one on the
new binary talk to the same deployment. The Rust definition of every shape
below lives in [`recall-wire`](../crates/recall-wire/src/), which both halves
share so they cannot drift apart.

Everything on this page is asserted against a running server by
[`scripts/api-doc-check.sh`](../scripts/api-doc-check.sh) — status codes,
error wording, field order, and the `null`-versus-`""` distinction. If a
handler changes and this document doesn't, that script fails.

| Route | Auth | Purpose |
|---|:---:|---|
| [`POST /sync`](#post-sync) | yes | Send one memory file, or one delete |
| [`GET /sync`](#get-sync) | yes | Fetch every file held for one project |
| [`GET /health`](#get-health) | **no** | Liveness, and whether merge actually works |
| [`GET /admin/stats`](#get-adminstats) | yes | What is stored, per project |
| [`GET /admin`](#get-admin) | **no** | An HTML page rendering the above |

---

## Authentication

One bearer token, sent on every authenticated route:

```
Authorization: Bearer <RECALL_TOKEN>
```

There is exactly one token and one owner — see the ground rules in
`CLAUDE.md`. The comparison is constant-time, so a wrong token takes the same
time to reject whether the first character was right or the first thirty
were.

`GET /health` and `GET /admin` are deliberately unauthenticated so uptime
tooling can poll them without holding the token. `/health` reports no file
contents and no project keys.

| Failure | Status | Body |
|---|:---:|---|
| Missing or malformed `Authorization` | `401` | `{"error":"unauthorized"}` |
| Wrong token | `401` | `{"error":"unauthorized"}` |
| Too many requests | `429` | `{"error":"rate limit exceeded, try again later"}`, plus a `Retry-After` header |

Rate limiting is per client IP, defaulting to **60 requests per 60 seconds**
(`RECALL_RATE_LIMIT_MAX`, `RECALL_RATE_LIMIT_WINDOW_MS`), and runs *before*
the auth check — so a flood of invalid tokens is limited too, rather than
escaping the limiter by never reaching auth.

The client's address is read from exactly one request header, named by
`RECALL_TRUSTED_IP_HEADER` — `cf-connecting-ip` behind a Cloudflare tunnel,
`x-real-ip` behind Traefik or nginx. One header, not a list: anything the
server is willing to read from an untrusted client is something that client
can choose, and choosing your own bucket defeats the limit. It is trustworthy
only because the container has no published port, so every request really
does arrive through that ingress.

## Errors

Every non-2xx response, on every route, has the same shape:

```json
{ "error": "file_path must be relative, no traversal" }
```

Requests larger than **5 MiB** are rejected before they are parsed — `413`,
or `400` if the truncated body fails to parse first. Memory files are prose;
anything that size is a bug or an attack, not a note.

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
  "deleted": false
}
```

| Field | Type | Required | Notes |
|---|---|:---:|---|
| `project_key` | string | yes | How two machines agree they mean the same project. See [Project identity](../ARCHITECTURE.md#project-identity). |
| `file_path` | string | yes | Relative to the memory directory, forward slashes. Validated — see below. |
| `content` | string | for a write | The file's **exact** bytes, trailing newlines included. |
| `source_env` | string | no | A display label for the machine. Nothing keys off it. |
| `deleted` | bool | no | `true` makes this a delete; `content` is then omitted. |

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

`merged: true` means the stored content is the result of a semantic merge
rather than the bytes you sent. That happens only when there was genuinely
something to reconcile — an existing, non-tombstoned row whose content differs
from the push. A new file, a revived tombstone, or a re-push of unchanged
content all skip straight to a write.

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
| `200` | Stored. Check `merged` to see whether a merge happened. |
| `400` | Bad JSON, a missing required field, or a rejected `file_path`. |
| `401` | Bad or missing token. |
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
| `401` | Bad or missing token. |
| `429` | Rate limited. |

### Example

```sh
curl -sS -G "$RECALL_URL/sync" \
  -H "Authorization: Bearer $RECALL_TOKEN" \
  --data-urlencode "project_key=acme/app"
```

---

## `GET /health`

Unauthenticated. Safe to point uptime monitoring at.

```json
{
  "status": "ok",
  "git_commit": "a1b2c3d",
  "started_at": "2026-09-03T09:00:00.000Z",
  "last_sync_at": "2026-09-03T21:49:55.191Z",
  "last_backup_at": "2026-09-03T09:00:01.412Z",
  "merge": {
    "enabled": true,
    "claude_cli": {
      "checked_at": "2026-09-03T21:30:00.000Z",
      "available": true,
      "logged_in": true,
      "error": ""
    },
    "last_merge_at": "2026-09-03T21:49:55.101Z",
    "last_merge_error": null
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
| `merge.last_merge_error` | Non-null means a real merge was attempted and failed. |

Fields that would be empty are **omitted rather than sent empty**:
`last_sync_at` before anything has synced, `last_backup_at` when backups are
off, `merge.last_merge_at` before a merge has succeeded. `last_merge_error` is
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

Authenticated. What the owner is storing, per project.

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
admin *write* surface at all, deliberately: nothing on this route can delete a
project or edit a note, so a leaked token cannot be used to quietly destroy
history through it.

## `GET /admin`

The same numbers as an HTML page, for a browser. Unauthenticated because it
ships no data of its own — it fetches `/admin/stats` from the browser, which
means the person looking at it still needs the token. Served under a strict
CSP that allows no external anything.

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
alternative is silent corruption.
