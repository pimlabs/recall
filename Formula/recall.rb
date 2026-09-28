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
  version "0.4.6"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-apple-darwin.tar.gz"
      sha256 "5824f26a417abb62766b186e93322066f47805543da2e76b53990112f8cc14ae"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-apple-darwin.tar.gz"
      sha256 "7cf21053a05f02f54522fef9d257028a888d41c7e5265748b8a4337bd79d94ab"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "e4d4c81014c1693303afde6b7cbb742d78246b75fa5416f04b3f6b8e584718d3"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "fe7e1bd9d09053c7675c8fab4128110c074cd17e5b15b9fd7e41b8c39060e0c0"
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
      # Each archive holds one directory named for its Rust target —
      # recall-aarch64-apple-darwin/ and so on — with `recall` in it.
      # Homebrew changes into an archive's only directory before this runs.
      bin.install "recall"
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
