#!/usr/bin/env bash
# Linux tests that need more than an ordinary user. Three run inside a private
# user and mount namespace (two of the walker's, one of the elevated helper's).
# Five run as root: the walker's test that mounts real file systems, the
# helper's test with the rights of root, the raw ext4 reader on the machine's
# own root device, and the btrfs and XFS readers on file systems made for the
# test. A test that cannot do its job prints "NOT RUN" and
# still passes, so this script fails when it sees that text. A skipped test is
# not a result.
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
target="${CARGO_TARGET_DIR:-target}"

# The helper's tests build their fixtures in folders named bs-helper-it-*,
# below /tmp or $TMPDIR, and the root test leaves a set-user-id root copy of
# `env` in one of them. A test program that is killed (a step time limit, for
# one) never reaches its own clean-up, so this script removes the folders when
# it exits, as root where it needs to, whatever the exit status. It only
# looks at the top level of those two folders.
cleanup_helper_dirs() {
  local dir
  for dir in /tmp "${TMPDIR:-/tmp}"; do
    [ -d "$dir" ] || continue
    # The folders this user made go first; what is left belongs to root.
    find "$dir" -maxdepth 1 -type d -name 'bs-helper-it-*' -exec rm -rf -- {} + 2> /dev/null || true
    if [ -n "$(find "$dir" -maxdepth 1 -type d -name 'bs-helper-it-*' -print -quit)" ] && [ "$(id -u)" != 0 ]; then
      find "$dir" -maxdepth 1 -type d -name 'bs-helper-it-*' -exec sudo rm -rf -- {} + || true
    fi
    if [ -n "$(find "$dir" -maxdepth 1 -type d -name 'bs-helper-it-*' -print -quit)" ]; then
      echo "warning: could not remove the bs-helper-it-* folders in $dir" >&2
    fi
  done
}
trap 'cleanup_helper_dirs; rm -f "$log" "$build_log"' EXIT

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

# The helper's root test makes a copy of `env` that is set-user-id root, in a
# folder below the system's temporary folder. It runs as root through sudo,
# which drops TMPDIR, so that folder is /tmp; the run below pins TMPDIR=/tmp
# so it is the same when this script already runs as root. A file system
# mounted `nosuid` there would make the stand-in useless, so check before
# building.
if findmnt -no OPTIONS --target /tmp | tr ',' '\n' | grep -qx nosuid; then
  echo "/tmp is on a file system mounted nosuid; the helper's root test cannot run there" >&2
  exit 1
fi

# The helper's two privileged test programs, built the same way.
cargo test --locked -p bytescape-helper --no-run --test mounts_namespace --test root 2>&1 | tee "$build_log"
find_helper_test() {
  local name="$1" path
  path="$(sed 's/\x1b\[[0-9;]*m//g' "$build_log" |
    sed -n "s|^ *Executable tests/$name\.rs (\(.*\))\$|\1|p")"
  if [ -z "$path" ] || [ ! -x "$path" ]; then
    echo "could not find the built $name test program" >&2
    exit 1
  fi
  echo "$(cd "$(dirname "$path")" && pwd)/$(basename "$path")"
}
helper_ns_test="$(find_helper_test mounts_namespace)"
helper_root_test="$(find_helper_test root)"

echo "== in a user and mount namespace"
cargo test --locked -p bs-scan --test huge_sizes --test mount_race \
  -- --nocapture --test-threads=1 2>&1 | tee -a "$log"
# The helper's delete code around other mounts, in a namespace of its own.
( cd crates/bytescape-helper
  unshare --user --map-root-user --mount "$helper_ns_test" --nocapture --test-threads=1
) 2>&1 | tee -a "$log"

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
# The clean-up comes first and each part is allowed to fail: one that cannot
# do its job (no sudo, say) must not stop the others.
trap 'cleanup_helper_dirs; rm -f "$log" "$build_log"; fix_ownership || true' EXIT

echo "== as root"
(
  cd crates/bs-scan
  if [ "$(id -u)" = 0 ]; then
    "$root_test" --nocapture --test-threads=1
  else
    sudo -E "$root_test" --nocapture --test-threads=1
  fi
) 2>&1 | tee -a "$log"

echo "== the elevated helper, as root"
(
  cd crates/bytescape-helper
  if [ "$(id -u)" = 0 ]; then
    BYTESCAPE_DISPOSABLE_HOST=1 TMPDIR=/tmp "$helper_root_test" --nocapture --test-threads=1
  else
    sudo -E env BYTESCAPE_DISPOSABLE_HOST=1 TMPDIR=/tmp "$helper_root_test" --nocapture --test-threads=1
  fi
) 2>&1 | tee -a "$log"

echo "== raw ext4 reader on this machine's own root file system"
# Reads the root device as root and compares the raw reader with the walker.
# On a root file system that is not ext4 the test prints NOT RUN, and the
# check at the end fails the script: a skipped comparison is not a result.
cargo test --locked -p bs-scan-ext4 --features image-tests --test live --no-run 2>&1 | tee "$build_log"
live_test="$(sed 's/\x1b\[[0-9;]*m//g' "$build_log" |
  sed -n 's|^ *Executable tests/live\.rs (\(.*\))$|\1|p')"
if [ -z "$live_test" ] || [ ! -x "$live_test" ]; then
  echo "could not find the built live test program" >&2
  exit 1
fi
live_test="$(cd "$(dirname "$live_test")" && pwd)/$(basename "$live_test")"
(
  cd crates/bs-scan-ext4
  if [ "$(id -u)" = 0 ]; then
    BYTESCAPE_DISPOSABLE_HOST=1 "$live_test" --nocapture --test-threads=1
  else
    sudo -E env BYTESCAPE_DISPOSABLE_HOST=1 "$live_test" --nocapture --test-threads=1
  fi
) 2>&1 | tee -a "$log"

echo "== btrfs and XFS readers against the walker, on mounted file systems"
# Each test makes a loop-mounted file system, fills it, and compares the
# reader with the walker path by path. XFS needs its module loaded.
sudo modprobe xfs 2> /dev/null || true
run_live() { # crate
  local crate="$1" prog
  cargo test --locked -p "$crate" --test live --no-run 2>&1 | tee "$build_log"
  prog="$(sed 's/\x1b\[[0-9;]*m//g' "$build_log" |
    sed -n 's|^ *Executable tests/live\.rs (\(.*\))$|\1|p')"
  if [ -z "$prog" ] || [ ! -x "$prog" ]; then
    echo "could not find the built $crate live test program" >&2
    exit 1
  fi
  prog="$(cd "$(dirname "$prog")" && pwd)/$(basename "$prog")"
  (
    cd "crates/$crate"
    if [ "$(id -u)" = 0 ]; then
      BYTESCAPE_DISPOSABLE_HOST=1 "$prog" --nocapture --test-threads=1
    else
      sudo -E env BYTESCAPE_DISPOSABLE_HOST=1 "$prog" --nocapture --test-threads=1
    fi
  ) 2>&1 | tee -a "$log"
}
run_live bs-scan-btrfs
run_live bs-scan-xfs

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
if [ "$(grep -c '^test result: ok\. [1-9][0-9]* passed' "$log")" -ne 8 ]; then
  echo "expected eight test programs that each passed at least one test" >&2
  exit 1
fi
echo "all privileged and namespace tests ran"
