# Installing and using the Recall CLI

Install once per machine, then run `recall init` once per project. Nothing
gets copied into your projects except a small `.claude/settings.json` diff.

## Install

Pick whichever fits the machine. All three deliver the same thing: a
`recall` command on `PATH`, with the hook scripts kept next to it.

**npm** (or bun/pnpm — any of them can install a global bin):

```sh
npm install -g @pimlabs/recall
```

**Homebrew** — the tap needs its URL given explicitly, since this repo
isn't named `homebrew-recall`:

```sh
brew tap pimlabs/recall https://github.com/pimlabs/recall
brew install --HEAD pimlabs/recall/recall
```

**Plain curl**, for machines with neither:

```sh
curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
```

Installs to `~/.local/share/recall`, symlinks `~/.local/bin/recall`.
Override with `RECALL_INSTALL_DIR` / `RECALL_BIN_DIR`.

### Runtime requirements

`bash`, `curl`, `jq`. Homebrew pulls `jq` and `curl` in automatically; the
other two channels don't, so `recall` checks at runtime and tells you
what's missing. (Why not a single dependency-free binary: see
`ROADMAP.md` Phase 4 — it's a deliberate, revisitable call.)

## Set the environment variables

Once per machine, in your shell profile:

```sh
export RECALL_URL="https://your-recall-host"
export RECALL_TOKEN="<your token>"
```

See `token-setup.md` for generating the token and for what a claude.ai
cloud environment additionally needs.

## Enable sync for a project

From inside the project:

```sh
recall init
```

That merges the hook wiring into the project's own `.claude/settings.json`
— idempotent, and it appends to any hooks already there rather than
replacing them. Then commit it:

```sh
git add .claude/settings.json
git commit -m "Enable Recall memory sync"
```

**The commit is the point, not a formality.** The hook config has to be in
the project's repo for a fresh clone — especially an ephemeral cloud
session that has never seen your machine — to pick sync up with no setup
of its own. That's the whole reason Recall works the way it does; see
`CLAUDE.md`'s ground rules.

## Check it's working

```sh
recall status
```

Reports, for the project you're standing in: the derived `project_key`,
where Claude Code's memory directory actually is on this machine, how many
memory files exist locally, whether the hooks are wired, whether
`RECALL_URL`/`RECALL_TOKEN` are set, whether the server answers, and how
many files it already holds for this project.

## Cloud environments need the CLI too

A claude.ai cloud session runs the hooks from the repo it cloned, but
`recall` itself has to already be on `PATH` in that environment — it isn't
part of the repo. Install it as part of that environment's setup, the same
one-time place you set `RECALL_TOKEN` and `CLAUDE_CODE_REMOTE_MEMORY_DIR`:

```sh
npm install -g @pimlabs/recall
```

This is per-environment, not per-project or per-session. Once it's in the
environment's setup, every session spawned from it has `recall` available.

## What this replaces

Before the CLI, opting a project in meant copying `hooks/` into that
project's repo and hand-merging `hooks/settings.snippet.json`. That still
works and is still supported — see `../hooks/README.md` — but it means
every project carries its own copy of the scripts, and updating them means
updating every project. `recall init` exists so the only per-project
artifact is the settings diff.
