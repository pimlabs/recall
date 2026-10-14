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
  version "0.4.17"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-apple-darwin.tar.gz"
      sha256 "a96a51370f1436131203184d9cb8efa734ee61882ce804d55f7bc5a5fc98f903"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-apple-darwin.tar.gz"
      sha256 "2326b9ed9238a41b78d0740a2b4ac80f4ca85665e04905a2a2eb03606a963ee0"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "33f546199fb2031f60518c5106b07ce93aaf48eed0b03ec7593e1f21d5cad4eb"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ef4106a719b63232455b63923e5944b75e66e44348cb38efcdee4b9ffe481a35"
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
