#!/usr/bin/env bash
# Walker throughput benchmark: how fast the walker reads a directory tree, next
# to the best existing tools. One command, no arguments needed.
#
#   crates/bs-scan/bench/walker-bench.sh [options] [DATASET...]
#
# Without datasets the script builds one itself (`mktree`, deterministic) and
# measures that. With datasets it measures those, which must each lie on one
# file system with no mounts below them, because not every tool can stay on one
# file system.
#
# Cache state. When BYTESCAPE_DISPOSABLE_HOST=1 is set the benchmark is cold:
# before every timed run it empties the file cache,
#     sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
# so every run reads from the device. That needs passwordless sudo (or root),
# and the script refuses to start without them rather than quietly measuring
# something else. Without BYTESCAPE_DISPOSABLE_HOST=1 it never touches the
# cache and measures warm: one untimed run of each tool primes it. --warm
# forces warm even on a disposable host.
#
# Options (each also settable through the environment variable in brackets):
#   --runs N          timed runs per tool and dataset, default 10   [BENCH_RUNS]
#   --threads N       thread budget for every multi-threaded tool,
#                     default: all logical CPUs                      [BENCH_THREADS]
#   --taskset CPUS    run every command under `taskset -c CPUS`      [BENCH_TASKSET]
#   --files N         size of the generated dataset, default 200000  [BENCH_FILES]
#   --warm            measure warm even when BYTESCAPE_DISPOSABLE_HOST=1
#
# Environment:
#   BENCH_RESULTS     directory for the raw runs and the summary
#                     (default ./walker-bench-results); existing files in it
#                     are kept
#   BENCH_TOOLS       where the reference tools live, installed there at the
#                     pinned versions if missing (default <target>/bench-tools)
#   BENCH_DATASET_DIR where the generated dataset goes
#                     (default <target>/bench-dataset); reused when it was
#                     made with the same size
#   BENCH_COMMIT      commit to print in the method section
#
# Tools compared, all on the same datasets with the same thread budget:
#   bs-scan (tree)  the walker, applier thread and final snapshot: what the
#                   application does
#   bs-scan (walk)  the walker alone, batches discarded
#   diskus          multi-threaded, hard links counted once
#   dua             `dua aggregate`, multi-threaded, hard links counted once
#   du              GNU coreutils `du -s`, single-threaded
#
# Protocol: N timed runs per tool and dataset, the tools interleaved run by run
# so that slow drift on a shared machine affects them all alike. Wall-clock time
# is taken around the whole command. Every tool's reported total is compared
# with the walker's.

set -euo pipefail

DISKUS_VERSION=0.9.0
DUA_VERSION=2.45.1

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"

runs="${BENCH_RUNS:-10}"
threads="${BENCH_THREADS:-$(nproc)}"
taskset_cpus="${BENCH_TASKSET:-}"
files="${BENCH_FILES:-200000}"
results="${BENCH_RESULTS:-./walker-bench-results}"
force_warm=0
datasets=()

while [ $# -gt 0 ]; do
  case "$1" in
    --runs) runs="$2"; shift 2 ;;
    --threads) threads="$2"; shift 2 ;;
    --taskset) taskset_cpus="$2"; shift 2 ;;
    --files) files="$2"; shift 2 ;;
    --warm) force_warm=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) datasets+=("$1"); shift ;;
  esac
done

cold=0
if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" = 1 ] && [ "$force_warm" = 0 ]; then
  cold=1
  if [ "$(id -u)" != 0 ] && ! sudo -n true 2>/dev/null; then
    echo "cold-cache run needs root or passwordless sudo to write /proc/sys/vm/drop_caches; neither is available" >&2
    exit 3
  fi
fi

(cd "$repo" && cargo build --release --locked -p bs-scan --examples >&2)
target_dir="$(cd "$repo" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
walk="$target_dir/release/examples/walk"
mktree="$target_dir/release/examples/mktree"
for b in "$walk" "$mktree"; do
  [ -x "$b" ] || { echo "not built: $b" >&2; exit 2; }
done

tools="${BENCH_TOOLS:-$target_dir/bench-tools}"
diskus="$tools/bin/diskus"
dua="$tools/bin/dua"
if [ ! -x "$diskus" ]; then cargo install --locked --root "$tools" diskus --version "$DISKUS_VERSION" >&2; fi
if [ ! -x "$dua" ]; then cargo install --locked --root "$tools" dua-cli --version "$DUA_VERSION" >&2; fi

# GNU du: `du` may be another implementation.
gnu_du=""
for c in du gnudu; do
  if command -v "$c" >/dev/null && "$c" --version 2>/dev/null | grep -q 'GNU coreutils'; then
    gnu_du="$(command -v "$c")"; break
  fi
done
[ -n "$gnu_du" ] || { echo "GNU du not found" >&2; exit 2; }

generated=""
if [ ${#datasets[@]} -eq 0 ]; then
  generated="${BENCH_DATASET_DIR:-$target_dir/bench-dataset}"
  "$mktree" "$generated" --files "$files" >&2
  datasets=("$generated")
fi

mkdir -p "$results"
runs_tsv="$results/runs.tsv"
printf 'dataset\ttool\trun\tms\tbytes\n' > "$runs_tsv"

prefix=()
[ -n "$taskset_cpus" ] && prefix=(taskset -c "$taskset_cpus")

drop_caches() {
  sync
  if [ "$(id -u)" = 0 ]; then echo 3 > /proc/sys/vm/drop_caches; else echo 3 | sudo -n tee /proc/sys/vm/drop_caches >/dev/null; fi
}

tool_names=(bs-scan-tree bs-scan-walk diskus dua du)
tool_cmd() { # name path -> prints the command line
  local p="$2"
  case "$1" in
    bs-scan-tree) echo "$walk --threads $threads --sink tree $p" ;;
    bs-scan-walk) echo "$walk --threads $threads --sink drop $p" ;;
    diskus) echo "$diskus -j $threads $p" ;;
    dua) echo "$dua -f bytes -x -t $threads aggregate --no-sort --no-total $p" ;;
    du) echo "$gnu_du -s -B1 -x $p" ;;
  esac
}

# Prints the byte total a tool reported, from its output.
tool_bytes() { # name path output
  case "$1" in
    bs-scan-tree|bs-scan-walk) sed -n 's/.*allocated=\([0-9]*\).*/\1/p' <<<"$3" ;;
    diskus) awk '{print $1; exit}' <<<"$3" ;;
    # dua lists the entries of a directory; the directory's own blocks are not
    # in the list, so they are added back.
    dua)
      local own
      own=$(( $(stat -c %b "$2") * 512 ))
      awk -v own="$own" '{s += $1} END {print s + own}' <<<"$3" ;;
    du) awk '{print $1; exit}' <<<"$3" ;;
  esac
}

now_us() { echo "${EPOCHREALTIME/./}"; }

time_one() { # name path run -> appends to runs.tsv
  local name="$1" path="$2" run="$3" cmd start end output
  cmd="$(tool_cmd "$name" "$path")"
  [ "$cold" = 1 ] && drop_caches
  start=$(now_us)
  # shellcheck disable=SC2086
  output="$("${prefix[@]}" $cmd 2>/dev/null)"
  end=$(now_us)
  local us=$(( end - start ))
  printf '%s\t%s\t%s\t%s.%03d\t%s\n' "$path" "$name" "$run" "$(( us / 1000 ))" "$(( us % 1000 ))" "$(tool_bytes "$name" "$path" "$output")" >> "$runs_tsv"
}

load_before="$(cut -d' ' -f1-3 /proc/loadavg)"

for ds in "${datasets[@]}"; do
  ds="$(readlink -f "$ds")"
  if [ "$cold" != 1 ]; then
    # Warm: one untimed run of each tool primes the caches.
    for name in "${tool_names[@]}"; do
      cmd="$(tool_cmd "$name" "$ds")"
      # shellcheck disable=SC2086
      "${prefix[@]}" $cmd >/dev/null 2>&1 || true
    done
  else
    sync
  fi
  for run in $(seq 1 "$runs"); do
    for name in "${tool_names[@]}"; do
      time_one "$name" "$ds" "$run"
    done
  done
done

# ---- report -------------------------------------------------------------------

median() { sort -n | awk '{a[NR]=$1} END {if (NR%2) print a[(NR+1)/2]; else print (a[NR/2]+a[NR/2+1])/2}'; }

mismatch=0
{
  echo "# Walker throughput"
  echo
  echo "## Method"
  echo
  echo "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "- commit: ${BENCH_COMMIT:-$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)$(git -C "$repo" status --porcelain -- crates/bs-scan crates/bs-tree 2>/dev/null | grep -q . && echo ' + uncommitted changes')}"
  echo "- CPU: $(lscpu | sed -n 's/^Model name: *//p'), $(nproc) logical CPUs available$( [ -n "$taskset_cpus" ] && echo ", every command under taskset -c $taskset_cpus")"
  echo "- RAM: $(awk '/MemTotal/ {printf "%.0f GiB", $2/1048576}' /proc/meminfo)"
  echo "- kernel: $(uname -sr)"
  echo "- rustc: $(rustc --version), build profile: release (lto = fat, codegen-units = 1)"
  echo "- thread budget: $threads for every multi-threaded tool; GNU du is single-threaded"
  echo "- host load average before the runs: $load_before (after: see the end); a shared host makes timings noisy"
  if [ "$cold" = 1 ]; then
    echo "- cache state: COLD. Before every timed run: sync; echo 3 | sudo tee /proc/sys/vm/drop_caches (page cache, dentries and inodes)"
  else
    echo "- cache state: WARM. One untimed run of each tool primed the caches; nothing was dropped. These are not cold-cache numbers"
  fi
  echo "- protocol: $runs timed runs per tool and dataset, tools interleaved run by run; median with min and max; wall-clock around the whole process"
  echo "- tools:"
  echo "  - bs-scan: \`walk --threads $threads --sink tree|drop PATH\` (walker, applier and snapshot / walker only)"
  echo "  - $("$diskus" --version): \`diskus -j $threads PATH\` (disk usage, directories included)"
  echo "  - $("$dua" --version): \`dua -f bytes -x -t $threads aggregate --no-sort --no-total PATH\`"
  echo "  - $("$gnu_du" --version | head -1): \`du -s -B1 -x PATH\`"
  echo "- storage:"
  for ds in "${datasets[@]}"; do
    ds="$(readlink -f "$ds")"
    src="$(findmnt -no SOURCE -T "$ds" 2>/dev/null || echo unknown)"
    fst="$(findmnt -no FSTYPE -T "$ds" 2>/dev/null || echo unknown)"
    disk="$(lsblk -no PKNAME "$src" 2>/dev/null | head -1)"
    [ -n "$disk" ] || disk="$(basename "$src")"
    model="$(lsblk -dno MODEL "/dev/$disk" 2>/dev/null | sed 's/ *$//')"
    rota="$(cat "/sys/block/$disk/queue/rotational" 2>/dev/null || echo ?)"
    echo "  - $ds: $fst on $src, device ${model:-unknown} ($([ "$rota" = 0 ] && echo 'non-rotational' || ([ "$rota" = 1 ] && echo rotational || echo 'type unknown')))"
  done
  echo
  echo "## Datasets"
  echo
  if [ -n "$generated" ]; then
    echo "Generated by \`mktree $generated --files $files\` (deterministic: about $files files, some hard links and symbolic links, up to 6 levels deep)."
  else
    echo "Existing trees, not generated."
  fi
  echo "Counts are from the walker alone."
  echo
  echo "| dataset | entries | directories | allocated bytes |"
  echo "|---|---|---|---|"
  for ds in "${datasets[@]}"; do
    ds="$(readlink -f "$ds")"
    line="$("$walk" --threads "$threads" --sink drop "$ds")"
    f="$(sed -n 's/.*files=\([0-9]*\).*/\1/p' <<<"$line")"; d="$(sed -n 's/.*dirs=\([0-9]*\).*/\1/p' <<<"$line")"
    a="$(sed -n 's/.*allocated=\([0-9]*\).*/\1/p' <<<"$line")"
    echo "| $ds | $((f + d)) | $d | $a |"
  done
  echo
  echo "## Results (milliseconds, wall clock, n = $runs)"
  echo
  for ds in "${datasets[@]}"; do
    ds="$(readlink -f "$ds")"
    echo "### $ds"
    echo
    echo "| tool | median | min | max | reported bytes | vs bs-scan |"
    echo "|---|---|---|---|---|---|"
    ours="$(awk -F'\t' -v d="$ds" '$1==d && $2=="bs-scan-tree" {print $5; exit}' "$runs_tsv")"
    for name in "${tool_names[@]}"; do
      col="$(awk -F'\t' -v d="$ds" -v t="$name" '$1==d && $2==t {print $4}' "$runs_tsv")"
      med="$(median <<<"$col")"; mn="$(sort -n <<<"$col" | head -1)"; mx="$(sort -n <<<"$col" | tail -1)"
      bytes="$(awk -F'\t' -v d="$ds" -v t="$name" '$1==d && $2==t {print $5; exit}' "$runs_tsv")"
      distinct="$(awk -F'\t' -v d="$ds" -v t="$name" '$1==d && $2==t {print $5}' "$runs_tsv" | sort -u | wc -l)"
      verdict="equal"
      if [ "$bytes" != "$ours" ]; then
        verdict="differs by $(( bytes - ours ))"
        # Timings of tools that disagree about the total are not a comparison.
        [ "$name" = bs-scan-walk ] || mismatch=1
      fi
      [ "$name" = bs-scan-walk ] && verdict="counts every name of a hard link: $(( bytes - ours )) more"
      if [ "$distinct" != 1 ]; then
        verdict="$verdict; the tree changed between runs ($distinct different totals)"
        mismatch=1
      fi
      echo "| $name | $med | $mn | $mx | $bytes | $verdict |"
    done
    echo
  done
  echo "Host load average after the runs: $(cut -d' ' -f1-3 /proc/loadavg)"
} > "$results/summary.md"

cat "$results/summary.md"
echo
echo "raw runs: $runs_tsv"
if [ "$mismatch" = 1 ]; then
  echo "FAILED: a tool's total differs from the walker's, or the tree changed during the runs; the timings above are not a valid comparison." >&2
  exit 1
fi
