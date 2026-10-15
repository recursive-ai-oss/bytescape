#!/usr/bin/env bash
# Unpacks the AppImage and starts it under a virtual display, in a fresh
# container. Run it inside the container, as root:
#
#   docker run --rm -v "$PWD:/repo:ro" -v "$PWD/dist:/pkg:ro" ubuntu:22.04 \
#     bash /repo/packaging/linux/smoke-appimage.sh /pkg/bytescape-<version>-linux-x64.AppImage
#
# The AppImage is unpacked with --appimage-extract, so no FUSE is needed. The
# program starts with --bench-first-frame, which draws one frame of a folder
# and exits; a window that cannot be created or drawn makes it fail.

set -euo pipefail

appimage="${1:?usage: smoke-appimage.sh PATH-TO-APPIMAGE}"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null
apt-get install -y -qq --no-install-recommends xvfb xauth libx11-6 libx11-xcb1 libxcursor1 libxi6 libxrandr2 libxext6 libxfixes3 libxkbcommon0 libxkbcommon-x11-0 libxcb1 fontconfig fonts-dejavu-core > /dev/null

work="$(mktemp -d)"
cp "$appimage" "$work/app.AppImage"
chmod +x "$work/app.AppImage"
cd "$work"
./app.AppImage --appimage-extract > /dev/null

test -x squashfs-root/AppRun
test -x squashfs-root/usr/bin/bytescape
test -f squashfs-root/org.recursiveai.bytescape.desktop
test -f squashfs-root/usr/share/metainfo/org.recursiveai.bytescape.metainfo.xml
# The helper is not in the AppImage.
test ! -e squashfs-root/usr/bin/bytescape-helper
find squashfs-root -type f | sort | sed -n 1,40p

mkdir -p /tmp/map && cp -r /repo/packaging /tmp/map/ 2> /dev/null || true
env -u WAYLAND_DISPLAY xvfb-run -a ./squashfs-root/AppRun --bench-first-frame /tmp/map
echo "the AppImage check passed"
