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
  version "0.4.8"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-apple-darwin.tar.gz"
      sha256 "a9cfc83192afc52545b2a24bbde6afb29749ec62bc43dbb7adc03b4c77ffd870"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-apple-darwin.tar.gz"
      sha256 "1bdd7a7636a9c67844a0a6eb4138cb2412fbec72489307cfdcaed1ef939b544f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "72d0ad217a7b6b595340fc1b12a0e498e07cba707bc6d697983ffb62cf832512"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "b4a05a03643d3a2aa3129ddbda72d6d02a5be2df3a2a3f2f42e46a9acbd07050"
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
