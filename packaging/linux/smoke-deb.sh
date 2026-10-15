#!/usr/bin/env bash
# Installs the .deb in a fresh Debian or Ubuntu container and checks it. Run
# it inside the container, as root:
#
#   docker run --rm -v "$PWD:/repo:ro" -v "$PWD/dist:/pkg:ro" ubuntu:24.04 \
#     bash /repo/packaging/linux/smoke-deb.sh /pkg/bytescape-<version>-linux-x64.deb
#
# It checks where the files land and who owns them, that polkit knows both
# helper actions with the right authorization levels, the desktop file, the
# metadata and the policy, runs lintian, and removes the package again.

set -euo pipefail

deb="${1:?usage: smoke-deb.sh PATH-TO-DEB}"
export DEBIAN_FRONTEND=noninteractive

apt-get update -qq > /dev/null
apt-get install -y -qq --no-install-recommends "$deb" lintian pkexec desktop-file-utils appstream libxml2-utils dbus xvfb xauth > /dev/null 2>&1 \
  || apt-get install -y -qq --no-install-recommends "$deb" lintian policykit-1 desktop-file-utils appstream libxml2-utils dbus xvfb xauth

fail=0
check() { if "$@"; then echo "ok: $*"; else echo "FAILED: $*" >&2; fail=1; fi; }
owned() { [ "$(stat -c '%U:%G %a' "$1")" = "$2" ]; }

echo "== files"
check owned /usr/bin/bytescape "root:root 755"
check owned /usr/libexec/bytescape "root:root 755"
check owned /usr/libexec/bytescape/bytescape-helper "root:root 755"
check test -f /usr/share/polkit-1/actions/org.recursiveai.bytescape.helper.policy
check test -f /usr/share/applications/org.recursiveai.bytescape.desktop
check test -f /usr/share/metainfo/org.recursiveai.bytescape.metainfo.xml
check test -f /usr/share/icons/hicolor/scalable/apps/org.recursiveai.bytescape.svg
check test -f /usr/share/icons/hicolor/256x256/apps/org.recursiveai.bytescape.png
check test -f /usr/share/kio/servicemenus/bytescape-analyze.desktop
check test -f /usr/share/nautilus-python/extensions/bytescape-nautilus.py
# The main entry never claims to open folders.
check sh -c '! grep -qi "^MimeType" /usr/share/applications/org.recursiveai.bytescape.desktop'

echo "== refuses administrator rights"
# This script runs as root, so the installed program must refuse, say why, and
# fail. It never reaches the window code, so no display is needed.
status=0
refusal="$(/usr/bin/bytescape --bench-first-frame /tmp 2>&1)" || status=$?
echo "$refusal"
check test "$status" -ne 0
check grep -q 'running with administrator rights, which it never does' <<< "$refusal"
# Printing the version is harmless and still works as root.
check /usr/bin/bytescape --version

echo "== starts as an ordinary user with only the package's own dependencies"
# xvfb and xauth are installed above, but not the display libraries or a
# font: the package must have pulled those in itself (it is installed without its
# recommended packages).
useradd --create-home smoke
# A folder with something in it to draw.
mkdir -p /home/smoke/map
cp -r /usr/share/polkit-1 /usr/share/applications /usr/share/doc /home/smoke/map/ 2> /dev/null || true
chown -R smoke:smoke /home/smoke/map
# The run is bounded: a program that does not exit must fail the check
# quickly and say so, not use up the job.
check timeout 120 setpriv --reuid=smoke --regid=smoke --init-groups env -u WAYLAND_DISPLAY HOME=/home/smoke xvfb-run -a /usr/bin/bytescape --bench-first-frame /home/smoke/map

echo "== polkit"
check xmllint --noout /usr/share/polkit-1/actions/org.recursiveai.bytescape.helper.policy
mkdir -p /run/dbus
dbus-daemon --system --fork
polkitd="$(find /usr/lib /usr/libexec -name polkitd -type f -print -quit 2> /dev/null)"
"$polkitd" --no-debug &
for _ in $(seq 1 30); do
  pkaction --action-id org.recursiveai.bytescape.helper.scan --verbose 2> /dev/null | grep -q 'implicit active' && break
  sleep 1
done
# Some pkaction versions exit with 1 even after printing the action, so the
# output is what gets checked.
scan="$(pkaction --action-id org.recursiveai.bytescape.helper.scan --verbose 2>&1 || true)"
delete="$(pkaction --action-id org.recursiveai.bytescape.helper.delete --verbose 2>&1 || true)"
echo "$scan"
echo "$delete"
check grep -q 'implicit active:.*auth_admin_keep' <<< "$scan"
check grep -q 'implicit active:.*auth_admin$' <<< "$delete"
check grep -q 'org.freedesktop.policykit.exec.path -> /usr/libexec/bytescape/bytescape-helper' <<< "$scan"
check grep -q 'org.freedesktop.policykit.exec.argv1 -> scan' <<< "$scan"
check grep -q 'org.freedesktop.policykit.exec.argv1 -> delete' <<< "$delete"

echo "== metadata"
check desktop-file-validate /usr/share/applications/org.recursiveai.bytescape.desktop
check appstreamcli validate --no-net /usr/share/metainfo/org.recursiveai.bytescape.metainfo.xml

echo "== lintian (errors fail; warnings are shown)"
lintian --tag-display-limit 0 -I "$deb" | tee /tmp/lintian.txt || true
check sh -c '! grep -q "^E:" /tmp/lintian.txt'

echo "== remove"
apt-get remove -y -qq bytescape > /dev/null
check sh -c '! test -e /usr/bin/bytescape && ! test -e /usr/libexec/bytescape'
check sh -c '! test -e /usr/share/polkit-1/actions/org.recursiveai.bytescape.helper.policy'

[ "$fail" = 0 ] || { echo "the package check failed" >&2; exit 1; }
echo "the package check passed"
