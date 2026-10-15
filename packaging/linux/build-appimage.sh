#!/usr/bin/env bash
# Builds the AppImage: bytescape-<version>-linux-<x64|arm64>.AppImage.
#
#   packaging/linux/build-appimage.sh [--target TRIPLE] [--out DIR] [--tools DIR] [--save-appdir FILE]
#   packaging/linux/build-appimage.sh --pack-appdir FILE --version V [--target TRIPLE] [--out DIR] [--tools DIR]
#
# What goes in: the interface only. The helper is not in the AppImage: an
# AppImage is mounted for the user's account alone, so a program started with
# administrator rights cannot read it. Where the Debian package is installed
# the interface finds the system's helper by itself.
#
# The only library the program links is the C library, and the display
# libraries are loaded at run time from the system, so there is nothing to
# bundle with linuxdeploy. Build on the oldest system you want to support: the
# release workflow uses Ubuntu 22.04.
#
# appimagetool is downloaded into the tools folder and checked against the
# SHA-256 below. When APPIMAGE_SIGN_KEY names a GPG key the AppImage gets an
# embedded signature; without it a notice says the build is unsigned.
#
# The release workflow builds without any key in reach. The first form builds
# the program and can also save the folder that goes into the AppImage as a tar
# file (--save-appdir). A later job that holds the key runs the second form,
# which only packs that folder and signs it: no program is compiled there.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

tools="${BYTESCAPE_TOOLS:-target/tools}"
out=dist
target=""
save_appdir=""
pack_appdir=""
version=""
while [ $# -gt 0 ]; do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --tools) tools="$2"; shift 2 ;;
    --save-appdir) save_appdir="$2"; shift 2 ;;
    --pack-appdir) pack_appdir="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

triple="${target:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$triple" in
  x86_64-unknown-linux-gnu)
    arch=x64; appimage_arch=x86_64
    tool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 ;;
  aarch64-unknown-linux-gnu)
    arch=arm64; appimage_arch=aarch64
    tool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158 ;;
  *) echo "unsupported target $triple" >&2; exit 2 ;;
esac
tool_version=1.9.1
id=org.recursiveai.bytescape
if [ -n "$pack_appdir" ] && [ -z "$version" ]; then
  echo "--pack-appdir needs --version" >&2; exit 2
fi
if [ -z "$version" ]; then
  version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "bytescape") | .version')"
fi

if [ -z "$pack_appdir" ]; then
  cargo build --release --locked --target "$triple" -p bytescape
  binary="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/$triple/release/bytescape"
fi

mkdir -p "$tools" "$out"
tool="$tools/appimagetool-$tool_version-$appimage_arch.AppImage"
if [ ! -x "$tool" ]; then
  curl -fsSL -o "$tool" "https://github.com/AppImage/appimagetool/releases/download/$tool_version/appimagetool-$appimage_arch.AppImage"
  echo "$tool_sha  $tool" | sha256sum -c -
  chmod +x "$tool"
fi

appdir="$(mktemp -d)"
trap 'rm -rf "$appdir"' EXIT
build_appdir() {
  install -Dm755 "$binary" "$appdir/usr/bin/bytescape"
  install -Dm644 packaging/linux/$id.desktop "$appdir/usr/share/applications/$id.desktop"
  install -Dm644 packaging/linux/$id.metainfo.xml "$appdir/usr/share/metainfo/$id.metainfo.xml"
  install -Dm644 packaging/icons/bytescape.svg "$appdir/usr/share/icons/hicolor/scalable/apps/$id.svg"
  for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "packaging/icons/bytescape-$size.png" "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/$id.png"
  done
  # The files appimagetool looks for at the top of the folder.
  cp "packaging/linux/$id.desktop" "$appdir/$id.desktop"
  cp packaging/icons/bytescape-256.png "$appdir/$id.png"
  cp packaging/icons/bytescape-256.png "$appdir/.DirIcon"
  cat > "$appdir/AppRun" <<'RUN'
#!/bin/sh
here="$(dirname "$(readlink -f "$0")")"
exec "$here/usr/bin/bytescape" "$@"
RUN
  chmod 755 "$appdir/AppRun"
}
if [ -n "$pack_appdir" ]; then
  tar -C "$appdir" -xf "$pack_appdir"
else
  build_appdir
fi
if [ -n "$save_appdir" ]; then
  mkdir -p "$(dirname "$save_appdir")"
  tar -C "$appdir" -cf "$save_appdir" .
fi

sign=()
if [ -n "${APPIMAGE_SIGN_KEY:-}" ]; then
  sign=(--sign --sign-key "$APPIMAGE_SIGN_KEY")
else
  echo "::notice title=Unsigned AppImage::no APPIMAGE_SIGN_KEY, so the AppImage has no embedded signature"
fi

file="$out/bytescape-$version-linux-$arch.AppImage"
ARCH="$appimage_arch" "$tool" --appimage-extract-and-run ${sign[@]+"${sign[@]}"} "$appdir" "$file"
echo "built $file"
