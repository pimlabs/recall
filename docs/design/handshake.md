# Design: version discovery and device handshake

Status: **proposal**, not built. Nothing here describes today's behaviour;
[`../reference/api.md`](../reference/api.md) does. When a part of this is
built, the reference docs become the authority for it and this file moves to
`history/` as the record of why.

## Why

Two gaps, found while reviewing how the server is deployed (September 2026):

1. **Nothing says which client a server supports.** The server reports only
   the commit it was built from (`/health` → `git_commit`), the client sends
   no version at all, and compatibility has held only because every wire
   change so far happened to be additive. Deploying the server from releases
   instead of `main` makes this worse to leave alone: server and client then
   move on the same version numbers, and nothing checks that they agree.
2. **One shared secret authenticates everything.** Every machine holds the
   same `RECALL_TOKEN`, sends it whole on every request, and cannot be
   revoked without rotating it on every other machine. The server cannot
   tell machines apart; the machine name in a push is whatever the client
   says it is.

Recall stays single-owner. The goal is to handle a leaked credential, a lost
laptop and a mismatched version the way a well-run tool would, not to add
users.

## What exists today

```
Owner: openssl rand -hex 32 → deploy/.env RECALL_TOKEN (read once at server start)
       paste it on each machine → recall connect, or a cloud env variable

recall connect:
  GET /health                          no auth       reachable?
  GET /admin/stats  Bearer <token>     rate limit per IP, then constant-time compare
  save → ~/.recall/credentials.toml (0600), server + name → config.toml

Every hook request:
  GET/POST /sync    Bearer <token>     same check, no session, no expiry
```

## Part 1: discovery and versions

### `GET /.well-known/recall`

A dedicated, unauthenticated discovery document, at the path RFC 8615 sets
aside for exactly this. `/health` stays what it is: a liveness check for
uptime tools.

```jsonc
{
  "protocol": { "current": 1, "supported": [1] },
  "server": {
    "version": "0.3.2",
    "build": {
      "channel": "release",
      "revision": "e100cfdd88e8a0e6659b357ad88979b381f1e548",
      "created": "2026-09-23T01:10:00Z"
    }
  },
  "min_client": "0.3.0",
  "auth": { "methods": ["device-sig-v1", "bearer-legacy"] },
  "capabilities": {
    "merge_base": {},
    "scopes": { "kinds": ["project", "global", "machine"] },
    "limits": { "max_body_bytes": 1048576, "rate_per_minute": 60 }
  }
}
```

**`server.version` is the identity; the commit is provenance.** A release
build reports its release version. Anything else, such as a build from
`main` or a local build, reports the next patch as a SemVer pre-release with
build metadata, for example `0.3.3-dev+g1a2b3c4`. SemVer orders a
pre-release before its release and says build metadata *"MUST be ignored
when determining version precedence"*, so comparisons against `min_client`
work unchanged. `build.channel` says which kind it is and `doctor` warns
about a `dev` server. `build.revision` and `build.created` mirror the OCI
image annotations `org.opencontainers.image.revision` and `.created`, which
the release image also carries. No decision is ever made on the revision.

`/health` keeps `git_commit`: removing a field breaks whoever reads it
(AIP-180), so it stays, documented as informational.

### Rules that make it last

These are the rules git protocol v2, Matrix `/versions` and MCP each rely
on, written down for Recall:

1. **Two layers.** `protocol` changes only for a breaking change.
   `capabilities` only ever grows. A breaking protocol change is a breaking
   release under [`../reference/releasing.md`](../reference/releasing.md).
2. **Unknown keys are ignored.** A client must accept keys it does not
   know, at any depth (git v2: *"Clients must ignore all unknown keys"*).
3. **Absent means unsupported.** A capability that is not listed is not
   available (Matrix).
4. **A map, not a list.** Each capability is an object, so it can carry
   parameters later without a new key.
5. **Nothing is removed within a protocol version**, and no default
   changes (AIP-180).
6. **Both directions identify themselves.** Every request carries
   `Recall-Protocol: 1` and `User-Agent: recall/<version> (<os>-<arch>)`,
   as MCP requires `MCP-Protocol-Version` on every HTTP request and git
   exchanges `agent=`.

A server that does not support the client's protocol answers with a JSON
error naming the versions it does support. A client below `min_client`
gets an error telling it to upgrade, and the hooks print one line about it
at session start rather than failing silently.

### Proof in CI

Golden fixtures: every request and response shape that has shipped is kept
as a JSON file under `recall-wire`. The server's tests must accept every
historical request; the client's tests must accept every historical
response. This is what turns the rules above from a promise into a check.

The repository already works this way for storage: `scripts/compat-check.sh`
runs the server against `fixtures/node-written.db`, a database the retired
Node server really wrote, and it has caught two bugs the unit tests missed.
Golden wire fixtures apply the same idea to the protocol.

## Part 2: device handshake

### Principles, and where they come from

| Principle | Source |
|---|---|
| A machine is identified by a key pair it generated; the private key never leaves it | Syncthing device IDs: *"a direct property of the public key"* |
| A stolen credential is useless without the key | RFC 9700 (OAuth security BCP) §4.10.1, sender-constrained tokens; RFC 9449 (DPoP) |
| Requests are signed at the HTTP layer, so it works behind Traefik | RFC 9421 HTTP Message Signatures. mTLS (RFC 8705) is ruled out because Traefik terminates TLS |
| A new machine joins with a short code approved elsewhere, not a pasted secret | RFC 8628 device authorization; `gh auth login` |
| Non-interactive environments get a narrow, expiring credential | Tailscale auth keys; `claude setup-token` |
| Credentials live in the OS keychain, with a `0600` file as a visible fallback | `gh` credential store; Claude Code on macOS and Linux |

### Roles

- **Operator**: whoever runs the server. Holds the bootstrap secret in
  `deploy/.env`. Used once, to approve the first device, and can then be
  disabled.
- **Owner**: the person whose memory this is. Today operator and owner are
  the same person; the design keeps them apart (see Part 3).
- **Device**: one machine. Has a scope: `sync`, or `sync` + `admin`
  (approve and revoke other devices).

### Enrolment (once per machine, inside `recall connect`)

```
C: generate an Ed25519 key pair; store it in the OS keychain
   (fallback ~/.recall/device.key, 0600, which doctor warns about)
C → POST /v1/devices/enroll { name, public_key, agent }
S → { enrollment_id, user_code: "WDJB-MJHT", expires_in: 900, interval: 5 }
C: show the code and the key's fingerprint
Owner approves from somewhere already trusted:
   • recall devices approve WDJB-MJHT   on an enrolled admin device, or
   • the first device only: the operator's bootstrap secret, once
   The approver sees the name and fingerprint and checks they match.
C → poll until approved, backing off on "slow_down"
S → { device_id, scope }
S stores: device_id, owner_id, name, public_key, scope, agent,
          created_at, last_seen, revoked_at
```

The code format and timings follow GitHub's device flow: eight characters
with a hyphen, valid fifteen minutes, polled every five seconds. RFC 8628
§5.1 and §5.4 cover what else the code needs: rate-limited guessing, and
showing enough on both sides that a phished approval is noticed.

### Every request

```
Recall-Protocol: 1
User-Agent: recall/0.3.3 (macos-arm64)
Content-Digest: sha-256=:…:
Signature-Input: sig1=("@method" "@target-uri" "content-digest" "recall-protocol");
                 keyid="dev_7Q…";created=1790000000;nonce="…"
Signature: sig1=:…:
```

The server looks up the key, refuses a revoked one, verifies the signature,
requires `created` within ±60 seconds, rejects a nonce it has already seen
in that window (RFC 9421 §7.2.2), and records `last_seen`. No secret travels
with the request, so a request copied out of a proxy log cannot be replayed.

### Cloud sessions

A cloud session is a new machine every time and cannot answer a prompt.
Following Tailscale's auth keys:

- The owner creates an **enrolment key**: reusable, ephemeral, tagged
  `cloud`, scope `sync` only, with an expiry. It is shown once.
- It goes in the cloud environment's variables. It can enrol devices; it
  cannot read or write memory.
- Each session generates its own key pair, enrols with the enrolment key as
  an ephemeral device, and is removed after a period of inactivity.
- Revoking the enrolment key stops new sessions without touching the laptop.

The enrolment is automatic. When `recall pull` runs from the SessionStart
hook and finds no device key but a `RECALL_ENROLL_KEY`, it generates a key
pair in the container, enrols with the enrolment key, and is approved at
once (Tailscale's `preauthorized`). Nothing is typed in the session.

### Without a terminal: the admin page

The owner may have only a phone. Everything an owner does is also
available on the server's existing `/admin` page, which gains a Devices
tab, the way Tailscale creates auth keys in its web admin console:

- list devices, with last seen and version, and revoke one
- approve a new device by entering the code it shows
- create an enrolment key (shown once) and revoke one

The page signs in with a **passkey** (WebAuthn): no password, bound to the
site so it cannot be phished, and usable from a phone. The first sign-in
uses the operator's bootstrap secret once to register the passkey; the
bootstrap secret can then be disabled.

Setting up cloud sessions from a phone is then: open `/admin`, create an
enrolment key, paste it into the cloud environment's variables as
`RECALL_ENROLL_KEY`. That is the same one step as pasting `RECALL_TOKEN`
today.

### Owner commands

```
recall devices list                 name, scope, last seen, version, ephemeral?
recall devices approve <code>
recall devices revoke <name>
recall devices enroll-key create --tag cloud --expires 90d   (shown once)
recall devices enroll-key revoke <id>
```

### Moving off the shared token

1. The server advertises `bearer-legacy` alongside `device-sig-v1`, and keeps
   accepting `RECALL_TOKEN` for several releases.
2. `recall connect` enrols a device when the server supports it. `doctor`
   warns while a machine still uses the shared token.
3. Removing `bearer-legacy` is a breaking change, so it ships as the next
   minor version, with the step in the changelog.

### Threats, before and after

| Threat | Today | With this design |
|---|---|---|
| Token copied from a log, shell history or dotfile | Full access until rotated everywhere | Nothing to copy: requests carry a signature, not a secret |
| Laptop lost | Rotate the one token, reconnect every machine | `recall devices revoke laptop` |
| Cloud environment variables leak | Full read/write | An expiring key that can only enrol `sync` devices; revoke it |
| Replay of a captured request | Possible within TLS failures | Rejected: `created` window and nonce |
| A machine claims another's name | Undetectable | The name belongs to a key |
| Server compromised | Memory readable | Still readable. See the open decision on end-to-end encryption |

## Part 3: if Recall ever had more than one owner

Recall is single-owner, and [`../../CLAUDE.md`](../../CLAUDE.md) says a move
towards multiple owners is a different project that starts with asking. This
section only records what the design above would and would not survive, so
that nothing built now makes that harder than it has to be.

**Survives unchanged:** discovery, protocol versions, capabilities, device
keys, request signatures, enrolment codes, scopes, revocation and the
legacy transition. None of it assumes one owner.

**Cheap to reserve now:**

- `owner_id` on every device record, always the single owner for now.
- Operator and owner as separate roles, even while one person holds both.
- Opaque ids (`dev_…`) and `auth.methods` in discovery, so a future method
  can be added without a new protocol version.

**Would have to change:**

- **Storage.** `memory_files` is keyed by `(project_key, file_path)`. Two
  owners with the same repository would share rows. Adding `owner_id` to
  the key is a migration of a schema that is frozen today, so it waits
  until it is needed rather than happening speculatively.
- **Authorisation.** Every query filtered by the requesting device's owner;
  per-owner rate limits.
- **Getting started.** An invitation from the operator, which is an
  enrolment key bound to a new owner.
- **Merge.** The server merges with its operator's `claude` login. Merging
  someone else's memory with it raises account and data-handling questions
  that are not technical. Client-side merge would avoid them.
- **Everything that is not code:** other people's data, a privacy policy,
  and the questions `CLAUDE.md` names.

## Part 4: one binary or two

Today `recall` is one binary for both halves: `recall serve` runs the
server, everything else is the client. Every laptop and cloud session
therefore ships the server too.

**Measured** (release profile, linux x86_64, 0.3.2): 4,507,640 bytes with
the server, 3,108,584 without, so the server is 1.4 MB, 31% of what every
client downloads. It is also about 15 crates the client never runs:
`axum`, `rusqlite` and `libsqlite3-sys`, which compiles SQLite's C source
into the binary.

**What comparable tools do:**

- **Atuin**, the closest analogue (self-hostable sync server plus a CLI on
  every machine), moved its server into its own `atuin-server` binary in
  18.12 ("Move atuin-server to its own binary", #3112).
- **Tailscale** ships `tailscale` (CLI) and `tailscaled` (daemon)
  separately. It links them into one binary only behind the
  `ts_include_cli` build tag, "for space savings reasons" on small devices.
- **Syncthing** and **Vault** are single binaries, but there the same
  process plays both roles: every Syncthing node is a peer, and the Vault
  CLI is itself a client of `vault server`. Recall's client never serves.

**Agreed by the owner (2026-09-23):** two binaries from the same workspace and the same version.

- `recall`: the client, without `recall-server` in its dependency tree.
  This is what npm, Homebrew, curl and `cargo install recall` deliver.
- `recall-server`: a `[[bin]]` in the existing `recall-server` crate. The
  release builds it for the server image, and `cargo install recall-server`
  gets it.
- One version for both, so Part 1 is unaffected: `server.version` and
  `min_client` are still compared on the same numbers.
- The client's integration tests start `recall-server` instead of
  `recall serve`.

Removing `recall serve` from the client is a breaking change under
[`../reference/releasing.md`](../reference/releasing.md), so it belongs in a
minor release. It fits best alongside the move to release-built server
images, which changes the Dockerfile anyway. Until then `recall serve` keeps
working.

## Part 5: end-to-end encryption, and merging without the server

The owner wants memory contents encrypted end to end (2026-09-23): the
server stores ciphertext and never sees a note. Atuin does this for shell
history. The obstacle is merge: today the server reconciles concurrent
edits with the Claude CLI, which needs plaintext. So merge moves to the
client, in layers, cheapest first.

### Encryption

- One **content key** per owner, generated on the first device. Every
  file body is encrypted with an AEAD cipher (XChaCha20-Poly1305, as
  secsync uses) before it leaves the machine.
- A device receives the content key wrapped to its public key when it is
  approved, by the device that approves it. This is why Part 5 comes
  after Part 2: key distribution rides on enrolment.
- **Cloud sessions** have no approving device online. The enrolment key
  therefore has two halves: one it sends to the server to enrol, and one
  it never sends, used locally to unwrap a copy of the content key the
  server stores but cannot open.
- A **recovery key** is shown once when the content key is created. If
  every device is lost without it, the memory is gone. That is the cost
  of the server being unable to read it.
- Not hidden: project keys, file paths, sizes and timings. secsync states
  the same limit: its protocol *"doesn't hide meta data from the server"*.
  Encrypting paths is possible later and is not proposed.

### Merge, in layers

1. **Detect on the server without reading.** A push carries the version it
   was based on, as an `If-Match` precondition (RFC 9110 §13.1.1). If the
   stored version has moved on, the server answers `412 Precondition
   Failed` and changes nothing: the "lost update" problem RFC 9110 names.
   This replaces today's `base_sha256`, which the server compares against
   plaintext.
2. **Three-way merge on the client.** The client keeps the plaintext of
   the version it last synced (its base) in `~/.recall`, fetches and
   decrypts the new server version, and merges the three the way
   `git merge-file` does: changes to different lines combine
   automatically. Memory files are short Markdown notes, so most
   concurrent edits touch different lines and need nothing more.
3. **Let the session's own Claude resolve the rest.** When lines overlap
   and the merge runs inside a Claude Code session (the push hook), the
   hook returns both versions to the running session through the hook's
   `additionalContext`, which Claude Code feeds back to the model. Claude
   rewrites the file, and that edit is pushed like any other, against the
   new base. The model already in the session does the semantic merge the
   server does today, with no second login and no plaintext on the server.
4. **Outside a session**, for example `recall backfill` in a terminal:
   `claude -p` when the machine is logged in; otherwise a conflict copy
   next to the file, as Syncthing does with
   `<file>.sync-conflict-<date>-<time>-<device>.<ext>`, picked up by
   layer 3 at the next session start.
5. `MEMORY.md` is never merged. It is an index Recall regenerates.

### What changes on the server

It stops merging and stops needing a `claude` login. It stores ciphertext,
versions and metadata, and enforces `If-Match`. That also removes the
multi-owner merge concern in Part 3.

### Why not a CRDT

Automerge and Yjs merge without conflicts and secsync relays them end to
end encrypted. But they need the file stored as a CRDT document, and
Claude Code reads and writes plain Markdown. The three-way merge above
works on the files as they are.

## Open decisions

1. **End-to-end encryption.** Wanted by the owner; designed in Part 5. It
   depends on Part 2 for key distribution, and replacing server-side
   merge is a breaking change to the push contract, so it ships in a
   minor release.
2. **`CLAUDE.md`'s ground rule.** It says *"single owner, one bearer
   token"*. This design keeps the single owner and replaces the single
   token with enrolled devices. That wording changes only with the owner's
   explicit agreement.
3. **Order of work.** Part 1 does not depend on Part 2 and ships first:
   discovery, version headers, golden fixtures, release-built server images,
   and with those the split into two binaries (Part 4). Then Part 2, with
   the admin page, then Part 5.

## References

- RFC 8615, Well-Known URIs
- RFC 8628, OAuth 2.0 Device Authorization Grant
- RFC 9421, HTTP Message Signatures
- RFC 9449, OAuth 2.0 Demonstrating Proof of Possession (DPoP)
- RFC 9700, Best Current Practice for OAuth 2.0 Security
- RFC 6750, Bearer Token Usage; RFC 8705, OAuth mTLS
- Semantic Versioning 2.0.0
- OCI image-spec, annotations
- Google AIP-180, Backwards compatibility
- git protocol v2 (`gitprotocol-v2`)
- Matrix client-server API, `GET /_matrix/client/versions`
- Model Context Protocol, lifecycle (2025-06-18)
- GitHub docs, authorizing OAuth apps (device flow); GitHub CLI `gh auth login`
- Claude Code docs, authentication (credential storage, `claude setup-token`)
- Tailscale auth keys (`tailscale_tailnet_key`)
- Syncthing, understanding device IDs
- Atuin, sync and encryption; CHANGELOG 18.12 (#3112, server moved to its own binary)
- Tailscale `cmd/tailscaled` (`ts_include_cli` combined build)
- RFC 9110, HTTP Semantics, §13.1.1 `If-Match` and 412
- `git merge-file` (three-way file merge)
- secsync, end-to-end encrypted CRDT relay (encryption, metadata limits)
- Automerge
- Syncthing, syncing: conflicting changes
- Claude Code hooks reference (`additionalContext` on PostToolUse and SessionStart)
