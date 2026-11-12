#!/bin/sh
# Installs leaf from its GitHub releases:
#
#   curl -fsSL https://leaf.dipxsy.app | sh
#
# Settings, as environment variables:
#   LEAF_VERSION=0.12.0           a release other than the newest
#   LEAF_INSTALL_DIR=/usr/local/bin   where the leaf command goes (default ~/.local/bin)
#
# The archive is checked against its published SHA-256 and the binary is run
# once before it replaces anything, so a failed install leaves things as they
# were. Later, `leaf upgrade` installs new releases the same way.
#
# Everything runs inside main(), called on the last line, so a download cut
# short never runs half a script.

set -eu

REPO="DeepanshuMishraa/leaf"

say() { printf '%s\n' "$*"; }
fail() {
  printf 'leaf install: %s\n' "$*" >&2
  exit 1
}
has() { command -v "$1" >/dev/null 2>&1; }

fetch() { # url → stdout
  if has curl; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$1"
  elif has wget; then
    wget -q -O - "$1"
  else
    fail "needs curl or wget to download leaf. Install one and run this again."
  fi
}

fetch_to() { # url file
  if has curl; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"
  else
    wget -q -O "$2" "$1"
  fi
}

# The release target for this machine, as the release workflow names archives.
target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Darwin)
      # A shell under Rosetta reports x86_64 on an arm64 Mac; the native build is better.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
        arch=arm64
      fi
      case "$arch" in
        arm64 | aarch64) echo aarch64-apple-darwin ;;
        x86_64) echo x86_64-apple-darwin ;;
        *) fail "there's no prebuilt leaf for macOS on $arch. Build it from source: cargo install leaf-reader" ;;
      esac
      ;;
    Linux)
      if (ldd --version 2>&1 || true) | grep -qi musl; then
        fail "the prebuilt leaf needs glibc, and this Linux uses musl. Build it from source: cargo install leaf-reader"
      fi
      case "$arch" in
        x86_64 | amd64) echo x86_64-unknown-linux-gnu ;;
        aarch64 | arm64) echo aarch64-unknown-linux-gnu ;;
        *) fail "there's no prebuilt leaf for Linux on $arch. Build it from source: cargo install leaf-reader" ;;
      esac
      ;;
    *) fail "there's no prebuilt leaf for $os. Build it from source: cargo install leaf-reader" ;;
  esac
}

# The newest release's version, from where /releases/latest redirects.
latest() {
  if has curl; then
    url=$(curl --proto '=https' --tlsv1.2 -fsSLI -o /dev/null -w '%{url_effective}' \
      "https://github.com/$REPO/releases/latest") || fail "couldn't reach GitHub. Check the connection and run this again."
  else
    url=$(wget -q -S --spider "https://github.com/$REPO/releases/latest" 2>&1 |
      sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r') || true
  fi
  case "$url" in
    */releases/tag/v*) echo "${url##*/releases/tag/v}" ;;
    *) fail "no leaf release is published yet ($url). Build it from source: cargo install leaf-reader" ;;
  esac
}

sha256() { # file → hex digest
  if has sha256sum; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif has shasum; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  elif has openssl; then
    openssl dgst -sha256 "$1" | sed 's/^.*= *//'
  else
    fail "needs sha256sum, shasum or openssl to check the download."
  fi
}

main() {
  has tar || fail "needs tar to unpack leaf."
  has uname || fail "needs uname to tell which leaf to download."

  target=$(target)
  version=${LEAF_VERSION:-$(latest)}
  version=${version#v}

  # A leaf already on the PATH (from cargo install, say) is updated where it
  # is, so `leaf` keeps meaning the same file. LEAF_INSTALL_DIR overrides that.
  dest=""
  existing=$(command -v leaf 2>/dev/null || true)
  if [ -z "${LEAF_INSTALL_DIR:-}" ] && [ -n "$existing" ] && [ -f "$existing" ]; then
    # Builds before 0.12 have no `version` command; they print nothing here.
    current=$("$existing" version 2>/dev/null || true)
    if [ "$current" = "leaf $version" ]; then
      say "leaf $version is already installed at $existing; it's the newest release."
      return 0
    fi
    real=$(readlink -f "$existing" 2>/dev/null || printf '%s' "$existing")
    if [ -w "$(dirname "$real")" ]; then
      dest=$real
      say "Updating ${current:-the leaf} at $existing to leaf $version"
    else
      say "Can't update $existing in place (its folder isn't writable, as with Nix or a root-owned install); installing a new copy instead."
    fi
  fi
  if [ -z "$dest" ]; then
    dir=${LEAF_INSTALL_DIR:-$HOME/.local/bin}
    dest="$dir/leaf"
  fi
  dir=$(dirname "$dest")
  base="https://github.com/$REPO/releases/download/v$version/leaf-$target.tar.gz"

  tmp=$(mktemp -d 2>/dev/null || mktemp -d -t leaf)
  trap 'rm -rf "$tmp"' EXIT INT TERM

  say "Downloading leaf $version for $target"
  fetch_to "$base" "$tmp/leaf.tar.gz" ||
    fail "couldn't download $base. Is $version a leaf release? See https://github.com/$REPO/releases"
  expected=$(fetch "$base.sha256" | cut -d ' ' -f 1 | tr 'A-F' 'a-f') ||
    fail "couldn't download the checksum for $base."
  actual=$(sha256 "$tmp/leaf.tar.gz")
  [ "${#expected}" -eq 64 ] || fail "the published checksum for $base is unreadable."
  [ "$expected" = "$actual" ] ||
    fail "the download doesn't match its published checksum (expected $expected, got $actual). Nothing was installed; run this again."

  tar -xzf "$tmp/leaf.tar.gz" -C "$tmp" || fail "couldn't unpack the download."
  [ -f "$tmp/leaf" ] || fail "the download has no leaf binary in it."
  chmod 755 "$tmp/leaf"

  # Run once before installing, so a binary that can't start here is never put in place.
  if ! said=$("$tmp/leaf" version 2>&1); then
    if [ "$(uname -s)" = Linux ] && printf '%s' "$said" | grep -q 'libasound'; then
      fail "leaf needs the ALSA sound library, which isn't installed. Debian/Ubuntu: sudo apt install libasound2t64 (or libasound2); Fedora: sudo dnf install alsa-lib. Then run this again."
    fi
    fail "the downloaded leaf doesn't run on this machine: $said"
  fi
  [ "$said" = "leaf $version" ] || fail "the downloaded leaf reports \"$said\", not leaf $version. Nothing was installed."

  mkdir -p "$dir" 2>/dev/null || fail "couldn't create $dir. Pick another folder with LEAF_INSTALL_DIR=..."
  [ -w "$dir" ] || fail "can't write to $dir. Pick another folder with LEAF_INSTALL_DIR=..., or run this with sudo."
  # Copied beside the old one, then renamed over it: never a half-written leaf.
  cp "$tmp/leaf" "$dir/.leaf-install.$$"
  mv -f "$dir/.leaf-install.$$" "$dest"

  say "Installed leaf $version to $dest"
  case ":$PATH:" in
    *":$dir:"*)
      found=$(command -v leaf 2>/dev/null || true)
      found_real=$(readlink -f "$found" 2>/dev/null || printf '%s' "$found")
      if [ -n "$found" ] && [ "$found_real" != "$dest" ]; then
        say "Note: $found comes first on your PATH, so \`leaf\` still runs that one. Remove it, or put $dir earlier on your PATH."
      fi
      ;;
    *)
      say ""
      say "$dir isn't on your PATH yet. Add it:"
      say "  bash/zsh: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile"
      say "  fish:     fish_add_path $dir"
      ;;
  esac
  say ""
  say "Run leaf --help to start. leaf upgrade installs new releases."
}

main "$@"
