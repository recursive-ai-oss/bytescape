#!/usr/bin/env bash
# Measures the first-frame time and the zoom frame times, on the whole machine
# and on four pinned cores, and writes results.json and summary.md.
#
#   crates/bs-gate/run.sh OUTPUT_DIR
#
# Environment:
#   CARGO_TARGET_DIR  where to build (default: <repo>/target)
#   PROXY_CPUS        core list for the laptop proxy (default: 0-3)
#   EXTRA_CPUS        more core lists to measure, space separated, for example
#                     "0 0-1" for one-core and two-core stress runs
#   WRAP              a command prefix for every step, for example a memory cap
#   BACKEND           "software" (default) measures the software renderer only.
#                     "wgpu" also measures the GPU backend on the synthetic
#                     scene: a windowed run inside the virtual display and an
#                     offscreen one, on the full machine and on the pinned
#                     cores. Both are paced in real time.
#   WGPU_ADAPTER      "software" (default) forces a software adapter
#                     (llvmpipe), which is what a virtual display offers and
#                     says nothing about a graphics card. "default" lets the
#                     system choose; the windowed run then needs a display
#                     server that a graphics driver can present to.
#   WGPU_SIZE         window size of the GPU runs (default 3840x2160)
#   REAL_PATH         a big real folder to scan for the real-data series: the
#                     first frame of `bytescape <path>`, the zoom on the
#                     finished scan and the live map while it scans (default:
#                     your home folder; pick a folder with a million or more
#                     entries for a meaningful run). Set it to an empty string
#                     to measure only the synthetic scene.
#
# Windowed runs go through Xvfb, never the desktop session. Xvfb runs with
# -noreset: without it the server restarts when a client exits, and the next
# launch waits for the restart, which is not what a real display does.
set -euo pipefail

OUT=$(realpath -m "${1:?usage: run.sh OUTPUT_DIR}")
REPO=$(cd "$(dirname "$0")/../.." && pwd)
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
PROXY_CPUS="${PROXY_CPUS:-0-3}"
EXTRA_CPUS="${EXTRA_CPUS:-}"
WRAP="${WRAP:-}"
REAL_PATH="${REAL_PATH-$HOME}"
BACKEND="${BACKEND:-software}"
WGPU_ADAPTER="${WGPU_ADAPTER:-software}"
WGPU_SIZE="${WGPU_SIZE:-3840x2160}"

(cd "$REPO" && $WRAP cargo build --release -j "${JOBS:-8}" -p bytescape -p bs-gate --bins)
BIN="$CARGO_TARGET_DIR/release/bytescape"
GATE="$CARGO_TARGET_DIR/release/bs-gate"
GPU_ZOOM="$CARGO_TARGET_DIR/release/gpu_zoom"
SCREEN="-screen 0 3840x2160x24 -noreset"

mkdir -p "$OUT"
rm -f "$OUT"/*.kv
echo "load average before measuring: $(cut -d' ' -f1-3 /proc/loadavg)"

measure() {
  local label=$1
  shift
  env -u WAYLAND_DISPLAY $WRAP "$@" xvfb-run -a -s "$SCREEN" \
    "$GATE" run --label "$label" --bytescape "$BIN" --out "$OUT" --repo "$REPO" \
    --real "$REAL_PATH"
}

# Adds the GPU backend's zoom series to the result file of `label`. The
# windowed run goes through the virtual display; the offscreen one needs no
# display at all.
measure_wgpu() {
  local label=$1
  shift
  local adapter=()
  [ "$WGPU_ADAPTER" = software ] && adapter=(--software-adapter)
  local part="$OUT/$label.wgpu.out"
  : >"$part"
  env -u WAYLAND_DISPLAY $WRAP "$@" xvfb-run -a -s "$SCREEN" \
    "$GPU_ZOOM" --windowed --paced --size "$WGPU_SIZE" "${adapter[@]}" >>"$part" || true
  env -u WAYLAND_DISPLAY $WRAP "$@" \
    "$GPU_ZOOM" --paced --size "$WGPU_SIZE" "${adapter[@]}" >>"$part" || true
  grep -E '^wgpu[a-z_]*\.[^=]*=' "$part" >>"$OUT/$label.kv" || true
}

measure full
[ "$BACKEND" = wgpu ] && measure_wgpu full
measure proxy-taskset-$PROXY_CPUS taskset -c "$PROXY_CPUS"
[ "$BACKEND" = wgpu ] && measure_wgpu proxy-taskset-$PROXY_CPUS taskset -c "$PROXY_CPUS"
for cpus in $EXTRA_CPUS; do
  measure stress-taskset-$cpus taskset -c "$cpus"
  [ "$BACKEND" = wgpu ] && measure_wgpu stress-taskset-$cpus taskset -c "$cpus"
done

"$GATE" merge --out "$OUT"
