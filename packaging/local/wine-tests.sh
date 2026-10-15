#!/usr/bin/env bash
# Build the workspace's test programs for Windows and run them under wine on a
# Linux machine. This is wine (not Windows): it narrows what is unknown when no
# Windows machine is at hand. It never replaces the Windows job in CI, and a
# pass here proves nothing about the real Windows API behaviour that wine
# does not implement.
#
#   packaging/local/wine-tests.sh --mingw DIR --target-dir DIR [options]
#
#   --mingw DIR       an unpacked llvm-mingw release (the folder with bin/),
#                     from https://github.com/mstorsjo/llvm-mingw/releases
#                     (the ucrt build for your host). Required.
#   --target-dir DIR  CARGO_TARGET_DIR for the build. Required; keep it out of
#                     the source folder.
#   --work DIR        where the summary, logs and the wine prefix go
#                     (default: <target-dir>/wine-tests)
#   --timeout SEC     per test program (default 300)
#   --mem SIZE        memory cap per test program, for example 8G
#                     (default 8G; needs systemd-run --user, otherwise skipped
#                     with a warning)
#   --fonts DIR       copy the .ttf files in DIR into the wine prefix's font
#                     folder. A fresh prefix has no system fonts that the text
#                     code can find, so text tests fail without this.
#   --package NAME    only this package (repeatable; default: the workspace)
#
# Needs: rustup with the x86_64-pc-windows-gnullvm target, wine64 on PATH,
# xvfb-run, jq. No part of this installs anything or uses sudo.
#
# The test programs run WITHOUT BYTESCAPE_DISPOSABLE_HOST, so tests that
# change system state (Recycle Bin, registry, elevation) report NOT RUN or
# skip themselves. Each program runs in a fresh wine prefix made for this run,
# under a virtual display, with the display variable for Wayland removed.
#
# Exit status: 0 when every program passed, 1 when any failed, timed out or
# crashed, 2 for a setup problem. The summary is <work>/summary.tsv.

set -euo pipefail

mingw=""
target_dir=""
work=""
limit=300
mem="8G"
packages=()
fonts=""

while [ $# -gt 0 ]; do
  case "$1" in
    --mingw) mingw="$2"; shift 2 ;;
    --target-dir) target_dir="$2"; shift 2 ;;
    --work) work="$2"; shift 2 ;;
    --timeout) limit="$2"; shift 2 ;;
    --mem) mem="$2"; shift 2 ;;
    --fonts) fonts="$2"; shift 2 ;;
    --package) packages+=("-p" "$2"); shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

[ -n "$mingw" ] && [ -n "$target_dir" ] || { echo "give --mingw and --target-dir (see --help)" >&2; exit 2; }
for tool in wine64 xvfb-run jq cargo; do
  command -v "$tool" > /dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done
clang="$mingw/bin/x86_64-w64-mingw32-clang"
[ -x "$clang" ] || { echo "not found: $clang" >&2; exit 2; }
if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" = 1 ]; then
  echo "BYTESCAPE_DISPOSABLE_HOST is set; unset it. These runs must not change system state." >&2
  exit 2
fi

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
mkdir -p "$target_dir"
target_dir="$(cd "$target_dir" && pwd)"
work="${work:-$target_dir/wine-tests}"
mkdir -p "$work/logs"
work="$(cd "$work" && pwd)"

triple=x86_64-pc-windows-gnullvm
# The linker is set for this run only, through the environment.
export CARGO_TARGET_DIR="$target_dir"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER="$clang"
export CC_x86_64_pc_windows_gnullvm="$clang"
export AR_x86_64_pc_windows_gnullvm="$mingw/bin/llvm-ar"
export PATH="$mingw/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TERM_COLOR=never

echo "== building test programs for $triple (wine, not Windows)"
[ ${#packages[@]} -gt 0 ] || packages=(--workspace)
cargo test --locked --no-run --target "$triple" --message-format json \
  "${packages[@]}" \
  > "$work/build.json" 2> "$work/build.log" || {
  echo "the build failed; see $work/build.log" >&2
  tail -20 "$work/build.log" >&2
  exit 2
}

# One line per test program: executable, then the package folder it runs in.
jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .executable != null)
       | [.executable, (.manifest_path | sub("/Cargo.toml$"; ""))] | @tsv' \
  "$work/build.json" | sort -u > "$work/programs.tsv"
count="$(wc -l < "$work/programs.tsv")"
[ "$count" -gt 0 ] || { echo "no test programs found" >&2; exit 2; }
echo "== $count test programs"

export WINEPREFIX="$work/prefix"
rm -rf "$WINEPREFIX"
export WINEDEBUG=-all
# The test programs import libunwind.dll from the llvm-mingw runtime, so wine
# must find that folder (as a Windows path on the Z: drive).
runtime="$mingw/x86_64-w64-mingw32/bin"
[ -f "$runtime/libunwind.dll" ] || { echo "not found: $runtime/libunwind.dll" >&2; exit 2; }
export WINEPATH="Z:${runtime//\//\\}"
export WINEDLLOVERRIDES="mscoree,mshtml="
unset WAYLAND_DISPLAY BYTESCAPE_DISPOSABLE_HOST

# Without this, wine opens its debugger on a crash and waits for a person, so
# a test that crashes on purpose would run until the time limit.
wine64 reg add 'HKLM\Software\Microsoft\Windows NT\CurrentVersion\AeDebug' /v Debugger /t REG_SZ /d false /f > /dev/null 2>&1 < /dev/null || true
wine64 reg add 'HKLM\Software\Microsoft\Windows NT\CurrentVersion\AeDebug' /v Auto /t REG_SZ /d 1 /f > /dev/null 2>&1 < /dev/null || true
if [ -n "$fonts" ]; then
  mkdir -p "$WINEPREFIX/drive_c/windows/Fonts"
  find "$fonts" -name '*.ttf' -exec cp {} "$WINEPREFIX/drive_c/windows/Fonts/" \;
fi

cap=()
if command -v systemd-run > /dev/null && systemd-run --user --scope true 2> /dev/null; then
  cap=(systemd-run --user --scope --quiet -p "MemoryMax=$mem" -p MemorySwapMax=0)
else
  echo "warning: systemd-run --user is not available; no memory cap" >&2
fi

printf 'program\tpackage\tresult\tseconds\tpassed\tfailed\tdetail\n' > "$work/summary.tsv"
fails=0
while IFS=$'\t' read -r exe pkgdir; do
  name="$(basename "$exe" .exe)"
  pkg="$(basename "$pkgdir")"
  log="$work/logs/$pkg--$name.log"
  start=$SECONDS
  status=0
  (
    cd "$pkgdir"
    export CARGO_MANIFEST_DIR="$pkgdir" CARGO_PKG_NAME="$pkg"
    ${cap[@]+"${cap[@]}"} timeout --kill-after=10 "$limit" \
      env -u WAYLAND_DISPLAY xvfb-run -a wine64 "$exe" --test-threads=1
  ) > "$log" 2>&1 < /dev/null || status=$?
  secs=$((SECONDS - start))
  passed="$(sed -n 's/^test result: [a-zA-Z]*\. \([0-9]*\) passed; \([0-9]*\) failed.*/\1/p' "$log" | awk '{s+=$1} END {print s+0}')"
  failed="$(sed -n 's/^test result: [a-zA-Z]*\. \([0-9]*\) passed; \([0-9]*\) failed.*/\2/p' "$log" | awk '{s+=$1} END {print s+0}')"
  case "$status" in
    0) result=pass; detail="" ;;
    101) result=fail; detail="$(grep -m3 '^    [a-z_0-9:]*$\|panicked at' "$log" | tr '\n' ' ' | cut -c1-200)" || true ;;
    124|137) result=timeout; detail="over ${limit}s" ;;
    *) result=crash; detail="exit $status: $(grep -m1 -i 'err:\|fixme:\|unhandled\|exception' "$log" | cut -c1-160)" || true ;;
  esac
  [ "$result" = pass ] || fails=$((fails + 1))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$pkg" "$result" "$secs" "$passed" "$failed" "$detail" >> "$work/summary.tsv"
  printf '%-8s %-14s %-40s %4ss  passed %s failed %s\n' "$result" "$pkg" "$name" "$secs" "$passed" "$failed"
done < "$work/programs.tsv"

echo "== wine (not Windows): $((count - fails)) of $count programs passed; summary: $work/summary.tsv"
grep -l 'NOT RUN' "$work"/logs/*.log 2> /dev/null | sed 's/^/NOT RUN seen in /' || true
[ "$fails" -eq 0 ]
