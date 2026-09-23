# recall-server

The server of [Recall](https://github.com/pimlabs/recall), self-hosted sync
for Claude Code's auto memory. `cargo install recall-server` gives you the
`recall-server` binary; the repository above carries the Docker setup most
servers run it from, and each release publishes Linux builds of it. The
client, for your machines, is [`recall`](https://crates.io/crates/recall).

It is published under its own name because the split is a compile-time guard,
not an offer of reuse: this crate depends on `recall-wire` and nothing else, so
the half that faces the internet cannot reach `recall-hooks`, the half that
reads `~/.claude`. Writing `use recall_hooks::…` here is `error[E0433]`, not a
review comment.

What this crate contains is on [docs.rs](https://docs.rs/recall-server), where
it is generated from the code. Repeating any of it here would only give it
somewhere to go stale.
