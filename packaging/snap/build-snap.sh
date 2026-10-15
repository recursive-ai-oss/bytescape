#!/usr/bin/env bash
# Builds the snap: bytescape-<version>-linux-<x64|arm64>-<strict|classic>.snap.
#
#   packaging/snap/build-snap.sh strict|classic [--out DIR] [--lint-only]
#
# Needs snapcraft (sudo snap install snapcraft --classic). It builds with
# --destructive-mode, on the machine itself, so run it on Ubuntu 24.04 (the
# snap's base is core24). With --lint-only it only checks that the manifest is
# well formed YAML and names the plugs the design lists.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

edition="${1:-}"
[ "$edition" = strict ] || [ "$edition" = classic ] || { echo "usage: build-snap.sh strict|classic [--out DIR] [--lint-only]" >&2; exit 2; }
shift
out=dist
lint_only=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --lint-only) lint_only=1; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

manifest=packaging/snap/snapcraft.yaml
[ "$edition" = classic ] && manifest=packaging/snap/snapcraft-classic.yaml

if [ "$lint_only" = 1 ]; then
  python3 - "$manifest" "$edition" <<'PY'
import sys, yaml
manifest, edition = sys.argv[1:3]
doc = yaml.safe_load(open(manifest))
assert doc["base"] == "core24", doc["base"]
assert doc["confinement"] == edition, doc["confinement"]
plugs = doc["apps"]["bytescape"].get("plugs", [])
want = ["home", "removable-media", "mount-observe", "hardware-observe", "desktop", "wayland", "x11"]
if edition == "strict":
    assert plugs == want, plugs
else:
    assert not plugs, plugs
print(f"{manifest}: ok")
PY
  exit 0
fi

command -v snapcraft > /dev/null || { echo "snapcraft is not installed" >&2; exit 2; }
# The rust plugin drives the build through rustup and installs the toolchain
# named in rust-toolchain.toml. The plugin looks for rustup from inside
# snapcraft's own process, where the rustup snap cannot be started, so rustup
# is installed the ordinary way instead, into the home folder of the account
# running this script (the plugin adds ~/.cargo/bin to its path). The installer
# is a pinned release, checked against its SHA-256. No toolchain is installed
# here: rustup fetches the one rust-toolchain.toml names when the build asks.
rustup_version=1.29.0
case "$(uname -m)" in
  x86_64) rustup_triple=x86_64-unknown-linux-gnu
    rustup_sha=4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10 ;;
  aarch64) rustup_triple=aarch64-unknown-linux-gnu
    rustup_sha=9732d6c5e2a098d3521fca8145d826ae0aaa067ef2385ead08e6feac88fa5792 ;;
  *) echo "unsupported machine $(uname -m)" >&2; exit 2 ;;
esac
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  # The program decides what to do by its own name, so it must be called
  # rustup-init.
  initdir="$(mktemp -d)"
  init="$initdir/rustup-init"
  curl --proto '=https' --tlsv1.2 -fsSL -o "$init" "https://static.rust-lang.org/rustup/archive/$rustup_version/$rustup_triple/rustup-init"
  echo "$rustup_sha  $init" | sha256sum -c -
  chmod +x "$init"
  "$init" -y --no-modify-path --profile minimal --default-toolchain none
  rm -rf "$initdir"
fi
"$HOME/.cargo/bin/rustup" dump-testament > /dev/null
rm -rf snap
mkdir snap
cp "$manifest" snap/snapcraft.yaml
cp -r packaging/snap/gui snap/gui
trap 'rm -rf snap' EXIT
snapcraft pack --destructive-mode
mkdir -p "$out"
# The version is read from Cargo.toml without cargo, so the script also works
# under sudo, where the Rust tools are not on the path.
version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -1)"
case "$(uname -m)" in x86_64) arch=x64 ;; aarch64) arch=arm64 ;; *) echo "unsupported machine" >&2; exit 2 ;; esac
for file in ./*.snap; do
  mv "$file" "$out/bytescape-$version-linux-$arch-$edition.snap"
done
ls -l "$out"
