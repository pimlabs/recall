# Homebrew formula for the Recall CLI.
#
# This repo isn't named homebrew-recall, so the tap needs its URL given
# explicitly (Homebrew only infers the homebrew-* naming convention):
#
#   brew tap pimlabs/recall https://github.com/pimlabs/recall
#   brew install --HEAD pimlabs/recall/recall
#
# HEAD-only on purpose: this project deploys straight from main and
# deliberately has no tagged-release process (see ROADMAP.md Phase 4 on
# why formal semver wasn't worth it for a single-owner tool). Adding a
# stable `url`/`sha256` here would mean maintaining tagged releases just
# to satisfy the formula.
class Recall < Formula
  desc "Sync Claude Code's auto memory across machines and cloud sessions"
  homepage "https://github.com/pimlabs/recall"
  head "https://github.com/pimlabs/recall.git", branch: "main"
  license "MIT"

  depends_on "curl"
  depends_on "jq"

  def install
    # bin/recall resolves hooks/ relative to its own real path, so the two
    # have to stay siblings — hence libexec rather than a bare bin.install.
    libexec.install "bin", "hooks"
    bin.install_symlink libexec/"bin/recall"
  end

  test do
    assert_match "recall", shell_output("#{bin}/recall version")
  end
end
