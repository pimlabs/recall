# @pimlabs/recall

Sync [Claude Code](https://claude.com/claude-code)'s auto memory — the notes
Claude writes about a project as it works — across your machines and into
ephemeral cloud sessions, with no device pairing.

```sh
npm install -g @pimlabs/recall
```

This package downloads the prebuilt `recall` binary for your platform
(macOS and Linux, x64 and arm64) and verifies it against the release's
checksums. There's no Node dependency at runtime — the binary is Go.

Then, once per machine:

```sh
export RECALL_URL="https://your-recall-host"
export RECALL_TOKEN="<your token>"
```

And once per project you want synced:

```sh
recall init
git add .claude/settings.json && git commit -m "Enable Recall memory sync"
recall status
```

`recall serve` runs the server side — the same binary, self-hosted.

Full documentation, including standing up the server:
https://github.com/pimlabs/recall
