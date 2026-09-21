# recall-server

An internal crate of [Recall](https://github.com/pimlabs/recall), self-hosted
sync for Claude Code's auto memory. If you arrived here looking for something
to run, you want [`recall`](https://crates.io/crates/recall) —
`cargo install recall`, then `recall serve` — or the repository above, which
carries the deployment setup.

It is published under its own name because the split is a compile-time guard,
not an offer of reuse: this crate depends on `recall-wire` and nothing else, so
the half that faces the internet cannot reach `recall-hooks`, the half that
reads `~/.claude`. Writing `use recall_hooks::…` here is `error[E0433]`, not a
review comment.

What this crate contains is on [docs.rs](https://docs.rs/recall-server), where
it is generated from the code. Repeating any of it here would only give it
somewhere to go stale.
