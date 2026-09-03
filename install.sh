#!/usr/bin/env bash
# Recall CLI installer, for machines without npm or Homebrew:
#
#   curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
#
# Installs into ~/.local/share/recall and symlinks ~/.local/bin/recall.
# Override with RECALL_INSTALL_DIR / RECALL_BIN_DIR.
set -euo pipefail

REPO_TARBALL="https://github.com/pimlabs/recall/archive/refs/heads/main.tar.gz"
INSTALL_DIR="${RECALL_INSTALL_DIR:-$HOME/.local/share/recall}"
BIN_DIR="${RECALL_BIN_DIR:-$HOME/.local/bin}"

die() {
  echo "install: $*" >&2
  exit 1
}

for dep in curl tar; do
  command -v "$dep" >/dev/null || die "$dep is required but not installed"
done

# Not fatal — the CLI itself checks for these and explains. But telling
# someone now beats a confusing failure on their first `recall init`.
missing_runtime=()
command -v jq >/dev/null || missing_runtime+=("jq")
command -v bash >/dev/null || missing_runtime+=("bash")

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "install: downloading recall..."
curl -fsSL "$REPO_TARBALL" | tar -xz -C "$tmp" --strip-components=1

# Replace rather than merge, so a reinstall can't leave a stale hook
# script behind next to a newer bin/recall.
rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
cp -R "$tmp/bin" "$tmp/hooks" "$INSTALL_DIR/"
chmod +x "$INSTALL_DIR/bin/recall" "$INSTALL_DIR/hooks/recall-push" "$INSTALL_DIR/hooks/recall-pull"
ln -sf "$INSTALL_DIR/bin/recall" "$BIN_DIR/recall"

echo "install: installed to $INSTALL_DIR"
echo "install: linked $BIN_DIR/recall"

if [[ ${#missing_runtime[@]} -gt 0 ]]; then
  echo
  echo "  ! recall also needs: ${missing_runtime[*]}"
  echo "    install those before running 'recall init' (e.g. brew install jq)"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "  ! $BIN_DIR isn't on your PATH. Add to your shell profile:"
    echo "      export PATH=\"$BIN_DIR:\$PATH\""
    ;;
esac

echo
echo "Next: set RECALL_URL and RECALL_TOKEN (see docs/token-setup.md), then"
echo "run 'recall init' inside a project you want synced."
