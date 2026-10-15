//! The sorting of differences on a live volume (`common::live`), run on
//! trees made by hand and a stand-in for the system. The comparison itself
//! runs only on Windows against the machine's own drive; these tests show,
//! on every platform, what each cause needs before it explains a difference,
//! and that a difference without that evidence stays unexplained.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use bs_scan_ntfs::NtfsOptions;
use bs_tree::{NodeFlags, NodeId, NodeKind};
use common::compare::{Diff, SYSTEM_FOLDER};
use common::live::*;
use common::testkit::{Info, ROOT, Spec, Volume, flatten, scan_memory};
use common::wofrec::{DataStream, Fixups, RecordFacts, WOF_NAME};
use std::collections::HashMap;

/// The scan began here.
const FROM: i64 = 1_000_000;
/// A time before the scan.
const OLD: i64 = 500_000;
/// A time inside the scan.
const NEW: i64 = 1_000_100;

struct Fake(
    HashMap<String, Answer>,
    HashMap<String, Result<RecordFacts, String>>,
);

impl Oracle for Fake {
    fn ask(&self, path: &str) -> Answer {
        self.0
            .get(path)
            .cloned()
            .unwrap_or_else(|| Answer::Unavailable("the stand-in has no answer".into()))
    }

    fn file_record(&self, path: &str) -> Result<RecordFacts, String> {
        self.1
            .get(path)
            .cloned()
            .unwrap_or_else(|| Err("the stand-in has no record".into()))
    }
}

fn says(list: &[(&str, Answer)]) -> Fake {
    Fake(
        list.iter()
            .map(|(p, a)| ((*p).to_owned(), a.clone()))
            .collect(),
        HashMap::new(),
    )
}

impl Fake {
    /// The record the stand-in returns for `path`.
    fn record(mut self, path: &str, record: Result<RecordFacts, String>) -> Fake {
        self.1.insert(path.to_owned(), record);
        self
    }
}

/// A base record with the given data streams.
fn record_of(streams: &[DataStream]) -> RecordFacts {
    RecordFacts {
        sequence: 7,
        number: Some(100),
        is_extension: false,
        has_attribute_list: false,
        fixups: Fixups::Restored,
        streams: streams.to_vec(),
    }
}

fn stream(name: &str, resident: bool, length: u64, allocated: u64) -> DataStream {
    DataStream {
        name: name.to_owned(),
        resident,
        length,
        allocated,
        // A stream in clusters holds what its header allocates, unless a test
        // says otherwise.
        held_clusters: if resident { 0 } else { allocated / 4096 },
        total_allocated: None,
    }
}

/// The main stream of an overlay-compressed file as the volume has it: all
/// hole. Its header allocates the whole length (rounded up to compression
/// units) but the stream holds no cluster.
fn main_stream(length: u64) -> DataStream {
    DataStream {
        allocated: length.div_ceil(65536) * 65536,
        held_clusters: 0,
        total_allocated: Some(0),
        ..stream("", false, length, 0)
    }
}

/// A volume with 4 KiB clusters and 1 KiB table records.
const GEO: Geometry = Geometry {
    cluster_bytes: 4096,
    record_bytes: 1024,
};

fn truth(mtime: i64, logical: u64, allocation: u64) -> Truth {
    Truth {
        mtime,
        logical,
        allocation,
        ..Truth::default()
    }
}

fn found(t: Truth) -> Answer {
    Answer::Found(t)
}

fn info(kind: NodeKind, mtime: i64, size: u64, flags: NodeFlags) -> Info {
    Info {
        id: NodeId::from_index(0),
        kind,
        logical: size,
        allocated: size,
        mtime,
        flags,
        category: 0,
        failed: false,
        origin: None,
    }
}

struct Trees {
    reader: common::live::Tree,
    walker: common::live::Tree,
    next: u32,
    full: Option<common::live::Tree>,
}

impl Trees {
    fn new() -> Trees {
        Trees {
            reader: Default::default(),
            walker: Default::default(),
            next: 1,
            full: None,
        }
    }

    /// An entry both trees have, with the time the reader gives it (the table's,
    /// current) and the time the walker gives it (the listing's copy).
    fn both(
        &mut self,
        path: &str,
        kind: NodeKind,
        reader_time: i64,
        walker_time: i64,
    ) -> &mut Trees {
        self.reader.insert(
            path.as_bytes().to_vec(),
            info(kind, reader_time, 0, NodeFlags::EMPTY),
        );
        self.walker.insert(
            path.as_bytes().to_vec(),
            info(kind, walker_time, 0, NodeFlags::EMPTY),
        );
        self
    }

    fn dir(&mut self, path: &str, time: i64) -> &mut Trees {
        self.both(path, NodeKind::Dir, time, time)
    }

    fn file(&mut self, path: &str, time: i64) -> &mut Trees {
        self.both(path, NodeKind::File, time, time)
    }

    fn reader_only(&mut self, path: &str, kind: NodeKind, time: i64) -> &mut Trees {
        self.reader.insert(
            path.as_bytes().to_vec(),
            info(kind, time, 0, NodeFlags::EMPTY),
        );
        self
    }

    fn walker_only(&mut self, path: &str, kind: NodeKind, time: i64) -> &mut Trees {
        self.walker.insert(
            path.as_bytes().to_vec(),
            info(kind, time, 0, NodeFlags::EMPTY),
        );
        self
    }

    /// The reader's full count of `path`: its allocated bytes.
    fn full_count(&mut self, path: &str, allocated: u64) -> &mut Trees {
        let mut i = info(NodeKind::File, OLD, 0, NodeFlags::EMPTY);
        i.allocated = allocated;
        self.full
            .get_or_insert_with(Default::default)
            .insert(path.as_bytes().to_vec(), i);
        self
    }

    fn fail_in_walker(&mut self, path: &str) -> &mut Trees {
        self.walker.get_mut(path.as_bytes()).unwrap().failed = true;
        self
    }

    /// Makes the named entries the hard-linked names of one file, in both trees.
    fn linked(&mut self, names: &[(&str, bool, bool)]) -> &mut Trees {
        let id = NodeId::from_index(self.next);
        self.next += 1;
        for (name, in_reader, in_walker) in names {
            for (tree, present) in [(&mut self.reader, in_reader), (&mut self.walker, in_walker)] {
                if *present {
                    let mut i = info(NodeKind::File, OLD, 0, NodeFlags::HARD_LINK);
                    i.id = id;
                    tree.insert(name.as_bytes().to_vec(), i);
                }
            }
        }
        self
    }

    fn flags(&mut self, path: &str, reader: NodeFlags, walker: NodeFlags) -> &mut Trees {
        self.reader.get_mut(path.as_bytes()).unwrap().flags = reader;
        self.walker.get_mut(path.as_bytes()).unwrap().flags = walker;
        self
    }

    fn sort(&self, diffs: Vec<Diff>, root_time: i64, oracle: &Fake) -> Outcome {
        classify_diffs(
            diffs,
            &self.reader,
            &self.walker,
            root_time,
            FROM,
            oracle,
            self.full.as_ref(),
        )
    }

    /// The verdict on a single difference.
    fn judge(&self, d: Diff, root_time: i64, oracle: &Fake) -> Result<Verdict, String> {
        let mut out = self.sort(vec![d], root_time, oracle);
        assert_eq!(out.explained.len() + out.unexplained.len(), 1);
        match out.explained.pop() {
            Some((_, v)) => Ok(v),
            None => Err(out.unexplained.pop().unwrap().1),
        }
    }
}

fn diff(path: &str, field: &'static str, reader: &str, other: &str) -> Diff {
    Diff {
        path: path.into(),
        field,
        reader: reader.into(),
        other: other.into(),
    }
}

fn class_of(v: Result<Verdict, String>) -> &'static str {
    match v {
        Ok(v) => v.class,
        Err(_) => "unexplained",
    }
}

#[test]
fn a_listing_behind_the_system_is_stale_only_where_the_system_agrees_with_the_reader() {
    let mut t = Trees::new();
    t.dir("logs", OLD).file("logs/a.log", OLD);
    let mtime = diff("logs/a.log", "mtime", "400000", "300000");
    // The system gives the reader's time: the listing's copy is the stale one.
    let agree = says(&[("logs/a.log", found(truth(400_000, 0, 0)))]);
    let v = t.judge(mtime.clone(), 0, &agree).unwrap();
    assert_eq!(v.class, C_STALE);
    assert_eq!(v.basis, B_STALE);
    // The system gives the walker's time: the reader is the odd one out.
    let walker_right = says(&[("logs/a.log", found(truth(300_000, 0, 0)))]);
    assert!(t.judge(mtime.clone(), 0, &walker_right).is_err());
    // A third time: neither is confirmed.
    let other = says(&[("logs/a.log", found(truth(450_000, 0, 0)))]);
    assert!(t.judge(mtime.clone(), 0, &other).is_err());
    // The system would not say, or says the file is gone: no evidence.
    let silent = says(&[]);
    assert!(t.judge(mtime.clone(), 0, &silent).is_err());
    let gone = says(&[("logs/a.log", Answer::Gone)]);
    assert!(t.judge(mtime, 0, &gone).is_err());
}

#[test]
fn sizes_are_judged_like_times_and_a_sparse_file_may_be_held_in_fewer_clusters() {
    let mut t = Trees::new();
    t.file("f.log", OLD);
    let logical = diff("f.log", "logical", "11749", "0");
    let allocated = diff("f.log", "allocated", "16384", "0");
    let sys = says(&[("f.log", found(truth(OLD, 11_749, 16_384)))]);
    assert_eq!(class_of(t.judge(logical.clone(), 0, &sys)), C_STALE);
    assert_eq!(class_of(t.judge(allocated.clone(), 0, &sys)), C_STALE);
    // The system gives other sizes: the reader is not confirmed.
    let wrong = says(&[("f.log", found(truth(OLD, 11_000, 12_288)))]);
    assert_eq!(class_of(t.judge(logical, 0, &wrong)), "unexplained");
    assert_eq!(
        class_of(t.judge(allocated.clone(), 0, &wrong)),
        "unexplained"
    );
    // A sparse file: the clusters it holds are what the reader reports.
    let sparse = says(&[(
        "f.log",
        found(Truth {
            held: Some(16_384),
            ..truth(OLD, 5 << 20, 5 << 20)
        }),
    )]);
    assert_eq!(class_of(t.judge(allocated, 0, &sparse)), C_STALE);
}

#[test]
fn a_write_after_the_scan_began_explains_a_difference_and_one_before_it_does_not() {
    let mut t = Trees::new();
    t.file("busy.etl", NEW).file("idle.etl", OLD);
    // Written inside the scan: the two looks may differ.
    let d = diff("busy.etl", "logical", "900", "700");
    let sys = says(&[("busy.etl", found(truth(NEW + 50, 1000, 4096)))]);
    let v = t.judge(d.clone(), 0, &sys).unwrap();
    assert_eq!((v.class, v.basis), (C_CHANGED, B_WRITTEN));
    // Even with no answer from the system, the entry's own time is evidence.
    assert_eq!(class_of(t.judge(d, 0, &says(&[]))), C_CHANGED);
    // Not written since before the scan, no answer, nothing explains it.
    let d = diff("idle.etl", "logical", "900", "700");
    assert_eq!(class_of(t.judge(d, 0, &says(&[]))), "unexplained");
    // The system's answer is old and disagrees with the reader.
    let d = diff("idle.etl", "logical", "900", "700");
    let sys = says(&[("idle.etl", found(truth(OLD, 700, 4096)))]);
    assert_eq!(class_of(t.judge(d, 0, &sys)), "unexplained");
}

fn reparse_facts(tag: u32, backing: Option<u32>) -> Answer {
    found(Truth {
        attributes: if tag == 0 { 0x20 } else { 0x420 },
        reparse_tag: tag,
        backing,
        ..truth(OLD, 100, 4096)
    })
}

#[test]
fn a_file_the_reader_calls_a_link_is_explained_only_by_the_systems_own_overlay_tag() {
    let mut t = Trees::new();
    t.dir("ndk", OLD).file("ndk/a.md", OLD);
    t.flags(
        "ndk/a.md",
        NodeFlags::NOT_FOLLOWED | NodeFlags::SPARSE_OR_COMPRESSED,
        NodeFlags::SPARSE_OR_COMPRESSED,
    );
    let d = diff("ndk/a.md", "flags", "0x180", "0x100");
    let wof = says(&[("ndk/a.md", reparse_facts(TAG_WOF, None))]);
    let v = t.judge(d.clone(), 0, &wof).unwrap();
    assert_eq!((v.class, v.basis), (C_REPARSE, B_REPARSE));
    // The tag may be hidden even from a handle opened as a reparse point; the
    // external backing then says the same.
    let backed = says(&[("ndk/a.md", reparse_facts(0, Some(PROVIDER_FILE)))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &backed)), C_REPARSE);
    // Any other tag is not covered: it might be a real link or a placeholder.
    let dedup = says(&[("ndk/a.md", reparse_facts(0x8000_0013, None))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &dedup)), "unexplained");
    let symlink = says(&[("ndk/a.md", reparse_facts(0xA000_000C, None))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &symlink)), "unexplained");
    // The system shows no reparse point and no backing: the reader is wrong.
    let plain = says(&[("ndk/a.md", reparse_facts(0, None))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &plain)), "unexplained");
    // No answer, or the file is gone.
    assert!(t.judge(d.clone(), 0, &says(&[])).is_err());
    let gone = says(&[("ndk/a.md", Answer::Gone)]);
    assert!(t.judge(d, 0, &gone).is_err());
}

#[test]
fn no_other_difference_in_flags_is_explained_by_the_overlay_tag() {
    let mut t = Trees::new();
    t.file("a", OLD).file("b", OLD).file("c", OLD);
    // A placeholder the reader sees and the walker does not.
    t.flags("a", NodeFlags::CLOUD_PLACEHOLDER, NodeFlags::EMPTY);
    // The walker sees a link that the reader does not.
    t.flags("b", NodeFlags::EMPTY, NodeFlags::NOT_FOLLOWED);
    // Both: not followed in the reader, and a different bit in the walker.
    t.flags(
        "c",
        NodeFlags::NOT_FOLLOWED,
        NodeFlags::SPARSE_OR_COMPRESSED,
    );
    let wof = says(&[
        ("a", reparse_facts(TAG_WOF, None)),
        ("b", reparse_facts(TAG_WOF, None)),
        ("c", reparse_facts(TAG_WOF, None)),
    ]);
    for (p, r, w) in [
        ("a", "0x40", "0x0"),
        ("b", "0x0", "0x80"),
        ("c", "0x80", "0x100"),
    ] {
        let v = t.judge(diff(p, "flags", r, w), 0, &wof);
        assert!(v.is_err(), "{p}: {v:?}");
    }
}

#[test]
fn a_name_that_appeared_is_explained_by_a_folder_written_since_the_scan_began() {
    let mut t = Trees::new();
    t.dir("apps", NEW).dir("old", OLD);
    t.reader_only("apps/new", NodeKind::Dir, NEW)
        .reader_only("apps/new/f.dll", NodeKind::File, NEW)
        .reader_only("old/new", NodeKind::File, OLD);
    let none = says(&[]);
    for p in ["apps/new", "apps/new/f.dll"] {
        let v = t
            .judge(diff(p, "presence", "present", "missing"), 0, &none)
            .unwrap();
        assert_eq!((v.class, v.basis), (C_CHANGED, B_FOLDER_WRITTEN), "{p}");
    }
    // Its folder was last written before the scan began.
    let d = diff("old/new", "presence", "present", "missing");
    assert!(t.judge(d, 0, &none).is_err());
}

#[test]
fn a_name_in_the_root_is_judged_by_the_roots_own_time() {
    let mut t = Trees::new();
    t.reader_only("newfile", NodeKind::File, OLD);
    let d = diff("newfile", "presence", "present", "missing");
    assert!(t.judge(d.clone(), OLD, &says(&[])).is_err());
    assert_eq!(class_of(t.judge(d, NEW, &says(&[]))), C_CHANGED);
}

#[test]
fn a_name_that_went_needs_the_system_to_say_it_is_gone() {
    let mut t = Trees::new();
    t.dir("apps", NEW);
    t.walker_only("apps/pkg", NodeKind::Dir, OLD).walker_only(
        "apps/pkg/f.png",
        NodeKind::File,
        OLD,
    );
    let d = diff("apps/pkg/f.png", "presence", "missing", "present");
    // Gone, and the folder it was in was written since the scan began.
    let gone = says(&[("apps/pkg/f.png", Answer::Gone)]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &gone)), C_CHANGED);
    // The system still has it: the reader lost it.
    let there = says(&[("apps/pkg/f.png", found(truth(OLD, 1, 4096)))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &there)), "unexplained");
    // The system would not say, but the folder was written: still explained,
    // and the detail says what was not known.
    let silent = says(&[]);
    let v = t.judge(d, 0, &silent).unwrap();
    assert!(v.detail.contains("would not say"), "{}", v.detail);
    // Gone, but the folder was not written during the scan.
    let mut t = Trees::new();
    t.dir("apps", OLD)
        .walker_only("apps/pkg", NodeKind::Dir, OLD);
    let d = diff("apps/pkg", "presence", "missing", "present");
    let gone = says(&[("apps/pkg", Answer::Gone)]);
    assert_eq!(class_of(t.judge(d, 0, &gone)), "unexplained");
}

#[test]
fn entries_below_a_folder_the_walker_could_not_read_are_explained_by_its_own_error() {
    let mut t = Trees::new();
    t.dir("sys", OLD).fail_in_walker("sys");
    t.reader_only("sys/a", NodeKind::Dir, OLD)
        .reader_only("sys/a/b.dat", NodeKind::File, OLD);
    let none = says(&[]);
    for p in ["sys/a", "sys/a/b.dat"] {
        let v = t
            .judge(diff(p, "presence", "present", "missing"), 0, &none)
            .unwrap();
        assert_eq!(
            (v.class, v.basis),
            (C_WALKER_FAILED, B_WALKER_FAILED),
            "{p}"
        );
    }
    // A folder the walker read fine is not covered.
    let mut t = Trees::new();
    t.dir("ok", OLD);
    t.reader_only("ok/a", NodeKind::File, OLD);
    let d = diff("ok/a", "presence", "present", "missing");
    assert!(t.judge(d, 0, &none).is_err());
}

#[test]
fn only_the_reserved_records_are_system_files_and_only_when_the_reader_alone_has_them() {
    let mut t = Trees::new();
    t.reader_only("$MFT", NodeKind::File, OLD)
        .reader_only("$Extend/$Reparse", NodeKind::File, OLD)
        .reader_only("$MFTcopy", NodeKind::File, OLD)
        .reader_only("docs/$MFT", NodeKind::File, OLD)
        .walker_only("$Bitmap", NodeKind::File, OLD);
    let none = says(&[]);
    for p in ["$MFT", "$Extend/$Reparse"] {
        let v = t
            .judge(diff(p, "presence", "present", "missing"), 0, &none)
            .unwrap();
        assert_eq!((v.class, v.basis), (C_SYSTEM, B_SYSTEM), "{p}");
    }
    // A look-alike, a copy of the name deeper in, and a record the walker has
    // and the reader lacks.
    for (p, r, w) in [
        ("$MFTcopy", "present", "missing"),
        ("docs/$MFT", "present", "missing"),
        ("$Bitmap", "missing", "present"),
    ] {
        assert!(t.judge(diff(p, "presence", r, w), 0, &none).is_err(), "{p}");
    }
}

#[test]
fn hard_linked_bytes_are_judged_by_the_file_and_its_group_by_its_names() {
    let mut t = Trees::new();
    t.dir("tz", OLD)
        .linked(&[("tz/Africa", true, true), ("tz/Algiers", true, true)]);
    let d = diff("tz/Africa", "link group bytes", "(704, 4096)", "(704, 704)");
    // The system agrees with the reader about the file.
    let sys = says(&[("tz/Africa", found(truth(OLD, 704, 4096)))]);
    let v = t.judge(d.clone(), 0, &sys).unwrap();
    assert_eq!((v.class, v.basis), (C_STALE, B_STALE));
    // The system agrees with the listing: the reader is not confirmed.
    let sys = says(&[("tz/Africa", found(truth(OLD, 704, 704)))]);
    assert!(t.judge(d.clone(), 0, &sys).is_err());
    assert!(t.judge(d, 0, &says(&[])).is_err());
}

#[test]
fn a_group_with_a_name_one_side_lacks_is_judged_as_that_name_and_otherwise_by_its_folders() {
    let mut t = Trees::new();
    t.dir("new", NEW).dir("old", OLD);
    t.linked(&[("new/a", true, false), ("old/b", true, true)]);
    let d = diff("new/a", "link group", "2 names", "no such group");
    let v = t.judge(d, 0, &says(&[])).unwrap();
    assert_eq!(v.class, C_CHANGED);
    // Every name in both trees and no folder written: nothing explains it.
    let mut t = Trees::new();
    t.dir("old", OLD)
        .linked(&[("old/a", true, true), ("old/b", true, true)]);
    let d = diff("old/a", "link group", "2 names", "no such group");
    assert!(t.judge(d, 0, &says(&[])).is_err());
    // A name the walker lacks under a folder old enough that no write explains it.
    let mut t = Trees::new();
    t.dir("old", OLD)
        .linked(&[("old/a", true, false), ("old/b", true, true)]);
    let d = diff("old/a", "link group", "2 names", "no such group");
    assert!(t.judge(d, 0, &says(&[])).is_err());
}

#[test]
fn a_difference_in_kind_or_category_is_never_explained() {
    let mut t = Trees::new();
    t.file("busy", NEW);
    let sys = says(&[("busy", found(truth(NEW, 1, 1)))]);
    for field in ["kind", "category"] {
        assert!(
            t.judge(diff("busy", field, "File", "Dir"), 0, &sys)
                .is_err()
        );
    }
}

#[test]
fn the_system_folders_time_is_covered_by_direction_and_the_systems_answer() {
    let mut t = Trees::new();
    t.dir(SYSTEM_FOLDER, OLD);
    let later = diff(SYSTEM_FOLDER, "mtime", "400000", "300000");
    let sys = says(&[(SYSTEM_FOLDER, found(truth(400_000, 0, 0)))]);
    assert_eq!(class_of(t.judge(later.clone(), 0, &sys)), C_SYSTEM_FOLDER);
    // Without the system's answer, the direction alone is checked.
    assert_eq!(class_of(t.judge(later, 0, &says(&[]))), C_SYSTEM_FOLDER);
    // Earlier in the reader than in the walker: not a write.
    let earlier = diff(SYSTEM_FOLDER, "mtime", "300000", "400000");
    assert_eq!(class_of(t.judge(earlier, 0, &says(&[]))), "unexplained");
    // A reader's time later than the system's own answer afterwards.
    let ahead = diff(SYSTEM_FOLDER, "mtime", "500000", "300000");
    let sys = says(&[(SYSTEM_FOLDER, found(truth(400_000, 0, 0)))]);
    assert_eq!(class_of(t.judge(ahead, 0, &sys)), "unexplained");
    // The same name deeper in is an ordinary folder.
    let mut t = Trees::new();
    t.dir("docs", OLD)
        .dir("docs/System Volume Information", OLD);
    let d = diff(
        "docs/System Volume Information",
        "mtime",
        "400000",
        "300000",
    );
    assert_eq!(class_of(t.judge(d, 0, &says(&[]))), "unexplained");
}

#[test]
fn the_outcome_counts_what_the_system_was_asked_once_per_path() {
    let mut t = Trees::new();
    t.file("a", OLD).file("b", OLD).file("c", OLD);
    let sys = says(&[("a", found(truth(400_000, 5, 4096))), ("b", Answer::Gone)]);
    let diffs = vec![
        diff("a", "mtime", "400000", "1"),
        diff("a", "logical", "5", "0"),
        diff("b", "mtime", "400000", "1"),
        diff("c", "mtime", "400000", "1"),
    ];
    let out = t.sort(diffs, 0, &sys);
    assert_eq!((out.found, out.gone, out.unavailable), (1, 1, 1));
    assert_eq!(out.explained.len(), 2);
    assert_eq!(out.unexplained.len(), 2);
    // The report prints without failing.
    report(&out);
}

/// Two in-memory volumes standing for the reader's table and the walker's
/// listing of the same drive: a file the reader sees as a reparse point and a
/// file whose time the listing has not caught up with.
#[test]
fn trees_from_snapshots_are_sorted_the_same_way() {
    let build = |reparse: bool, time: i64| {
        let mut v = Volume::new(Spec::default());
        let d = v.dir("tools", ROOT);
        let f = v.file("a.md", d, 300);
        v.file("b.md", d, 300);
        v.set_mtime(f, time);
        if reparse {
            v.reparse(f, TAG_WOF);
        }
        let (r, _) = scan_memory(v.finish(), NtfsOptions::default());
        assert!(r.stats.error.is_none(), "{:?}", r.stats.error);
        r
    };
    let reader = build(true, 1_700_000_500);
    let walker = build(false, 1_700_000_400);
    let flat = flatten(&reader.snapshot);
    assert!(flat.contains_key(b"tools/a.md".as_slice()));
    let sys = says(&[(
        "tools/a.md",
        found(Truth {
            attributes: 0x420,
            reparse_tag: TAG_WOF,
            ..truth(1_700_000_500, 300, 4096)
        }),
    )]);
    let out = classify(
        &reader.snapshot,
        &walker.snapshot,
        2_000_000_000,
        &sys,
        None,
    );
    let by_field = |f: &str| out.explained.iter().filter(|(d, _)| d.field == f).count();
    assert_eq!(by_field("mtime"), 1, "{:?}", out.unexplained);
    assert_eq!(by_field("flags"), 1, "{:?}", out.unexplained);
    // The reparse flag also changes what the file is, so any other difference
    // would be reported; there is none.
    assert!(out.unexplained.is_empty(), "{:?}", out.unexplained);
    assert!(out.explained.iter().any(|(_, v)| v.class == C_REPARSE));
    assert!(out.explained.iter().any(|(_, v)| v.class == C_STALE));
}

const LENGTH: u64 = 585_728;
const PACKED: u64 = 208_896;

/// What the system says of a file the overlay layer compressed.
fn compressed_facts(allocation: u64) -> Answer {
    found(Truth {
        backing: Some(PROVIDER_FILE),
        ..truth(OLD, LENGTH, allocation)
    })
}

const NF: NodeFlags = NodeFlags::NOT_FOLLOWED;
const SP: NodeFlags = NodeFlags::SPARSE_OR_COMPRESSED;

#[test]
fn a_link_and_a_short_file_flag_are_each_explained_by_what_the_system_says() {
    let mut t = Trees::new();
    t.dir("sys", OLD).file("sys/a.dll", OLD);
    t.flags(
        "sys/a.dll",
        NF | SP | NodeFlags::HARD_LINK,
        NodeFlags::HARD_LINK,
    );
    let d = diff("sys/a.dll", "flags", "0x188", "0x8");
    // Overlay-backed and holding fewer bytes than its length.
    let sys = says(&[("sys/a.dll", compressed_facts(PACKED))]);
    let v = t.judge(d.clone(), 0, &sys).unwrap();
    assert_eq!((v.class, v.basis), (C_REPARSE, B_REPARSE));
    // Overlay-backed, but the system says it holds its whole length: the
    // reader's sparse flag is not confirmed.
    let whole = says(&[("sys/a.dll", compressed_facts(LENGTH))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &whole)), "unexplained");
    // Short, but not behind the overlay layer: the reader's link flag is not
    // confirmed.
    let plain = says(&[("sys/a.dll", found(truth(OLD, LENGTH, PACKED)))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &plain)), "unexplained");
    // No answer.
    assert!(t.judge(d, 0, &says(&[])).is_err());
}

#[test]
fn the_sparse_flag_alone_needs_the_system_to_say_the_file_is_short() {
    let mut t = Trees::new();
    t.file("f", OLD);
    t.flags("f", SP, NodeFlags::EMPTY);
    let d = diff("f", "flags", "0x100", "0x0");
    let short = says(&[("f", found(truth(OLD, LENGTH, PACKED)))]);
    let v = t.judge(d.clone(), 0, &short).unwrap();
    assert_eq!((v.class, v.basis), (C_STALE, B_STALE_SPARSE));
    // A held figure smaller than the allocation counts too.
    let held = says(&[(
        "f",
        found(Truth {
            held: Some(PACKED),
            ..truth(OLD, LENGTH, LENGTH)
        }),
    )]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &held)), C_STALE);
    let whole = says(&[("f", found(truth(OLD, LENGTH, LENGTH)))]);
    assert_eq!(class_of(t.judge(d, 0, &whole)), "unexplained");
}

#[test]
fn flags_beyond_the_two_overlay_bits_are_never_explained_this_way() {
    let mut t = Trees::new();
    t.file("f", OLD);
    let sys = says(&[("f", compressed_facts(PACKED))]);
    for (r, w) in [
        // Also a placeholder in the reader.
        (NF | SP | NodeFlags::CLOUD_PLACEHOLDER, NodeFlags::EMPTY),
        // The walker has a flag the reader lacks.
        (NF | SP, NodeFlags::HARD_LINK),
        // Unreadable in the reader.
        (NF | NodeFlags::UNREADABLE, NodeFlags::EMPTY),
    ] {
        t.flags("f", r, w);
        let d = diff(
            "f",
            "flags",
            &format!("{:#x}", r.bits()),
            &format!("{:#x}", w.bits()),
        );
        assert_eq!(
            class_of(t.judge(d, 0, &sys)),
            "unexplained",
            "{r:?} / {w:?}"
        );
    }
}

#[test]
fn bytes_in_the_compressed_stream_are_explained_only_where_the_full_count_is_the_systems_figure() {
    let mut t = Trees::new();
    t.dir("sys", OLD)
        .linked(&[("sys/a.dll", true, true), ("sys/b.dll", true, true)]);
    // The reader's listing-style count is 0 and the walker's listing says the
    // whole length; the system says what the file holds.
    let d = diff(
        "sys/a.dll",
        "link group bytes",
        &format!("({LENGTH}, 0)"),
        &format!("({LENGTH}, {LENGTH})"),
    );
    let sys = says(&[("sys/a.dll", compressed_facts(PACKED))]);
    // No full count taken: nothing to stand on.
    assert_eq!(class_of(t.judge(d.clone(), 0, &sys)), "unexplained");
    // The full count of the two names together is the system's figure.
    t.full_count("sys/a.dll", PACKED).full_count("sys/b.dll", 0);
    let v = t.judge(d.clone(), 0, &sys).unwrap();
    assert_eq!((v.class, v.basis), (C_COMPRESSED, B_COMPRESSED));
    // The full count is not the system's figure: an under-count is a finding.
    let mut t2 = Trees::new();
    t2.dir("sys", OLD)
        .linked(&[("sys/a.dll", true, true), ("sys/b.dll", true, true)]);
    t2.full_count("sys/a.dll", 0).full_count("sys/b.dll", 0);
    assert_eq!(class_of(t2.judge(d.clone(), 0, &sys)), "unexplained");
    // The system does not say the overlay layer backs the file.
    let plain = says(&[("sys/a.dll", found(truth(OLD, LENGTH, PACKED)))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &plain)), "unexplained");
    // The reader's length is not the system's.
    let other_length = diff(
        "sys/a.dll",
        "link group bytes",
        &format!("({}, 0)", LENGTH + 1),
        &format!("({LENGTH}, {LENGTH})"),
    );
    assert_eq!(class_of(t.judge(other_length, 0, &sys)), "unexplained");
}

#[test]
fn a_single_file_is_judged_the_same_way_by_its_allocated_bytes() {
    let mut t = Trees::new();
    t.file("x.dll", OLD);
    let d = diff("x.dll", "allocated", "0", &LENGTH.to_string());
    let sys = says(&[("x.dll", compressed_facts(PACKED))]);
    assert_eq!(class_of(t.judge(d.clone(), 0, &sys)), "unexplained");
    t.full_count("x.dll", PACKED);
    assert_eq!(class_of(t.judge(d.clone(), 0, &sys)), C_COMPRESSED);
    // A full count that is not the system's figure.
    let mut t = Trees::new();
    t.file("x.dll", OLD).full_count("x.dll", PACKED + 4096);
    assert_eq!(class_of(t.judge(d, 0, &sys)), "unexplained");
}

#[test]
fn the_audit_checks_every_overlay_file_against_the_system() {
    let mut t = Trees::new();
    t.dir("d", OLD);
    for (name, packed) in [
        ("good", PACKED),
        ("short", PACKED - 4096),
        ("gone", PACKED),
        ("plain", PACKED),
    ] {
        t.file(&format!("d/{name}"), OLD);
        t.flags(&format!("d/{name}"), NF | SP, NF | SP);
        t.full_count(&format!("d/{name}"), packed);
    }
    // Not a link: not audited.
    t.file("d/ordinary", OLD);
    let sys = says(&[
        ("d/good", compressed_facts(PACKED)),
        ("d/short", compressed_facts(PACKED)),
        ("d/gone", Answer::Gone),
        ("d/plain", found(truth(OLD, LENGTH, PACKED))),
    ]);
    let full = t.full.clone().unwrap();
    let audit = audit_overlay_files(&t.reader, &t.walker, &full, &sys, GEO);
    assert_eq!(audit.files, 4);
    assert_eq!(audit.agree, 1);
    assert_eq!(audit.unanswered, 1);
    let names: Vec<&str> = audit.differ.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"d/short") && names.contains(&"d/plain"));
    // The report prints without failing.
    report_audit(&audit);
}

/// The audit of one small overlay file: the length and allocation the system
/// gives, the reader's full count (`full_logical`, `full_allocated`) and the
/// record the volume returns for it. Returns the audit.
fn audit_one(
    full_logical: u64,
    full_allocated: u64,
    system_allocation: u64,
    record: Result<RecordFacts, String>,
) -> Audit {
    let mut t = Trees::new();
    t.dir("d", OLD).file("d/small.xml", OLD);
    t.flags("d/small.xml", NF | SP, NF | SP);
    t.full_count("d/small.xml", full_allocated);
    t.full
        .as_mut()
        .unwrap()
        .get_mut("d/small.xml".as_bytes())
        .unwrap()
        .logical = full_logical;
    let facts = found(Truth {
        backing: Some(PROVIDER_FILE),
        ..truth(OLD, 6384, system_allocation)
    });
    let sys = says(&[("d/small.xml", facts)]).record("d/small.xml", record);
    let full = t.full.clone().unwrap();
    audit_overlay_files(&t.reader, &t.walker, &full, &sys, GEO)
}

fn resident_wof(len: u64) -> RecordFacts {
    record_of(&[main_stream(6384), stream(WOF_NAME, true, len, 0)])
}

#[test]
fn a_stream_the_record_holds_resident_explains_the_difference_for_that_file() {
    // The reader holds no cluster for the file; its length is the main stream
    // plus the 600 bytes the record holds for the compressed stream.
    let a = audit_one(6384 + 600, 0, 4096, Ok(resident_wof(600)));
    assert_eq!(
        (a.files, a.agree, a.resident.len(), a.differ.len()),
        (1, 0, 1, 0)
    );
    let (d, evidence) = &a.resident[0];
    assert_eq!(d.path, "d/small.xml");
    assert!(
        evidence.contains("resident value of 600 bytes"),
        "{evidence}"
    );
    // The record's facts are printed beside the reader's figures.
    assert!(
        d.reader.contains("full count 0 of length 6984"),
        "{}",
        d.reader
    );
    assert!(
        d.other.contains("WofCompressedData: resident length 600"),
        "{}",
        d.other
    );
    report_audit(&a);
    // A stream the record holds at a length the old rule would have refused
    // (past a record's worth) is still explained when the record says it is
    // resident: the fact decides, not a length bound.
    let a = audit_one(6384 + 900, 0, 4096, Ok(resident_wof(900)));
    assert_eq!((a.resident.len(), a.differ.len()), (1, 0));
    // Another named stream beside it is fine when the reader's length has it.
    let rec = record_of(&[
        main_stream(6384),
        stream(WOF_NAME, true, 600, 0),
        stream("Zone.Identifier", true, 26, 0),
    ]);
    let a = audit_one(6384 + 600 + 26, 0, 4096, Ok(rec));
    assert_eq!((a.resident.len(), a.differ.len()), (1, 0));
}

#[test]
fn a_stream_the_record_does_not_show_resident_stays_a_difference() {
    let mut extension = resident_wof(600);
    extension.is_extension = true;
    let mut listed = resident_wof(600);
    listed.has_attribute_list = true;
    let cases: Vec<(&str, Audit)> = vec![
        (
            "the reader holds clusters for it",
            audit_one(6384 + 600, 8192, 4096, Ok(resident_wof(600))),
        ),
        (
            "the system's allocation is more than one cluster",
            audit_one(6384 + 600, 0, 8192, Ok(resident_wof(600))),
        ),
        (
            "the record could not be had",
            audit_one(6384 + 600, 0, 4096, Err("access denied".into())),
        ),
        (
            "the record holds the stream in clusters: the reader may have under-counted",
            audit_one(
                6384 + 3000,
                0,
                4096,
                Ok(record_of(&[
                    main_stream(6384),
                    stream(WOF_NAME, false, 3000, 4096),
                ])),
            ),
        ),
        (
            "the record has no compressed stream",
            audit_one(6384, 0, 4096, Ok(record_of(&[main_stream(6384)]))),
        ),
        (
            "the compressed value is empty",
            audit_one(6384, 0, 4096, Ok(resident_wof(0))),
        ),
        (
            "two compressed values",
            audit_one(
                6384 + 1200,
                0,
                4096,
                Ok(record_of(&[
                    main_stream(6384),
                    stream(WOF_NAME, true, 600, 0),
                    stream(WOF_NAME, true, 600, 0),
                ])),
            ),
        ),
        (
            "the record is an extension record",
            audit_one(6384 + 600, 0, 4096, Ok(extension)),
        ),
        (
            "the record has an attribute list",
            audit_one(6384 + 600, 0, 4096, Ok(listed)),
        ),
        (
            "the reader's length lacks the stream: it did not see its bytes",
            audit_one(6384, 0, 4096, Ok(resident_wof(600))),
        ),
        (
            "the reader's length has more than the record holds",
            audit_one(6384 + 700, 0, 4096, Ok(resident_wof(600))),
        ),
        (
            "the reader's length lacks another named stream",
            audit_one(
                6384 + 600,
                0,
                4096,
                Ok(record_of(&[
                    main_stream(6384),
                    stream(WOF_NAME, true, 600, 0),
                    stream("Zone.Identifier", true, 26, 0),
                ])),
            ),
        ),
    ];
    for (what, a) in &cases {
        assert_eq!(
            (a.resident.len(), a.differ.len()),
            (0, 1),
            "{what}: {:?}",
            a.differ
        );
        // The difference says why it was not explained, with the facts.
        assert!(
            a.differ[0]
                .other
                .contains("not a stream held in the record"),
            "{what}: {}",
            a.differ[0].other
        );
    }
    // The under-count case names itself.
    assert!(cases[3].1.differ[0].other.contains("possible under-count"));
}

#[test]
fn a_file_the_reader_counts_as_the_system_does_needs_no_explanation() {
    let a = audit_one(6384 + 600, 4096, 4096, Ok(resident_wof(600)));
    assert_eq!((a.agree, a.resident.len(), a.differ.len()), (1, 0, 0));
    // Nothing on either side is agreement too, not a stream to explain.
    let a = audit_one(6384 + 600, 0, 0, Ok(resident_wof(600)));
    assert_eq!((a.agree, a.resident.len(), a.differ.len()), (1, 0, 0));
}

#[test]
fn each_folder_written_basis_claims_only_what_its_branch_checked() {
    let mut t = Trees::new();
    t.dir("apps", NEW);
    t.walker_only("apps/pkg", NodeKind::Dir, OLD).walker_only(
        "apps/pkg/f.png",
        NodeKind::File,
        OLD,
    );
    let d = diff("apps/pkg/f.png", "presence", "missing", "present");
    // The system says it is gone: the basis says so.
    let gone = says(&[("apps/pkg/f.png", Answer::Gone)]);
    let v = t.judge(d.clone(), 0, &gone).unwrap();
    assert_eq!((v.class, v.basis), (C_CHANGED, B_FOLDER_WRITTEN_GONE));
    assert!(v.basis.contains("also says it is gone"));
    // The system would not say: the basis does not claim it did.
    let silent = says(&[]);
    let v = t.judge(d, 0, &silent).unwrap();
    assert_eq!((v.class, v.basis), (C_CHANGED, B_FOLDER_WRITTEN_UNASKED));
    assert!(v.basis.contains("would not say"));
    assert!(!v.basis.contains("also says"));
    // The plain basis, used where the system is not asked, claims nothing of
    // the system.
    assert!(!B_FOLDER_WRITTEN.contains("system"));
}

#[test]
fn a_failed_allocation_question_is_no_answer_and_not_zero() {
    assert_eq!(allocation_answer(true, 4096), Ok(4096));
    assert_eq!(allocation_answer(true, 0), Ok(0));
    assert!(allocation_answer(false, 0).is_err());
    assert!(allocation_answer(false, 4096).is_err());
    assert!(allocation_answer(true, -1).is_err());
}

/// An audit of one link the reader does not follow, for which the system gives
/// `facts`; the reader's full count of the file is `full_allocated`.
fn audit_link(full_allocated: u64, facts: Truth) -> Audit {
    let mut t = Trees::new();
    t.dir("d", OLD).file("d/alias.exe", OLD);
    t.flags("d/alias.exe", NF, NF);
    t.full_count("d/alias.exe", full_allocated);
    let sys = says(&[("d/alias.exe", found(facts))]);
    let full = t.full.clone().unwrap();
    audit_overlay_files(&t.reader, &t.walker, &full, &sys, GEO)
}

const APP_ALIAS: u32 = 0x8000_001b;

#[test]
fn a_link_the_system_reports_with_another_tag_is_not_an_overlay_file() {
    // An app execution alias: tag 0x8000001b, reparse attribute set, no
    // backing. Nothing is allocated on either side. It is tallied with its
    // tag, and it is not a difference.
    let alias = Truth {
        attributes: ATTR_REPARSE,
        reparse_tag: APP_ALIAS,
        ..truth(OLD, 0, 0)
    };
    let a = audit_link(0, alias.clone());
    assert_eq!(
        (a.files, a.agree, a.resident.len(), a.differ.len()),
        (1, 0, 0, 0)
    );
    assert_eq!(a.other_reparse, [("d/alias.exe".to_owned(), APP_ALIAS)]);
    // The tag is printed (report_audit prints without failing).
    report_audit(&a);
    // Its bytes are still compared: a reader figure that the system does not
    // give is a difference, with the tag in it.
    let a = audit_link(4096, alias);
    assert_eq!((a.other_reparse.len(), a.differ.len()), (1, 1));
    assert!(
        a.differ[0]
            .other
            .contains("not an overlay file: tag 0x8000001b"),
        "{}",
        a.differ[0].other
    );
    assert_eq!(a.differ[0].field, "reparse point bytes");
}

#[test]
fn a_link_the_system_does_not_show_as_a_reparse_point_stays_a_difference() {
    // Tag 0 with the reparse attribute: the system names nothing behind it.
    let a = audit_link(
        0,
        Truth {
            attributes: ATTR_REPARSE,
            reparse_tag: 0,
            ..truth(OLD, 0, 0)
        },
    );
    assert_eq!((a.other_reparse.len(), a.differ.len()), (0, 1));
    assert!(a.differ[0].other.contains("names no overlay layer"));
    // A tag without the reparse attribute: the same.
    let a = audit_link(
        0,
        Truth {
            attributes: 0,
            reparse_tag: APP_ALIAS,
            ..truth(OLD, 0, 0)
        },
    );
    assert_eq!((a.other_reparse.len(), a.differ.len()), (0, 1));
    // An ordinary file the reader calls a link.
    let a = audit_link(0, truth(OLD, 10, 4096));
    assert_eq!((a.other_reparse.len(), a.differ.len()), (0, 1));
}

#[test]
fn an_overlay_file_is_still_judged_by_the_overlay_rule_and_not_tallied_as_other() {
    // Tag 0x80000017 (and reparse attribute): the overlay rule applies.
    let wof = Truth {
        attributes: ATTR_REPARSE,
        reparse_tag: TAG_WOF,
        ..truth(OLD, 6384, 4096)
    };
    let a = audit_link(4096, wof);
    assert_eq!((a.agree, a.other_reparse.len(), a.differ.len()), (1, 0, 0));
}

fn real_stream(
    name: &str,
    length: u64,
    allocated: u64,
    held: u64,
    total: Option<u64>,
) -> DataStream {
    DataStream {
        name: name.to_owned(),
        resident: false,
        length,
        allocated,
        held_clusters: held,
        total_allocated: total,
    }
}

#[test]
fn the_main_stream_must_hold_no_cluster_by_the_record_and_the_audit_prints_what_it_holds() {
    // The shape of the real records: the header allocates 65536 bytes (holes
    // included), the stream holds nothing.
    let a = audit_one(6384 + 600, 0, 4096, Ok(resident_wof(600)));
    assert_eq!(a.resident.len(), 1);
    let line = &a.resident[0].0.other;
    assert!(
        line.contains("allocated 65536, holds 0 clusters (total-allocated field 0)"),
        "{line}"
    );
    // The run list shows a cluster though the reader counted none: the
    // reader's 0 would be an under-count, and it is not explained.
    let held = |n: u64, total: Option<u64>| {
        record_of(&[
            real_stream("", 6384, 65536, n, total),
            stream(WOF_NAME, true, 600, 0),
        ])
    };
    let a = audit_one(6384 + 600, 0, 4096, Ok(held(1, Some(0))));
    assert_eq!((a.resident.len(), a.differ.len()), (0, 1));
    assert!(
        a.differ[0].other.contains("main stream holds 1 clusters"),
        "{}",
        a.differ[0].other
    );
    assert!(a.differ[0].other.contains("under-count"));
    // The header's total-allocated field shows bytes though the run list says
    // holes: not explained either.
    let a = audit_one(6384 + 600, 0, 4096, Ok(held(0, Some(4096))));
    assert_eq!((a.resident.len(), a.differ.len()), (0, 1));
    assert!(
        a.differ[0]
            .other
            .contains("total-allocated field Some(4096)"),
        "{}",
        a.differ[0].other
    );
    // A main stream with no such field is judged by its run list alone.
    let a = audit_one(6384 + 600, 0, 4096, Ok(held(0, None)));
    assert_eq!((a.resident.len(), a.differ.len()), (1, 0));
    // Two main streams, or none: not a record to rely on.
    let two = record_of(&[
        main_stream(6384),
        main_stream(6384),
        stream(WOF_NAME, true, 600, 0),
    ]);
    let a = audit_one(6384 + 600, 0, 4096, Ok(two));
    assert_eq!((a.resident.len(), a.differ.len()), (0, 1));
    let none = record_of(&[stream(WOF_NAME, true, 600, 0)]);
    let a = audit_one(600, 0, 4096, Ok(none));
    assert_eq!((a.resident.len(), a.differ.len()), (0, 1));
}
