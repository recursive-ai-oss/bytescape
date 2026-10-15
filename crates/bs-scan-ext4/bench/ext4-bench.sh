#!/usr/bin/env bash
# Raw ext4 reader benchmark: how long the reader takes to read a file system,
# and how that compares with the directory walker on the same file system.
#
#   crates/bs-scan-ext4/bench/ext4-bench.sh --image IMG [--source DIR] [--threads "1 2 4 8 16"] [--runs N]
#   BYTESCAPE_DISPOSABLE_HOST=1 crates/bs-scan-ext4/bench/ext4-bench.sh \
#       --device DEV --mount PATH [--runs N] [--walker-threads N]
#
# Image mode needs no privileges. It times the reader alone on an image file,
# warm (the file is in the page cache) and cold (the file's cached pages are
# dropped with posix_fadvise before every run, through `dd iflag=nocache`, so
# every block really comes from the device). Cold in this sense says nothing
# about the walker, whose cached directory entries and inodes cannot be
# dropped without root. With --source DIR (the directory the image was made
# from) the walker is timed warm on that tree beside the reader, for scale;
# the two scan different copies of the data, so their totals are shown and
# not compared.
#
# Device mode compares the reader on a mounted file system's block device with
# the walker scanning its mount point, both as root and both cold: before every
# timed run the whole page, dentry and inode cache is dropped
# (`echo 3 > drop_caches`). It needs root, refuses to run without
# BYTESCAPE_DISPOSABLE_HOST=1, and runs the two interleaved. Mounts below PATH
# are excluded from both. The walker runs with an explicit thread count
# (--walker-threads, default: every logical CPU), recorded in the summary.
#
# The two must report the same allocated bytes. The reader's total covers what
# its tree holds; the journal and other reserved inodes, inodes no directory
# names, and what lies under mounts are counted apart and printed beside it
# in the summary. The script compares the two totals and exits 1 when they
# differ by more than BENCH_TOLERANCE_PERCENT (default 1) of the walker's, since
# timings of tools that measured different data are not a comparison. The
# file system is live, so a small difference is expected on a busy machine;
# the largest one seen is printed.
#
# Output: a method strip and a table of medians with minimum and maximum, in
# $BENCH_RESULTS (default ./ext4-bench-results), and the raw runs beside it.
# summary.md is written even when a tool fails, with the failures listed.
# Exit status: 0 good, 1 totals differ, 2 usage, 3 not privileged, 4 a tool
# failed.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"

runs="${BENCH_RUNS:-10}"
threads_list="${BENCH_THREADS:-1 2 4 8 16}"
walker_threads="${BENCH_WALKER_THREADS:-$(nproc)}"
tolerance_percent="${BENCH_TOLERANCE_PERCENT:-1}"
results="${BENCH_RESULTS:-./ext4-bench-results}"
image=""
device=""
mount_point=""
source_dir=""

while [ $# -gt 0 ]; do
  case "$1" in
    --image) image="$2"; shift 2 ;;
    --device) device="$2"; shift 2 ;;
    --mount) mount_point="$2"; shift 2 ;;
    --source) source_dir="$2"; shift 2 ;;
    --runs) runs="$2"; shift 2 ;;
    --threads) threads_list="$2"; shift 2 ;;
    --walker-threads) walker_threads="$2"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

if [ -n "$image" ] && [ -n "$device" ]; then
  echo "give --image or --device, not both" >&2; exit 2
fi
if [ -z "$image" ] && [ -z "$device" ]; then
  echo "give --image IMG, or --device DEV --mount PATH" >&2; exit 2
fi
if [ -n "$device" ]; then
  [ -n "$mount_point" ] || { echo "--device needs --mount PATH" >&2; exit 2; }
  if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" != 1 ]; then
    echo "refusing to run: this empties the system's caches and reads a whole device. Set BYTESCAPE_DISPOSABLE_HOST=1 on a throwaway machine." >&2
    exit 2
  fi
  if [ "$(id -u)" != 0 ] && ! sudo -n true 2>/dev/null; then
    echo "device mode needs root or passwordless sudo" >&2; exit 3
  fi
fi

(cd "$repo" && cargo build --release --locked -p bs-scan-ext4 -p bs-scan --examples >&2)
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

drop_image_cache() { dd if="$image" iflag=nocache count=0 2>/dev/null; }
drop_all_caches() { sync; as_root sh -c 'echo 3 > /proc/sys/vm/drop_caches'; }

skip_args=()
if [ -n "$device" ]; then
  # Mount points below the file system's, which neither tool may enter.
  mapfile -t others < <(awk -v root="$mount_point" '
    { mp = $5; gsub(/\\040/, " ", mp) }
    mp != root && index(mp, root == "/" ? "/" : root "/") == 1 { print mp }' /proc/self/mountinfo | sort -u)
  for m in "${others[@]}"; do
    rel="${m#"$mount_point"}"; rel="${rel#/}"
    skip_args+=(--skip "$rel")
  done
fi

# A failed tool is recorded and the run goes on, so that the summary still
# says what was measured.
fail() { echo "$1" | tee -a "$failures" >&2; }

time_reader() { # label threads cold
  local label="$1" t="$2" cold="$3" out start end
  if [ "$cold" = 1 ]; then
    if [ -n "$device" ]; then drop_all_caches; else drop_image_cache; fi
  fi
  start=$(now_us)
  if [ -n "$device" ]; then
    out="$(as_root "$scan" --quiet --threads "$t" "${skip_args[@]}" "$device")" \
      || { fail "reader ($label, $t threads) failed"; return 0; }
  else
    out="$("$scan" --quiet --threads "$t" "$image")" \
      || { fail "reader ($label, $t threads) failed"; return 0; }
  fi
  end=$(now_us)
  local us=$(( end - start ))
  printf '%s\t%s\t%s\t%s.%03d\t%s\n' "$label" "reader-$t" "$3" "$(( us / 1000 ))" "$(( us % 1000 ))" "$(field allocated "$out")" >> "$runs_tsv"
  printf '%s\n' "$out" > "$reader_last"
}

time_walker() { # label cold path
  local label="$1" cold="$2" path="$3" out start end
  [ "$cold" = 1 ] && drop_all_caches
  start=$(now_us)
  if [ -n "$device" ]; then
    out="$(as_root "$walk" --threads "$walker_threads" --sink tree "$path")" \
      || { fail "walker ($label) failed"; return 0; }
  else
    out="$("$walk" --threads "$walker_threads" --sink tree "$path")" \
      || { fail "walker ($label) failed"; return 0; }
  fi
  end=$(now_us)
  local us=$(( end - start ))
  printf '%s\t%s\t%s\t%s.%03d\t%s\n' "$label" "walker" "$cold" "$(( us / 1000 ))" "$(( us % 1000 ))" "$(field allocated "$out")" >> "$runs_tsv"
}

median() { sort -n | awk '{a[NR]=$1} END {if (NR==0) print "-"; else if (NR%2) print a[(NR+1)/2]; else printf "%.3f\n", (a[NR/2]+a[NR/2+1])/2}'; }

load_before="$(cut -d' ' -f1-3 /proc/loadavg)"
mismatch=0
deviation_note=""

write_summary() {
  local status=$?
  set +e
  # Where the data lives, resolved through symlinks (the root device may be
  # /dev/root or /dev/block/MAJ:MIN), and the disk under it. Every lookup
  # may fail on an unusual machine; the summary then says "unknown".
  local src part disk
  src="${device:-$image}"
  part="$(df --output=source "$src" 2>/dev/null | tail -1)"
  [ -n "$device" ] && part="$device"
  part="$(readlink -f "$part" 2>/dev/null || echo "$part")"
  disk="$(lsblk -no PKNAME "$part" 2>/dev/null | head -1 || true)"
  disk="${disk:-${part##*/}}"
  {
    echo "# ext4 reader benchmark"
    echo
    echo "## Method"
    echo
    echo "- date: $(date -u +%Y-%m-%dT%H:%MZ)"
    echo "- commit: ${BENCH_COMMIT:-$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)}"
    echo "- CPU: $(sed -n 's/^model name\s*: //p' /proc/cpuinfo | head -1), $(nproc) logical cores"
    echo "- RAM: $(awk '/MemTotal/ {printf "%.0f GB", $2/1048576}' /proc/meminfo)"
    echo "- storage: $(lsblk -dno MODEL,ROTA "/dev/$disk" 2>/dev/null | tr -s ' ' || true) (second field ROTA: 0 = flash), transport $(lsblk -dno TRAN "/dev/$disk" 2>/dev/null || true); file system holding the data: $(stat -f -c %T "$src" 2>/dev/null || echo unknown) on ${part:-unknown} (disk ${disk:-unknown})"
    echo "- kernel: $(uname -sr)"
    echo "- rustc: $(rustc --version), release profile"
    if [ -n "$image" ]; then
      echo "- data: image file $image ($(du -h --apparent-size "$image" 2>/dev/null | cut -f1) apparent, $(du -h "$image" 2>/dev/null | cut -f1) on disk)"
    else
      echo "- data: block device $device mounted at $mount_point; mounts below it excluded: $(( ${#skip_args[@]} / 2 ))"
    fi
    echo "- allocated bytes found by the walker (first run): $(awk -F'\t' '$2=="walker" {print $5; exit}' "$runs_tsv")"
    echo "- allocated bytes found by the reader (first run): $(awk -F'\t' '$2 ~ /^reader/ {print $5; exit}' "$runs_tsv")"
    if [ -s "$reader_last" ]; then
      local last
      last="$(cat "$reader_last")"
      echo "- reader, apart from its tree (bytes, last run): reserved inodes $(field reserved "$last"), unreachable inodes $(field unreachable "$last"), under mounts $(field hidden "$last"); memory limit $(field memory_cap "$last"); needs recovery: $(sed -n 's/.*needs_recovery=\([a-z]*\).*/\1/p' <<<"$last")"
    fi
    echo "- runs: $runs per case, tools interleaved run by run; median, with minimum and maximum"
    echo "- threads: reader $threads_list; walker $walker_threads"
    if [ -n "$device" ]; then
      echo "- privileges: both tools ran as root"
    else
      echo "- privileges: reader unprivileged on the image; walker unprivileged on its source directory"
    fi
    echo "- load average before / after: $load_before / $(cut -d' ' -f1-3 /proc/loadavg)"
    if [ -n "$device" ]; then
      echo "- cache state: cold = sync and drop_caches=3 before every timed run; warm = one untimed walk first"
    else
      echo "- cache state: cold = the image file's cached pages dropped by posix_fadvise before every run (not the dentry or inode cache of any other file); warm = after an untimed run"
    fi
    if [ -n "$device" ]; then
      echo "- totals check: tolerance $tolerance_percent% of the walker's total; $deviation_note"
    fi
    echo
    if [ -s "$failures" ]; then
      echo "## Failures"
      echo
      sed 's/^/- /' "$failures"
      echo
    fi
    if [ "$mismatch" = 1 ]; then
      echo "## Totals differ"
      echo
      echo "The reader and the walker reported different allocated totals, so the timings below are not a comparison."
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

if [ -n "$image" ]; then
  # One untimed warm-up run primes the page cache for the warm case.
  "$scan" --quiet "$image" >/dev/null || fail "reader warm-up failed"
  [ -z "$source_dir" ] || "$walk" --threads "$walker_threads" --sink tree "$source_dir" >/dev/null \
    || fail "walker warm-up failed"
  for _ in $(seq 1 "$runs"); do
    for t in $threads_list; do
      time_reader "image warm" "$t" 0
      time_reader "image cold" "$t" 1
    done
    if [ -n "$source_dir" ]; then time_walker "image warm" 0 "$source_dir"; fi
  done
else
  for _ in $(seq 1 "$runs"); do
    for cold in 1 0; do
      label=$([ "$cold" = 1 ] && echo "device cold" || echo "device warm")
      [ "$cold" = 0 ] && { as_root "$walk" --threads "$walker_threads" --sink tree "$mount_point" >/dev/null || true; }
      for t in $threads_list; do time_reader "$label" "$t" "$cold"; done
      time_walker "$label" "$cold" "$mount_point"
    done
  done
  # Both tools must have measured the same data. The reference is the walker's
  # median total; the largest deviation of any run is printed.
  reference="$(awk -F'\t' '$2=="walker" && $5!="" {print $5}' "$runs_tsv" | median)"
  reference="${reference%%.*}"
  if [ "$reference" = "-" ]; then
    deviation_note="no walker run produced a total"
    mismatch=1
  else
    worst="$(awk -F'\t' -v ref="$reference" 'NR>1 && $5!="" { d = ($5 > ref) ? $5 - ref : ref - $5; if (d > w) { w = d; who = $2 " " $1 } } END { printf "%d %s", w, who }' "$runs_tsv")"
    worst_bytes="${worst%% *}"
    limit="$(awk -v ref="$reference" -v p="$tolerance_percent" 'BEGIN { printf "%d", ref * p / 100 }')"
    deviation_note="walker median $reference B; largest deviation of any run $worst_bytes B (${worst#* }); allowed $limit B"
    if [ "$worst_bytes" -gt "$limit" ]; then mismatch=1; fi
  fi
fi

if [ "$mismatch" = 1 ]; then
  echo "FAILED: the reader's and the walker's allocated totals differ by more than $tolerance_percent%; the timings are not a valid comparison." >&2
  exit 1
fi
if [ -s "$failures" ]; then exit 4; fi
