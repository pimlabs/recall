# Design: Recall as a single Go binary

**Status: proposal, awaiting the owner's decision.** Nothing here is built
yet. This exists so the architecture and the public surface can be agreed
before any code moves.

## Why move at all

The current split is Node server + bash client (+ `jq`, `curl`). It works
and is deployed. The case for changing it isn't distribution — Go tools
ship fine over npm — it's that **bash is holding back the client's
quality ceiling**, in ways that have already cost real bugs:

| Evidence | Impact |
|---|---|
| `sed "s\|^$MEM_DIR/\|\|"` in `recall-push`/`recall-pull` breaks outright on any path containing `\|` — verified, not theoretical | Silent wrong `file_path` or a hard failure, depending on the path |
| Trailing-newline fidelity bug (fixed live during Phase 1) | Files silently changed content on round-trip |
| State file written by plain redirect, not atomically | Two concurrent hooks can truncate the delete-reconciliation baseline |
| The `jq` filter behind `recall init` | Correct, but effectively unmaintainable by inspection |
| **No test suite at all for the client** | Every one of the above was found by hand, live, and nothing prevents regressions |

That last row is the real argument. The client's trickiest logic —
`project_key` normalization across SSH/HTTPS/proxied remotes, the memory
directory slug that must match Claude Code's own algorithm exactly, delete
reconciliation, settings merging — is pure, deterministic, and *ideal* for
table-driven tests. Today that knowledge lives in prose in
`docs/phase-0-findings.md` and in the maintainer's head. In Go it becomes
`go test`, run by CI on every PR.

## Scope

**In:** everything — client and server — as **one binary**, per the
owner's call. `recall serve` runs the server; the same artifact on a
laptop runs `recall init` / `push` / `pull`.

**Out (unchanged):** the ground rules in `CLAUDE.md`. No Anthropic API key
(merge still shells out to the local `claude` CLI). Single owner, one
bearer token. Hook config still lives in each project's own
`.claude/settings.json` — that's why cloud sessions work, and no rewrite
touches it.

## Command surface (the CLI public API)

```
recall serve      Run the sync server (replaces `node server/index.js`)
recall init       Wire the current project's .claude/settings.json
recall status     Diagnose sync for the project you're standing in
recall push       Hook entry point — called by PostToolUse
recall pull       Hook entry point — called by SessionStart
recall version    Print version and build commit
```

Everything is configured by environment variable, exactly as today — no
config file, no flags that duplicate an env var. Flags are only for things
that genuinely vary per invocation:

```
recall serve  [--addr :8787] [--db /data/recall.db]
recall init   [--path <dir>]        # default: git root of cwd
recall status [--json]              # machine-readable for scripts/CI
```

Env vars keep their current names and meanings, so no environment needs
re-provisioning: `RECALL_URL`, `RECALL_TOKEN`, `RECALL_SOURCE_ENV`,
`RECALL_PORT`, `RECALL_DB_PATH`, `RECALL_BACKUP_*`, `RECALL_RATE_LIMIT_*`,
`RECALL_MERGE_*`, `RECALL_CLAUDE_BIN`, plus the Claude Code ones the
client reads (`CLAUDE_CODE_REMOTE_MEMORY_DIR`, `CLAUDE_CONFIG_DIR`).

### Exit codes

Hooks run inside Claude Code, so failure behavior is part of the contract:

| Code | Meaning |
|---|---|
| 0 | Success, **or** deliberately-ignored no-op (file wasn't a memory file, server unreachable on pull) |
| 1 | Misconfiguration the user must fix (missing `RECALL_TOKEN`, not a git repo) |
| 2 | Server rejected the request (4xx/5xx worth surfacing) |

`pull` failing must never block a session from starting — it exits 0 with
a warning on stderr, same as the bash version does today.

## HTTP API — unchanged, and that's a hard requirement

The server keeps the exact contract it has now, because a mixed fleet has
to work during migration: a laptop still on bash hooks and a cloud session
already on Go must both talk to the same server.

```
POST /sync          {project_key, file_path, content, source_env}          → {ok, merged, updated_at}
POST /sync          {project_key, file_path, deleted: true, source_env}    → {ok, deleted, updated_at}
GET  /sync?project_key=…                                                    → {project_key, files[]}
GET  /health                                                                → {status, git_commit, started_at, last_sync_at, last_backup_at, merge{…}}
GET  /admin                                                                 → HTML (token entered client-side)
GET  /admin/stats                                                           → {projects[], totals, git_commit, last_backup_at}
```

Same auth (one bearer token, constant-time compare), same rate limiting,
same tombstone semantics (`deleted` rows keep content server-side, `GET`
withholds it).

**The SQLite schema is unchanged too** — same `memory_files` table, same
columns, same primary key. The Go server opens the *existing production
database file* as-is. No migration, no export/import, and rollback is just
starting the old container against the same file.

No Go packages are exported for outside use — everything lives under
`internal/`. If a public Go API is ever wanted, that's a separate decision.

## Package layout

```
cmd/recall/main.go          Subcommand dispatch, nothing else
internal/
  config/                   Env loading + validation, one place
  project/                  project_key derivation, memory dir, slug, state file
  claudecode/               Facts about Claude Code itself (path algorithm,
                            settings.json shape) — isolated because it tracks
                            someone else's implementation, see phase-0-findings
  settings/                 .claude/settings.json read/merge/write (init)
  hookio/                   Hook stdin payload parsing, exit-code policy
  syncclient/               HTTP client: push, pushDelete, pull, health
  server/                   Routes, auth, rate limit, admin page (go:embed)
  store/                    SQLite: schema, queries, tombstones, backups
  merge/                    `claude -p` subprocess, prompt, fallback policy
```

The split that matters most is `claudecode/`: the memory-path algorithm
and the settings.json hook shape are **reverse-engineered facts about a
tool we don't control**. Isolating them means a future Claude Code change
has exactly one place to fix, with its own tests, instead of being smeared
across the client.

## Key decisions

**Pure-Go SQLite (`modernc.org/sqlite`), not cgo.** Cross-compiling for
darwin/linux × amd64/arm64 stays a single `GOOS/GOARCH` matrix with
`CGO_ENABLED=0`. Costs ~10MB of binary size; buys a release pipeline with
no C toolchain per platform. For a tool distributed as prebuilt binaries,
that trade is clearly right.

**Standard library for CLI dispatch, no cobra.** The command set is six
verbs with almost no flags. `flag` plus a switch is ~50 lines and keeps
the dependency list at effectively one (the SQLite driver), matching the
repo's existing "deliberately boring, zero dependencies" posture.

**Admin page via `go:embed`.** It's static HTML that fetches
`/admin/stats` client-side; embedding keeps the single-binary property.

**Merge still shells out to `claude -p`.** Same flags found to matter
(`--system-prompt`, `--exclude-dynamic-system-prompt-sections`,
`--strict-mcp-config`, neutral cwd — the difference between ~$0.01 and
~$0.19 per call), same fallback-to-last-write-wins on every failure. Go
gains proper `context.WithTimeout` instead of a manual kill timer.

**Atomic writes everywhere the bash version isn't.** Memory files and the
state file get write-temp-then-`os.Rename`, fixing the truncation race by
construction.

## Testing — the actual payoff

Table-driven unit tests, no network, for everything that was previously
verified by hand:

- `project_key` from SSH, HTTPS, `.git`-suffixed, trailing-slash, and the
  proxied `http://local_proxy@127.0.0.1:PORT/git/owner/repo` form that
  cloud sandboxes rewrite `origin` to; plus the no-remote fallback.
- Memory-dir slug against known-good pairs, including the real
  `/home/user/recall` → `-home-user-recall` case, and paths containing
  `|`, spaces, and unicode — the class of input that broke the `sed`.
- Newline fidelity: 0, 1, and 2+ trailing newlines survive push→pull byte
  for byte.
- `settings.json` merge: empty file, missing file, unrelated keys
  preserved, another tool's hook on the same matcher appended-not-replaced,
  and idempotency across repeated runs.
- Delete reconciliation: state file present/absent/stale, and the
  first-run case that must not read an empty dir as "everything deleted".

Integration tests with `httptest` + a temp SQLite file for the server:
auth, rate limiting, tombstone withholding, per-`project_key` isolation,
and merge fallback when the `claude` binary is missing or fails.

CI runs `go test ./...`, `go vet`, and `gofmt -l` on every PR — replacing
today's syntax-only shell checks.

## Distribution and the versioning question

Go changes this, and it's the one place the rewrite forces a process
decision:

- **npm**: the esbuild pattern — a thin wrapper package with per-platform
  `optionalDependencies` carrying prebuilt binaries.
- **Homebrew**: the formula moves from `--HEAD` to a real `url` +
  `sha256` per release.
- **install.sh**: downloads the right prebuilt binary instead of copying
  scripts.

All three want **tagged releases with real version numbers**, which
`ROADMAP.md` Phase 4 explicitly decided against ("formal semver/CHANGELOG
wasn't worth it for a single-owner tool deployed straight from `main`").
That reasoning held for a server deployed from `main` — it doesn't hold
for binaries distributed to machines. Recommendation: adopt lightweight
tags (`v0.1.0`) with a GitHub Actions release workflow doing the build
matrix, and keep the server still deployable straight from `main`
independently. Worth an explicit yes/no from the owner.

## Deployment changes

Multi-stage Dockerfile: build the binary, then copy it into the runtime
image. **The image won't get much smaller, and that's expected** — the
runtime still needs Node and the `claude` CLI installed for merge, which
dominates the image size regardless of what language the server is in.
`docker-compose.yml`, the Cloudflare tunnel, volumes, backups, and the
`claude setup-token` step all stay exactly as they are.

## Migration plan

Staged so production data is never at risk in the same step as a rewrite:

**Phase A — Go client, old server.** Build the client to parity, ship it,
point it at the *existing Node server*. Zero server risk; the HTTP
contract is the seam. Cutover is per-machine and reversible by reinstalling
the bash version.

**Phase B — Go server, same database.** Stand it up against a *copy* of
the production DB, replay recorded requests against both implementations,
diff the responses. Cut over only when they match. Rollback is starting
the old container against the same untouched file.

**Phase C — retire.** Delete `server/index.js`, `hooks/*.sh`, `bin/recall`
once both phases are stable. Git history keeps them; the tagged release
before cutover is the rollback point.

## Open questions for the owner

1. **Tagged releases** — adopt `v*` tags for binary distribution (see
   above)? This is the one process change the rewrite forces.
2. **Phase B appetite** — is rewriting a working production server worth
   it for one-language consistency? My honest read: Phase A carries most
   of the quality win (that's where the bugs and the untested logic are),
   while Phase B is mostly consistency. Phase A alone is a legitimate
   stopping point if the appetite runs out.
3. **Binary size** — ~10-15MB with pure-Go SQLite, vs ~2MB if the client
   were split from the server. Single binary is the stated preference;
   confirming that's still true knowing the number.
