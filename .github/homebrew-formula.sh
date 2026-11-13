#!/bin/sh
# Prints the Homebrew formula for a leaf release, from its SHA256SUMS.
#   sh .github/homebrew-formula.sh 0.14.0 SHA256SUMS > Formula/leaf.rb
# The release workflow runs this to bump DeepanshuMishraa/homebrew-tap.
set -eu

version=$1
sums=$2

sha() {
  hash=$(awk -v name="leaf-$1.tar.gz" '$2 == name { print $1 }' "$sums")
  if [ -z "$hash" ]; then
    echo "homebrew-formula: $sums has no checksum for leaf-$1.tar.gz" >&2
    exit 1
  fi
  echo "$hash"
}

mac_arm=$(sha aarch64-apple-darwin)
mac_intel=$(sha x86_64-apple-darwin)
linux_arm=$(sha aarch64-unknown-linux-gnu)
linux_intel=$(sha x86_64-unknown-linux-gnu)
base="https://github.com/DeepanshuMishraa/leaf/releases/download/v$version"

cat <<EOF
class Leaf < Formula
  desc "Read PDFs, markdown files and web articles in the terminal, and review diffs"
  homepage "https://leaf.dipxsy.app"
  license "MIT"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    on_arm do
      url "$base/leaf-aarch64-apple-darwin.tar.gz"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$base/leaf-x86_64-apple-darwin.tar.gz"
      sha256 "$mac_intel"
    end
  end

  on_linux do
    depends_on "alsa-lib"

    on_arm do
      url "$base/leaf-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_arm"
    end
    on_intel do
      url "$base/leaf-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_intel"
    end
  end

  def install
    bin.install "leaf"
  end

  test do
    assert_equal "leaf #{version}", shell_output("#{bin}/leaf version").strip
  end
end
EOF
