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
  version "0.3.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_arm64.tar.gz"
      sha256 "27dae554ed14b2b38e444001f57384bcf359cac5a53dd664391fc61329a4b424"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_amd64.tar.gz"
      sha256 "2e02145db6b05fc51ce496a9cd52b5d8e0964fda1e1ea10c5aaae59a9954af30"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_arm64.tar.gz"
      sha256 "68957ea9d7dfd7063cc752e10c88f2f5c478fda60f654af2dc369b7489c9038a"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_amd64.tar.gz"
      sha256 "5f93e38424ce0d12c14f4615498e7ce910169d63a7d9f453e0a2e639bb141013"
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
