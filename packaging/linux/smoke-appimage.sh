#!/usr/bin/env bash
# Unpacks the AppImage and starts it under a virtual display, in a fresh
# container. Run it inside the container, as root (to install the display
# libraries); the program itself is started as an ordinary user, because it
# refuses to run with administrator rights:
#
#   docker run --rm -v "$PWD:/repo:ro" -v "$PWD/dist:/pkg:ro" ubuntu:22.04 \
#     bash /repo/packaging/linux/smoke-appimage.sh /pkg/bytescape-<version>-linux-x64.AppImage
#
# The AppImage is unpacked with --appimage-extract, so no FUSE is needed. The
# program starts with --bench-first-frame, which draws one frame of a folder
# and exits; a window that cannot be created or drawn makes it fail. A last
# check starts it as root and expects the refusal, so the refusal is known to
# be in the file that ships.

set -euo pipefail

appimage="${1:?usage: smoke-appimage.sh PATH-TO-APPIMAGE}"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq > /dev/null
apt-get install -y -qq --no-install-recommends xvfb xauth libx11-6 libx11-xcb1 libxcursor1 libxi6 libxrandr2 libxext6 libxfixes3 libxkbcommon0 libxkbcommon-x11-0 libxcb1 fontconfig fonts-dejavu-core > /dev/null

useradd --create-home --shell /bin/bash smoke
work=/home/smoke/work
install -d -o smoke -g smoke "$work"
cp "$appimage" "$work/app.AppImage"
chown smoke:smoke "$work/app.AppImage"
# Everything below runs as the ordinary user.
as_user() { setpriv --reuid=smoke --regid=smoke --init-groups env HOME=/home/smoke "$@"; }

as_user bash -euo pipefail -c '
  cd /home/smoke/work
  chmod +x app.AppImage
  ./app.AppImage --appimage-extract > /dev/null
  test -x squashfs-root/AppRun
  test -x squashfs-root/usr/bin/bytescape
  test -f squashfs-root/org.recursiveai.bytescape.desktop
  test -f squashfs-root/usr/share/metainfo/org.recursiveai.bytescape.metainfo.xml
  # The helper is not in the AppImage.
  test ! -e squashfs-root/usr/bin/bytescape-helper
  find squashfs-root -type f | sort | sed -n 1,40p
  mkdir -p /home/smoke/map && cp -r /repo/packaging /home/smoke/map/ 2> /dev/null || true
  env -u WAYLAND_DISPLAY xvfb-run -a ./squashfs-root/AppRun --bench-first-frame /home/smoke/map
'

# As root it must refuse, say why, and exit with a failure.
status=0
out="$(env -u WAYLAND_DISPLAY "$work/squashfs-root/AppRun" --bench-first-frame /tmp 2>&1)" || status=$?
echo "$out"
if [ "$status" -eq 0 ] || ! grep -q 'running with administrator rights, which it never does' <<< "$out"; then
  echo "the program did not refuse to run as root (status $status)" >&2
  exit 1
fi
echo "ok: as root it refuses (status $status)"
echo "the AppImage check passed"
