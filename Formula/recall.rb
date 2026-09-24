# Homebrew formula for Recall.
#
#   brew install pimlabs/tap/recall          # latest release, a download
#   brew install --HEAD pimlabs/tap/recall   # built from main, between releases
#
# The tap is pimlabs/homebrew-tap, shared with the other pimlabs tools, which
# is what lets Homebrew infer the URL from `pimlabs/tap` — a formula living in
# this repository instead would need `brew tap pimlabs/recall <url>` first,
# since `recall` is not named `homebrew-*`.
#
# THIS FILE IS THE SOURCE. scripts/release.sh rewrites the version and the
# four checksums from the release's own checksums.txt, then copies it into the
# tap. Edit it here; the copy in the tap is output.
#
# The stable install takes a prebuilt archive rather than compiling: the
# release workflow already publishes the same four binaries npm and install.sh
# use, and building them again locally means a Rust toolchain plus SQLite's C
# amalgamation for no difference in the result. `--HEAD` still compiles,
# because there is nothing prebuilt to point it at.
class Recall < Formula
  desc "Sync Claude Code's auto memory across machines and cloud sessions"
  homepage "https://github.com/pimlabs/recall"
  version "0.4.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_arm64.tar.gz"
      sha256 "bc3126e49e548502e5e59ef14854d3d314ca83966f213a27a99b7bce51029dc8"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_amd64.tar.gz"
      sha256 "b67f4e5584b4d296213e3da2f8df6430d7ee2e239a9c28279e86ea8f3098f325"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_arm64.tar.gz"
      sha256 "e004db0fe21c5b5c820e6cdcc979f713d3846c9a8d32e9681d00304720bc14d3"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_amd64.tar.gz"
      sha256 "46d432987c6112232002c25a7c424e0c72805f815170977d2ab08f50f1f65eba"
    end
  end

  head do
    url "https://github.com/pimlabs/recall.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args(path: "crates/recall")
    else
      # Each archive holds a single file named for its platform —
      # recall_darwin_arm64 and so on. install.sh renames it the same way.
      bin.install Dir["recall_*"].first => "recall"
    end
  end

  test do
    assert_match "recall", shell_output("#{bin}/recall version")

    # `recall init` must refuse to touch anything outside a git repository —
    # it edits a file the user is expected to commit.
    output = shell_output("#{bin}/recall init 2>&1", 1)
    assert_match "git repository", output
  end
end
