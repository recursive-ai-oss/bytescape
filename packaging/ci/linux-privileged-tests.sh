#!/usr/bin/env bash
# Linux tests that need more than an ordinary user: two that run inside a
# private user and mount namespace, and one that mounts real file systems as
# root. A test that cannot do its job prints "NOT RUN" and still passes, so
# this script fails when it sees that text. A skipped test is not a result.
#
#   BYTESCAPE_DISPOSABLE_HOST=1 packaging/ci/linux-privileged-tests.sh
#
# The root part mounts and unmounts file systems and needs passwordless sudo
# (or to run as root already), so it refuses to start unless the machine is
# declared disposable with BYTESCAPE_DISPOSABLE_HOST=1. CI sets that on its
# throwaway runners. Set it on your own machine only inside a virtual machine.
#
# Needs: unshare, mount, mkfs.btrfs and btrfs (package btrfs-progs), and
# unprivileged user namespaces. On Ubuntu 24.04 an AppArmor setting blocks
# them; CI lifts it with
#   sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

if [ "$(uname -s)" != Linux ]; then
  echo "these tests run on Linux only" >&2
  exit 2
fi
if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" != 1 ]; then
  echo "refusing to run: this mounts file systems as root. Set BYTESCAPE_DISPOSABLE_HOST=1 on a throwaway machine." >&2
  exit 2
fi
if ! unshare --user --map-root-user --mount true 2> /dev/null; then
  echo "unprivileged user and mount namespaces are not available here. On Ubuntu 24.04: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0" >&2
  exit 1
fi

log="$(mktemp)"
build_log="$(mktemp)"
trap 'rm -f "$log" "$build_log"' EXIT
target="${CARGO_TARGET_DIR:-target}"

# Build first, as the current user. Cargo prints where each test program is.
cargo test --locked -p bs-scan --no-run \
  --test huge_sizes --test mount_race --test mounts_privileged 2>&1 | tee "$build_log"
# Colored output (CARGO_TERM_COLOR=always in CI) is stripped before matching.
root_test="$(sed 's/\x1b\[[0-9;]*m//g' "$build_log" |
  sed -n 's|^ *Executable tests/mounts_privileged\.rs (\(.*\))$|\1|p')"
if [ -z "$root_test" ] || [ ! -x "$root_test" ]; then
  echo "could not find the built mounts_privileged test program" >&2
  exit 1
fi
root_test="$(cd "$(dirname "$root_test")" && pwd)/$(basename "$root_test")"

echo "== in a user and mount namespace"
cargo test --locked -p bs-scan --test huge_sizes --test mount_race \
  -- --nocapture --test-threads=1 2>&1 | tee -a "$log"

# The root run starts the finished test program directly instead of running
# cargo as root. Cargo as root could rebuild into the target folder and leave
# root-owned build files there, which the CI cache step cannot save. The
# program still writes its working files below the target folder, so the
# ownership is handed back afterwards, and the script checks that nothing in
# the target folder belongs to root.
fix_ownership() {
  if [ "$(id -u)" != 0 ] && [ -d "$target" ]; then
    sudo chown -R "$(id -u):$(id -g)" "$target"
  fi
}
trap 'fix_ownership; rm -f "$log" "$build_log"' EXIT

echo "== as root"
(
  cd crates/bs-scan
  if [ "$(id -u)" = 0 ]; then
    "$root_test" --nocapture --test-threads=1
  else
    sudo -E "$root_test" --nocapture --test-threads=1
  fi
) 2>&1 | tee -a "$log"

fix_ownership
if [ "$(id -u)" != 0 ] && [ -n "$(find "$target" -user root -print -quit)" ]; then
  echo "files owned by root are left in $target" >&2
  exit 1
fi

if grep -q 'NOT RUN' "$log"; then
  echo "::error title=Tests did not run::a test above printed NOT RUN" >&2
  grep 'NOT RUN' "$log" >&2
  exit 1
fi
# Every test program must have run at least one test.
if [ "$(grep -c '^test result: ok\. [1-9][0-9]* passed' "$log")" -ne 3 ]; then
  echo "expected three test programs that each passed at least one test" >&2
  exit 1
fi
echo "all privileged and namespace tests ran"
