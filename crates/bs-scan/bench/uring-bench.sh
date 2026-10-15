#!/usr/bin/env bash
# io_uring statx experiment: the walker next to the same walker with its
# per-entry statx calls batched through an io_uring. Benchmark only; nothing
# here ships.
#
#   crates/bs-scan/bench/uring-bench.sh [options] [DATASET...]
#
# Without datasets it measures /usr and a generated tree of BENCH_ENTRIES
# (default 1,000,000) empty files, made in BENCH_DATASET_DIR.
#
# Cache state. When BYTESCAPE_DISPOSABLE_HOST=1 is set the run is cold: before
# every timed run the benchmark does `sync; echo 3 > /proc/sys/vm/drop_caches`
# (page cache, dentries, inodes). That needs root; the benchmark exits with a
# failure rather than quietly measuring warm. Without the variable it never
# touches the cache and measures warm (one untimed run of each variant first).
#
# Options (each also settable through the variable in brackets):
#   --runs N         timed runs per variant, default 10           [BENCH_RUNS]
#   --threads LIST   walker thread counts, default 4,16           [BENCH_THREADS]
#   --sink S         drop (walker alone, default) or tree         [BENCH_SINK]
#   --depths LIST    ring depths, default 32,128,512              [BENCH_DEPTHS]
#   --async M        off, on or both (IOSQE_ASYNC), default both  [BENCH_ASYNC]
#
# Environment:
#   BENCH_RESULTS      where the summary and raw runs go (default
#                      ./uring-bench-results)
#   BENCH_DATASET_DIR  where the generated tree goes (default <target>/uring-dataset)
#   BENCH_ENTRIES      size of the generated tree
#   BENCH_COMMIT       commit to print in the method section
#
# Protocol: N timed runs per variant, the variants interleaved run by run so
# that slow drift on a shared machine affects them all alike; wall clock around
# each scan and the CPU time the process used (kernel worker threads included).
# Every variant must report the walker's totals or the run fails.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"

runs="${BENCH_RUNS:-10}"
threads="${BENCH_THREADS:-4,16}"
sink="${BENCH_SINK:-drop}"
depths="${BENCH_DEPTHS:-32,128,512}"
asyncs="${BENCH_ASYNC:-both}"
entries="${BENCH_ENTRIES:-1000000}"
results="${BENCH_RESULTS:-./uring-bench-results}"
datasets=()

while [ $# -gt 0 ]; do
  case "$1" in
    --runs) runs="$2"; shift 2 ;;
    --threads) threads="$2"; shift 2 ;;
    --sink) sink="$2"; shift 2 ;;
    --depths) depths="$2"; shift 2 ;;
    --async) asyncs="$2"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) datasets+=("$1"); shift ;;
  esac
done

cold=()
cold_text="WARM. One untimed run of each variant primed the caches; nothing was dropped. These are not cold-cache numbers"
if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" = 1 ]; then
  if [ "$(id -u)" != 0 ]; then
    echo "a cold run needs root to write /proc/sys/vm/drop_caches" >&2
    exit 3
  fi
  cold=(--cold)
  cold_text="COLD. Before every timed run: sync; echo 3 > /proc/sys/vm/drop_caches (page cache, dentries and inodes)"
fi

(cd "$repo" && cargo build --release --locked -p bs-scan --features iouring-bench --examples >&2)
target_dir="$(cd "$repo" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
bench="$target_dir/release/examples/uring_bench"
[ -x "$bench" ] || { echo "not built: $bench" >&2; exit 2; }

generated=""
if [ ${#datasets[@]} -eq 0 ]; then
  generated="${BENCH_DATASET_DIR:-$target_dir/uring-dataset}"
  "$bench" --make "$generated" --entries "$entries" >&2
  datasets=(/usr "$generated")
fi

mkdir -p "$results"
load_before="$(cut -d' ' -f1-3 /proc/loadavg)"
status=0
"$bench" --runs "$runs" --threads "$threads" --sink "$sink" --depths "$depths" --async "$asyncs" \
  "${cold[@]}" --tsv "$results/runs.tsv" "${datasets[@]}" > "$results/tables.md" || status=$?

{
  echo "# io_uring statx experiment"
  echo
  echo "## Method"
  echo
  echo "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "- commit: ${BENCH_COMMIT:-$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)$(git -C "$repo" status --porcelain -- crates/bs-scan 2>/dev/null | grep -q . && echo ' + uncommitted changes')}"
  echo "- CPU: $(lscpu | sed -n 's/^Model name: *//p'), $(nproc) logical CPUs"
  echo "- RAM: $(awk '/MemTotal/ {printf "%.0f GiB", $2/1048576}' /proc/meminfo)"
  echo "- kernel: $(uname -sr)"
  echo "- rustc: $(rustc --version), build profile: release"
  echo "- walker threads: $threads; ring depths: $depths; IOSQE_ASYNC: $asyncs; sink: $sink"
  echo "- host load average before the runs: $load_before (after: see the tables); a shared host makes timings noisy"
  echo "- cache state: $cold_text"
  echo "- protocol: $runs timed runs per variant, variants interleaved run by run; median with min and max; wall clock around each scan"
  echo "- storage:"
  for ds in "${datasets[@]}"; do
    ds="$(readlink -f "$ds")"
    src="$(findmnt -no SOURCE -T "$ds" 2>/dev/null || echo unknown)"
    fst="$(findmnt -no FSTYPE -T "$ds" 2>/dev/null || echo unknown)"
    disk="$(lsblk -no PKNAME "$src" 2>/dev/null | head -1)"
    [ -n "$disk" ] || disk="$(basename "$src")"
    model="$(lsblk -dno MODEL "/dev/$disk" 2>/dev/null | sed 's/ *$//')"
    rota="$(cat "/sys/block/$disk/queue/rotational" 2>/dev/null || echo ?)"
    if [ "$rota" = 0 ]; then kind='non-rotational'; elif [ "$rota" = 1 ]; then kind='rotational'; else kind='type unknown'; fi
    echo "  - $ds: $fst on $src, device ${model:-unknown} ($kind)"
  done
  echo
  echo "## Datasets"
  echo
  if [ -n "$generated" ]; then
    echo "- $generated: made by \`uring_bench --make $generated --entries $entries\` (deterministic: empty files, about 100 per directory, up to 6 levels)."
    echo "- /usr: the system's own tree, as it was."
  else
    echo "Existing trees, not generated."
  fi
  echo
  echo "## Results (milliseconds, n = $runs)"
  echo
  cat "$results/tables.md"
} > "$results/summary.md"

cat "$results/summary.md"
echo
echo "raw runs: $results/runs.tsv"
exit "$status"
