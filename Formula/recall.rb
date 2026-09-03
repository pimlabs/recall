# Homebrew formula for Recall.
#
# This repo isn't named homebrew-recall, so the tap needs its URL given
# explicitly (Homebrew only infers the homebrew-* naming convention):
#
#   brew tap pimlabs/recall https://github.com/pimlabs/recall
#   brew install pimlabs/recall/recall          # latest tagged release
#   brew install --HEAD pimlabs/recall/recall   # straight from main
#
# Built from source rather than pulling a release binary: Homebrew already
# has a Go toolchain available as a build dependency, and building here
# means the formula works against main before any tag exists.
class Recall < Formula
  desc "Sync Claude Code's auto memory across machines and cloud sessions"
  homepage "https://github.com/pimlabs/recall"
  url "https://github.com/pimlabs/recall/archive/refs/tags/v0.1.0.tar.gz"
  # Filled in when v0.1.0 is actually tagged; until then use --HEAD.
  sha256 ""
  license "MIT"
  head "https://github.com/pimlabs/recall.git", branch: "main"

  depends_on "go" => :build

  def install
    ldflags = %W[
      -s -w
      -X main.version=#{version}
      -X main.commit=#{tap&.installed? ? Utils.git_short_head : "unknown"}
    ]
    system "go", "build", *std_go_args(ldflags: ldflags), "./cmd/recall"
  end

  test do
    assert_match "recall", shell_output("#{bin}/recall version")

    # `recall init` must refuse to touch anything outside a git repository —
    # it edits a file the user is expected to commit.
    output = shell_output("#{bin}/recall init 2>&1", 1)
    assert_match "git repository", output
  end
end
