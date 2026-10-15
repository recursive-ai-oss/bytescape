//! Tests of the reader as a whole, over volumes built in memory.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::testkit::{Info, ROOT, RecordBuilder, Spec, Volume, flat, scan_memory, stamp, unstamp};
use crate::{Accounting, NtfsOptions, NtfsReport, ProblemKind};
use bs_scan::ScanResult;
use bs_tree::{NodeFlags, NodeKind};
use std::collections::BTreeMap;

const CB: u64 = 4096;

fn scan(v: Volume, accounting: Accounting) -> (ScanResult, NtfsReport, BTreeMap<String, Info>) {
    scan_image(v.finish(), accounting, Some(1))
}

fn scan_image(
    image: Vec<u8>,
    accounting: Accounting,
    threads: Option<usize>,
) -> (ScanResult, NtfsReport, BTreeMap<String, Info>) {
    let options = NtfsOptions {
        accounting,
        threads,
        ..NtfsOptions::default()
    };
    let (result, report) = scan_memory(image, options);
    assert!(
        result.stats.error.is_none(),
        "scan failed: {:?}",
        result.stats.error
    );
    assert_eq!(result.stats.records_parked, 0, "parents go before children");
    assert_eq!(result.stats.records_rejected, 0);
    result.snapshot.verify().expect("tree invariants");
    let map = flat(&result.snapshot);
    (result, report.expect("report"), map)
}

/// What the tree counts for the root's child `name` and everything in it.
fn subtree_allocated(result: &ScanResult, name: &str) -> u64 {
    let root = result.snapshot.root().unwrap();
    let child = root
        .children()
        .find(|c| c.name() == name.as_bytes())
        .unwrap_or_else(|| panic!("no {name} in the root"));
    child.allocated()
}

fn full(v: Volume) -> (ScanResult, NtfsReport, BTreeMap<String, Info>) {
    scan(v, Accounting::Full)
}

#[test]
fn files_directories_and_sizes() {
    let mut v = Volume::new(Spec::default());
    let docs = v.dir("docs", ROOT);
    v.file("a.txt", docs, 100);
    v.file("b.bin", docs, 10_000);
    v.file("empty", ROOT, 0);
    v.set_mtime(docs, 1_650_000_000);
    let (result, report, m) = full(v);

    let a = &m["docs/a.txt"];
    assert_eq!((a.kind, a.logical, a.allocated), (NodeKind::File, 100, 0));
    let b = &m["docs/b.bin"];
    assert_eq!((b.logical, b.allocated), (10_000, 3 * CB));
    assert_eq!((m["empty"].logical, m["empty"].allocated), (0, 0));
    assert_eq!(m["docs"].kind, NodeKind::Dir);
    assert_eq!(m["docs"].mtime, 1_650_000_000);
    assert_eq!(m["docs/b.bin"].mtime, 1_700_000_200);
    assert!(result.snapshot.scan_complete());
    let root = result.snapshot.root().unwrap();
    assert!(root.is_complete() && !root.is_unreadable() && !root.has_unreadable_below());
    assert_eq!(report.lost_records, 0);
    assert_eq!(report.unreachable.files, 0);
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
    assert!(!report.cancelled);
    assert_eq!(report.cluster_bytes, 4096);
    assert_eq!(report.record_bytes, 1024);
}

#[test]
fn the_table_and_system_files_are_items_of_the_root() {
    let mut v = Volume::new(Spec::default());
    v.system_file(6, "$Bitmap", 20_000);
    v.system_file(2, "$LogFile", 2_000_000);
    v.file("x", ROOT, 10);
    let (result, _, m) = full(v);
    let mft = &m["$MFT"];
    assert_eq!(mft.kind, NodeKind::File);
    // The whole table: 25 records of 1 KiB is 7 clusters.
    assert_eq!(mft.allocated, 7 * CB);
    assert_eq!(mft.logical, 25 * 1024);
    assert_eq!(m["$Bitmap"].logical, 20_000);
    assert_eq!(m["$Bitmap"].allocated, 5 * CB);
    assert_eq!(m["$LogFile"].allocated, 489 * CB);
    let root = result.snapshot.root().unwrap();
    assert_eq!(root.allocated(), 7 * CB + 5 * CB + 489 * CB);
}

#[test]
fn hard_links_count_once_and_are_flagged() {
    let mut v = Volume::new(Spec::default());
    let d = v.dir("d", ROOT);
    let f = v.file("one", ROOT, 20_000);
    v.link(f, "two", d);
    v.link(f, "three", d);
    let lonely = v.file("lonely", ROOT, 20_000);
    v.dos_alias(lonely, "LONELY~1");
    let (result, report, m) = full(v);
    let names = ["one", "d/two", "d/three"];
    for n in names {
        assert!(m[n].flags.contains(NodeFlags::HARD_LINK), "{n}");
    }
    let counted: u64 = names.iter().map(|n| m[*n].allocated).sum();
    assert_eq!(counted, 5 * CB, "the bytes are counted on one name only");
    let dupes = names
        .iter()
        .filter(|n| m[**n].flags.contains(NodeFlags::HARD_LINK_DUPLICATE))
        .count();
    assert_eq!(dupes, 2);
    // A DOS alias is another spelling of one name, not another link.
    assert!(!m["lonely"].flags.contains(NodeFlags::HARD_LINK));
    assert!(!m.contains_key("LONELY~1"));
    assert_eq!(report.unreachable.files, 0);
    let root = result.snapshot.root().unwrap();
    let mft = m["$MFT"].allocated;
    assert_eq!(root.allocated(), mft + 5 * CB + 5 * CB);
    assert_eq!(root.file_count(), 1 + 3 + 1);
}

#[test]
fn streams_count_in_full_and_not_in_a_listing() {
    let build = || {
        let mut v = Volume::new(Spec::default());
        let f = v.file("f.txt", ROOT, 5_000);
        v.stream(f, "small", 40);
        v.stream(f, "big", 30_000);
        let d = v.dir("dir", ROOT);
        v.index_buffers(d, 2);
        v.stream(d, "dirstream", 10);
        v
    };
    let (_, report, m) = scan(build(), Accounting::Full);
    assert_eq!(
        (m["f.txt"].logical, m["f.txt"].allocated),
        (5_000 + 40 + 30_000, 2 * CB + 8 * CB)
    );
    assert_eq!((m["dir"].logical, m["dir"].allocated), (10, 2 * CB));
    assert_eq!(report.extras.streams, 3);
    assert_eq!(report.extras.stream_logical, 40 + 30_000 + 10);
    assert_eq!(report.extras.stream_allocated, 8 * CB);
    assert_eq!(report.extras.attribute_allocated, 2 * CB);
    assert_eq!(report.extras.directory_allocated, 2 * CB);

    let (_, _, m) = scan(build(), Accounting::Listing);
    assert_eq!((m["f.txt"].logical, m["f.txt"].allocated), (5_000, 2 * CB));
    assert_eq!((m["dir"].logical, m["dir"].allocated), (0, 0));
}

#[test]
fn sparse_and_compressed_streams_count_only_backed_clusters() {
    let mut v = Volume::new(Spec::default());
    v.sparse_file("sparse", ROOT, 2, 100, 3);
    v.compressed_file("packed", ROOT, 16, 5);
    v.fragmented_file("frag", ROOT, 20, 4);
    let (_, report, m) = full(v);
    let s = &m["sparse"];
    assert_eq!(s.allocated, 5 * CB);
    assert_eq!(s.logical, 105 * CB - 100);
    assert!(s.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    let p = &m["packed"];
    assert_eq!(p.allocated, 5 * CB);
    assert_eq!(p.logical, 16 * CB - 7);
    assert!(p.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    let f = &m["frag"];
    assert_eq!((f.logical, f.allocated), (20 * CB, 20 * CB));
    assert!(!f.flags.contains(NodeFlags::SPARSE_OR_COMPRESSED));
    assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
}

#[test]
fn links_and_mount_points_are_not_entered() {
    let mut v = Volume::new(Spec::default());
    let junction = v.dir("junction", ROOT);
    v.reparse(junction, 0xA000_0003);
    let inside = v.file("inside", junction, 50_000);
    let symlink = v.file("link", ROOT, 0);
    v.reparse(symlink, 0xA000_000C);
    let dir_link = v.dir("dirlink", ROOT);
    v.reparse(dir_link, 0xA000_000C);
    let dedup = v.file("deduped", ROOT, 10_000);
    v.reparse(dedup, 0x8000_0013);
    let _ = inside;
    let (_, report, m) = full(v);
    assert_eq!(m["junction"].kind, NodeKind::Other);
    assert!(m["junction"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert!(!m.contains_key("junction/inside"));
    assert_eq!(m["link"].kind, NodeKind::Symlink);
    assert_eq!(m["dirlink"].kind, NodeKind::Symlink);
    assert!(m["dirlink"].flags.contains(NodeFlags::NOT_FOLLOWED));
    // Counted, flagged, kept a file.
    assert_eq!(m["deduped"].kind, NodeKind::File);
    assert!(m["deduped"].flags.contains(NodeFlags::NOT_FOLLOWED));
    assert_eq!(report.not_entered, 1);
    // What lies under the junction is left out on purpose, and said so: not
    // adopted, so no folder of strays appears for it.
    assert_eq!(report.unreachable.files, 1);
    assert_eq!(report.unreachable.allocated, 13 * CB);
    assert_eq!(report.adopted.files, 0);
    // The report says which tags the entries in the tree carry.
    let mut tags = report.reparse.by_tag.clone();
    tags.sort_unstable();
    assert_eq!(
        tags,
        [(0x8000_0013, 1), (0xA000_0003, 1), (0xA000_000C, 2),]
    );
    assert_eq!(report.reparse.other, 0);
}

/// A file the system's overlay layer compressed: the main stream is all hole
/// and marked as a reparse point, and the data lives in a named stream.
fn overlay_file(v: &mut Volume, name: &str, parent: u32, length: u64, packed: u64) -> u32 {
    let rec = v.file(name, parent, length);
    let hole = length.div_ceil(CB);
    {
        let e = v.ent_mut(rec);
        e.streams[0].runs = vec![(None, hole)];
        e.streams[0].flags = 0x8000;
        e.streams[0].unit = 4;
        e.attrs |= 0x200;
    }
    v.reparse(rec, 0x8000_0017);
    v.stream(rec, "WofCompressedData", packed);
    rec
}

#[test]
fn the_bytes_of_an_overlay_compressed_file_are_in_its_named_stream_and_the_full_count_has_them() {
    let build = || {
        let mut v = Volume::new(Spec::default());
        let d = v.dir("sys", ROOT);
        // 143 clusters long, 51 clusters of compressed data.
        overlay_file(&mut v, "a.dll", d, 143 * CB - 7, 51 * CB - 9);
        let linked = overlay_file(&mut v, "b.dll", d, 20 * CB, 6 * CB);
        v.link(linked, "b-again.dll", d);
        v
    };
    let (_, listing_report, listing) = scan(build(), Accounting::Listing);
    let (_, full_report, full) = scan(build(), Accounting::Full);
    let packed = 51 * CB;
    // Counting what a listing reports (the main stream) finds no clusters.
    assert_eq!(listing["sys/a.dll"].allocated, 0);
    assert_eq!(listing["sys/a.dll"].logical, 143 * CB - 7);
    // The full count has the named stream's clusters, and nothing is lost:
    // that is what the file holds on the disk.
    assert_eq!(full["sys/a.dll"].allocated, packed);
    assert_eq!(full_report.extras.stream_allocated, packed + 6 * CB);
    assert_eq!(listing_report.extras.stream_allocated, packed + 6 * CB);
    // A hard-linked one carries its bytes once.
    let names = ["sys/b.dll", "sys/b-again.dll"];
    let held: u64 = names.iter().map(|n| full[*n].allocated).sum();
    assert_eq!(held, 6 * CB);
    // Both counts mark it for what it is: not followed, and holding fewer
    // bytes than its length.
    for m in [&listing, &full] {
        for n in ["sys/a.dll", "sys/b.dll"] {
            assert!(m[n].flags.contains(NodeFlags::NOT_FOLLOWED), "{n}");
            assert!(m[n].flags.contains(NodeFlags::SPARSE_OR_COMPRESSED), "{n}");
        }
    }
    assert_eq!(full_report.reparse.by_tag, [(0x8000_0017, 3)]);
}

#[test]
fn a_compressed_stream_small_enough_for_the_record_holds_no_clusters_and_its_bytes_are_counted() {
    // A file of 6,384 bytes that the overlay layer packs into 500: the stream
    // is a value of the file's own record, so it has no clusters. The full
    // count holds none for the file and says how long the stream is (in the
    // file's length); a stream of 700 bytes does not fit and is in clusters.
    let build = || {
        let mut v = Volume::new(Spec::default());
        let d = v.dir("sys", ROOT);
        overlay_file(&mut v, "small.xml", d, 6384, 500);
        overlay_file(&mut v, "bigger.xml", d, 9000, 700);
        v
    };
    let (_, report, full) = scan(build(), Accounting::Full);
    let small = &full["sys/small.xml"];
    assert_eq!(
        small.allocated, 0,
        "a stream held in the record has no clusters"
    );
    assert_eq!(
        small.logical,
        6384 + 500,
        "the length is the main stream's plus the named stream's"
    );
    let bigger = &full["sys/bigger.xml"];
    assert_eq!(
        bigger.allocated, CB,
        "a stream past the record is one cluster"
    );
    assert_eq!(bigger.logical, 9000 + 700);
    // The extras say the same: 1,200 bytes of streams, one cluster of them.
    assert_eq!(report.extras.stream_logical, 500 + 700);
    assert_eq!(report.extras.stream_allocated, CB);
    assert_eq!(report.reparse.by_tag, [(0x8000_0017, 2)]);
}

#[test]
fn reparse_tags_are_counted_apart_up_to_a_limit() {
    let mut counts = crate::ReparseCounts::default();
    for tag in 0..70u32 {
        counts.note(tag);
        counts.note(tag);
    }
    // The tags seen first stay apart, the rest are pooled, and none is lost.
    assert_eq!(counts.by_tag.len(), crate::ReparseCounts::MAX_TAGS);
    assert!(counts.by_tag.iter().all(|&(t, n)| t < 64 && n == 2));
    assert_eq!(counts.other, 12);
}

#[test]
fn a_plain_volume_has_no_reparse_entries() {
    let mut v = Volume::new(Spec::default());
    let d = v.dir("docs", ROOT);
    v.file("a", d, 10);
    let (_, report, _) = full(v);
    assert_eq!(report.reparse, crate::ReparseCounts::default());
}

#[test]
fn cloud_placeholders_are_sized_from_their_records_and_flagged() {
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let mut v = Volume::new(Spec::default());
    let a = v.file("offline.docx", ROOT, 123_456);
    // The data is not on this machine: no clusters, a size.
    {
        let e = v.ent_mut(a);
        e.streams[0].runs = vec![(None, 31)];
        e.streams[0].size = 123_456;
        e.attrs |= OFFLINE;
    }
    // A resident stream cannot be that large; use a non-resident one with
    // no clusters backing it, as a placeholder has.
    let b = v.file("recall.bin", ROOT, 70_000);
    v.set_attrs(b, 0x20 | RECALL_ON_DATA_ACCESS | 0x400);
    let c = v.file("open.bin", ROOT, 70_000);
    v.set_attrs(c, 0x20 | RECALL_ON_OPEN);
    let d = v.dir("cloud", ROOT);
    v.set_attrs(d, 0x1000_0000 | RECALL_ON_DATA_ACCESS);
    v.file("child", d, 3_000);
    let (result, _, m) = full(v);
    for n in ["recall.bin", "open.bin"] {
        assert!(m[n].flags.contains(NodeFlags::CLOUD_PLACEHOLDER), "{n}");
        assert_eq!(m[n].kind, NodeKind::File);
    }
    assert_eq!(m["recall.bin"].logical, 70_000);
    assert_eq!(m["recall.bin"].allocated, 18 * CB);
    // Not sparse: placeholders keep their own accounting.
    assert!(
        !m["recall.bin"]
            .flags
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert!(
        m["offline.docx"]
            .flags
            .contains(NodeFlags::CLOUD_PLACEHOLDER)
    );
    // The table can be read without fetching anything, so a placeholder
    // folder is listed.
    assert!(m["cloud"].flags.contains(NodeFlags::CLOUD_PLACEHOLDER));
    assert_eq!(m["cloud/child"].logical, 3_000);
    assert!(result.snapshot.scan_complete());
}

#[test]
fn extension_records_are_joined_to_their_file() {
    for in_clusters in [false, true] {
        let mut v = Volume::new(Spec::default());
        let d = v.dir("d", ROOT);
        let f = v.file("big", ROOT, 30_000);
        v.stream(f, "ads", 9_000);
        v.link(f, "other", d);
        v.with_extension(f, in_clusters);
        let (_, report, m) = full(v);
        let big = &m["big"];
        let other = &m["d/other"];
        assert_eq!(
            big.logical + other.logical,
            30_000 + 9_000,
            "{in_clusters}: {m:#?}"
        );
        assert_eq!(big.allocated + other.allocated, 8 * CB + 3 * CB);
        assert!(big.flags.contains(NodeFlags::HARD_LINK));
        assert!(!big.failed && !other.failed);
        assert_eq!(report.diagnostics.total(), 0, "{:?}", report.diagnostics);
        assert_eq!(report.extras.streams, 1);
    }
}

fn find_record(_img: &[u8], rb: usize, area: u64, rec: usize) -> usize {
    // Single-run table at the area start.
    (area * CB) as usize + rec * rb
}

#[test]
fn an_extension_record_that_is_gone_marks_its_file_damaged() {
    let mut v = Volume::new(Spec::default());
    let f = v.file("big", ROOT, 30_000);
    v.stream(f, "ads", 9_000);
    v.with_extension(f, false);
    let img = v.finish();
    // Mark the extension record (the first number after the files) unused.
    let mut broken = img.clone();
    let at = find_record(&img, 1024, 4, 25);
    broken[at + 0x16] = 0;
    let (result, report, m) = scan_image(broken, Accounting::Full, Some(1));
    assert!(m["big"].failed, "{:?}", m["big"]);
    assert_eq!(report.diagnostics.count(ProblemKind::ExtensionMissing), 1);
    assert!(result.snapshot.root().unwrap().has_unreadable_below());
    assert!(result.snapshot.root().unwrap().bytes_are_lower_bound());
    // The base's own part is still counted: a lower bound, flagged.
    assert_eq!(m["big"].logical, 0);
}

#[test]
fn an_extension_record_the_list_does_not_name_is_ignored() {
    let mut v = Volume::new(Spec::default());
    let f = v.file("plain", ROOT, 700);
    let img = v.finish();
    // A record in use that claims to extend `plain` but is not in its list.
    let mut broken = img;
    let ext = RecordBuilder::new(1024, 1)
        .extension_of(u64::from(f), 1)
        .number(20)
        .data_resident("bogus", &[1u8; 500])
        .build();
    let at = 0;
    let _ = at;
    let at = find_record(&broken, 1024, 4, 20);
    broken[at..at + 1024].copy_from_slice(&ext);
    let (_, report, m) = scan_image(broken, Accounting::Full, Some(1));
    assert_eq!(m["plain"].logical, 700);
    assert_eq!(report.diagnostics.count(ProblemKind::ExtensionUnlisted), 1);
    assert_eq!(report.extras.streams, 0);
}

#[test]
fn orphans_and_loops_are_placed_in_a_marked_folder_not_followed() {
    let mut v = Volume::new(Spec::default());
    let a = v.dir("a", ROOT);
    let b = v.dir("b", a);
    v.file("ok", a, 10_000);
    let lost = v.file("lost", b, 20_000);
    // `a` and `b` name each other as parents and nothing names them from the
    // root any more: a cycle that the root does not reach.
    v.ent_mut(a).names[0].parent = u64::from(b);
    v.ent_mut(a).names[0].parent_seq = 1;
    // A file whose directory has been reused since the name was written.
    let stale = v.file("stale", ROOT, 8_000);
    v.ent_mut(stale).names[0].parent_seq = 77;
    // A file with no name at all.
    let nameless = v.file("nameless", ROOT, 6_000);
    v.ent_mut(nameless).names.clear();
    let _ = lost;
    let (result, report, m) = full(v);
    let folder = String::from_utf8_lossy(crate::emit::ORPHANS_NAME).into_owned();
    // Not at their old places, but in the folder, with their bytes.
    assert!(!m.contains_key("a") && !m.contains_key("stale"));
    for name in ["a", "b", "ok", "lost", "stale"] {
        assert!(
            m.contains_key(&format!("{folder}/{name}")),
            "{name}: {:?}",
            m.keys()
        );
    }
    assert!(m.contains_key(&format!("{folder}/<record {nameless}>")));
    assert_eq!(report.diagnostics.count(ProblemKind::ParentStale), 1);
    // ok (3), lost (5), stale (2), nameless (2) clusters; the two directories
    // hold none.
    assert_eq!(report.adopted.files, 2 + 2 + 2);
    assert_eq!(report.adopted.allocated, (3 + 5 + 2 + 2) * CB);
    assert_eq!(report.unreachable.files, 0);
    assert_eq!(subtree_allocated(&result, &folder), (3 + 5 + 2 + 2) * CB);
    // The folder says where the bytes are not known to belong, and the root
    // does not claim to be exact.
    assert!(m[&folder].failed);
    let root = result.snapshot.root().unwrap();
    assert!(root.has_unreadable_below());
    assert!(root.bytes_are_lower_bound());
    assert!(!root.is_unreadable());
    assert!(result.snapshot.scan_complete());
}

/// Files that nothing reaches must show up in the tree's state, not only in
/// the report: the root of a volume that hides bytes may not read complete.
#[test]
fn files_no_directory_reaches_do_not_leave_the_root_looking_exact() {
    let mut v = Volume::new(Spec::default());
    let gone = v.dir("gone", ROOT);
    v.file("user-video.mkv", gone, 4_000_000);
    // Its directory's name is stale.
    v.ent_mut(gone).names[0].parent_seq = 77;
    let stale = v.file("stale", ROOT, 2_000_000);
    v.ent_mut(stale).names[0].parent_seq = 77;
    let (result, report, m) = full(v);
    let root = result.snapshot.root().unwrap();
    assert!(root.has_unreadable_below(), "{:?}", root.flags());
    assert!(root.bytes_are_lower_bound());
    assert_eq!(report.adopted.files, 3);
    assert_eq!(report.adopted.allocated, 6_004_736);
    let folder = String::from_utf8_lossy(crate::emit::ORPHANS_NAME).into_owned();
    assert_eq!(subtree_allocated(&result, &folder), 6_004_736);
    // And the bytes are in the root's total, not left to the gap.
    let mft = m["$MFT"].allocated;
    assert_eq!(root.allocated(), mft + 6_004_736);
}

/// The root's children called `name`, as the tree has them (a name is not
/// unique: a real directory may be called what the made-up folder is).
fn root_children<'a>(result: &'a ScanResult, name: &str) -> Vec<bs_tree::NodeRef<'a>> {
    result
        .snapshot
        .root()
        .unwrap()
        .children()
        .filter(|c| c.name() == name.as_bytes())
        .collect()
}

#[test]
fn the_folder_of_strays_is_marked_made_up_and_nothing_in_it_is_a_real_path() {
    let mut v = Volume::new(Spec::default());
    v.file("fine", ROOT, 5_000);
    let stale = v.file("stale", ROOT, 8_000);
    v.ent_mut(stale).names[0].parent_seq = 77;
    let (result, report, _) = full(v);
    assert_eq!(report.adopted.files, 1);
    let folder = String::from_utf8_lossy(crate::emit::ORPHANS_NAME).into_owned();
    let made_up = root_children(&result, &folder);
    assert_eq!(made_up.len(), 1);
    assert!(made_up[0].is_synthetic());
    let inside: Vec<_> = made_up[0].children().collect();
    assert_eq!(inside.len(), 1);
    assert!(!inside[0].is_synthetic());
    assert!(inside[0].is_under_synthetic());
    // The rest of the tree is real.
    for c in result.snapshot.root().unwrap().children() {
        if c.id() != made_up[0].id() {
            assert!(
                !c.is_under_synthetic(),
                "{:?}",
                String::from_utf8_lossy(c.name())
            );
        }
    }
    assert!(!result.snapshot.root().unwrap().is_under_synthetic());
}

/// POSIX names on NTFS may hold `<` and `>`, so a real directory can be called
/// what the made-up folder is called. Only the mark tells them apart.
#[test]
fn a_real_directory_with_the_name_of_the_folder_of_strays_is_not_mistaken_for_it() {
    let folder = String::from_utf8_lossy(crate::emit::ORPHANS_NAME).into_owned();
    let mut v = Volume::new(Spec::default());
    let real = v.dir(&folder, ROOT);
    v.file("mine", real, 12_288);
    let stale = v.file("stray", ROOT, 8_000);
    v.ent_mut(stale).names[0].parent_seq = 77;
    let (result, report, _) = full(v);
    assert_eq!(report.adopted.files, 1);
    let same_name = root_children(&result, &folder);
    assert_eq!(
        same_name.len(),
        2,
        "the real directory and the made-up folder"
    );
    let synthetic: Vec<_> = same_name.iter().filter(|n| n.is_synthetic()).collect();
    let real_ones: Vec<_> = same_name.iter().filter(|n| !n.is_synthetic()).collect();
    assert_eq!((synthetic.len(), real_ones.len()), (1, 1));
    // The user's file is under the real one and may be acted on; the stray
    // is under the made-up one and may not.
    let mine = real_ones[0].children().next().expect("the real file");
    assert_eq!(mine.name(), b"mine");
    assert!(!mine.is_under_synthetic());
    assert!(!real_ones[0].is_under_synthetic());
    let stray = synthetic[0].children().next().expect("the stray");
    assert_eq!(stray.name(), b"stray");
    assert!(stray.is_under_synthetic());
    // The made-up one is the one that reads as a lower bound.
    assert!(synthetic[0].is_unreadable());
    assert!(!real_ones[0].is_unreadable());
    result.snapshot.verify().unwrap();
}

#[test]
fn a_stray_is_described_like_any_other_entry() {
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    let mut v = Volume::new(Spec::default());
    let strays = v.dir("strays", ROOT);
    // Each of these lives in a directory that is gone (a stale name).
    let placeholder = v.file("cloud.docx", strays, 70_000);
    v.set_attrs(placeholder, 0x20 | RECALL_ON_DATA_ACCESS | 0x400);
    let symlink = v.file("link", strays, 0);
    v.reparse(symlink, 0xA000_000C);
    let sparse = v.sparse_file("holes.bin", strays, 2, 100, 3);
    let movie = v.file("movie.mkv", strays, 50_000);
    let plain = v.file("notes.zzz", strays, 50_000);
    let hardly = v.file("twice", strays, 20_000);
    v.link(hardly, "twice-again", strays);
    for rec in [placeholder, symlink, sparse, movie, plain, hardly] {
        for n in &mut v.ent_mut(rec).names {
            n.parent_seq = 77;
        }
    }
    // The directory itself is gone as well: nothing reaches its children.
    v.ent_mut(strays).names[0].parent_seq = 77;
    let (result, report, _) = full(v);
    assert_eq!(report.adopted.files, 7);
    // A stray that is a link is counted by its tag like any other; the one
    // whose attributes say reparse point and whose table holds no tag is
    // counted under tag 0.
    let mut tags = report.reparse.by_tag.clone();
    tags.sort_unstable();
    assert_eq!(tags, [(0, 1), (0xA000_000C, 1)]);
    let folder = String::from_utf8_lossy(crate::emit::ORPHANS_NAME).into_owned();
    let made_up = root_children(&result, &folder);
    let at = |name: &str| {
        made_up[0]
            .children()
            .find(|c| c.name() == name.as_bytes())
            .unwrap_or_else(|| panic!("no {name}"))
    };
    assert!(
        at("cloud.docx")
            .flags()
            .contains(NodeFlags::CLOUD_PLACEHOLDER)
    );
    assert_eq!(at("link").kind(), NodeKind::Symlink);
    assert!(at("link").flags().contains(NodeFlags::NOT_FOLLOWED));
    assert!(
        at("holes.bin")
            .flags()
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert!(
        !at("notes.zzz")
            .flags()
            .contains(NodeFlags::SPARSE_OR_COMPRESSED)
    );
    assert_eq!(at("notes.zzz").category().0, 0);
    let video = bs_scan::Classifier::with_environment(&bs_scan::Environment::new())
        .category(None, NodeKind::File, b"movie.mkv")
        .0;
    assert_ne!(video, 0);
    assert_eq!(at("movie.mkv").category().0, video);
    assert_eq!(at("strays").kind(), NodeKind::Other);
    // A file that has two names is flagged, though only one is placed.
    let twice = made_up[0]
        .children()
        .find(|c| c.name() == b"twice" || c.name() == b"twice-again")
        .expect("the file with two names");
    assert!(twice.flags().contains(NodeFlags::HARD_LINK));
    result.snapshot.verify().unwrap();
}

/// The reserved records every volume has (in use, no name, no bytes) are
/// not files; they are the baseline, and they do not mark the root.
#[test]
fn reserved_records_with_no_name_and_no_bytes_are_not_files() {
    let mut v = Volume::new(Spec::default());
    v.file("x", ROOT, 10);
    for no in 12..16u32 {
        v.raw.insert(
            no,
            RecordBuilder::new(1024, 1).standard_info(0x6, 0).build(),
        );
    }
    let (result, report, m) = full(v);
    let folder = String::from_utf8_lossy(crate::emit::ORPHANS_NAME).into_owned();
    assert!(!m.contains_key(&folder));
    assert_eq!(report.adopted.files, 0);
    assert_eq!(
        (report.unreachable.files, report.unreachable.allocated),
        (4, 0)
    );
    let root = result.snapshot.root().unwrap();
    assert!(root.is_complete() && !root.has_unreadable_below());
    assert!(!root.bytes_are_lower_bound());
}

#[test]
fn a_record_that_names_another_slot_is_lost() {
    let mut v = Volume::new(Spec::default());
    v.file("a", ROOT, 100);
    let b = v.file("b", ROOT, 200);
    let mut img = v.finish();
    // The record in `b`'s slot says it is record 3.
    let at = find_record(&img, 1024, 4, b as usize);
    img[at + 0x2C..at + 0x30].copy_from_slice(&3u32.to_le_bytes());
    let (result, report, m) = scan_image(img, Accounting::Full, Some(1));
    assert!(m.contains_key("a") && !m.contains_key("b"));
    assert_eq!(report.lost_records, 1);
    assert_eq!(report.diagnostics.count(ProblemKind::RecordHeader), 1);
    assert!(result.snapshot.root().unwrap().is_unreadable());
}

#[test]
fn a_directory_with_two_names_is_entered_once() {
    let mut v = Volume::new(Spec::default());
    let a = v.dir("a", ROOT);
    v.file("inside", a, 5_000);
    v.link(a, "again", ROOT);
    let (_, report, m) = full(v);
    let present = ["a", "again"]
        .iter()
        .filter(|n| m.contains_key(**n))
        .count();
    assert_eq!(present, 1);
    assert_eq!(
        report.diagnostics.count(ProblemKind::DirectoryLinkedTwice),
        1
    );
    assert_eq!(m.keys().filter(|k| k.ends_with("inside")).count(), 1);
}

#[test]
fn names_are_kept_exactly_and_unusable_ones_are_marked() {
    let mut v = Volume::new(Spec::default());
    let long = "n".repeat(255);
    v.file(&long, ROOT, 1);
    v.file("caf\u{e9} \u{65e5}\u{672c} \u{1F600}", ROOT, 2);
    let lone = v.file("x", ROOT, 3);
    v.ent_mut(lone).names[0].units = vec![0x61, 0xD800, 0x62];
    let slash = v.file("y", ROOT, 4);
    v.ent_mut(slash).names[0].units = "a\\b".encode_utf16().collect();
    let (_, report, m) = full(v);
    assert!(m.contains_key(&long));
    assert_eq!(m["caf\u{e9} \u{65e5}\u{672c} \u{1F600}"].logical, 2);
    // An unpaired surrogate becomes three bytes, nothing lost.
    let raw: Vec<u8> = vec![b'a', 0xED, 0xA0, 0x80, b'b'];
    let key = String::from_utf8_lossy(&raw).into_owned();
    assert!(m.contains_key(&key), "{:?}", m.keys().collect::<Vec<_>>());
    // A name no directory can hold stays visible as damaged.
    assert_eq!(report.diagnostics.count(ProblemKind::NameUnusable), 1);
    let damaged = m.values().filter(|i| i.failed).count();
    assert_eq!(damaged, 1);
}

#[test]
fn records_that_cannot_be_read_mark_the_root() {
    let mut v = Volume::new(Spec::default());
    v.file("a", ROOT, 100);
    let b = v.file("b", ROOT, 200);
    let c = v.file("c", ROOT, 300);
    let img = v.finish();
    // `b`: a torn write. `c`: an interrupted one.
    let mut broken = img.clone();
    let at = find_record(&img, 1024, 4, b as usize);
    broken[at + 510] ^= 0xFF;
    let at = find_record(&img, 1024, 4, c as usize);
    broken[at..at + 4].copy_from_slice(b"BAAD");
    let (result, report, m) = scan_image(broken, Accounting::Full, Some(1));
    assert!(m.contains_key("a") && !m.contains_key("b") && !m.contains_key("c"));
    assert_eq!(report.lost_records, 2);
    assert_eq!(report.diagnostics.count(ProblemKind::RecordFixup), 1);
    assert_eq!(report.diagnostics.count(ProblemKind::RecordBadMagic), 1);
    let root = result.snapshot.root().unwrap();
    assert!(root.is_unreadable(), "the root cannot claim to be complete");
}

#[test]
fn a_damaged_attribute_keeps_the_file_and_flags_it() {
    let mut v = Volume::new(Spec::default());
    let f = v.file("f", ROOT, 20_000);
    let img = v.finish();
    let mut broken = img;
    let at = find_record(&broken, 1024, 4, f as usize);
    let mut rec = broken[at..at + 1024].to_vec();
    unstamp(&mut rec);
    // The data attribute is the last before the end marker; give it a
    // length that runs past the record.
    let attrs_at = u16::from_le_bytes([rec[0x14], rec[0x15]]) as usize;
    let mut pos = attrs_at;
    let mut last = pos;
    while u32::from_le_bytes(rec[pos..pos + 4].try_into().unwrap()) != 0xFFFF_FFFF {
        last = pos;
        pos += u32::from_le_bytes(rec[pos + 4..pos + 8].try_into().unwrap()) as usize;
    }
    rec[last + 4..last + 8].copy_from_slice(&0x7FFF_FFF8u32.to_le_bytes());
    stamp(&mut rec);
    broken[at..at + 1024].copy_from_slice(&rec);
    let (_, report, m) = scan_image(broken, Accounting::Full, Some(1));
    assert!(m["f"].failed);
    assert_eq!(report.diagnostics.count(ProblemKind::AttributeCorrupt), 1);
}

#[test]
fn a_bad_run_list_flags_the_file_and_counts_nothing_for_it() {
    let mut v = Volume::new(Spec::default());
    let f = v.file("f", ROOT, 20_000);
    // Runs that leave the volume.
    v.ent_mut(f).streams[0].runs = vec![(Some(1_000_000), 5)];
    let (_, report, m) = full(v);
    assert!(m["f"].failed);
    assert_eq!(report.diagnostics.count(ProblemKind::RunlistCorrupt), 1);
    assert_eq!(m["f"].allocated, 0);
}

#[test]
fn threads_do_not_change_the_answer() {
    let build = || {
        let mut v = Volume::new(Spec {
            mft_fragments: 3,
            ..Spec::default()
        });
        let mut dirs = vec![ROOT];
        for i in 0..150 {
            let parent = dirs[i % dirs.len()];
            if i % 5 == 0 {
                dirs.push(v.dir(&format!("d{i}"), parent));
            } else {
                let f = v.file(&format!("f{i}"), parent, (i * 997) as u64);
                if i % 17 == 0 {
                    v.link(f, &format!("l{i}"), ROOT);
                }
            }
        }
        v.finish()
    };
    let (_, r1, m1) = scan_image(build(), Accounting::Full, Some(1));
    for threads in [2, 5] {
        let (_, r, m) = scan_image(build(), Accounting::Full, Some(threads));
        let strip = |m: &BTreeMap<String, Info>| -> Vec<(String, u64, u64, u32)> {
            m.iter()
                .map(|(k, v)| (k.clone(), v.logical, v.allocated, v.flags.bits().into()))
                .collect()
        };
        assert_eq!(strip(&m1), strip(&m), "{threads} threads");
        assert_eq!(r1.files_in_use, r.files_in_use);
    }
}

#[test]
fn volumes_that_are_not_ntfs_or_too_big_are_refused() {
    let v = Volume::new(Spec::default());
    let img = v.finish();
    let mut nope = img.clone();
    nope[3] = b'X';
    let (result, _) = scan_memory(nope, NtfsOptions::default());
    assert!(result.stats.error.is_some());
    assert!(!result.snapshot.scan_complete());
    // A memory limit below what the table needs.
    let options = NtfsOptions {
        max_memory_bytes: 16,
        ..NtfsOptions::default()
    };
    let (result, _) = scan_memory(img.clone(), options);
    assert!(result.stats.error.is_some());
    // An image cut short.
    let (result, _) = scan_memory(img[..img.len() / 2].to_vec(), NtfsOptions::default());
    assert!(result.stats.error.is_some());
}

/// Reads a volume image after cancelling its own scan, so the cancel is
/// certain to come before the read, not racing it. (A cancel sent from
/// another thread right after the start may land after a small volume has
/// been read whole; then the tree really is complete, and that is correct.)
struct CancelsFirst {
    image: Vec<u8>,
    cancel: bool,
}

impl bs_scan::Scanner for CancelsFirst {
    fn name(&self) -> &'static str {
        "ntfs-cancels-first"
    }

    fn scan(
        &self,
        root: &std::path::Path,
        ctx: &bs_scan::ScanContext,
    ) -> Result<bs_scan::ScanEnd, bs_scan::ScanError> {
        if self.cancel {
            ctx.cancel();
        }
        crate::read_volume(
            self.image.as_slice(),
            root.to_string_lossy().as_bytes(),
            &NtfsOptions::default(),
            ctx,
        )
        .map(|(end, _)| end)
        .map_err(|e| bs_scan::ScanError::Root(std::io::Error::other(e)))
    }
}

fn scan_with_cancel(cancel: bool) -> bs_scan::ScanResult {
    use bs_scan::{ScanOptions, start};
    use std::sync::Arc;
    let mut v = Volume::new(Spec::default());
    for i in 0..50 {
        v.file(&format!("f{i}"), ROOT, 10);
    }
    let scanner = Arc::new(CancelsFirst {
        image: v.finish(),
        cancel,
    });
    start(scanner, "C:\\".into(), ScanOptions::default()).wait()
}

#[test]
fn cancelling_keeps_the_partial_tree_marked_incomplete() {
    let result = scan_with_cancel(true);
    result.snapshot.verify().unwrap();
    assert_eq!(result.stats.end, Some(bs_scan::ScanEnd::Cancelled));
    assert!(result.stats.error.is_none(), "{:?}", result.stats.error);
    assert!(!result.snapshot.scan_complete());
}

#[test]
fn the_same_scan_without_a_cancel_is_complete() {
    // The control for the test above: nothing but the cancel differs.
    let result = scan_with_cancel(false);
    result.snapshot.verify().unwrap();
    assert_eq!(result.stats.end, Some(bs_scan::ScanEnd::Completed));
    assert!(result.snapshot.scan_complete());
}

#[test]
fn a_listing_gives_resident_data_its_length_rounded_to_eight() {
    let build = || {
        let mut v = Volume::new(Spec::default());
        v.file("empty", ROOT, 0);
        v.file("ten", ROOT, 10);
        v.file("eight", ROOT, 8);
        v.file("threehundred", ROOT, 300);
        v.file("outside", ROOT, 5_000);
        v
    };
    let (_, _, listing) = scan(build(), Accounting::Listing);
    let sizes: Vec<(u64, u64)> = ["empty", "ten", "eight", "threehundred", "outside"]
        .iter()
        .map(|n| (listing[*n].logical, listing[*n].allocated))
        .collect();
    assert_eq!(
        sizes,
        vec![(0, 0), (10, 16), (8, 8), (300, 304), (5_000, 2 * CB)]
    );
    // The full count has no clusters for them: the bytes are in the table,
    // which is an item of its own.
    let (_, _, full) = full(build());
    assert_eq!(full["ten"].allocated, 0);
    assert_eq!(full["threehundred"].allocated, 0);
    assert_eq!(full["outside"].allocated, 2 * CB);
}

#[test]
fn the_bit_that_means_recall_on_open_also_marks_extended_attributes() {
    const BIT: u32 = 0x4_0000;
    let mut v = Volume::new(Spec::default());
    let with_ea = v.file("with-ea", ROOT, 100);
    v.set_attrs(with_ea, 0x20 | BIT);
    v.extended_attributes(with_ea);
    let without = v.file("placeholder", ROOT, 100);
    v.set_attrs(without, 0x20 | BIT);
    let (_, _, m) = full(v);
    assert!(!m["with-ea"].flags.contains(NodeFlags::CLOUD_PLACEHOLDER));
    assert!(
        m["placeholder"]
            .flags
            .contains(NodeFlags::CLOUD_PLACEHOLDER)
    );
}

#[test]
fn the_memory_limit_is_the_ceiling_or_half_of_what_is_available() {
    use crate::{MEMORY_CEILING, memory_cap};
    const GIB: u64 = 1 << 30;
    assert_eq!(MEMORY_CEILING, 4 * GIB);
    // Plenty of memory: the ceiling.
    assert_eq!(memory_cap(MEMORY_CEILING, Some(64 * GIB)), 4 * GIB);
    assert_eq!(memory_cap(MEMORY_CEILING, Some(8 * GIB)), 4 * GIB);
    // Less than twice the ceiling: half of it.
    assert_eq!(memory_cap(MEMORY_CEILING, Some(6 * GIB)), 3 * GIB);
    assert_eq!(memory_cap(MEMORY_CEILING, Some(GIB)), GIB / 2);
    assert_eq!(memory_cap(MEMORY_CEILING, Some(0)), 0);
    // Not known: the ceiling, unchanged.
    assert_eq!(memory_cap(MEMORY_CEILING, None), 4 * GIB);
    assert_eq!(memory_cap(GIB, Some(u64::MAX)), GIB);
    // As options.
    let o = NtfsOptions::default().with_available_memory(Some(2 * GIB));
    assert_eq!(o.max_memory_bytes, GIB);
    assert_eq!(NtfsOptions::default().max_memory_bytes, MEMORY_CEILING);
}
