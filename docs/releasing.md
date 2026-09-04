# Cutting a release

Four channels ship the same binary, and they do **not** all update
themselves. Tagging publishes the GitHub Release; npm and crates.io each need
a credentialed push that only the owner can make.

Order matters, because three of the four depend on the release existing.

---

## 0. Before the tag

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

cargo build --release
./scripts/compat-check.sh target/release/recall     # 11 checks
./scripts/api-doc-check.sh target/release/recall    # 27 checks
```

Confirm the version is the same in all three places — they are not linked, so
a mismatch ships a broken npm package (its `postinstall` looks for a release
named after *its own* version):

```sh
grep '^version' Cargo.toml           # workspace.package.version
grep '"version"' npm/package.json
grep 'refs/tags' Formula/recall.rb
```

## 1. Tag

```sh
git tag v0.1.0
git push origin v0.1.0
```

`.github/workflows/release.yml` fires on `v*`. It builds four targets, each on
a **native runner** — `rusqlite` compiles SQLite from C, so cross-compiling
would need a C toolchain per target — then writes `checksums.txt` and creates
the GitHub Release:

| Asset | Runner |
|---|---|
| `recall_darwin_amd64.tar.gz` | `macos-13` |
| `recall_darwin_arm64.tar.gz` | `macos-14` |
| `recall_linux_amd64.tar.gz` | `ubuntu-latest` |
| `recall_linux_arm64.tar.gz` | `ubuntu-24.04-arm` |

Those names are a contract: `install.sh` and `npm/install.js` both construct
them from `uname` / `process.platform`. Don't rename them without changing
both.

**The moment this finishes, `install.sh` works.** Verify it against the real
thing rather than assuming:

```sh
curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
recall version
```

## 2. Homebrew

The formula builds from a source tarball, so it needs that tarball's hash:

```sh
curl -fsSL https://github.com/pimlabs/recall/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256
```

Put the result in `Formula/recall.rb`'s `sha256`, and update the `url` to the
new tag. Then:

```sh
brew tap pimlabs/recall https://github.com/pimlabs/recall
brew install pimlabs/recall/recall
```

`brew install --HEAD` keeps working between releases and needs none of this.

## 3. npm

```sh
cd npm
npm publish --access public
```

Scoped packages default to private, hence `--access public`. Verify from a
clean machine — the interesting part is the `postinstall` download, which
never runs during `npm publish`:

```sh
npm install -g @pimlabs/recall && recall version
bun install -g @pimlabs/recall  && recall version
```

If `npm/package.json`'s version doesn't match a released tag, the install
fails with a clear message pointing at cargo and Homebrew — by design, since
the alternative is a half-installed package.

## 4. crates.io

Five crates, published **bottom-up**. Each one has to be on the index before
anything that depends on it can be packaged, so the order is not optional:

On a **first** publish, check the names are still free before you start —
crate names are global and first-come, and a half-published set is awkward to
back out of:

```sh
for n in recall-wire recall-paths recall-hooks recall-server recall-sync; do
  a=$(echo "$n" | cut -c1-2); b=$(echo "$n" | cut -c3-4)
  printf '%-16s %s\n' "$n" \
    "$(curl -s -o /dev/null -w '%{http_code}' "https://index.crates.io/$a/$b/$n")"
done
# 404 = available, 200 = taken
```

This is not hypothetical: the binary crate was called `recall-cli` until that
check found the name already belonged to an unrelated project — a TUI session
browser for AI coding assistants, in almost exactly this space. `recall` is
taken too. Hence `recall-sync`, publishing a binary still called `recall`.

Then, bottom-up:

```sh
cargo publish -p recall-wire
cargo publish -p recall-paths
cargo publish -p recall-hooks
cargo publish -p recall-server
cargo publish -p recall-sync
```

Wait for each to land before the next — the index takes a few seconds, and
`cargo publish` will fail with "no matching package named …" if you get ahead
of it. Then:

```sh
cargo install recall-sync
```

**crates.io is permanent.** A published version can be yanked but never
deleted, and the crate names are claimed for good. That is the reason this
step is deliberate rather than automated.

## 5. Afterwards

- `recall version` on a freshly installed binary should print the new version
  and the commit it was built from.
- Cutting over the production server is separate — see `deploy/README.md`, and
  run `./scripts/compat-check.sh` before and after.
