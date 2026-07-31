# Phase 0 findings

`ARCHITECTURE.md` and `ROADMAP.md` flagged several things to confirm
empirically rather than assume. This is what was actually found, checking
against the installed Claude Code CLI (`@anthropic-ai/claude-code` v2.1.42,
`claude --version` 2.1.220) — both by reading its bundled source and by
running it live against test hook configurations.

## 1. `FileChanged` does not exist as a hook event

The installed CLI's hook event enum is: `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `Notification`, `UserPromptSubmit`, `Stop`,
`SubagentStop`, `SubagentStart`, `PreCompact`, `SessionStart`, `SessionEnd`,
`PermissionRequest`, `Setup`. No `FileChanged`. `ARCHITECTURE.md`'s primary
proposal doesn't apply — go straight to the documented fallback.

## 2. Hook type `"http"` does not exist either

Valid hook `type` values in the current schema: `"command"` and `"prompt"`.
No `"http"`. The push hook has to be a `"command"` hook that does its own
HTTP call (via `curl`), not a declarative HTTP hook.

## 3. Memory files are written via the plain `Write`/`Edit` tools

`MEMORY.md` and topic files are read and written through Claude Code's
standard `Read`/`Write`/`Edit` tools — there's no dedicated "memory" tool
with its own `tool_name`. This makes `PostToolUse` with `matcher:
"Edit|Write"` the right hook.

## 4. The matcher only matches tool name, not path — and that's fine

The hook `matcher` field is a plain string matched against `tool_name`
only; there's no built-in path-glob filtering. So the push hook script
itself inspects `tool_input.file_path` from the JSON payload on stdin and
decides whether it's a memory file.

**This directly answers the open question from `ARCHITECTURE.md`/
`ROADMAP.md`: does the push hook catch dynamically-named topic files?**
Confirmed live: a real `claude -p` run configured with this exact
`PostToolUse`/`Edit|Write` hook fired for both a pre-known `MEMORY.md`
write and a `debugging.md` file Claude named on the fly in the same turn,
producing identical-shaped JSON for both:

```json
{"hook_event_name":"PostToolUse","tool_name":"Write",
 "tool_input":{"file_path":"/…/fake-memory/debugging.md","content":"…"},
 "tool_response":{...}, ...}
```

Because the filtering happens in the script against the actual file path
per call — not via a filename pattern registered in advance — every new
topic file is caught the moment it's written, with zero special-casing.

## 5. Auto memory is off by default in remote/cloud sessions

This is the one that isn't in the docs at all, and matters most for
Recall's actual use case. From the CLI source (paraphrased):

```
auto_memory_enabled():
  if CLAUDE_CODE_DISABLE_AUTO_MEMORY is set → false
  if CLAUDE_CODE_REMOTE is set AND CLAUDE_CODE_REMOTE_MEMORY_DIR is NOT set → false
  else → settings.autoMemoryEnabled ?? default
```

Every ephemeral cloud session sets `CLAUDE_CODE_REMOTE=true`. Confirmed
directly against *this very session*: `CLAUDE_CODE_REMOTE=true`,
`CLAUDE_CODE_REMOTE_MEMORY_DIR` unset, and — consistent with the code —
`~/.claude/projects/-home-user-recall/` has no `memory/` subdirectory at
all. Auto memory was never turned on.

Practical consequence: `recall-pull` writing files to disk is necessary
but not sufficient. Without `CLAUDE_CODE_REMOTE_MEMORY_DIR` set on the
remote environment, Claude Code won't be looking at that directory (or any
memory directory) in the first place. See `hooks/README.md` for why this
has to be an environment secret, not something baked into the committed
`settings.json` (short version: settings `env` values are literal strings,
no `$HOME` expansion — confirmed by live-testing `"$HOME/x"` and
`"${HOME}/x"` in a real settings.json and getting back the literal
unexpanded string both times).

## 6. Project directory naming: local path, not git remote

`ARCHITECTURE.md` assumed Claude Code scopes its own memory storage by git
remote. It doesn't — it's the git root (or cwd if no git root) with every
non-alphanumeric character replaced by `-`:

```
slug = project_root.replace(/[^a-zA-Z0-9]/g, "-")
dir  = $CLAUDE_CODE_REMOTE_MEMORY_DIR_or_~/.claude/projects/<slug>/memory
```

Confirmed against this session: cwd `/home/user/recall` →
`~/.claude/projects/-home-user-recall/` exists on disk with exactly that
name.

This matters because it means Claude Code's own scoping is
**machine-local** — a laptop clone at `~/code/recall` and a cloud clone at
`/home/user/recall` get *different* slugs for the same repo. That's
exactly the gap Recall exists to bridge, so it's intentional that Recall's
own `project_key` (git remote `owner/repo`, see `hooks/lib.sh`) uses a
*different* derivation than Claude Code's local slug — they solve two
different problems:

- `project_key` (server-side): must be the *same* across machines →
  derived from the git remote.
- local memory dir (client-side, per hook invocation): must match
  *wherever this specific machine's Claude Code is actually looking* →
  replicate Claude Code's own local-path-slug algorithm.

### The git remote itself isn't always what you'd expect either

In this sandboxed cloud environment, `git remote get-url origin` returns
`http://local_proxy@127.0.0.1:<random-port>/git/pimlabs/recall` — a
locally proxied URL, not the real `git@github.com:pimlabs/recall.git`.
`recall_project_key()` in `hooks/lib.sh` handles this by keying on just the
last two path segments (`owner/repo`), which normalizes identically across
SSH, HTTPS, and this proxied form. Verified directly:

| Input | Derived key |
|---|---|
| `http://local_proxy@127.0.0.1:41729/git/pimlabs/recall` | `pimlabs/recall` |
| `git@github.com:pimlabs/recall.git` | `pimlabs/recall` |
| `https://github.com/pimlabs/recall.git` | `pimlabs/recall` |

Known limitation: hosts with nested groups (e.g. GitLab subgroups) collapse
to their last two segments too, which can collide. Acceptable for Phase 0;
worth revisiting in Phase 2 if it ever comes up for real.

## Round-trip proof (Phase 0 "done when")

Simulated two machines against one server instance (SQLite, last-write-wins,
no merge — as scoped for Phase 0):

1. "Machine A" (`git@github.com:pimlabs/recall.git`-style remote): wrote
   `MEMORY.md` and a dynamically-named `debugging.md` under its local
   memory dir, fed the same PostToolUse JSON shape captured from the live
   test above into `recall-push`.
2. Server stored both, keyed under `project_key = pimlabs/recall`.
3. "Machine B" (different filesystem path, proxied-URL-style remote,
   simulating a fresh cloud clone): ran `recall-pull` with zero prior
   state. It computed the same `project_key`, fetched both files, and
   wrote them into *its own* locally-correct memory dir (different path
   than machine A's, correctly slugged for machine B).

Both files matched byte-for-byte between machine A's originals and machine
B's pulled copies. This is the mechanism-level proof; the remaining piece
(starting an actual second ephemeral cloud session against a real deployed
server) is an operational step for whoever deploys this, not something a
single session can self-verify further.
