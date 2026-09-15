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
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_arm64.tar.gz"
      sha256 "646a176c155dcb397da48373ab1809f4234a3359ee222f762ee9ed0e7fbbd8ec"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_darwin_amd64.tar.gz"
      sha256 "96cf3f0c9eafb2a4921c372537e219fe766c55b72fd5e7996d900cdec763b3c7"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_arm64.tar.gz"
      sha256 "6a4d08d344ffd618701da456d8d40e927fe502391bf64f441efb281f759013cb"
    end
    on_intel do
      url "https://github.com/pimlabs/recall/releases/download/v#{version}/recall_linux_amd64.tar.gz"
      sha256 "e295ed75ba3686c5fdec5129bf94936252699d9b66365dc0544437bb232bdb9e"
    end
  end

  head do
    url "https://github.com/pimlabs/recall.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args(path: "crates/recall-sync")
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
