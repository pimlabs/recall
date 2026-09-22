# Roadmap

## Phase 0 — Prove the round-trip, no merge logic yet — done

- [x] Stood up the server with `POST /sync` / `GET /sync` against SQLite (`server/`), no merge — last write simply overwrites. Zero external dependencies (`node:http` + `node:sqlite`).
- [x] Built the `PostToolUse`/`SessionStart` hooks, as bash scripts at the time — see below for why `PostToolUse` instead of `FileChanged`. (Retired in Phase 7; `recall init` now wires `recall push` / `recall pull`.)
- [x] Confirmed empirically: **`FileChanged` doesn't exist** in the installed Claude Code CLI (v2.1.42), and neither does hook type `"http"`. Used `PostToolUse` matching `Edit|Write` (`type: "command"`) instead. Confirmed live — not just by reading the source — that this **does** catch dynamically-named topic files (e.g. `debugging.md`), because the matcher only filters on tool name and the script itself checks the actual file path per call. Full writeup: `docs/history/phase-0-findings.md`.
- [x] Confirmed the project-key derivation does **not** match Claude Code's own scoping (which is local-filesystem-path-based, not git-remote-based) — and that this divergence is intentional/necessary. `project_key` derives from the git remote's `owner/repo`; the local memory directory is computed separately by replicating Claude Code's own path-slug algorithm. Details and edge cases (proxied remotes, GitLab subgroups) in `docs/history/phase-0-findings.md` §6.
- Also surfaced, not originally in scope but load-bearing: auto memory is **off by default in remote/cloud sessions** unless `CLAUDE_CODE_REMOTE_MEMORY_DIR` is set (`docs/history/phase-0-findings.md` §5) — this has to be configured as an environment secret alongside `RECALL_TOKEN`.

**Done when:** editing a memory file on one machine, then starting a fresh session (ideally an actual ephemeral cloud session) on the same project, shows the updated content — with zero manual setup on the second environment beyond having cloned the repo. **Proven at the mechanism level**: two simulated machines (different filesystem paths, different git-remote URL shapes) round-tripped `MEMORY.md` and a dynamically-named topic file through a live server instance byte-for-byte (`docs/history/phase-0-findings.md`, "Round-trip proof"). Deploying a real server and validating against an actual second ephemeral cloud session is the remaining operational step, not a mechanism gap — which is exactly what Phase 1 below is for.

## Guiding rule from here on

**Prototype works, then features get added — not the other way around.** Phase 0 proved every piece works in isolation/simulation. Before building anything new (merge quality, multi-project isolation, polish), Phase 1 makes the whole thing run for real, on real infrastructure, between a real laptop and a real fresh cloud session, with today's simplest behavior (last-write-wins). Every later phase assumes Phase 1's deployed server and wired-up hooks already exist and already work — they're additive on top of a working thing, not prerequisites to having one.

## Phase 1 — Make the prototype actually work, end to end

No new features. The server and hooks from Phase 0 already do everything needed — this phase is entirely about closing the gap between "proven in a simulated sandbox" and "a real person's memory actually syncs."

- [x] **Deploy the server somewhere it stays running.** Done via OrbStack + Cloudflare Tunnel (`deploy/`) — `recall-server` and `cloudflared` containers running on the owner's Mac, public hostname `recall.pimlabs.id`. Verified from outside: `GET /sync?project_key=smoke-test` returns `{"project_key":"smoke-test","files":[]}` with a valid bearer token, `401` without one. (That is the record of what was deployed at the time, not a recommendation — `deploy/README.md` is host-neutral and covers both ingresses.)
- [x] **Generate the real `RECALL_TOKEN`** — generated, stored in `deploy/.env` (gitignored, not committed).
- [x] **Set `CLAUDE_CODE_REMOTE_MEMORY_DIR` as a real environment secret** on a real claude.ai cloud environment for `pimlabs/recall` (`/home/user/.claude` — that sandbox's `$HOME` is actually `/root`, but the memory-path derivation prioritizes the explicit override, so it still resolves correctly).
- [x] **Wire the hooks into one real project's `.claude/settings.json`** — wired into `pimlabs/recall` itself, dogfooding.
- [x] **Run the real test**: verified 2026-08-12 in a genuine claude.ai cloud session — SessionStart's `recall-pull` synced all 3 existing memory files with zero manual intervention, then editing one of them triggered `recall-push` automatically and the change showed up server-side seconds later.
- [x] **Fix whatever breaks under real conditions**: two surprises, both fixed. (1) A trailing-newline round-trip bug in `recall-push`/`recall-pull` (fixed, see the newline-fidelity fix commit). (2) claude.ai cloud environments block egress to custom domains by default — `recall.pimlabs.id` needed adding under that environment's **Network access → Custom → Allowed domains**, not something a local simulation could have caught.

**Done when:** the exact "done when" from Phase 0 is true for real — a genuine second environment, zero simulation. **Done, 2026-08-12.**

## Phase 2 — Real merge — done

- [x] **Replaced last-write-wins with `claude -p` semantic merge** for genuine conflicts. `POST /sync` only attempts a merge when there's something to reconcile — an existing, non-tombstoned row whose content differs byte-for-byte from the incoming push. A brand-new file, a revived tombstone, or a re-push of unchanged content all skip straight to a plain write (cheaper, and avoids a merge call ever second-guessing content that didn't actually conflict).
- [x] **Handled the server needing its own logged-in `claude` CLI**: installed in the image (`npm install -g @anthropic-ai/claude-code`), one-time interactive `claude setup-token` documented in `deploy/README.md`, credentials stored under `/data/claude-config` — inside the same persistent volume the database already uses, so it survives rebuilds without a separate volume. Every failure mode (CLI missing, not logged in, timeout, malformed output) degrades to last-write-wins rather than rejecting the sync — visible via `GET /health`'s new `merge` object instead of failing silently or loudly.
- [x] **Found and fixed a real cost problem while proving this live**: running `claude -p` with its default agentic system prompt from inside a real project directory ballooned a trivial merge call to ~$0.19 in wasted cache-creation tokens (project CLAUDE.md, tool definitions, etc. — none of which a text-merge task needs). Fixed with a minimal custom `--system-prompt`, `--exclude-dynamic-system-prompt-sections`, and `--strict-mcp-config`, run from a neutral cwd — same call, ~$0.01.

**Done when:** two environments editing the same topic file with genuinely different information both end up represented after a sync, not just the more-recent one. **Done, verified live**: pushed version A (`--force` flag note + a Redis fact) then version B (a reworded `--force` note + an unrelated Nginx fact) for the same file — the stored result kept the clearer wording from B, the Redis fact only A had, and the Nginx fact only B had. A separate live check with a genuine contradiction (two different numbers for the same rate limit) confirmed both are kept with an inline `[CONFLICT: ...]` marker rather than one silently overwriting the other.

## Phase 3 — Multiple projects, token/auth hardening — done

- [x] **Confirmed the server separates memory by `project_key`** for more than one repo. Verified live: pushed different content under the same `file_path` (`MEMORY.md`) to two different `project_key`s, read each back — no cross-contamination. Also confirmed for deletes: tombstoning the file in one project left the other's row completely untouched, and `GET /admin/stats` bucketed both projects separately with correct per-project counts. This was already structurally guaranteed (`project_key` is half of every row's primary key, every query filters by it) — this step was confirming that empirically rather than trusting the schema by inspection alone, per this project's stated preference throughout.
- [x] **Bearer token setup made boring**: `docs/reference/token-setup.md` — generate once, install on a laptop shell profile and on a claude.ai cloud environment's secrets (plus the network-allowlist step that's easy to forget), verify with a couple of `curl`s, and what rotation actually involves for a single shared token.

**Done when:** two different projects synced through the same Recall server never cross-contaminate memory. **Done, verified live** (see above).

## Phase 4 — Operational polish (open-ended)

- [x] Basic observability: `GET /health` (unauthenticated) reports server status, start time, and the most recent sync across all projects. Global rather than per-project — good enough to answer "is this thing alive" from outside without a token; per-project last-synced-at can wait until it's actually needed. Also reports the deployed `git_commit` (baked in via a Docker build arg), added after deciding formal semver/CHANGELOG wasn't worth it for a single-owner tool deployed straight from `main` — the real risk was deployment drift (`main` has a fix, the running container doesn't yet), not version compatibility between independent consumers.
- [x] **Automatic backups.** Found during an architecture/security pass (2026-08-12) that there was no backup story at all — a real gap given cloud sessions are ephemeral by design, so the server can be the *only* surviving copy of content that originated there. Server now runs `VACUUM INTO` on an interval (default 24h, keeps last 7), writing to a host-mounted `deploy/backups/` folder so external backup tooling can reach it without going through Docker. See `deploy/README.md`.
- [x] **Delete/tombstone support.** Same pass found `recall-push` silently no-ops on a local delete — the server never learned a file was removed, so it came back on the next pull. Fixed with a tombstone design (content preserved in the row, `deleted` flag set, `GET /sync` withholds content for tombstoned rows so a pull can't resurrect them). The harder half: there's no hook event for a delete at all (a `Bash rm` doesn't match the `Edit|Write` matcher even if there were one), so `recall-push` reconciles instead — every run compares the current directory listing against a small state file (`.recall-state.json`, next to the memory dir, not inside it) and reports anything missing as a delete. Propagation is "next edit to any memory file in the project," not instant, since there's nothing to make it instant. Verified end to end: local delete + editing an unrelated file correctly produced a tombstone; a fresh pull skipped the tombstoned file; a stale local copy got actively removed on pull.
- [x] **Non-root container user.** The image now runs the server as the built-in `node` user. Not just a `USER node` line — the production volume already existed with root-owned content from before this fix, so a small `docker-entrypoint.sh` runs as root only long enough to `chown` `/data`, then drops to `node` via `su-exec` for the actual process. Verified against a hand-built volume with pre-existing root-owned files: server started clean, process confirmed running as `node` (not root), old files' ownership fixed automatically.
- [x] **Rate limiting on `/sync`.** Simple in-memory per-IP fixed-window limiter (default 60 req/min, tunable via `RECALL_RATE_LIMIT_MAX`/`RECALL_RATE_LIMIT_WINDOW_MS`) — no external store needed for a single-process personal server. Counts both valid and invalid-token requests so a flood of bad tokens can't dodge it by failing auth first. `/health` stays unlimited since it's meant to be pollable. This closes out every finding from the architecture/security pass.
- [x] Decide whether `recall-pull` should be a single static binary (e.g. Go) vs. a script needing a runtime — revisited 2026-08-12 now that Phase 1's real-world deployment is known to work: staying with bash + curl + jq. Both dependencies have now been proven present and working on every real environment tested (this laptop, and a genuine claude.ai cloud sandbox), the one real bug they caused (trailing-newline fidelity) is fixed, and a compiled-binary rewrite would trade "clone and it just works" for per-platform binary distribution to fix a problem that hasn't actually recurred. Revisit again only if curl/jq turn out missing on some future environment, or if the hooks' logic grows past what a shell script should reasonably hold.

### Later additions (2026-09)

Open-ended means open-ended. These surfaced only once the project was being
shipped rather than built, and they belong here rather than in a phase of
their own.

- [x] **A committed hook must not break a machine that has no Recall.** The
      wiring `recall init` writes is committed and reaches every clone,
      including ones where the binary was never installed — where it errored
      on every edit. Both commands are now guarded
      (`if command -v recall >/dev/null 2>&1; then recall push; fi`), so a
      machine without Recall does nothing instead of failing. `settings::is_wired`
      matches the command as a substring so projects wired before this still
      read as wired.
- [x] **`main` was permanently red.** The `deploy` job ran on every push to
      `main` and failed with "missing server host" whenever the VPS secrets
      were absent — a fork, a clone, or this repository before deployment was
      wired. Always-red CI is CI nobody reads, which is the structural reason
      five PRs got merged without their failures being noticed. The job now
      skips with a notice.
- [x] **The ingress is a choice, and the choice is load-bearing.** The VPS
      running Recall already routes other services through Traefik, so
      `deploy/docker-compose.traefik.yml` sits beside the Cloudflare Tunnel
      one. Two properties have to hold in either: no published port
      (`expose`, never `ports`), and `RECALL_TRUSTED_IP_HEADER` naming the
      header that ingress actually sets. They are one property — rate
      limiting keys off that header and runs *before* auth, so a client that
      can reach the origin directly, or supply the header itself, gets a
      fresh bucket per request and unlimited attempts at the token.
      `scripts/trusted-ip-check.sh` asserts it on a real socket.
- [x] **A way to undo a project stored under the wrong key.** Recall has no
      admin write surface by design — `GET /admin/stats` is read-only,
      sqlite-web mounts the volume read-only, and nothing in the HTTP API can
      delete a project, so a leaked token cannot destroy history through
      anything the server exposes. The cost is that a badly-keyed project
      stays. `deploy/README.md` now carries the procedure: inspect, back up,
      stop, `DELETE` — plus the `UPDATE` rename variant, with the warning
      that the primary key is `(project_key, file_path)` so a rename onto an
      occupied key collides.

## Phase 5 — One Go binary — done

Designed in the Go rewrite design doc (retired in Phase 7; see git history) and decided there: everything, one
binary, `recall serve` included.

- [x] **Client and server rewritten in Go**, sharing `internal/wire` — the
      validation rules and the tombstone/empty-file distinction that
      previously existed twice, in JavaScript and in bash, with nothing
      keeping them in agreement.
- [x] **The test suite that justified the move.** Everything previously
      verified by hand is now table-driven: `project_key` across every real
      remote form, the memory-dir slug, settings merging, delete
      reconciliation, newline fidelity, tombstone withholding, project
      isolation, rate limiting, merge fallback.
- [x] **Three defects found by writing those tests**, all of which the
      shell version had and none of which anything would have caught:
      the `sed` breaking on paths containing `|`; the slug being
      byte-wise and **locale-dependent**, so a project path with an accent
      or emoji resolved to a directory Claude Code never writes to and sync
      silently did nothing; and a sibling directory sharing a name prefix
      (`memory-notes` vs `memory`) being treated as inside the memory dir.
- [x] **Verified against the implementations being replaced**, not just in
      isolation: the Go server opens a database written by the Node server
      and serves it correctly, tombstones included, with no migration; the
      old shell hooks push to and pull from the Go server; the Go client
      pushes to and pulls from the Node server; and a real semantic merge
      through the Go server kept both distinct facts and the better
      wording.
- [x] **Distribution**: prebuilt binaries for darwin/linux × amd64/arm64
      via a tag-triggered release workflow, delivered through npm (checksum
      verified), Homebrew, and `install.sh`. Tagged releases adopted, for
      the reason recorded in the design doc.

**Done when:** one installable binary does everything, with the old
implementations still present as a rollback path. **Done.** (The retirement
itself happened in Phase 7.)

## Phase 6 — Rust — done

The owner chose Rust as a first project in the language. `docs/history/rust-rewrite.md`
records that as the actual reason, along with an honest accounting: none of
the five bugs the Go port surfaced would have been prevented by Rust, and
the one concrete cost is cross-compilation (rusqlite's bundled SQLite is C,
so the release matrix now builds on native runners rather than a single
CGO-free build).

- [x] **Cargo workspace, one crate per boundary** — `wire`, `paths`,
      `hooks`, `server`, `cli`. Not ceremony: it is what let the port run
      in parallel, since each crate compiles and tests on its own.
- [x] **Every frozen surface preserved** — SQLite schema (the server opens
      the existing production database with no migration), HTTP API and
      JSON down to field order and the `null`-vs-`""` tombstone
      distinction, timestamp format, env var names, and the CLI surface
      that projects already have committed in their `.claude/settings.json`.
- [x] **The Go implementation removed.** It never ran in production, so
      keeping it would have meant carrying two dead implementations instead
      of one. Node and the shell hooks stay — Node is what actually serves
      the owner's memory today, so it is the real rollback path.
- [x] **Two more bugs found, both of which predated the port.** Go's
      `json.Marshal` silently replaced invalid UTF-8 with U+FFFD, so a
      non-UTF-8 memory file was pushed corrupted; Rust's types force that
      into the open. And an empty memory file was unsyncable in *both*
      implementations, because `content` was omitted when empty and the
      Node server rejects a push without it — caught not by either test
      suite but by `scripts/compat-check.sh` running a real push at the
      real server. Details in `docs/history/rust-rewrite.md`.
- [x] **`scripts/compat-check.sh`** — the mixed-fleet matrix, automated:
      the new server opening a Node-written database, the old shell hooks
      against the new server, the new client against the Node server, and
      byte-exact round trips. 11 checks, all green. Run it before cutting
      production over, and again after.

- [x] **Edge-case hardening: 113 tests → 159, and two more bugs.** A
      deliberate adversarial pass over merge failure, content fidelity
      (unicode, NUL bytes, oversized bodies), awkward project keys and file
      paths, tombstones, auth header variants, concurrency, symlinks,
      permissions, and the CLI's exit-code contract. It found that an
      **empty merge result silently wiped memory** — the server stored `""`
      and reported `merged: true` — and that **`recall push` demanded
      configuration before checking whether the edited file was even a
      memory file**, so an unconfigured machine with a wired project errored
      on every unrelated edit. Both fixed; details in
      `docs/history/rust-rewrite.md`.

- [x] **A pass over the public API and the docs.** Names first: two
      unrelated types called `Env` (the CLI had already worked around it with
      `use recall_paths::Env as ClaudeEnv`), a `Client` that held
      configuration next to a `Client` that made requests, and
      `Result<PushResult, Error>`. Now `claude::Env` and `Context`,
      `ClientConfig` and `client::Client`, `PushOutcome`/`PullOutcome`. Then
      boundaries: `hooks.rs` was 1169 lines holding push, pull, path
      containment and 63 tests behind a `tests_support` shim, and `main.rs`
      held all five commands; both are split, one file per thing.
      `missing_docs` is denied in every library crate and CI runs rustdoc
      with `-D warnings`, so an undocumented public item or a stale doc link
      now fails the build. The HTTP API — the actual public surface, and the
      one part with no reference at all — is documented in `docs/reference/api.md`, and
      that document is *asserted* rather than trusted:
      `scripts/api-doc-check.sh` drives 27 checks against a real server on a
      real socket, covering every status code, error string, field order and
      `null`-versus-`""` claim it makes. Change a handler without changing the
      doc and CI fails.

**Done when:** one installable Rust binary does everything the Go one did,
against the same verification matrix. **Done.**

## Phase 7 — Retire the implementations that were replaced — done

The tree still carried a full Node server and a full bash client, months
after both were superseded, plus the design doc for a Go port that no longer
exists. All three were kept "as the rollback path", which git already is.

- [x] **Deleted `server/index.js`, `server/package.json`, `hooks/*.sh`,
      `hooks/settings.snippet.json`, `hooks/README.md`, and
      `docs/go-rewrite-design.md`.** Nothing is lost — `git log -- hooks/`
      still has all of it.
- [x] **Kept the one thing that mattered, in a better form.** The Node
      server's real value here was as a source of Node-written database rows
      for `scripts/compat-check.sh`, because production's database was
      written by Node and this server has to open it. That is now
      `fixtures/node-written.db`, a database the Node server actually
      wrote, captured before it was deleted. The matrix went from 11 checks
      to **19** and covers more: byte-exact content, no/one/two trailing
      newlines, an empty file, unicode, a nested path, a tombstone with its
      content still withheld, project isolation, and that this server can
      keep *writing* to a Node-written file. It also no longer needs Node
      installed to run.
- [x] **`server/` is gone as a directory.** What was left in it — the
      Dockerfile and the entrypoint — is the deployment image, so it lives in
      `deploy/` next to the compose file that builds it.
- [x] **Fixed two things the move surfaced.** `.dockerignore` was in
      `server/`, but the build context is the repository root, so Docker was
      never reading it and `target/` was being uploaded to the daemon on
      every build. And CI had been **red on every run since the `recall-cli`
      → `recall-sync` rename**: the Dockerfile still asked for the old
      package, and being extensionless it was missed by the search that
      updated everything else. Both fixed, and CI now also syntax-checks the
      scripts that actually ship.

**Done when:** nothing in the tree is a copy of something already replaced,
and the checks that protected the migration still run. **Done.**

## Phase 8 — A scope for memories about you, not the repository — done

Claude Code writes facts about the *person* into whichever project it
happened to learn them in — it labels them `type: user` in the file's own
front matter (`docs/history/memory-loading-findings.md` §2). Recall synced them
faithfully into exactly one repository's history, which is the wrong place
for them.

- [x] **Scopes.** A scope pairs a `project_key` on the wire with a subtree of
      the local memory directory. There are two: the project, rooted at the
      memory directory itself, and the global one at `<memory dir>/global/`,
      keyed `global:<RECALL_GLOBAL_KEY>`. The server learned nothing new — a
      scope key is just another opaque `project_key`, so the frozen HTTP
      surface and the SQLite schema are untouched and all the routing is
      client-side, in `recall_paths::scope`. Off unless `RECALL_GLOBAL_KEY`
      is set: files appearing in every project's memory directory is not
      something to do to someone by surprise.
- [x] **A path under `global/` never falls through to the project scope.**
      With global sync off it is ignored, not absorbed. Pushing someone's
      personal notes into one repository's history is a one-way door.
- [x] **`MEMORY.md` is maintained, because a file nothing links may as well
      not be on disk.** Established by probing the real CLI, not assumed: a
      file linked from `MEMORY.md` was read at the root and in a
      subdirectory; one linked from nothing came back `UNKNOWN`. Each link
      carries the file's own front-matter `description`, because that gloss
      is what the model sees when choosing what to open.
- [x] **What that measurement actually cost, recorded rather than tidied
      away.** Retrieval is probabilistic — the same files and the same
      question returned `UNKNOWN` four times and the right answer the fifth,
      with nothing changed between runs. Two conclusions drawn from single
      runs during this work were both wrong and are retracted in
      `docs/history/memory-loading-findings.md`, with the counts that refuted them.
      The rule that came out of it: a single failed probe proves nothing, and
      what is deterministic (the right bytes, at the right path, under the
      right key, with the links maintained) is separated from what is not
      (whether Claude opens the file), because only the first is testable.
- [x] **`recall promote <file>`** — the way *into* the scope, which shipped
      with the machinery to carry a note into every project but no way to put
      one there. A move, not a copy: stored under the global key, tombstoned
      under the project's, moved into `global/` on disk, linked from
      `MEMORY.md`. Ordering follows recovery — nothing is sent or moved until
      the store succeeds, the new copy is written before the old is removed
      (a crash leaves the note in *both* places, never in neither), and the
      baseline is refreshed last so an unsent tombstone is re-sent by the next
      push hook.
- [x] **`RECALL_PROJECT_KEY`.** Declares the key a project syncs under
      instead of deriving it from the git remote, covering the four cases no
      derivation reaches: a repo with no remote at all, sub-projects of a
      monorepo that should or should not share one history, a fork that wants
      to keep reading the upstream's memory, and the nested-subgroup
      collision Phase 0 documented as a known limitation.
- [x] **A refused setting is reported, not silent.** A `RECALL_PROJECT_KEY`
      or `RECALL_GLOBAL_KEY` that cannot be used is dropped and the default
      stands — refusing rather than failing keeps a working project working.
      The cost of that choice is a setting that silently does nothing, which
      is the hardest kind of misconfiguration to notice, so the refusal is
      recorded and `recall status` reports it, alongside whether the key in
      force was declared, taken from the git remote, or taken from this
      checkout's path.
- [x] **One bug the unit tests could not have found.** Driving the whole
      chain against a real server showed that after a promotion the project's
      `MEMORY.md` still linked the path the note had just left — and since
      `MEMORY.md` is itself synced, that dead link returned on the next pull
      and reached every other machine. Promotion now drops that one line and
      pushes the result; only that line, and only when it was really there.

**Done when:** a note about the user, written while working in one
repository, is readable from every other one without being moved by hand.
**Done.**

## Phase 9 — The production cutover that had never happened — done

Every phase above described the Rust binary as finished. It had never run in
production. The server answering `recall.pimlabs.id` was still the **Node**
implementation, deployed 2026-08-13 and untouched since — the one Phase 7
deleted from the tree. So the frozen schema, the frozen JSON, and
`fixtures/node-written.db` were not precautions any more; they were the
thing standing between a month of memory and a bad afternoon.

Done 2026-09-14, split into two independently reversible steps rather than one.

- [x] **Node → Rust, ingress unchanged.** The rollback was secured first: the
      Node image tagged out of the way (`up --build` overwrites the tag it
      builds under) and exported to a tarball, because the deployed directory
      was neither a git repository nor a tree containing `crates/` — losing
      that image meant no way back at all.
- [x] **Proven against the real database before switching.** The new server
      was run against a copy of production's own `VACUUM INTO` snapshot, on a
      loopback port, and asked for `/admin/stats`. It read every row. The
      fixture had shown Rust could read *a* Node-written database; this showed
      it could read *this* one.
- [x] **Cloudflare Tunnel → Traefik.** `RECALL_TRUSTED_IP_HEADER` moved from
      `cf-connecting-ip` to `x-real-ip`, and the DNS record from a proxied
      tunnel to a grey-clouded `A` at the host. Grey matters: behind the proxy
      Traefik sees Cloudflare's edge address, so every client shares one
      rate-limit bucket and anyone can exhaust it. Verified after the switch
      that the origin port is unreachable from outside, which is the other
      half of the same property.
- [x] **The whole chain proven from a real client**, not `curl /health`: a
      laptop reading the project's files through Traefik, from the Rust
      server, with the token unchanged from before the migration.

Two things the migration taught that the documentation had wrong. Editing the
placeholders in a tracked compose file leaves the server's clone permanently
dirty and breaks `git pull --ff-only` forever; an untracked override file
(`networks.traefik.name` remaps the network, labels override the host rule)
does the same job and survives every future pull. And `./backups` is relative
to the compose file, so moving the deployment moves the backup directory —
quietly, with the old snapshots still on disk and no longer mounted.

## Found by using it

Three things surfaced by running Recall against a second real client, none of
which is reachable by reading the code. Recorded while the evidence is fresh.
All three have since been fixed; the machine scope was the last, and the
shape it took is at the end of its entry.

- **`recall status` does not read the project's `.claude/settings.json`.**
  `docs/reference/install.md` recommends declaring `RECALL_PROJECT_KEY` there, because
  that is the one place that travels with a repository. But `ClientConfig`
  reads the *process* environment, and Claude Code applies that `env` block to
  the hooks it spawns — not to an interactive shell. So the hooks sync under
  the declared key while `recall status`, typed by hand, reports the derived
  one. A diagnostic command that disagrees with the thing it diagnoses is the
  worst kind of bug, and this one was found within an hour of a real setup.

  **Fixed.** The finding above is left as written and what the fix turned up
  is appended here rather than folded in — a finding rewritten after the fact
  stops being evidence. Two things it surfaced that the finding had not. The
  shell does not merely *disagree* with the settings file, it **loses** to
  it: Claude Code replaces the inherited value rather than deferring to it,
  so a declaration is in force even on a machine whose profile exports
  something else. And `status` was not the only command typed by hand —
  `recall promote` resolved the key the same way, so promoting a note in a
  project that declared its key would have pushed the move into the derived
  key's history instead. Both of them, and the hooks, now resolve through one
  path.

- **There is no machine scope.** A directory used for general work accumulated
  ten memory files, every one of them a fact about *the machine*: how much RAM
  it has, which container runtime is installed, which JDK, which of two
  conflicting `dotnet` installs wins. Recall has exactly two scopes — this
  repository, and everywhere. The global scope is actively wrong for this
  content: "the machine has only 8 GB" is false on the next machine, and
  memory that is confidently wrong is worse than no memory. It was filed under
  a declared `project_key` naming the machine, which works and is a disguise.
  Claude Code already labels memories `type: user` in their front matter, so
  the distinction exists upstream; only Recall is missing it.

  **Fixed**, as a third scope mirroring the global one: `machine/` on disk,
  keyed `machine:<RECALL_MACHINE_KEY>`, off unless that variable is set.

  Deciding it turned up something the finding above had not. The
  `RECALL_PROJECT_KEY` workaround is not only a disguise — in a directory of
  general work with no repository it is the *right* answer, because there is
  no project scope for it to be standing in for. The gap it cannot cover is
  narrower and more specific: machine facts **alongside** project memory,
  inside a real repository, where the workaround forces a choice between
  them.

  Opt-in for a different reason than the global scope. Global is opt-in
  because sharing more than someone expected is rude; this is opt-in because
  a machine that has not said which machine it is must not receive another
  one's facts. That also makes the ephemeral cloud session correct by
  default: it is a new machine every time, it declares no key, and it gets no
  machine scope — rather than inheriting a laptop's RAM.

  One thing it shipped broken, found by asking the next obvious question
  rather than by a test: `index.rs` knew only `global/`, so machine files
  synced into a directory `MEMORY.md` never linked — and Claude Code reads
  what that file links. The scope worked in every mechanical sense and the
  memory was inert. The index is now driven off the same `RESERVED_DIRS` list
  as the router, and `recall status` reports link state per scope so the same
  gap cannot be invisible twice.

  Proven against a real server rather than in unit tests alone: with
  `RECALL_MACHINE_KEY=mbp` the three files split cleanly across
  `acme/app`, `global:eko` and `machine:mbp`; a second machine declaring
  `vps` sees an empty machine scope, so "this machine has 8 GB" does not
  reach it; and with no key declared the directory is skipped rather than
  swept into the project's history. Running it also showed `backfill`
  advising `RECALL_GLOBAL_KEY` for a path under `machine/`, which no test
  would have caught because the wrong advice is still a refusal.
- **There is no first sync.** `push` sends the file that triggered the hook
  and tombstones for files that vanished. Nothing else. A memory directory
  that already holds files when Recall arrives keeps holding them: each one
  reaches the server only if Claude happens to edit it, and a `touch` from the
  shell fires no hook at all. Backfilling those ten files meant `POST /sync`
  by hand, one `curl` per file. This is the concrete form of the
  export/import idea, and the reason it is worth more than convenience: right
  now Recall can only protect memory written after it was installed.

  **Fixed** — `recall backfill`, with the finding above left as written. What
  building it turned up, which the finding had not: the naive shape is
  dangerous. `POST /sync` overwrites in place, comparing no timestamps and
  returning no conflict, so "send everything on disk" is safe only on the
  very first machine — anywhere else it deletes whatever another machine
  pushed more recently. So the command asks what the server holds before it
  sends anything, and a file the server has with different bytes is left
  alone and reported rather than overwritten. Two smaller things fell out of
  the same question: a file the server has tombstoned must not be re-sent
  either, because that undoes a delete rather than filling a gap; and because
  every file is one request against a 60-per-minute budget shared with the
  session's own hooks, the run stops at a refusal about the *run* instead of
  pushing through. It resumes by construction — what was sent is on the
  server, and the next run finds it there.

  Reviewing it surfaced two more, both of which are now guarded: a refusal
  about one *file* must not end the run, or a single name the validator
  rejects makes everything sorted after it permanently unsendable; and a run
  that stops early must not write the delete baseline, because a baseline is
  a claim that this disk and the server have been compared and half a
  comparison is not one.

## Found by reviewing it

Not from using Recall but from reading it — the counterpart to the section
above, and worth keeping apart from it, because "nobody could have found this
without running it" is the whole claim those three make.

- **`route()` matches the global directory case-sensitively.** Found while
  reviewing the backfill, and left undecided rather than fixed in passing.
  `scope.rs` compares `rel` against the literal `global`, so a directory
  named `Global/` or `GLOBAL/` routes to the *project* scope — with global
  sync on or off. On a case-insensitive filesystem, which is the macOS
  default, that directory **is** the global directory as far as the user and
  Claude Code are concerned. The consequence is the one the scope guard
  exists to prevent: personal notes filed into one repository's history. It
  has always been reachable through `push`, one file at a time; `backfill` is
  the first thing that would sweep a whole directory that way. Fixing it
  means a case-insensitive prefix match in `route()` and its inverse
  `local_path`, which is the same surface as the `route()` guard already
  noted above — so the two belong together.

  **Fixed**, though not the way the last sentence above guesses, and the
  divergence is the interesting part. A case-insensitive match cannot be
  right: `local_path` *generates* a path and has no case to be insensitive
  about, and the correct destination genuinely differs by filesystem. On
  macOS `Global/` **is** the global directory, so routing it there is right.
  On Linux — where the cloud sessions run — it is a different directory, so
  routing it there files a project's notes into the user's every project.
  Recall cannot see which filesystem a path came from.

  So `route()` refuses instead: a first segment that matches `global`
  case-insensitively but not exactly belongs to no scope. That is correct on
  both, and it is the direction that can be walked back — a file not synced
  is fixed by renaming a directory, while a file synced to the wrong scope is
  the one-way door this guard exists for.

  Refusing silently would only have traded one invisible bug for another, so
  the refusal is visible: `backfill` names the file and the fix per line, and
  `recall status` reports the directory. Proven end to end rather than by unit
  test alone — against a local server, the unguarded binary put
  `Global/notes.md` into `acme/app`'s history, and the guarded one sent 2 of 3
  files and said why the third stayed. `globalish/` still syncs, because the
  reserved name is the directory name and not a word that resembles it.

## Found by releasing it

- **Seven tests could not pass on macOS, and CI never knew.** Found by running
  `./scripts/release.sh v0.2.0` for real — step 3 stopped at `cargo test`
  with seven failures in `crates/recall/tests/cli.rs`, every one of them a
  path compared against a path.

  macOS puts the per-user temporary directory under `/var`, which is a
  symlink to `/private/var`. `recall` finds its project root with `git
  rev-parse --show-toplevel`, and git resolves symlinks — so the fixtures
  built expectations from `/var/folders/…` while every path the binary
  printed said `/private/var/folders/…`. Not a product bug: the binary was
  reporting the path Claude Code will use.

  What makes it worth recording is not the symlink. It is that the suite was
  green on every run anyone had done — the Linux runner's `/tmp` is a real
  directory, so both spellings agree there and the assertions passed without
  ever being *tested*. A whole class of assertion was inert on the only
  machine that ran it, and the first thing to notice was the release.

  **Fixed**, and the reproduction is the useful part: setting `TMPDIR` to a
  symlink on Linux fails exactly the same seven tests, by name. So the fixture
  resolves the path once, where it is created, and CI now runs the suite a
  second time behind a symlinked `TMPDIR` — a runner that cannot see a class
  of failure is a runner that will let it back in.

## Designed, not yet built (2026-09-22)

Everything above this line is done. These came out of a long session spent
using Recall rather than building it, and they are written down because the
reasoning is the expensive part — the code is not. Ordered by leverage as
judged on the day, which the entries themselves explain.

Two lenses were stated for everything below: **it has to be easy for someone
arriving new**, and **it has to stay cheap**.

- [ ] **The client token sits in a shell profile, in plain text, in the
      environment.** `RECALL_TOKEN` is read through the settings/shell layers
      and nothing else, so the only places to put it are a dotfile or a
      settings file. The exposure is not "plain text on disk" — it is three
      specific paths. Being an environment variable, it is inherited by every
      subprocess, including the postinstall script of any package in any
      project you touch. `~/.zshrc` is among the most-committed files in
      existence, and dotfiles repositories are public. And `export
      RECALL_TOKEN=…` typed once stays in `.zsh_history` forever. The blast
      radius is total: one token, read-write across every `project_key` and
      every scope, no expiry and no partial revoke — revoking means
      re-provisioning every client. `install.md` already concedes the last
      mile of this, telling you to gitignore `.claude/settings.local.json` and
      admitting "nothing does that for you".

      The design follows `cargo login`, whose shape fits: one file provider
      that always works, a named source, and room for a keychain provider
      later without touching callers. Cargo's *machinery* — configurable
      provider lists, a subprocess protocol — is for many registries and many
      organisations, and is not worth copying for one owner and one server.

      `recall connect <url>` reads the token from the TTY without echoing it,
      verifies it against `/health` **and** an authenticated call, and only
      then writes. Verify-then-write is the same rule the off-box backup stamp
      follows: the artefact means "this worked", not "we got this far". A
      wrong token leaves nothing behind and you find out immediately.

      Storage is `~/.recall/credentials.json`, mode `0600`, written
      atomically — temp file in the same directory, permissions set at
      creation rather than after, then rename, so there is no window where it
      is readable by anyone. `~/.recall` rather than `~/.config/recall`
      follows cargo, npm, docker, aws and kubectl; `gh` is the only common
      tool using XDG. `RECALL_HOME` overrides it, mirroring `CARGO_HOME`, and
      is what lets tests avoid the real home directory. The file carries a
      `version` field, cheap now and the difference between migrating and
      guessing later.

      Keyed per server URL, following npm's per-registry `_authToken`, so
      connecting to a second server cannot silently reuse the first one's
      token. **It must be normalised on both write and read**: `https://x.id`,
      `https://x.id/` and a trailing space are otherwise three keys, and the
      resulting "no token" is indistinguishable from "wrong token".

      The file sits **below** the shell in precedence, so an explicit
      `RECALL_TOKEN` still wins. That is not a compromise — a cloud
      environment's variables *are* a secrets store, and so is CI's. The env
      var is right where something else holds the secret properly; the file is
      right where the alternative is a human pasting into a dotfile. Cargo and
      npm both work this way (`CARGO_REGISTRY_TOKEN`, `${NPM_TOKEN}`).

      Four things found while reviewing the design, all of which bite if left:
      (1) the token now depends on the URL being resolved first, a new
      ordering coupling in `ClientConfig::from_lookup` that needs a test
      pinning it rather than a comment; (2) URL normalisation, above;
      (3) `recall disconnect` **cannot** see which file exported a shell
      variable — it sees only the effective value — so its report has to say
      "your shell supplied one, check your profile" rather than naming a file
      it is guessing at, or it manufactures exactly the false safety it exists
      to prevent; (4) `0600` means nothing on Windows, which is deferred
      anyway, but the code should say so rather than imply a guarantee.

      Migration is by invitation, not force. The env var keeps working
      permanently. `recall doctor` warns when the token came from the shell
      **and** this is not a remote session — gated on `CLAUDE_CODE_REMOTE`,
      the signal the SessionStart hook and doctor's memory-dir check already
      use — so it speaks once on a laptop and never in a cloud environment,
      where the env var is correct. `recall connect` refuses outright in a
      cloud session: the container is ephemeral, so a credentials file written
      there evaporates, and appearing to succeed is worse than declining.

      Decided: `connect` does **not** also run `init` — one command, one job,
      and doctor already tells you to run `init`. Reading the token from stdin
      for automation waits until something needs it, because every extra way
      in is another way out. A `version` field is in from the start.

      Deliberately not copied: `gh auth token`, a command that prints the
      secret to stdout. `gh` has it because git's credential helper needs it;
      Recall has no such consumer, and a command that puts a secret in
      scrollback is a new leak path for no gain.

- [ ] **Deploy builds the server image on the machine serving traffic, and CI
      builds the same image first and throws it away.** The evidence is one
      run: `ci · Server image builds — 2m38s`, then `deploy · Deploy over SSH
      — 3m25s`, the second rebuilding from scratch what the first had just
      produced. Publishing it from CI and having the VPS pull a tag is not a
      new feature; it is stopping work already done from being discarded. It
      also removes the Rust toolchain and the repository clone from the
      production box entirely.

      **Not before 0.3.0.** Publishing an image adds a step to the release
      flow, and that flow has only ever been dry-run tested since the resume
      logic was added — 0.2.0 failed partway through, twice. Adding surface to
      a path that has never survived a real run is the wrong order.

- [ ] **Setting up the off-box backup is eight manual steps and a trap.**
      Two `rclone config` invocations, a crypt password that must be stored
      outside the machine before anything else happens, a write-read test, a
      first copy, and a crontab line — where the rclone config and the crontab
      are both per-user, so doing them as different users leaves a job that
      fails every night into a mailbox nobody reads.

      `recall backup init` should walk it: ask for the provider and
      credentials, create both remotes, generate and display the crypt
      password with the warning that a copy you cannot decrypt is not a copy,
      run the encrypted round-trip test, do the first copy, and install the
      cron line for the user it is actually running as.

      **The uploader stays outside the server**, and this was reconsidered and
      rejected rather than assumed. Moving it in would put the bucket
      credentials into `deploy/.env`, which lands directly in the server
      container's environment — so compromising the internet-facing process
      would compromise the backups, which is the same shape as the three
      separations this project already enforces deliberately. It would also
      make the backup path depend on the health of the software it backs up:
      cron, bash and rclone keep working when the server will not start. The
      problem is the *setup*, so the fix guides the setup and leaves the
      runtime where it is. If a platform without cron ever matters, the answer
      is a sidecar container with its own `env_file`, not code in the server.

- [ ] **The admin surface can read but not write, and the write path is forty
      lines of hand-written SQL.** `GET /admin` serves a page that asks for a
      token and fetches `/admin/stats`; sqlite-web mounts the volume read-only
      on `127.0.0.1`. Both are read-only **on purpose** — `ARCHITECTURE.md`
      names the absence of an admin write surface as a security property, so
      that a leaked token cannot destroy history through any route the server
      exposes.

      The cost is that removing or renaming a project key means the procedure
      in `deploy/README.md`: back up, stop the server, hand-edit the only copy
      of your memory with `sqlite3`, minding that the primary key is
      `(project_key, file_path)` so a rename onto an occupied key collides.
      Performed rarely, and always at a moment when something is already
      wrong.

      A write surface must not go on the public listener. The pattern is
      already in this repository and already proven: bind it to `127.0.0.1`
      the way sqlite-web is bound, reached over an SSH tunnel. Then a leaked
      bearer token still cannot touch it, because the route is not on the
      listener it can reach. The commands — list, rename, remove, restore —
      would each do what the README currently asks a human to remember: take
      the backup first, run inside a transaction, check `changes()` before
      committing, and refuse a rename that would collide.

- [ ] **Owning a VPS is the real barrier, and every other idea here saves
      minutes.** Parked deliberately, with the shape recorded so the next
      discussion does not restart from zero.

      Keystatic was examined as a model and does not fit, for a structural
      reason rather than a matter of taste. Keystatic Cloud is a
      pre-configured GitHub App: it answers "which of these editors are you?"
      against storage that already has identity and per-user access control.
      Its free tier is priced at three users because its value is multiple
      editors. Recall stores to one owner's SQLite on one owner's VPS; the
      token does not prove *who* you are, it proves the request is allowed at
      all. There is no population to distinguish — and there is no "Recall
      Cloud" to borrow, so adopting the model means building and operating the
      hosted half.

      There is a real ladder underneath the instinct, which is about
      distributing credentials rather than establishing identity:
      (a) `recall connect`, above — one paste, verified, into a mode-0600 file;
      (b) **per-device tokens issued by the server** — buys per-device
      revocation, which does not exist today at any price, and stays single
      owner; (c) OIDC or a hosted broker — browser login and short-lived
      credentials, at the cost of a third party in the auth path, JWT and JWKS
      validation inside the half of the system the crate split exists to keep
      small, and a component that must stay online.

      (c) is not a violation of the single-owner rule: one owner authenticated
      through an external provider is still one owner, with no signup, no
      billing and nobody else's data. But the deferred entry below notes that
      real per-user isolation needs the auth rewritten rather than extended,
      and (c) *is* that rewrite performed for a different reason. It is the
      doorway, so it stays behind the same question as the entry below.

      When it is picked up, the question to ask is not "OIDC or tokens". It is:
      what is the cheapest thing that removes the need to own a VPS, without
      putting anyone else's memory in your hands?

- [ ] **Recall moves memory faithfully and has no idea whether any of it is
      still true.** It is transport. Transport is invisible when it works and
      replaceable when someone ships it natively, and its ceiling is set by
      the quality of what it carries rather than by anything Recall does.

      The evidence is this project's own memory, read on 2026-09-22 against
      the live system. `project_phase1_deploy.md` — written 2026-08-12, synced
      perfectly ever since, and read by Claude at every session start —
      announces a server "live at `recall.pimlabs.id`, deployed via OrbStack +
      Cloudflare Tunnel on the owner's Mac". Production is Traefik on a VPS,
      answering at `recall-server.pimlabs.id`. It names `lib.sh` and
      `hooks/recall-pull`, neither of which exists since the Rust rewrite. It
      describes a trade-off — "only runs while this Mac is up; a VPS is the
      fallback" — that was resolved weeks ago. Nothing in the system noticed,
      because nothing in the system is looking.

      Three findings from that review matter more than the list of errors:

      **A stale wrapper buries the true parts inside it.** That same file
      carries, in its last paragraph, `CLAUDE_CODE_REMOTE_MEMORY_DIR=
      /home/user/.claude`, `$HOME` there is `/root`. That is the one value in
      the whole setup that cannot be reasoned out — the one this repository
      spent a day rediscovering, and wrote `recall doctor` and two
      documentation fixes to surface. It was in memory the entire time. Nobody
      read it, because the file's own heading announces a deployment that no
      longer exists.

      **Memory that says "do not redo this" gets redone anyway.**
      `project_saas_idea_shelved.md` ends with "don't restart the
      SaaS/multi-tenant conversation from scratch next time it comes up — this
      reasoning already happened", followed by the three costs. Hours before
      this entry was written, that conversation restarted from scratch and
      re-derived the same three costs in the same order. The memory was
      correct; it simply was not in front of anyone at the moment it applied.

      **Three of the four files are mostly right.** The errors are one or two
      claims inside a file that otherwise still holds. A tool that offers
      "keep or discard" will discard true things, so the unit of review has to
      be the *claim*, not the file — show the line that no longer holds, with
      the evidence, and leave the editing to a human.

      The design follows `doctor`'s discipline: evidence rather than verdicts,
      silence where there is nothing to say. Layered, cheapest first.
      **(1) Dead artefact references** — `PROMPT.md`, `lib.sh`,
      `hooks/recall-pull` are each one `[ -e ]` away, and three of the four
      files named one. This narrows five files to three lines for free, and
      that is all it does: it is a **candidate filter, not a verdict**. Tested
      immediately after the four files were corrected, it fired three times
      and every hit was a false positive — because a memory that correctly
      records something as dead has to name the dead thing. "Not the old
      `hooks/recall-*` scripts, and not `lib.sh`; both were deleted in the
      Rust rewrite" is the most useful sentence in that file and trips the
      check. **(2) Claims against current truth** — `GET /health` knows the
      hostname and commit, the running compose file knows the ingress,
      `recall doctor --json` knows where the token lives. Narrow, mechanical,
      certain. **(3) Everything else** — the local `claude` CLI, under the
      same no-API-key rule as merge, and only over files that survive the
      first two layers.

      Term matching was tried first and is the wrong primitive: it measures
      whether a *word* still appears, not whether a *claim* still holds. Of
      four terms tested, one was a true positive, one hit for the wrong reason
      (the term was never in the repository), and two were missed — including
      `recall.pimlabs.id`, which is alive and correct but now names the
      install URL rather than the sync server.

      **This runs on the client, and the crate split decides that**, not
      preference: `recall-server` depends on `recall-wire` alone and cannot
      read a repository or a memory directory. It is a command rather than a
      hook — a model pass over every memory file at every session start would
      be slow, expensive, and would fail quietly — and incremental, checking
      only what changed since the last review.

      That failure exposed a distinction the design needs and did not have:
      **a claim about the present can go stale; a record of what changed
      cannot.** "The server is at X" expires. "The server moved from X to Y,
      and X now serves something else" stays true forever and is exactly what
      stops the confusion recurring — the corrected file keeps its history
      paragraph deliberately. A checker that pushes toward deleting those
      makes memory worse, which is the opposite of the point.

      The report must name what is **still true** as well as what is not.
      The first finding above is why: the danger is not only believing a
      false claim, it is discarding a true one alongside it.

## Explicitly deferred

- **Multi-user / a hosted "Recall as a service for others" product.** Raised and discussed 2026-08-12, shelved: use Recall personally for a while first to get real signal before committing to this. The technical shape is already mapped out if it comes back — it needs deciding on demand, not feasibility:
  - The VPS-hosting requirement is real friction, but a SaaS trades it for a different one (trusting a third party with potentially sensitive memory content), not eliminating friction outright.
  - Current auth (one shared bearer token, readable across every `project_key`) would need a full rewrite for real per-user isolation, not an extension.
  - Phase 2's planned merge shells out to a locally-logged-in `claude` CLI, which doesn't scale to many users' merges and directly conflicts with the no-API-key rule in `CLAUDE.md` — a SaaS needs a different answer to this specifically, independent of the auth question.
  - Cross-device project-path differences (raised as a concern, turned out already solved) are *not* a blocker: `project_key` derives from the git remote, not the local checkout path, proven live during the Phase 1 cloud test. A project with no git remote at all used to be the one real gap — it falls back to a path-based key that won't agree across machines — but Phase 8's `RECALL_PROJECT_KEY` closes it.
- Syncing anything other than auto memory (`CLAUDE.md`, skills, settings, sessions — leave those to git, or to not existing as a problem in the first place).
- Real-time collaborative editing between two humans.
- A GUI. This is a backend + a couple of hook scripts.
- Supporting Windows without WSL, unless it turns out to be trivial.
