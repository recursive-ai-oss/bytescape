#!/usr/bin/env bash
# Builds a btrfs or XFS file system on a loop-mounted image, fills it with a
# generated data set of known size, and runs that file system's reader
# benchmark on it:
#
#   BYTESCAPE_DISPOSABLE_HOST=1 packaging/ci/bench-image.sh btrfs|xfs [--files N] [--image-gib N] [--runs N]
#
# For a throwaway machine with passwordless sudo (a hosted CI runner): it
# mounts, writes under /mnt and empties nothing itself (the benchmark script it
# calls empties the caches).
#
# The data set is made by the `mktree` example of bs-scan: the same arguments
# always give the same tree, whatever the runner's own files look like, so two
# runs can be compared. It is copied into the image with hard links kept, and
# copied once more with reflinks so that shared extents are part of what is
# measured. On btrfs a snapshot is added too. Before anything is copied the
# script compares the size of the data set (twice, plus a margin) with the free
# space on the image and on the disk that holds it, and stops with a plain
# message when it would not fit, instead of failing in the middle of a copy.
#
# The results go to $BENCH_RESULTS (default ./bench-results/<kind>), and its
# summary.md ends with a section that says what was copied.

set -euo pipefail

kind="${1:-}"
case "$kind" in
  btrfs | xfs) shift ;;
  *) echo "usage: bench-image.sh btrfs|xfs [--files N] [--image-gib N] [--runs N]" >&2; exit 2 ;;
esac

files=200000
image_gib=16
runs=10
while [ $# -gt 0 ]; do
  case "$1" in
    --files) files="$2"; shift 2 ;;
    --image-gib) image_gib="$2"; shift 2 ;;
    --runs) runs="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

if [ "${BYTESCAPE_DISPOSABLE_HOST:-}" != 1 ]; then
  echo "refusing to run: this mounts file systems and writes under /mnt. Set BYTESCAPE_DISPOSABLE_HOST=1 on a throwaway machine." >&2
  exit 2
fi

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
results="${BENCH_RESULTS:-$PWD/bench-results/$kind}"
mkdir -p "$results"
img="/mnt/$kind-bench.img"
mp="/mnt/$kind-bench"
work="${RUNNER_TEMP:-$PWD/target}/$kind-bench-dataset"

# shellcheck disable=SC2329  # called by the trap below
cleanup() {
  sudo umount "$mp" 2> /dev/null || true
  sudo rm -f "$img"
}
trap cleanup EXIT

# 1. The data set, on the runner's own disk.
cargo build --release --locked -p bs-scan --examples
target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)"
rm -rf "$work"
"$target_dir/release/examples/mktree" "$work" --files "$files"
# Bytes the files take on disk, and how many names there are.
data_bytes="$(du -s -B1 -x "$work" | cut -f1)"
data_names="$(find "$work" | wc -l)"
echo "data set: $data_names names, $data_bytes bytes on disk"

# 2. Does it fit? The data set is stored twice (a copy and a reflinked copy,
# which on btrfs and XFS shares its data and so needs little more), plus
# metadata. Insist on two times the data set and a quarter more.
need=$((data_bytes * 2 + data_bytes / 4))
image_bytes=$((image_gib * 1024 * 1024 * 1024))
if [ "$need" -gt "$image_bytes" ]; then
  echo "::error title=Image too small::the data set needs about $need bytes on the image, but the image is $image_bytes bytes. Raise --image-gib or lower --files."
  exit 1
fi
disk_free="$(df --output=avail -B1 /mnt | tail -1 | tr -d ' ')"
# The image file is sparse: only what is written takes space on /mnt.
if [ "$disk_free" -lt "$need" ]; then
  echo "::error title=Not enough disk space::/mnt has $disk_free bytes free and the benchmark needs about $need."
  exit 1
fi

# 3. The file system.
sudo truncate -s "${image_gib}G" "$img"
case "$kind" in
  btrfs) sudo mkfs.btrfs -q -f "$img" ;;
  xfs)
    # The module must load: without it the mount below fails and so does the
    # job, which is the right result.
    sudo modprobe xfs
    sudo mkfs.xfs -q -f "$img"
    ;;
esac
sudo mkdir -p "$mp"
sudo mount -o loop "$img" "$mp"
fs_free="$(df --output=avail -B1 "$mp" | tail -1 | tr -d ' ')"
echo "free on the image: $fs_free bytes"
if [ "$fs_free" -lt "$need" ]; then
  echo "::error title=Not enough space on the image::the new $kind file system has $fs_free bytes free and the data set needs about $need."
  exit 1
fi

# 4. Fill it.
sudo cp -a "$work" "$mp/data"
sudo cp -a --reflink=always "$mp/data" "$mp/data-reflink"
snapshot=none
if [ "$kind" = btrfs ]; then
  # The snapshot is part of the data set too. If it cannot be made the job
  # fails: a run without it would measure something else.
  sudo btrfs subvolume snapshot "$mp" "$mp/snapshot" > /dev/null
  snapshot="a snapshot of the whole file system at $mp/snapshot"
fi
sudo sync
echo "free after the copies: $(df --output=avail -B1 "$mp" | tail -1 | tr -d ' ') bytes"

# 5. The benchmark. Its summary is kept even when it fails.
status=0
case "$kind" in
  btrfs) bash crates/bs-scan-btrfs/bench/btrfs-bench.sh --mount "$mp" --runs "$runs" || status=$? ;;
  xfs) bash crates/bs-scan-xfs/bench/xfs-bench.sh --mount "$mp" --runs "$runs" || status=$? ;;
esac

# 6. What was measured, next to the numbers.
{
  echo
  echo "## What was copied"
  echo
  echo "- Data set: \`mktree --files $files\` (deterministic: the same arguments always make the same tree), $data_names names, $data_bytes bytes on disk as made on the runner."
  echo "- Copied into \`$mp/data\` with hard links kept, then again with reflinks into \`$mp/data-reflink\`, so shared extents are measured."
  echo "- Snapshot: $snapshot."
  echo "- Image: ${image_gib} GiB sparse file, $kind, loop-mounted; $fs_free bytes free on it before the copies."
  echo "- The runner's own \`/usr\` is not used: its size depends on the runner image, and a data set that changes between runs cannot be compared."
} >> "$results/summary.md"
exit "$status"
