#!/usr/bin/env bash
# btrfs tree-search reader benchmark: how long the reader takes to read a
# mounted btrfs file system, and how that compares with the directory walker
# on the same mount, both as root and both cold.
#
#   BYTESCAPE_DISPOSABLE_HOST=1 crates/bs-scan-btrfs/bench/btrfs-bench.sh --mount PATH [--runs N] [--walker-threads N]
#
# Before every cold run the whole page, dentry and inode cache is dropped
# (`sync; echo 3 > /proc/sys/vm/drop_caches`). It needs root, refuses to run
# without BYTESCAPE_DISPOSABLE_HOST=1, and runs the two tools interleaved: per
# round a cold run of each, then a warm run of each, straight after, with the
# caches the cold runs left. Mounts below PATH are not excluded: run it on a
# mount with none. The walker runs with an explicit thread count (--walker-threads,
# default: every logical CPU), recorded in the summary.
#
# The two do not count the same bytes where files share extents: the walker
# adds up what every file refers to, the reader counts each extent once. Both
# totals are printed; the script does not require them to agree, and says how
# much of the difference the reader attributes to sharing.
#
# Output: a method strip and a table of medians with minimum and maximum, in
# $BENCH_RESULTS (default ./btrfs-bench-results), and the raw runs beside it.
# summary.md is written even when a tool fails, with the failures listed.
# Exit status: 0 good, 2 usage, 3 not privileged, 4 a tool failed.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"

runs="${BENCH_RUNS:-10}"
walker_threads="${BENCH_WALKER_THREADS:-$(nproc)}"
results="${BENCH_RESULTS:-./btrfs-bench-results}"
mount_point=""

while [ $# -gt 0 ]; do
  case "$1" in
    --mount) mount_point="$2"; shift 2 ;;
    --runs) runs="$2"; shift 2 ;;
    --walker-threads) walker_threads="$2"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

[ -n "$mount_point" ] || { echo "give --mount PATH" >&2; exit 2; }
if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" != 1 ]; then
  echo "refusing to run: this empties the system's caches. Set BYTESCAPE_DISPOSABLE_HOST=1 on a throwaway machine." >&2
  exit 2
fi
if [ "$(id -u)" != 0 ] && ! sudo -n true 2>/dev/null; then
  echo "this needs root or passwordless sudo" >&2; exit 3
fi
if [ "$(stat -f -c %T "$mount_point")" != btrfs ]; then
  echo "$mount_point is not on btrfs" >&2; exit 2
fi

(cd "$repo" && cargo build --release --locked -p bs-scan-btrfs -p bs-scan --examples >&2)
target_dir="$(cd "$repo" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
scan="$target_dir/release/examples/scan"
walk="$target_dir/release/examples/walk"
for b in "$scan" "$walk"; do [ -x "$b" ] || { echo "not built: $b" >&2; exit 2; }; done

mkdir -p "$results"
runs_tsv="$results/runs.tsv"
failures="$results/failures.txt"
printf 'case\ttool\trun\tms\tallocated\n' > "$runs_tsv"
: > "$failures"
reader_last="$results/reader-last.txt"
: > "$reader_last"

as_root() { if [ "$(id -u)" = 0 ]; then "$@"; else sudo -n "$@"; fi; }
now_us() { echo "${EPOCHREALTIME/./}"; }
field() { sed -n "s/.*[ ]$1=\([0-9.]*\).*/\1/p" <<<" $2"; }
drop_all_caches() { sync; as_root sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }
fail() { echo "$1" | tee -a "$failures" >&2; }

time_tool() { # label tool cold
  local label="$1" tool="$2" cold="$3" out start end
  [ "$cold" = 1 ] && drop_all_caches
  start=$(now_us)
  if [ "$tool" = reader ]; then
    out="$(as_root "$scan" --quiet "$mount_point")" || { fail "reader ($label) failed"; return 0; }
  else
    out="$(as_root "$walk" --threads "$walker_threads" --sink tree "$mount_point")" || { fail "walker ($label) failed"; return 0; }
  fi
  end=$(now_us)
  local us=$(( end - start ))
  printf '%s\t%s\t%s\t%s.%03d\t%s\n' "$label" "$tool" "$cold" "$(( us / 1000 ))" "$(( us % 1000 ))" "$(field allocated "$out")" >> "$runs_tsv"
  [ "$tool" = reader ] && printf '%s\n' "$out" > "$reader_last"
  return 0
}

median() { sort -n | awk '{a[NR]=$1} END {if (NR==0) print "-"; else if (NR%2) print a[(NR+1)/2]; else printf "%.3f\n", (a[NR/2]+a[NR/2+1])/2}'; }

load_before="$(cut -d' ' -f1-3 /proc/loadavg)"

write_summary() {
  local status=$?
  set +e
  local part disk
  part="$(df --output=source "$mount_point" 2>/dev/null | tail -1)"
  part="$(readlink -f "$part" 2>/dev/null || echo "$part")"
  disk="$(lsblk -no PKNAME "$part" 2>/dev/null | head -1 || true)"
  disk="${disk:-${part##*/}}"
  {
    echo "# btrfs reader benchmark"
    echo
    echo "## Method"
    echo
    echo "- date: $(date -u +%Y-%m-%dT%H:%MZ)"
    echo "- commit: ${BENCH_COMMIT:-$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)}"
    echo "- CPU: $(sed -n 's/^model name\s*: //p' /proc/cpuinfo | head -1), $(nproc) logical cores"
    echo "- RAM: $(awk '/MemTotal/ {printf "%.0f GB", $2/1048576}' /proc/meminfo)"
    echo "- storage: $(lsblk -dno MODEL,ROTA "/dev/$disk" 2>/dev/null | tr -s ' ' || true) (second field ROTA: 0 = flash), transport $(lsblk -dno TRAN "/dev/$disk" 2>/dev/null || true); file system holding the data: btrfs on ${part:-unknown} (disk ${disk:-unknown}), mounted at $mount_point with $(findmnt -no OPTIONS "$mount_point" 2>/dev/null || echo unknown)"
    echo "- kernel: $(uname -sr)"
    echo "- rustc: $(rustc --version), release profile"
    echo "- data: $(df -h --output=used,itotal "$mount_point" 2>/dev/null | tail -1) used / inodes; allocated bytes found by the walker (first run): $(awk -F'\t' '$2=="walker" {print $5; exit}' "$runs_tsv"), by the reader (first run): $(awk -F'\t' '$2=="reader" {print $5; exit}' "$runs_tsv")"
    if [ -s "$reader_last" ]; then
      local last
      last="$(cat "$reader_last")"
      echo "- reader, last run: $(field trees "$last") subvolumes, $(field census_trees "$last") others read for sharing, $(field shared_files "$last") files share extents; load $(field load_ms "$last") ms, emit $(field emit_ms "$last") ms; memory limit $(field memory_cap "$last"), charged $(field memory_used "$last")"
    fi
    echo "- runs: $runs per case, tools interleaved round by round; median, with minimum and maximum"
    echo "- threads: reader 1 (one search at a time); walker $walker_threads"
    echo "- privileges: both tools ran as root"
    echo "- load average before / after: $load_before / $(cut -d' ' -f1-3 /proc/loadavg)"
    echo "- cache state: cold = sync and drop_caches=3 before every timed run; warm = straight after the cold runs of both tools, with the caches they left (the metadata is in the page cache)"
    echo
    if [ -s "$failures" ]; then
      echo "## Failures"
      echo
      sed 's/^/- /' "$failures"
      echo
    fi
    echo "## Results (milliseconds, wall clock)"
    echo
    echo "| case | tool | median | min | max |"
    echo "|---|---|---|---|---|"
    awk -F'\t' 'NR>1 {print $1 "\t" $2}' "$runs_tsv" | sort -u | while IFS=$'\t' read -r c t; do
      col="$(awk -F'\t' -v c="$c" -v t="$t" '$1==c && $2==t {print $4}' "$runs_tsv")"
      echo "| $c | $t | $(median <<<"$col") | $(sort -n <<<"$col" | head -1) | $(sort -n <<<"$col" | tail -1) |"
    done
  } > "$results/summary.md"
  cat "$results/summary.md"
  echo
  echo "raw runs: $runs_tsv"
  exit "$status"
}
trap write_summary EXIT

for _ in $(seq 1 "$runs"); do
  time_tool "mount cold" reader 1
  time_tool "mount cold" walker 1
  time_tool "mount warm" reader 0 # follows the cold run just made: the cache is full
  time_tool "mount warm" walker 0
done
[ ! -s "$failures" ] || exit 4
