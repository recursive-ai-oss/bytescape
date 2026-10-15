//! The btrfs reader against the directory walker, on a real btrfs file system:
//! the same fixture is scanned by both and compared path by path (type,
//! logical size, allocated size, link identity, modification time), with
//! every legitimate difference named in the exceptions table below.
//!
//! It mounts a file system, so it needs root, and it is declared with
//!
//! ```text
//! BYTESCAPE_DISPOSABLE_HOST=1 cargo test -p bs-scan-btrfs --test live -- --nocapture
//! ```
//!
//! With the variable unset every test returns at once and prints that it did
//! not run. With it set, a test that cannot do its job (no root, no `mount`,
//! no `mkfs.btrfs`) fails instead of passing quietly.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

#[path = "common/diff.rs"]
mod diff;
#[path = "common/exceptions.rs"]
mod exceptions;

use bs_scan::{ScanOptions, ScanResult, Walker, start};
use bs_scan_btrfs::consts::{FS_TREE_ID, INODE_ITEM};
use bs_scan_btrfs::{BtrfsOptions, BtrfsScanner, Flow, IoctlSource, Key, TreeSource};
use bs_tree::NodeFlags;
use diff::{compare, tree_of};
use exceptions::{STAND_INS, class_table};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn enabled(test: &str) -> bool {
    if std::env::var("BYTESCAPE_DISPOSABLE_HOST").as_deref() != Ok("1") {
        eprintln!("NOT RUN: {test} needs BYTESCAPE_DISPOSABLE_HOST=1 and root");
        return false;
    }
    let uid = Command::new("id").arg("-u").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&uid.stdout).trim(),
        "0",
        "{test}: BYTESCAPE_DISPOSABLE_HOST=1 was given but this is not root"
    );
    true
}

/// Runs a shell command and fails the test with its output when it fails.
fn sh(script: &str) {
    let out = Command::new("bash")
        .args(["-ec", script])
        .output()
        .unwrap_or_else(|e| panic!("cannot run bash: {e}"));
    assert!(
        out.status.success(),
        "`{script}` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A mounted loop file system, unmounted and removed when dropped.
struct Fs {
    dir: PathBuf,
    mnt: PathBuf,
}

impl Fs {
    fn new(name: &str, mount_options: &str) -> Fs {
        let dir = std::env::temp_dir().join(format!("bs-btrfs-live-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        let img = dir.join("fs.img");
        sh(&format!(
            "truncate -s 700M '{img}' && mkfs.btrfs -q -f '{img}' && mount -o loop,{opts} '{img}' '{mnt}'",
            img = img.display(),
            mnt = mnt.display(),
            opts = mount_options
        ));
        Fs { dir, mnt }
    }

    fn sync(&self) {
        sh(&format!(
            "sync; btrfs filesystem sync '{}'",
            self.mnt.display()
        ));
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mnt).status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Everything the reader describes that a btrfs file system can hold.
fn build_fixture(mnt: &Path) {
    let m = mnt.display();
    sh(&format!(
        r#"cd '{m}'
mkdir -p docs deep/a/b/c/d empty-dir
for i in $(seq 1 20); do head -c $((i * 20)) /dev/urandom > docs/tiny-$i; done
head -c 6000 /dev/urandom > docs/readme.txt
head -c 8388608 /dev/urandom > big.bin
sync
cp --reflink=always big.bin clone-a.bin
cp --reflink=always big.bin clone-b.bin
yes 'a line of text that compresses very well' | head -c 4194304 > text.txt
truncate -s 64M sparse.bin
dd if=/dev/urandom of=sparse.bin bs=4096 count=1 seek=256 conv=notrunc status=none
head -c 1048576 /dev/urandom > overwritten.bin
sync
dd if=/dev/urandom of=overwritten.bin bs=4096 count=1 seek=64 conv=notrunc status=none
ln docs/readme.txt hard-readme
ln docs/tiny-3 docs/tiny-3-link
ln -s docs/readme.txt link
ln -s /nonexistent/target dangling
mkfifo fifo
printf x > "$(printf 'odd-\377-name\nwith-newline')"
head -c 300000 /dev/urandom > deep/a/b/c/d/leaf.bin
btrfs -q subvolume create sub1
head -c 1048576 /dev/urandom > sub1/data.bin
head -c 100 /dev/urandom > sub1/tiny
btrfs -q subvolume create sub1/inner
head -c 524288 /dev/urandom > sub1/inner/inner.bin
btrfs -q subvolume snapshot sub1 snap1
btrfs -q subvolume snapshot -r sub1 snap1-ro
head -c 65536 /dev/urandom > snap1/only-in-snapshot.bin
"#
    ));
}

fn walk(mnt: &Path) -> ScanResult {
    Walker::scan_blocking(mnt, ScanOptions::default())
}

fn read(mnt: &Path) -> (ScanResult, bs_scan_btrfs::BtrfsReport) {
    let scanner = Arc::new(BtrfsScanner::new(BtrfsOptions::default()));
    let result = start(scanner.clone(), mnt.to_path_buf(), ScanOptions::default()).wait();
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    let report = scanner.last_report().expect("a report");
    (result, report)
}

/// The inode item's `nbytes` for `ino` of `tree`, read through the tree
/// search: the number the kernel makes `st_blocks` from.
fn inode_nbytes(mnt: &Path, tree: u64, ino: u64) -> u64 {
    let (source, _) = IoctlSource::open(mnt).unwrap();
    let key = Key {
        objectid: ino,
        ty: INODE_ITEM,
        offset: 0,
    };
    let mut nbytes = None;
    source
        .search(tree, key, key, &mut |item| {
            nbytes = item
                .data
                .get(24..32)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_le_bytes);
            Flow::Stop
        })
        .unwrap();
    nbytes.expect("the inode item")
}

#[test]
fn the_reader_matches_the_walker_on_a_btrfs_file_system() {
    if !enabled("the_reader_matches_the_walker_on_a_btrfs_file_system") {
        return;
    }
    let fs = Fs::new("diff", "compress-force=zstd:3");
    build_fixture(&fs.mnt);
    fs.sync();
    let walker = walk(&fs.mnt);
    assert!(walker.snapshot.scan_complete());
    let (reader, report) = read(&fs.mnt);
    assert!(reader.snapshot.scan_complete(), "{:?}", report.diagnostics);
    let w = tree_of(&walker.snapshot);
    let r = tree_of(&reader.snapshot);
    let outcome = compare(&w, &r, &exceptions::all());
    let mut text = outcome.render();
    text.push_str(&class_table(&w, &r, &outcome));
    // The compressed file's two figures, and the number the kernel makes the
    // walker's from: the claim behind the "compressed extents" entry.
    let md = std::fs::metadata(fs.mnt.join("text.txt")).unwrap();
    let nbytes = inode_nbytes(&fs.mnt, FS_TREE_ID, md.ino());
    text.push_str(&format!(
        "text.txt: length {}, inode nbytes {nbytes}, st_blocks*512 {} (the walker), on disk by the reader {}\n",
        md.len(),
        md.blocks() * 512,
        r["text.txt"].allocated
    ));
    println!("{text}");
    if let Ok(dir) = std::env::var("BENCH_RESULTS") {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(Path::new(&dir).join("btrfs-differential.txt"), &text).unwrap();
        std::fs::write(
            Path::new(&dir).join("btrfs-report.txt"),
            format!("{report:#?}\n"),
        )
        .unwrap();
    }
    assert!(outcome.is_clean(), "{text}");
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
    // The path the elevated helper takes: the same scan, pinned to the root
    // that was judged, from a descriptor the caller still holds. It must give
    // exactly the unpinned result.
    let judged = std::sync::Arc::new(bs_scan::rawdir::open_root(&fs.mnt).unwrap());
    let pin = bs_scan::RootPin::from_fd(std::sync::Arc::clone(&judged)).unwrap();
    let pinned_scanner = Arc::new(BtrfsScanner::new(BtrfsOptions::default()));
    let pinned = start(
        pinned_scanner.clone(),
        fs.mnt.clone(),
        ScanOptions::default().pin_root(pin),
    )
    .wait();
    assert!(pinned.stats.error.is_none(), "{:?}", pinned.stats.error);
    let pinned_tree = tree_of(&pinned.snapshot);
    let same = compare(&r, &pinned_tree, &[]);
    assert!(
        same.is_clean(),
        "pinned differs from unpinned: {}",
        same.render()
    );
    assert_eq!(
        pinned.snapshot.root().unwrap().allocated(),
        reader.snapshot.root().unwrap().allocated()
    );
    assert_eq!(
        pinned.snapshot.root().unwrap().logical(),
        reader.snapshot.root().unwrap().logical()
    );
    drop(judged);
    // The claim behind the "compressed extents" entry, and that it covers the
    // one compressible file of the fixture and nothing else.
    assert_eq!(nbytes, md.len(), "the kernel counts the file at its length");
    assert_eq!(nbytes, md.blocks() * 512);
    assert!(r["text.txt"].allocated < nbytes, "it is smaller on disk");
    // The paths an exception covers, as a sorted set: the outcome lists them
    // in the order the two trees were enumerated, which is the file
    // system's and not the fixture's.
    let covered = |name: &str| -> Vec<&str> {
        let mut paths: Vec<&str> = outcome
            .explained
            .iter()
            .filter(|(_, n)| *n == name)
            .map(|(f, _)| f.path.as_str())
            .collect();
        paths.sort_unstable();
        paths
    };
    let sorted = |paths: &[&'static str]| -> Vec<&'static str> {
        let mut v = paths.to_vec();
        v.sort_unstable();
        v
    };
    assert_eq!(covered("compressed extents"), sorted(&["text.txt"]));
    // The folders the kernel makes up have no time on disk; they are the two
    // snapshots' copies of the entry for `sub1/inner`, and the report counts
    // them.
    assert_eq!(covered("stand-in directory time"), sorted(STAND_INS));
    assert_eq!(report.stand_in_dirs, 2);
    for p in STAND_INS {
        assert!(!r[*p].failed && !w[*p].failed, "{p}");
    }

    // What the fixture is meant to show, so that a fixture that stopped
    // exercising something fails and does not just pass.
    assert!(report.trees >= 5, "subvolumes and snapshots: {report:?}");
    assert!(
        report.sharing.files >= 4,
        "reflinks and snapshots: {report:?}"
    );
    assert!(report.sharing_exact);
    assert!(report.compression.files >= 1, "{report:?}");
    assert!(report.compression.on_disk < report.compression.logical);
    for (path, shared) in [
        ("clone-a.bin", true),
        ("clone-b.bin", true),
        ("big.bin", true),
        ("snap1/data.bin", true),
        ("sub1/data.bin", true),
        ("snap1/only-in-snapshot.bin", false),
        ("overwritten.bin", false),
    ] {
        assert_eq!(r[path].flags.contains(NodeFlags::SHARED), shared, "{path}");
    }
    // Counted once: the reader's whole-file-system total is below the sum the
    // walker reaches by counting every user of an extent.
    let (wt, rt) = (
        walker.snapshot.root().unwrap().allocated(),
        reader.snapshot.root().unwrap().allocated(),
    );
    assert!(rt < wt, "reader {rt} walker {wt}");
}

#[test]
fn deleting_a_clone_makes_the_other_exclusive_and_the_reader_sees_it() {
    if !enabled("deleting_a_clone_makes_the_other_exclusive_and_the_reader_sees_it") {
        return;
    }
    let fs = Fs::new("exclusive", "compress=zstd");
    sh(&format!(
        "cd '{m}'; head -c 4194304 /dev/urandom > orig.bin; sync; \
         cp --reflink=always orig.bin copy.bin; \
         btrfs -q subvolume create side; cp --reflink=always orig.bin side/third.bin",
        m = fs.mnt.display()
    ));
    fs.sync();
    let (before, report) = read(&fs.mnt);
    assert!(report.sharing_exact);
    let t = tree_of(&before.snapshot);
    for p in ["orig.bin", "copy.bin", "side/third.bin"] {
        assert!(t[p].flags.contains(NodeFlags::SHARED), "{p}");
    }
    let sum: u64 = ["orig.bin", "copy.bin", "side/third.bin"]
        .iter()
        .map(|p| t[*p].allocated)
        .sum();
    assert_eq!(sum, 4 << 20, "the extent is counted once");

    // Deleting two of the three frees nothing; deleting the last frees it.
    sh(&format!(
        "rm '{m}/copy.bin' '{m}/side/third.bin'",
        m = fs.mnt.display()
    ));
    fs.sync();
    let (after, _) = read(&fs.mnt);
    let t = tree_of(&after.snapshot);
    assert!(
        !t["orig.bin"].flags.contains(NodeFlags::SHARED),
        "no one else uses it now"
    );
    assert_eq!(t["orig.bin"].allocated, 4 << 20);
    let id = t["orig.bin"].id;
    let est = after.snapshot.delete_would_free(id).unwrap();
    assert!(est.is_exact());
    assert_eq!(est.at_most, 4 << 20);
}

#[test]
fn a_directory_that_is_not_the_mount_point_is_read_as_far_as_it_goes() {
    if !enabled("a_directory_that_is_not_the_mount_point_is_read_as_far_as_it_goes") {
        return;
    }
    let fs = Fs::new("below", "compress=zstd");
    sh(&format!(
        "cd '{m}'; mkdir -p a/b; head -c 100000 /dev/urandom > a/b/f.bin; \
         head -c 50000 /dev/urandom > outside.bin; btrfs -q subvolume create a/sv; \
         head -c 20000 /dev/urandom > a/sv/g.bin",
        m = fs.mnt.display()
    ));
    fs.sync();
    let below = fs.mnt.join("a");
    let walker = tree_of(&walk(&below).snapshot);
    let (reader, report) = read(&below);
    let r = tree_of(&reader.snapshot);
    let outcome = compare(&walker, &r, &[]);
    assert!(outcome.is_clean(), "{}", outcome.render());
    assert!(r.contains_key("sv/g.bin") && r.contains_key("b/f.bin"));
    assert!(!r.contains_key("outside.bin"));
    assert!(report.unreachable.inodes >= 1);
}

/// The mount ID of the mount at `path`, from the mount table.
fn mount_id(path: &Path) -> u64 {
    let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    text.lines()
        .find_map(|l| {
            let mut f = l.split_whitespace();
            let id: u64 = f.next()?.parse().ok()?;
            (f.nth(3)? == path.to_str()?).then_some(id)
        })
        .unwrap_or_else(|| panic!("{} is not a mount point", path.display()))
}

/// Unmounts what the sibling-subvolume test mounted, whatever happens.
struct Layout {
    root: PathBuf,
}

impl Drop for Layout {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(self.root.join("home")).status();
        let _ = Command::new("umount").arg(&self.root).status();
    }
}

/// The usual distribution layout: subvolumes `rootsv` and `homesv` side by
/// side, mounted at `/` and `/home`. A scan of the drive that starts at `/`
/// must find the home files under `home`, as the walker does, and must not
/// show `home` as an empty folder.
#[test]
fn a_sibling_subvolume_mounted_below_the_root_is_read_where_the_walker_finds_it() {
    if !enabled("a_sibling_subvolume_mounted_below_the_root_is_read_where_the_walker_finds_it") {
        return;
    }
    let fs = Fs::new("sibling", "compress=zstd");
    let top = fs.mnt.display();
    sh(&format!(
        "cd '{top}'; btrfs -q subvolume create rootsv; btrfs -q subvolume create homesv; \
         head -c 300000 /dev/urandom > rootsv/system.bin; mkdir rootsv/home; \
         mkdir homesv/alice; head -c 3000000 /dev/urandom > homesv/alice/photos.bin; \
         head -c 100 /dev/urandom > homesv/alice/note.txt"
    ));
    fs.sync();
    let device = String::from_utf8(
        Command::new("findmnt")
            .args(["-no", "SOURCE"])
            .arg(&fs.mnt)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let device = device.trim();
    let root = fs.dir.join("drive");
    std::fs::create_dir_all(&root).unwrap();
    let layout = Layout { root: root.clone() };
    sh(&format!(
        "mount -o subvol=rootsv,compress=zstd '{device}' '{r}'; \
         mount -o subvol=homesv,compress=zstd '{device}' '{r}/home'",
        r = root.display()
    ));
    let options = || ScanOptions {
        boundary: bs_scan::Boundary::with_mount_ids([mount_id(&root.join("home"))]),
        ..ScanOptions::default()
    };
    let walker = Walker::scan_blocking(&root, options());
    let scanner = Arc::new(BtrfsScanner::new(BtrfsOptions::default()));
    let reader = start(scanner.clone(), root.clone(), options()).wait();
    assert!(reader.stats.error.is_none(), "{:?}", reader.stats.error);
    let report = scanner.last_report().expect("a report");
    let w = tree_of(&walker.snapshot);
    let r = tree_of(&reader.snapshot);
    // Only the 100-byte note differs: its data lives inside the tree.
    let outcome = compare(&w, &r, &[exceptions::inline_data()]);
    println!("{}", outcome.render());
    println!("{}", class_table(&w, &r, &outcome));
    assert!(r.contains_key("home/alice/photos.bin"), "{:?}", r.keys());
    assert!(!r["home"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(outcome.is_clean(), "{}", outcome.render());
    assert_eq!(report.mounts_entered, 1);
    // The two totals differ by exactly what the explained differences add up
    // to (the sector the kernel counts for the inline note, less its bytes).
    let explained: u64 = outcome
        .explained
        .iter()
        .map(|(f, _)| f.walker.parse::<u64>().unwrap() - f.reader.parse::<u64>().unwrap())
        .sum();
    assert!(explained > 0);
    assert_eq!(
        walker.snapshot.root().unwrap().allocated(),
        reader.snapshot.root().unwrap().allocated() + explained
    );
    // Left to the default boundary neither tool enters the mount, and both
    // say so.
    let walker = Walker::scan_blocking(&root, ScanOptions::default());
    let reader = start(scanner.clone(), root.clone(), ScanOptions::default()).wait();
    let (w, r) = (tree_of(&walker.snapshot), tree_of(&reader.snapshot));
    assert!(w["home"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(r["home"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!r.keys().any(|k| k.starts_with("home/")));
    drop(layout);
}
