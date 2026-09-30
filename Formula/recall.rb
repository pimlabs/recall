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
  version "0.4.14"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-apple-darwin.tar.gz"
      sha256 "00405460abe62cd8190120bdd9b2a268a072071e4a8e46bcc7d0850b96c74a0d"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-apple-darwin.tar.gz"
      sha256 "40e12fad880c05799bb24fd24e0e541aa91672678b3edce31ccf505c389f0022"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "922ee31187b7836851961e2eeca62818b537845797a2a2c1e7b19a27860af353"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "568e00ed8f6337d7aaec48633a008c68ed8aa4cdb28598069250fc0a2da822b2"
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
