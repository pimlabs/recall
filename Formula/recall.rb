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
  version "0.4.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_arm64.tar.gz"
      sha256 "55ff0ce38b9e36b882d492248ea75cf004695fd1ed7cc28bcc33394c078762dd"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_amd64.tar.gz"
      sha256 "e61b0a36c5b63c8b1af490406843b0d0961cfc120dddc57cf112e4c78750f2d8"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_arm64.tar.gz"
      sha256 "b07d7ec382f13390d0cb94664180365bfb21b768e30dfab22433566412008b27"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_amd64.tar.gz"
      sha256 "0747b6cc09c276f55c5c1a5b3f3659b67159d791cb2a9d4fdeda41af3c4e7e41"
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
