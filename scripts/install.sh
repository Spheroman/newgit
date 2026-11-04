#!/bin/sh
# Install newgit from a GitHub release.
#
#   curl -fsSL https://newgit.dev/install.sh | sh
#
# Environment:
#   NEWGIT_VERSION      a release tag such as v0.6.0 (default: the latest)
#   NEWGIT_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
#
# POSIX sh rather than bash: this runs on whatever `sh` the machine has.
# Everything is inside main(), called on the last line, so a download cut off
# halfway runs nothing instead of half a script.
set -eu

REPO="Spheroman/newgit"

say() { printf 'newgit-install: %s\n' "$*" >&2; }
die() {
  say "error: $*"
  exit 1
}
need() { command -v "$1" >/dev/null 2>&1 || die "\`$1\` is required"; }

target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin) os=apple-darwin ;;
    # Static musl binaries: they run on any distro, whatever its libc.
    Linux) os=unknown-linux-musl ;;
    *) die "no prebuilt binary for $os; try \`cargo install newgit --locked\`" ;;
  esac
  case "$arch" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) die "no prebuilt binary for $arch; try \`cargo install newgit --locked\`" ;;
  esac
  # An x86_64 shell under Rosetta reports x86_64 on an Apple silicon Mac;
  # the native binary is the one to install there.
  if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
    arch=aarch64
  fi
  echo "$arch-$os"
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1"
  else
    die "\`curl\` or \`wget\` is required"
  fi
}

sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    die "\`shasum\` or \`sha256sum\` is required to verify the download"
  fi
}

# `newgit merge` runs `git merge-tree --write-tree`, new in git 2.38.
check_git() {
  if ! command -v git >/dev/null 2>&1; then
    say "warning: newgit needs git 2.38 or newer on your PATH"
    return
  fi
  ver="$(git --version | sed -n 's/^git version \([0-9]*\)\.\([0-9]*\).*/\1 \2/p')"
  set -- $ver
  if [ $# -eq 2 ] && { [ "$1" -lt 2 ] || { [ "$1" -eq 2 ] && [ "$2" -lt 38 ]; }; }; then
    say "warning: newgit needs git 2.38 or newer; this is $(git --version)"
  fi
}

main() {
  need uname
  need tar
  need mktemp

  target="$(target)"
  version="${NEWGIT_VERSION:-latest}"
  dir="${NEWGIT_INSTALL_DIR:-$HOME/.local/bin}"
  name="newgit-$target"
  if [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    case "$version" in v*) ;; *) version="v$version" ;; esac
    base="https://github.com/$REPO/releases/download/$version"
  fi

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT INT TERM

  say "downloading $name ($version)"
  if ! fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" 2>/dev/null; then
    case "$target" in
      *-linux-musl)
        # Releases up to v0.6.0 shipped only glibc builds, linked against
        # glibc 2.39: they run on recent distros and nowhere else.
        if ls /lib/ld-musl-* >/dev/null 2>&1; then
          die "$version has no build for musl (Alpine); try \`cargo install newgit --locked\`"
        fi
        name="newgit-${target%-musl}-gnu"
        say "no static build in $version; trying $name, which needs glibc 2.39 or newer"
        fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || die "download failed: $base/$name.tar.gz"
        ;;
      *) die "download failed: $base/$name.tar.gz" ;;
    esac
  fi
  fetch "$base/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" || die "download failed: $base/$name.tar.gz.sha256"

  # The checksum file is `<hex>  <file>`; compare the hex, not the name.
  expected="$(cut -d' ' -f1 <"$tmp/$name.tar.gz.sha256")"
  actual="$(sha256 "$tmp/$name.tar.gz")"
  [ -n "$expected" ] && [ "$expected" = "$actual" ] ||
    die "checksum mismatch for $name.tar.gz (expected $expected, got $actual)"

  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
  # Run it before installing it, so a binary that cannot start here never
  # replaces one that can.
  installed="$("$tmp/$name/newgit" --version 2>&1)" ||
    die "$name does not run on this machine: $installed"
  mkdir -p "$dir"
  # Copy then rename, so a newgit that is running right now keeps its inode
  # and never sees a half-written file.
  cp "$tmp/$name/newgit" "$dir/.newgit.tmp"
  chmod 755 "$dir/.newgit.tmp"
  mv -f "$dir/.newgit.tmp" "$dir/newgit"

  say "installed $installed to $dir/newgit"
  check_git
  case ":$PATH:" in
    *":$dir:"*) ;;
    *)
      say "$dir is not on your PATH; add this to your shell profile:"
      say "  export PATH=\"$dir:\$PATH\""
      ;;
  esac
}

main "$@"
