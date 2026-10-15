//! The reader against the directory walker, on images built from generated
//! trees, with `debugfs` as the independent witness.
//!
//! The walker scans the directory the image was made from. The reader scans
//! the image. They must agree on every path: kind, logical size, allocated
//! size, hard-link grouping, modification time and category. The one table
//! of allowed differences is `EXCEPTIONS`; each entry says why. A difference
//! not covered there is a finding, not something to add to the table.
//!
//! Needs `mke2fs`, `e2fsck` and `debugfs` (package e2fsprogs).

// The reader and the tools these tests drive are Linux-only.
#![cfg(target_os = "linux")]

mod common;

use bs_scan::ScanOptions;
use bs_scan_ext4::Ext4Options;
use bs_tree::{NodeFlags, NodeKind};
use common::*;

/// Differences between the reader's tree and the walker's that are not bugs.
const EXCEPTIONS: &[Exception] = &[Exception {
    field: "allocated",
    reason: "A directory's blocks depend on how its file system laid it out: the walker \
                 sees the host file system's directory, the reader the one mke2fs wrote. The \
                 oracle test checks directory sizes against debugfs instead.",
    covers: |_, info| info.kind == NodeKind::Dir,
}];

/// Paths that exist only in the image.
const IMAGE_ONLY: &[&[u8]] = &[b"lost+found"];

fn dumpe2fs(img: &std::path::Path) -> String {
    run(std::process::Command::new("dumpe2fs").arg(img))
}

fn long_name(n: usize, tag: &str) -> String {
    let mut s = String::from(tag);
    while s.len() < n {
        s.push('x');
    }
    s
}

fn mixed_items() -> Vec<Item> {
    let mut items = vec![
        dir("docs"),
        dir("docs/empty"),
        file("docs/readme.txt", 1234),
        file("docs/photo.jpg", 70_000),
        file("docs/movie.mkv", 3_000_000),
        file("docs/song.mp3", 400_000),
        file("docs/archive.zip", 12_345),
        file("docs/zero", 0),
        file("docs/exactly_one_block", 4096),
        file("docs/one_byte_over", 4097),
        // Several names for one file, in different directories.
        dir("links"),
        file("links/original", 50_000),
        link("links/second", "links/original"),
        link("docs/third", "links/original"),
        file("links/pair_a", 9000),
        link("links/pair_b", "links/pair_a"),
        symlink_to("links/short", "original"),
        symlink_to("links/long", &"d/".repeat(60)),
        symlink_to("links/dangling", "no/such/target"),
        // Holes at the start, in the middle and at the end, and one with
        // more extents than an inode can hold, so its tree has a node.
        dir("holes"),
        sparse("holes/start", 10_000_000, &[(9_990_000, 8000)]),
        sparse("holes/middle", 10_000_000, &[(0, 5000), (9_995_000, 5000)]),
        sparse("holes/end", 10_000_000, &[(0, 20_000)]),
        sparse(
            "holes/many_extents",
            4_000_000,
            &(0..40).map(|i| (i * 100_000, 4096)).collect::<Vec<_>>(),
        ),
        sparse("holes/all_hole", 5_000_000, &[]),
        Item::Fifo(b"fifo".to_vec()),
        // Names: the longest allowed, non-UTF-8, newline, spaces, a leading dot.
        dir("names"),
        file(&format!("names/{}", long_name(255, "long_")), 10),
        Item::File(b"names/not_utf8_\xff\xfe\xc3".to_vec(), 20),
        Item::File(b"names/line\nfeed".to_vec(), 30),
        Item::File(b"names/with space and 'quote\"".to_vec(), 40),
        file("names/.hidden", 50),
        Item::Dir(b"names/dir_\xf0\x28\x8c\x28".to_vec()),
        Item::File(b"names/dir_\xf0\x28\x8c\x28/inside".to_vec(), 60),
    ];
    // Deep nesting.
    let mut path = String::from("deep");
    items.push(dir(&path));
    for i in 0..60 {
        path.push_str(&format!("/level{i}"));
        items.push(dir(&path));
    }
    items.push(file(&format!("{path}/bottom.txt"), 777));
    items
}

/// For images whose block size or storage method differs from the host file
/// system's, a file occupies a different number of bytes there than the same
/// file does in the image the walker's source directory sits on. The walker
/// cannot speak for allocated sizes then; `debugfs` does, strictly, for every
/// path (these cases use plain names so that every path can be asked).
const OTHER_GEOMETRY: &[Exception] = &[
    Exception {
        field: "allocated",
        reason: "the image allocates in different units than the host file system",
        covers: |_, _| true,
    },
    Exception {
        field: "link group bytes",
        reason: "the image allocates in different units than the host file system",
        covers: |_, _| true,
    },
];

fn plain(items: Vec<Item>) -> Vec<Item> {
    items
        .into_iter()
        .filter(|i| {
            i.path()
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        })
        .collect()
}

fn run_case(name: &str, spec: ImageSpec, extra: &[Exception]) -> (Fixture, ImageScan) {
    let fixture = build(name, &spec);
    let image = scan_image(&fixture.img);
    assert_sound(&image.result);
    assert!(image.result.snapshot.scan_complete(), "{name}: incomplete");
    let walker = scan_walker(&fixture.src);
    assert_sound(&walker);

    let diffs = compare_trees(&image.result.snapshot, &walker.snapshot, IMAGE_ONLY);
    let table: Vec<&Exception> = EXCEPTIONS.iter().chain(extra.iter()).collect();
    let tree = &image.result.snapshot;
    let infos = flatten(tree);
    let mut rest = Vec::new();
    'next: for d in diffs {
        if let Some(info) = infos.get(d.path.as_bytes()) {
            for e in &table {
                if e.field == d.field && (e.covers)(&d.path, info) {
                    continue 'next;
                }
            }
        }
        rest.push(d);
    }
    let rest = explain_host_extent_index(&fixture.src, rest);
    assert_no_diffs(&format!("{name}: reader vs walker"), &rest);

    // The oracle: debugfs's own view of every plain path, for sizes the
    // walker cannot vouch for (directories) and as a second opinion on the
    // rest.
    let paths: Vec<Vec<u8>> = infos.keys().cloned().collect();
    let truth = oracle(&fixture.img, &paths);
    if !extra.is_empty() {
        assert_eq!(
            truth.len(),
            paths.len(),
            "{name}: the oracle must see every path"
        );
    }
    assert!(
        truth.len() * 2 >= paths.len() || paths.len() > 1000,
        "{name}: the oracle saw too little ({} of {})",
        truth.len(),
        paths.len()
    );
    // Bytes of each hard-linked file, once.
    let mut group_bytes: std::collections::BTreeMap<bs_tree::NodeId, (u64, u64)> =
        Default::default();
    for info in infos.values() {
        if info.flags.contains(NodeFlags::HARD_LINK) {
            let e = group_bytes
                .entry(info.origin.unwrap_or(info.id))
                .or_default();
            e.0 += info.logical;
            e.1 += info.allocated;
        }
    }
    let mut bad = Vec::new();
    for (path, fact) in &truth {
        let info = &infos[path];
        let want_alloc = fact.blocks512 * 512;
        let linked = info.flags.contains(NodeFlags::HARD_LINK);
        if fact.is_dir != (info.kind == NodeKind::Dir) {
            bad.push(format!("{} kind", lossy(path)));
        }
        let (have_logical, have_alloc) = if linked {
            group_bytes[&info.origin.unwrap_or(info.id)]
        } else {
            (info.logical, info.allocated)
        };
        if have_alloc != want_alloc {
            bad.push(format!(
                "{} allocated {} vs debugfs {}",
                lossy(path),
                have_alloc,
                want_alloc
            ));
        }
        if !fact.is_dir && info.kind == NodeKind::File && have_logical != fact.size {
            bad.push(format!(
                "{} size {} vs {}",
                lossy(path),
                have_logical,
                fact.size
            ));
        }
        if linked != (fact.links > 1) && !fact.is_dir {
            bad.push(format!("{} link flag vs {} links", lossy(path), fact.links));
        }
        if info.mtime != fact.mtime {
            bad.push(format!(
                "{} mtime {} vs {}",
                lossy(path),
                info.mtime,
                fact.mtime
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{name}: reader vs debugfs:\n{}",
        bad.join("\n")
    );
    (fixture, image)
}

#[test]
fn a_mixed_tree_equals_the_walkers() {
    let (f, image) = run_case("mixed", ImageSpec::new(mixed_items()), &[]);
    let report = image.report.unwrap();
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
    assert_eq!(report.vanished_entries, 0);
    assert_eq!(report.unreadable_entries, 0);
    assert_eq!(
        report.unreachable.inodes,
        orphan_file_inodes(&std::fs::read(&f.img).unwrap()),
        "only the orphan file, if the image has one, is unreached"
    );
}

#[test]
fn indexed_directories_of_one_and_two_levels() {
    let mut items = vec![dir("wide")];
    for i in 0..3000 {
        items.push(file(&format!("wide/file_{i:05}"), (i % 7) as u64 * 100));
    }
    items.push(dir("longnames"));
    for i in 0..9000 {
        items.push(file(
            &format!("longnames/{}", long_name(250, &format!("n{i:05}_"))),
            1,
        ));
    }
    let spec = ImageSpec::new(items)
        .args(&["-N", "20000"])
        .size("256M")
        .indexed();
    let (fixture, _) = run_case("htree", spec, &[]);
    let out = run(std::process::Command::new("debugfs")
        .args(["-R", "htree_dump /longnames"])
        .arg(&fixture.img));
    assert!(
        out.contains("Indirect levels: 1"),
        "expected a two-level index: {out}"
    );
}

#[test]
fn inline_data_files_and_directories() {
    let mut items = vec![dir("small"), file("small/tiny", 20), file("small/also", 55)];
    for i in 0..6 {
        items.push(file(&format!("small/f{i}"), 3));
    }
    // Four names fit in the inode itself.
    items.push(dir("four"));
    for name in ["a1", "a2", "a3", "a4"] {
        items.push(file(&format!("four/{name}"), 3));
    }
    items.push(dir("small/sub"));
    items.push(file("small/sub/x", 10));
    items.push(dir("bigger"));
    // Too many names for the inode: the rest spill into the attribute area
    // and then into a block.
    for i in 0..14 {
        items.push(file(&format!("bigger/entry_number_{i}"), 100));
    }
    items.push(file("big", 100_000));
    let spec = ImageSpec::new(items).args(&["-O", "inline_data", "-I", "256"]);
    let (fixture, _) = run_case("inline", spec, OTHER_GEOMETRY);
    let stat = |p: &str| {
        run(std::process::Command::new("debugfs")
            .args(["-R", &format!("stat {p}")])
            .arg(&fixture.img))
    };
    assert!(
        stat("/small/tiny").contains("Flags: 0x10000000"),
        "no inline file"
    );
    assert!(
        stat("/four").contains("Flags: 0x10000000"),
        "no inline directory"
    );
}

#[test]
fn block_mapped_files_and_directories() {
    let mut items = vec![dir("many")];
    for i in 0..2500 {
        items.push(file(&format!("many/entry_with_some_length_{i:05}"), 10));
    }
    // 12 direct blocks, then single, double indirect.
    items.push(file("direct", 40_000));
    items.push(file("single", 500_000));
    items.push(file("double", 6_000_000));
    items.push(sparse("hole", 70_000_000, &[(0, 4096), (69_000_000, 4096)]));
    let spec = ImageSpec::new(items)
        .args(&[
            "-O",
            "^extents,^flex_bg,^64bit,^metadata_csum,^orphan_file,^resize_inode,^huge_file",
        ])
        .size("128M");
    run_case("blockmap", spec, OTHER_GEOMETRY);
}

#[test]
fn one_kilobyte_blocks_and_small_inodes() {
    let spec = ImageSpec::new(plain(mixed_items())).args(&[
        "-b",
        "1024",
        "-I",
        "128",
        "-O",
        "^metadata_csum,^64bit",
    ]);
    run_case("small", spec, OTHER_GEOMETRY);
}

#[test]
fn sixty_four_kilobyte_blocks() {
    let mut items = plain(mixed_items());
    items.push(dir("wide"));
    for i in 0..1500 {
        items.push(file(&format!("wide/file_{i:05}"), 5));
    }
    let spec = ImageSpec::new(items)
        .args(&["-b", "65536"])
        .size("768M")
        .indexed();
    let (fixture, _) = run_case("blocks64k", spec, OTHER_GEOMETRY);
    assert!(dumpe2fs(&fixture.img).contains("Block size:               65536"));
}

#[test]
fn meta_bg_sparse_super2_and_many_groups() {
    let spec = ImageSpec::new(plain(mixed_items()))
        .args(&[
            "-b",
            "1024",
            "-g",
            "256",
            "-O",
            "meta_bg,^resize_inode,sparse_super2,^metadata_csum",
        ])
        .size("48M");
    let (fixture, _) = run_case("metabg", spec, OTHER_GEOMETRY);
    let dump = dumpe2fs(&fixture.img);
    assert!(dump.contains("meta_bg") && dump.contains("sparse_super2"));
    // Descriptors of the later groups live in their own meta group.
    assert!(dump.contains("Group 33: (Blocks 8449-8704)\n  Group descriptor at 8449"));
}

#[test]
fn bigalloc() {
    let spec = ImageSpec::new(plain(mixed_items()))
        .args(&["-O", "bigalloc,^resize_inode", "-C", "16384"])
        .size("64M");
    let (fixture, _) = run_case("bigalloc", spec, OTHER_GEOMETRY);
    assert!(dumpe2fs(&fixture.img).contains("bigalloc"));
}

#[test]
fn categories_are_the_walkers() {
    // Category is part of the comparison above; this checks that the
    // fixture really has classified files, so the comparison means something.
    let fixture = build("categories", &ImageSpec::new(mixed_items()));
    let image = scan_image(&fixture.img);
    let infos = flatten(&image.result.snapshot);
    let classified = infos.values().filter(|i| i.category != 0).count();
    assert!(classified >= 4, "only {classified} classified files");
}

#[test]
fn categories_can_be_turned_off_like_the_walkers() {
    let fixture = build("nocat", &ImageSpec::new(mixed_items()));
    let image = scan_image_with(
        &fixture.img,
        Ext4Options::default(),
        ScanOptions {
            categories: false,
            ..ScanOptions::default()
        },
    );
    assert!(
        flatten(&image.result.snapshot)
            .values()
            .all(|i| i.category == 0)
    );
}

#[test]
fn mount_points_are_empty_and_what_lies_under_them_is_counted() {
    let items = vec![
        dir("mnt"),
        dir("mnt/usb"),
        file("mnt/usb/hidden_a", 10_000),
        dir("mnt/usb/deeper"),
        file("mnt/usb/deeper/hidden_b", 20_000),
        file("mnt/kept", 300),
        file("top", 5),
    ];
    let fixture = build("mounts", &ImageSpec::new(items));
    let options = Ext4Options {
        skip: vec![b"mnt/usb".to_vec()],
        ..Ext4Options::default()
    };
    let image = scan_image_with(&fixture.img, options, ScanOptions::default());
    assert_sound(&image.result);
    let infos = flatten(&image.result.snapshot);
    let usb = &infos[&b"mnt/usb"[..]];
    assert_eq!(usb.kind, NodeKind::Dir);
    assert!(usb.flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!((usb.logical, usb.allocated), (0, 0));
    assert!(!infos.contains_key(&b"mnt/usb/hidden_a"[..]));
    assert!(infos.contains_key(&b"mnt/kept"[..]));
    let hidden = image.report.unwrap().hidden_under_mounts;
    assert_eq!(hidden.files, 2);
    assert_eq!(hidden.logical, 30_000);
    // Two directories (the mount point's own directory and its subdirectory)
    // and the files' blocks.
    assert_eq!(hidden.dirs, 2);
    assert!(hidden.allocated >= 8192 + 2 * 4096);
}

#[test]
fn only_one_extra_block_on_a_large_host_file_is_explained() {
    let items = vec![dir("d"), file("big", 100_000), file("small", 9000)];
    let fixture = build("hostlayout", &ImageSpec::new(items));
    let block = 4096;
    let diff = |path: &str, reader: u64, walker: u64| Diff {
        path: path.into(),
        field: "allocated",
        reader: reader.to_string(),
        other: walker.to_string(),
    };
    let kept = |d: Diff| explain_host_extent_index(&fixture.src, vec![d]).len();
    let host_is_ext4 = run(std::process::Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(&fixture.src))
    .trim()
        == "ext2/ext3";
    // One extra block on a regular file of five blocks or more: explained on
    // an ext4 host, and on no other.
    assert_eq!(
        kept(diff("big", 25 * block, 26 * block)),
        usize::from(!host_is_ext4)
    );
    assert_eq!(
        kept(diff("big", 5 * block, 6 * block)),
        usize::from(!host_is_ext4)
    );
    // Anything but exactly one block more is not.
    assert_eq!(kept(diff("big", 25 * block, 27 * block)), 1);
    assert_eq!(kept(diff("big", 25 * block, 25 * block)), 1);
    assert_eq!(kept(diff("big", 25 * block, 26 * block - 512)), 1);
    assert_eq!(kept(diff("big", 26 * block, 25 * block)), 1);
    // A file too small to be in five extents is not explained.
    assert_eq!(kept(diff("small", 4 * block, 5 * block)), 1);
    // A directory is not a file.
    assert_eq!(kept(diff("d", 25 * block, 26 * block)), 1);
    // Other fields are never touched.
    let mut other = diff("big", 25 * block, 26 * block);
    other.field = "logical";
    assert_eq!(kept(other), 1);
    // A path that is not on the host is not explained.
    assert_eq!(kept(diff("missing", 25 * block, 26 * block)), 1);
}
