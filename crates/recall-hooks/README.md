# recall-hooks

An internal crate of [Recall](https://github.com/pimlabs/recall), self-hosted
sync for Claude Code's auto memory. If you arrived here looking for something
to install or use, you want [`recall`](https://crates.io/crates/recall) —
`cargo install recall` — or the repository above.

It is published under its own name because the split is a compile-time guard,
not an offer of reuse: this is the half that reads `~/.claude`, and
`recall-server` — the half that faces the internet — depends on `recall-wire`
alone, so it cannot reach this code even by accident. Writing
`use recall_hooks::…` inside `recall-server` is `error[E0433]`, not a review
comment.

What this crate contains is on [docs.rs](https://docs.rs/recall-hooks), where it
is generated from the code. Repeating any of it here would only give it
somewhere to go stale.
