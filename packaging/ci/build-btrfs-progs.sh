#!/usr/bin/env bash
# Builds btrfs-progs from the upstream release tarball, for the btrfs reader's
# image tests: they make test file systems with `mkfs.btrfs --subvol`, which
# the btrfs-progs of Ubuntu 24.04 (6.6) does not have.
#
#   packaging/ci/build-btrfs-progs.sh [PREFIX]
#
# The tarball is pinned by version and by SHA-256 (the value is the one in the
# signed sha256sums.asc that sits beside the tarballs at kernel.org), and the
# download is refused when it does not match. PREFIX defaults to
# $RUNNER_TEMP/btrfs-progs (or target/btrfs-progs outside CI). The folder
# PREFIX/bin gets mkfs.btrfs and btrfs, and is added to GITHUB_PATH when that
# is set. On a CI runner (CI is set) the script first installs the development
# packages the build needs, with sudo apt-get.
#
# It ends by running `mkfs.btrfs --help` and failing unless the new program
# knows --subvol, so a version that dropped the option is caught here and not
# in a test.

set -euo pipefail

version=6.17.1
sha256=a4be0a6ebb3c476427fb5d97b2cf027b0ccdb6b0c55ff16323320c1e8cb77658
url="https://mirrors.edge.kernel.org/pub/linux/kernel/people/kdave/btrfs-progs/btrfs-progs-v$version.tar.xz"

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
prefix="${1:-${RUNNER_TEMP:-$PWD/target}/btrfs-progs}"
mkdir -p "$prefix"
prefix="$(cd "$prefix" && pwd)"

if [ -n "${CI:-}" ]; then
  sudo apt-get update
  sudo apt-get install -y --no-install-recommends \
    build-essential pkg-config xz-utils curl ca-certificates \
    uuid-dev libblkid-dev libext2fs-dev comerr-dev \
    zlib1g-dev liblzo2-dev libzstd-dev
fi

src="$(mktemp -d)"
trap 'rm -rf "$src"' EXIT
curl -fsSL --retry 5 -o "$src/btrfs-progs.tar.xz" "$url"
echo "$sha256  $src/btrfs-progs.tar.xz" | sha256sum -c -
tar -xJf "$src/btrfs-progs.tar.xz" -C "$src"
cd "$src/btrfs-progs-v$version"

./configure --prefix="$prefix" --disable-documentation --disable-python \
  --disable-libudev --disable-zoned --disable-convert --disable-backtrace > configure.log 2>&1 \
  || { tail -30 configure.log >&2; exit 1; }
make -j"$(nproc)" mkfs.btrfs btrfs > make.log 2>&1 || { tail -30 make.log >&2; exit 1; }
mkdir -p "$prefix/bin"
install -m755 mkfs.btrfs btrfs "$prefix/bin/"

"$prefix/bin/mkfs.btrfs" --version
# The help is read into a variable first: with `pipefail`, `grep -q` closing
# the pipe early would make a good result look like a failure.
help="$("$prefix/bin/mkfs.btrfs" --help 2>&1 || true)"
if ! grep -q -- '--subvol' <<< "$help"; then
  echo "::error title=btrfs-progs without --subvol::the mkfs.btrfs built from version $version does not know --subvol" >&2
  exit 1
fi
echo "mkfs.btrfs with --subvol: $prefix/bin/mkfs.btrfs"
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$prefix/bin" >> "$GITHUB_PATH"
fi
