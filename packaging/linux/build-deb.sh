#!/usr/bin/env bash
# Builds the Debian package: bytescape-<version>-linux-<x64|arm64>.deb.
#
#   packaging/linux/build-deb.sh [--target TRIPLE] [--out DIR]
#
# Needs cargo-deb (cargo install --locked cargo-deb) and, for the automatic
# dependency list, dpkg-shlibdeps (package dpkg-dev). Build the package on the
# oldest system you want to support, because the dependency list names the
# libraries it was linked against; the release workflow uses Ubuntu 22.04.
# Set SOURCE_DATE_EPOCH to the commit time for a build that can be repeated.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

target=""
out="dist"
while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

triple="${target:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$triple" in
  x86_64-unknown-linux-gnu) arch=x64 ;;
  aarch64-unknown-linux-gnu) arch=arm64 ;;
  *) echo "unsupported target $triple" >&2; exit 2 ;;
esac
version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "bytescape") | .version')"

cargo build --release --locked --target "$triple" -p bytescape -p bytescape-helper
mkdir -p "$out"
cargo deb --no-build --target "$triple" -p bytescape --output "$out/bytescape-$version-linux-$arch.deb"
echo "built $out/bytescape-$version-linux-$arch.deb"
