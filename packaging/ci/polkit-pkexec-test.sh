#!/usr/bin/env bash
# Checks the installed Debian package's polkit setup in a fresh container:
# both helper actions are known to polkit with the right annotations and
# levels, pkexec starts the helper as root, and pkexec removes DISPLAY and
# XAUTHORITY from its environment. Run it inside the container, as root:
#
#   docker run --rm --memory=4g -v "$PWD:/repo:ro" -v "$PWD/dist:/pkg:ro" ubuntu:24.04 \
#     bash /repo/packaging/ci/polkit-pkexec-test.sh /pkg/bytescape-<version>-linux-x64.deb
#
# The container is disposable: the script creates a user, adds a polkit rule,
# replaces the helper for one step and starts polkitd and a system bus inside
# it. Never run it on a machine you use.
#
# Two things shape the test, and what each leaves untested is said here.
#
# 1. A container has no login manager, and polkit cannot finish a password
#    prompt without one (it answers "No session for cookie"). So a rule,
#    written before polkitd starts, grants the scan action to the test user,
#    and only that action. This still shows that pkexec found the action by
#    the helper's path and first argument (a rule for an action that did not
#    match would not apply, and the generic prompt would appear instead). How
#    `auth_admin_keep` and `auth_admin` behave at a prompt is not exercised;
#    the levels are read from the installed policy instead.
# 2. The real helper leaves within milliseconds when it is not started by the
#    interface, too fast to read its environment from /proc. So the
#    environment is checked with a stand-in at the helper's path that writes
#    the variables it was given to a file; the pkexec behaviour under test is
#    the same. The real helper is then put back and started the same way, and
#    the test checks that pkexec ran it (its exit status is the helper's own
#    refusal, not pkexec's "not authorized" or "cannot run").

set -euo pipefail

deb="${1:?usage: polkit-pkexec-test.sh PATH-TO-DEB}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export DEBIAN_FRONTEND=noninteractive

if [ ! -f /.dockerenv ] && [ "${BYTESCAPE_DISPOSABLE_HOST:-}" != 1 ]; then
  echo "refusing to run: this creates a user and a polkit rule. Run it in a container." >&2
  exit 2
fi

apt-get update -qq > /dev/null
apt-get install -y -qq --no-install-recommends "$deb" pkexec dbus python3 passwd > /dev/null

helper=/usr/libexec/bytescape/bytescape-helper
fail=0
check() { if "$@"; then echo "ok: $*"; else echo "FAILED: $*" >&2; fail=1; fi; }
harness() { python3 "$here/polkit-pkexec-harness.py" tester unused "$@" "$helper"; }

# The test user, and the rule that grants the scan action to it. The rule has
# to be in place before polkitd starts.
useradd --create-home --shell /bin/bash tester
cat > /etc/polkit-1/rules.d/00-bytescape-ci.rules << 'POLKITRULE'
polkit.addRule(function (action, subject) {
  if (action.id == "org.recursiveai.bytescape.helper.scan" && subject.user == "tester") {
    return polkit.Result.YES;
  }
});
POLKITRULE

# polkitd needs the system bus.
mkdir -p /run/dbus
dbus-daemon --system --fork
polkitd="$(find /usr/lib /usr/libexec -name polkitd -type f -print -quit 2> /dev/null)"
"$polkitd" --no-debug &
for _ in $(seq 1 30); do
  pkaction --action-id org.recursiveai.bytescape.helper.scan --verbose 2> /dev/null | grep -q 'implicit active' && break
  sleep 1
done

echo "== the two actions"
# Some pkaction versions exit with 1 even after printing the action, so the
# output is what gets checked.
scan_out="$(pkaction --action-id org.recursiveai.bytescape.helper.scan --verbose 2> /dev/null || true)"
delete_out="$(pkaction --action-id org.recursiveai.bytescape.helper.delete --verbose 2> /dev/null || true)"
echo "$scan_out"
echo "$delete_out"
check grep -q "policykit.exec.path.*$helper" <<< "$scan_out"
check grep -q "policykit.exec.argv1.*scan" <<< "$scan_out"
check grep -q "implicit active:.*auth_admin_keep" <<< "$scan_out"
check grep -q "policykit.exec.path.*$helper" <<< "$delete_out"
check grep -q "policykit.exec.argv1.*delete" <<< "$delete_out"
check grep -q "implicit active:.*auth_admin$" <<< "$delete_out"

echo "== pkexec removes the desktop's environment (stand-in at the helper's path)"
mv "$helper" "$helper.real"
cat > "$helper" << 'STANDIN'
#!/bin/sh
env > /run/helper-environment
id -u >> /run/helper-environment
STANDIN
chmod 755 "$helper"
rm -f /run/helper-environment
if harness scan > /tmp/pkexec-scan.log 2>&1; then
  echo "ok: pkexec scan ran the helper's path as root"
else
  echo "FAILED: pkexec scan" >&2
  cat /tmp/pkexec-scan.log >&2
  fail=1
fi
if [ -f /run/helper-environment ]; then
  echo "variables the elevated process had: $(sed -n 's/=.*//p' /run/helper-environment | tr '\n' ' ')"
  check test "$(tail -n 1 /run/helper-environment)" = 0
  check grep -q '^PKEXEC_UID=' /run/helper-environment
  check sh -c '! grep -q "^DISPLAY=" /run/helper-environment'
  check sh -c '! grep -q "^XAUTHORITY=" /run/helper-environment'
else
  echo "FAILED: the stand-in did not run" >&2
  fail=1
fi

echo "== pkexec delete is not granted by the scan rule"
# The delete action must still need an administrator's authentication, which
# cannot be given here, so the helper's path must not run.
rm -f /run/helper-environment
if harness delete > /tmp/pkexec-delete.log 2>&1; then
  echo "FAILED: pkexec delete ran without authentication" >&2
  fail=1
else
  check test ! -f /run/helper-environment
  echo "ok: pkexec delete did not run the helper"
fi

echo "== the real helper, started through pkexec"
mv "$helper.real" "$helper"
status=0
harness scan > /tmp/pkexec-real.log 2>&1 || status=$?
tail -n 3 /tmp/pkexec-real.log
echo "exit status $status"
# 126 and 127 are pkexec's own (not authorized, could not run it): anything
# else is the helper's exit status, which means it started.
check test "$status" -ne 126
check test "$status" -ne 127
check test "$status" -ne 124

if [ "$fail" -ne 0 ]; then
  echo "polkit check FAILED" >&2
  exit 1
fi
echo "polkit check passed"
