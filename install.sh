#!/usr/bin/env bash
# Recall installer, for machines without npm or Homebrew:
#
#   curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
#
# Downloads the prebuilt binary for this platform from the latest GitHub
# release. Installs to ~/.local/bin/recall; override with RECALL_BIN_DIR,
# or pin a version with RECALL_VERSION=v0.1.0.
set -euo pipefail

REPO="pimlabs/recall"
BIN_DIR="${RECALL_BIN_DIR:-$HOME/.local/bin}"
VERSION="${RECALL_VERSION:-latest}"

die() {
  echo "install: $*" >&2
  exit 1
}

for dep in curl tar uname; do
  command -v "$dep" >/dev/null || die "$dep is required but not installed"
done

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$os" in
  darwin | linux) ;;
  *) die "unsupported OS: $os (macOS and Linux only; Windows needs WSL)" ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64) arch="amd64" ;;
  arm64 | aarch64) arch="arm64" ;;
  *) die "unsupported architecture: $arch" ;;
esac

asset="recall_${os}_${arch}.tar.gz"
if [[ "$VERSION" == "latest" ]]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "install: downloading $asset ($VERSION)..."
if ! curl -fsSL "$url" -o "$tmp/$asset"; then
  die "could not download $url
    If no release exists yet, build from source instead:
      git clone https://github.com/$REPO && cd recall
      cargo build --release -p recall-sync   # binary at target/release/recall
    Or, without a Rust toolchain of your own:
      brew tap pimlabs/recall https://github.com/$REPO
      brew install --HEAD pimlabs/recall/recall"
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BIN_DIR"
install -m 0755 "$tmp/recall_${os}_${arch}" "$BIN_DIR/recall"

echo "install: installed $BIN_DIR/recall"
"$BIN_DIR/recall" version

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "  ! $BIN_DIR isn't on your PATH. Add to your shell profile:"
    echo "      export PATH=\"$BIN_DIR:\$PATH\""
    ;;
esac

cat <<'EOF'

Next: set RECALL_URL and RECALL_TOKEN (see docs/token-setup.md), then run
'recall init' inside a project you want synced.
EOF
