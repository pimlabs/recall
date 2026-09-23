# @pimlabs/recall

Sync [Claude Code](https://claude.com/claude-code)'s auto memory — the notes
Claude writes about a project as it works — across your machines and into
ephemeral cloud sessions, with no device pairing.

```sh
npm install -g @pimlabs/recall
```

This package downloads the prebuilt `recall` binary for your platform
(macOS and Linux, x64 and arm64) and verifies it against the release's
checksums. There's no Node dependency at runtime — the binary is Rust.

Then, from inside a project you want synced:

```sh
recall connect https://your-recall-host    # the token, this machine's name, and this project
git add .claude/settings.json && git commit -m "Enable Recall memory sync"
recall doctor
```

`connect` checks the token before saving it, and skips whatever is already
done, so it is safe to run again. Each further project:

```sh
recall init
git add .claude/settings.json && git commit -m "Enable Recall memory sync"
recall backfill
```

Notes about *you* rather than about one repository can follow you into every
project: set `RECALL_GLOBAL_KEY`, then `recall promote <file>` moves one
there. Notes true of one box and wrong on the next — its RAM, which of two
`dotnet` installs wins — get a scope of their own instead, so they never
reach a machine they would be false on: name the machine in
`~/.recall/config.toml`, and `recall promote <file> --to machine`.

The server is a separate binary, `recall-server`, usually run from the
Docker setup in the repository below. This package installs only the client.

Full documentation, including standing up the server:
https://github.com/pimlabs/recall
