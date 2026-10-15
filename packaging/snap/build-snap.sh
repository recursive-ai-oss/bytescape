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
