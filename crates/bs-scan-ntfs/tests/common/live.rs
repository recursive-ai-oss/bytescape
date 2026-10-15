//! Sorting the differences found on a live volume into their causes.
//!
//! A mounted system drive changes while it is read twice (once through the
//! file system, once through its table), and the file system's listings are
//! not always current. So a difference between the two trees is not by
//! itself a finding. Each one is sorted here into a cause, and a cause only
//! counts when evidence backs it for that very path:
//!
//! * the entry, or the folder holding it, was written after the scan began;
//! * the system, asked by opening the entry the way any program does, gives
//!   the reader's figure (so the listing's copy is the stale one);
//! * the system says the entry carries a reparse tag that its own layer hides
//!   from listings;
//! * the walker recorded that it could not read the folder.
//!
//! Every answer from the system (its attributes, its allocation, a file's
//! record) is asked **by the path** the reader's tree gives, opened at the
//! time of the sorting, not by a file reference the reader recorded: the
//! reader's tree carries none. A file replaced after the scan is therefore
//! judged by its successor. That cannot turn a difference into an
//! explanation by itself: each class also needs the figures to agree (the
//! system's length, plus the record's named streams, must equal the reader's
//! length), so a successor that does not match leaves the difference
//! unexplained. The same holds for every by-path answer here.
//!
//! Anything left over is unexplained, and the test fails on it. The system is
//! asked through [`Oracle`], so the sorting runs on every platform against a
//! stand-in, and on Windows against the real drive.
#![allow(dead_code)]

use super::compare::{Diff, SYSTEM_FOLDER, check_system_folder_time, compare_trees};
use super::testkit::{Info, flatten};
use super::wofrec::{RecordFacts, WOF_NAME};
use bs_tree::{NodeFlags, NodeId, NodeKind, Snapshot};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};

/// The reserved records every NTFS volume has. A listing cannot show them.
pub const SYSTEM_NAMES: &[&str] = &[
    "$MFT", "$MFTMirr", "$LogFile", "$Volume", "$AttrDef", "$Bitmap", "$Boot", "$BadClus",
    "$Secure", "$UpCase", "$Extend",
];

pub fn first_part(path: &str) -> &str {
    path.split('/').next().unwrap_or("")
}

/// `IO_REPARSE_TAG_WOF`: the Windows Overlay Filter, which backs a file's data
/// with a compressed copy kept elsewhere.
pub const TAG_WOF: u32 = 0x8000_0017;
/// `FILE_ATTRIBUTE_REPARSE_POINT`.
pub const ATTR_REPARSE: u32 = 0x400;
/// The providers `FSCTL_GET_EXTERNAL_BACKING` names: a Windows image file and
/// the compressed-file provider. Both are the overlay filter's.
pub const PROVIDER_WIM: u32 = 1;
pub const PROVIDER_FILE: u32 = 2;

/// What the system says about one entry when it is opened to be asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Truth {
    pub attributes: u32,
    /// The reparse tag, 0 when the entry is not a reparse point.
    pub reparse_tag: u32,
    /// The provider backing the entry's data from elsewhere, if the system
    /// says there is one.
    pub backing: Option<u32>,
    /// Last write time, Unix seconds.
    pub mtime: i64,
    pub logical: u64,
    /// The allocation size the open handle reports.
    pub allocation: u64,
    /// The bytes a sparse or compressed file holds in clusters.
    pub held: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Found(Truth),
    /// The system says there is no such entry.
    Gone,
    /// The system would not say (no access, or a name that cannot be asked).
    Unavailable(String),
}

/// The allocation size from the system's answer to `FileStandardInfo`. A call
/// that failed, or a size that makes no sense, is no answer: reading it as 0
/// would "agree" with a reader figure of 0 and explain a real difference away.
pub fn allocation_answer(call_ok: bool, raw: i64) -> Result<u64, String> {
    if !call_ok {
        return Err("the system would not give the allocation size".into());
    }
    u64::try_from(raw).map_err(|_| format!("the system reports an allocation size of {raw}"))
}

/// The system, as a source of independent answers.
pub trait Oracle {
    /// `path` is relative to the scanned root, `/`-separated.
    fn ask(&self, path: &str) -> Answer;

    /// What the file's own record says about its data streams. Like every
    /// answer here it is asked by path, not by the reader's file reference
    /// (see the module documentation). Fetched from
    /// the volume (the system's stream listing hides the overlay layer's
    /// stream, so the record is the source that sees it). `Err` says why the
    /// record could not be had or read.
    fn file_record(&self, _path: &str) -> Result<RecordFacts, String> {
        Err("this stand-in has no records".into())
    }
}

/// An oracle that answers nothing.
pub struct Mute;

impl Oracle for Mute {
    fn ask(&self, _: &str) -> Answer {
        Answer::Unavailable("no oracle".into())
    }
}

pub type Tree = BTreeMap<Vec<u8>, Info>;

// The classes.
pub const C_SYSTEM: &str = "system files (reader only)";
pub const C_WALKER_FAILED: &str = "walker could not read the folder";
pub const C_CHANGED: &str = "changed during the scan";
pub const C_REPARSE: &str = "reparse tag the listing hides";
pub const C_STALE: &str = "listing out of date, the system agrees with the reader";
pub const C_SYSTEM_FOLDER: &str = "system folder time";
pub const C_COMPRESSED: &str =
    "bytes held in the compressed-data stream (the listing's copy is the file's full length)";
pub const C_RESIDENT_STREAM: &str =
    "overlay-compressed data held inside the file's own table record";

// What each class rests on, in words. These are printed with the counts.
pub const B_SYSTEM: &str = "The reserved records ($MFT, $LogFile, $Bitmap, $Extend ...) are hidden by the file system, so no walker can list them; they exist only for a reader of the table.";
pub const B_WALKER_FAILED: &str = "The walker recorded that it could not read the folder (access refused), so it lists nothing below it; the reader does not need access.";
pub const B_WRITTEN: &str = "The entry itself was written after the scan began, so the walker and the reader looked at different moments.";
pub const B_FOLDER_WRITTEN: &str = "The folder that holds the entry was written after the scan began (its time, taken from the table, is not older than the scan), so a name appeared or went between the two looks.";
pub const B_FOLDER_WRITTEN_GONE: &str = "The folder that holds the entry was written after the scan began (its time, taken from the table, is not older than the scan), so a name went between the two looks; the system, asked by opening the entry, also says it is gone.";
pub const B_FOLDER_WRITTEN_UNASKED: &str = "The folder that holds the entry was written after the scan began (its time, taken from the table, is not older than the scan). The system would not say whether the entry is gone, so the folder's time is the only evidence that a name went between the two looks.";
pub const B_REPARSE: &str = "The table says the file carries a reparse tag that the system's compression layer hides from directory listings. Opened as a reparse point, the system reports the tag (or the layer's external backing) for this very file.";
pub const B_STALE: &str = "A directory listing reports the copy of a file's times and sizes that its folder keeps. The file system updates that copy when the file is closed, not while it is open and not when a folder's contents change, so the listing can lag. Asked by opening the entry, the system gives the reader's figure.";
pub const B_STALE_SPARSE: &str = "The walker marks a file sparse or compressed from the allocation size in the entry's copy that its folder keeps. For a hard-linked name, or a file compressed after the name was made, that copy can still say the full length, while the table (and the system, asked by opening the file) say the file holds fewer bytes than its length.";
pub const B_COMPRESSED: &str = "A file compressed by the system's overlay layer keeps its data in a named stream (WofCompressedData) and its main stream holds no clusters. The listing-style count (main stream only) is therefore 0 for it, and the directory listing reports a stale copy, often the full length. The full count includes the named stream, and for this very file it equals the system's own allocation.";
pub const B_RESIDENT_STREAM: &str = "The file's own record, fetched from the volume, holds the compressed-data stream (WofCompressedData) as a resident value: the bytes are inside the record, which the full scan shows as the table item, and the stream has no clusters. The reader counts 0 clusters for such a stream, as for any value held in a record. The system, asked by opening the file, reports a larger allocation (one cluster at most for this class); that is its own figure, and the record, not that figure, decides the class. Required per file: no cluster counted by the reader; the record shows the main stream holds none (its run list is all holes); the record is a base record without an attribute list, and has exactly one resident WofCompressedData value; the reader's full length is the system's main length plus the record's named stream lengths.";
pub const B_SYSTEM_FOLDER: &str = "Windows writes into the System Volume Information folder while the volume is in use; the reader's time is later than the walker's and not later than the system's own answer afterwards.";

/// One difference, sorted.
#[derive(Clone, Debug)]
pub struct Verdict {
    pub class: &'static str,
    pub basis: &'static str,
    /// What was found for this path, for the report.
    pub detail: String,
}

#[derive(Default)]
pub struct Outcome {
    pub explained: Vec<(Diff, Verdict)>,
    /// Differences nothing explains, with the evidence that was looked at.
    pub unexplained: Vec<(Diff, String)>,
    /// What the system answered, over all questions.
    pub found: usize,
    pub gone: usize,
    pub unavailable: usize,
    /// For differences in flags: the reparse facts of each file, counted.
    pub reparse_facts: BTreeMap<String, usize>,
}

/// The scan window and the two trees.
struct Context<'a> {
    reader: &'a Tree,
    walker: &'a Tree,
    /// The reader's time for the volume's root folder.
    root_time: i64,
    /// Changes at or after this time (Unix seconds) happened during the scan.
    from: i64,
    oracle: &'a dyn Oracle,
    /// The reader's tree counted in full (every stream), when it was taken.
    full: Option<&'a Tree>,
    asked: RefCell<HashMap<String, Answer>>,
    reader_groups: OnceCell<HashMap<NodeId, Vec<String>>>,
    walker_groups: OnceCell<HashMap<NodeId, Vec<String>>>,
}

fn number(s: &str) -> Option<i64> {
    s.parse().ok()
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

/// "(a, b)" as two numbers.
fn pair(s: &str) -> Option<(u64, u64)> {
    let inner = s.strip_prefix('(')?.strip_suffix(')')?;
    let (a, b) = inner.split_once(", ")?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(p, _)| p)
}

impl Context<'_> {
    fn ask(&self, path: &str) -> Answer {
        if let Some(a) = self.asked.borrow().get(path) {
            return a.clone();
        }
        let a = self.oracle.ask(path);
        self.asked.borrow_mut().insert(path.to_owned(), a.clone());
        a
    }

    fn entry<'t>(tree: &'t Tree, path: &str) -> Option<&'t Info> {
        tree.get(path.as_bytes())
    }

    /// The nearest folder above `path` that both trees have; `None` is the
    /// volume's root.
    fn shared_ancestor<'p>(&self, path: &'p str) -> Option<&'p str> {
        let mut at = path;
        while let Some(parent) = parent_of(at) {
            let both = [self.reader, self.walker]
                .iter()
                .all(|t| Self::entry(t, parent).is_some_and(|i| i.kind == NodeKind::Dir));
            if both {
                return Some(parent);
            }
            at = parent;
        }
        None
    }

    /// When the folder was last written, by the table's (current) figure.
    fn folder_time(&self, dir: Option<&str>) -> i64 {
        match dir {
            None => self.root_time,
            Some(d) => Self::entry(self.reader, d).map_or(i64::MIN, |i| i.mtime),
        }
    }

    /// The time the entry was last written, if that is inside the scan.
    fn written_in_scan(&self, path: &str) -> Option<i64> {
        [self.reader, self.walker]
            .iter()
            .filter_map(|t| Self::entry(t, path))
            .map(|i| i.mtime)
            .filter(|&m| m >= self.from)
            .max()
    }

    fn verdict(class: &'static str, basis: &'static str, detail: impl Into<String>) -> Verdict {
        Verdict {
            class,
            basis,
            detail: detail.into(),
        }
    }

    /// The names of the hard-linked file that `path` is one of, in the
    /// reader's tree or (when `reader` is false) the walker's.
    fn group_names(&self, reader: bool, path: &str) -> Vec<String> {
        let (tree, cell) = if reader {
            (self.reader, &self.reader_groups)
        } else {
            (self.walker, &self.walker_groups)
        };
        let Some(me) = Self::entry(tree, path) else {
            return Vec::new();
        };
        if !me.flags.contains(NodeFlags::HARD_LINK) {
            return vec![path.to_owned()];
        }
        // Built once per tree: the trees hold millions of entries and there
        // can be thousands of groups to look up.
        let groups = cell.get_or_init(|| {
            let mut by_file: HashMap<NodeId, Vec<String>> = HashMap::new();
            for (p, i) in tree {
                if i.flags.contains(NodeFlags::HARD_LINK) {
                    by_file
                        .entry(i.origin.unwrap_or(i.id))
                        .or_default()
                        .push(String::from_utf8_lossy(p).into_owned());
                }
            }
            by_file
        });
        let mut names = groups
            .get(&me.origin.unwrap_or(me.id))
            .cloned()
            .unwrap_or_default();
        names.sort();
        names
    }

    fn judge(&self, d: &Diff) -> Result<Verdict, String> {
        match d.field {
            "flags" => self.flags(d),
            "mtime" | "logical" | "allocated" => self.value(d),
            "presence" => self.presence(d),
            "link group bytes" => self.group_bytes(d),
            "link group" => self.group(d),
            other => Err(format!(
                "a difference in {other}: a write does not change it, and no rule explains it"
            )),
        }
    }

    fn flags(&self, d: &Diff) -> Result<Verdict, String> {
        let (Some(r), Some(w)) = (hex(&d.reader), hex(&d.other)) else {
            return Err("flags that do not read as numbers".into());
        };
        let (extra, missing) = (r & !w, w & !r);
        let not_followed = u32::from(NodeFlags::NOT_FOLLOWED.bits());
        let sparse = u32::from(NodeFlags::SPARSE_OR_COMPRESSED.bits());
        // What the table knows about a file and a listing cannot say: that
        // the overlay layer backs it, and that it holds fewer bytes than its
        // length. Each bit needs the system's word for this very file.
        if missing == 0 && extra != 0 && extra & !(not_followed | sparse) == 0 {
            return self.hidden_by_the_listing(d, extra & not_followed != 0, extra & sparse != 0);
        }
        // A name added or removed while the volume was read.
        if (extra | missing) == u32::from(NodeFlags::HARD_LINK.bits()) {
            let t = self.folder_time(parent_of(&d.path));
            if t >= self.from {
                return Ok(Self::verdict(
                    C_CHANGED,
                    B_FOLDER_WRITTEN,
                    format!("link flag differs; its folder was written at {t}"),
                ));
            }
        }
        Err(format!(
            "flags differ in bits {extra:#x} (reader only) and {missing:#x} (walker only)"
        ))
    }

    /// Whether the system says the file is backed by the overlay layer.
    fn is_overlay_backed(t: &Truth) -> bool {
        t.reparse_tag == TAG_WOF || matches!(t.backing, Some(PROVIDER_WIM | PROVIDER_FILE))
    }

    /// The reader marks a file as a reparse point that was not followed
    /// (`link`) and/or as holding fewer bytes than its length (`short`), and
    /// the listing does not. Opened as a reparse point, the system must say
    /// there is an overlay layer behind the file for the first, and that the
    /// file holds fewer bytes than its length for the second.
    fn hidden_by_the_listing(&self, d: &Diff, link: bool, short: bool) -> Result<Verdict, String> {
        match self.ask(&d.path) {
            Answer::Found(t) => {
                let fact = format!(
                    "tag {:#010x} attributes {:#x} backing {:?}, length {}, allocation {}",
                    t.reparse_tag, t.attributes, t.backing, t.logical, t.allocation
                );
                if link && !Self::is_overlay_backed(&t) {
                    return Err(format!("the system names no overlay-layer tag: {fact}"));
                }
                if short {
                    let held = t.held.unwrap_or(t.allocation).min(t.allocation);
                    if held >= t.logical {
                        return Err(format!(
                            "the system says the file holds its whole length, so it is not short: {fact}"
                        ));
                    }
                }
                Ok(if link {
                    Self::verdict(C_REPARSE, B_REPARSE, fact)
                } else {
                    Self::verdict(C_STALE, B_STALE_SPARSE, fact)
                })
            }
            Answer::Gone => Err("the system says the file is gone".into()),
            Answer::Unavailable(why) => Err(format!("the system would not say: {why}")),
        }
    }

    /// Whether the reader's full count of the file (all its names summed, as a
    /// hard-linked file carries its bytes on one name) equals the system's own
    /// allocation for it, for a file the system says the overlay layer backs.
    /// Returns the full figure.
    fn full_allocation_matches(&self, names: &[String], t: &Truth) -> Option<u64> {
        let full = self.full?;
        if !Self::is_overlay_backed(t) {
            return None;
        }
        let mut sum = 0u64;
        for n in names {
            sum = sum.saturating_add(full.get(n.as_bytes())?.allocated);
        }
        (sum == t.allocation).then_some(sum)
    }

    fn value(&self, d: &Diff) -> Result<Verdict, String> {
        let Some(read) = number(&d.reader) else {
            return Err("a figure that does not read as a number".into());
        };
        let why = match self.ask(&d.path) {
            Answer::Found(t) => {
                let (agrees, truth) = match d.field {
                    "mtime" => (read == t.mtime, t.mtime.to_string()),
                    "logical" => (u64::try_from(read) == Ok(t.logical), t.logical.to_string()),
                    _ => (
                        u64::try_from(read).is_ok_and(|r| r == t.allocation || Some(r) == t.held),
                        format!("{} (held {:?})", t.allocation, t.held),
                    ),
                };
                if !agrees
                    && d.field == "allocated"
                    && let Some(full) =
                        self.full_allocation_matches(std::slice::from_ref(&d.path), &t)
                {
                    return Ok(Self::verdict(
                        C_COMPRESSED,
                        B_COMPRESSED,
                        format!(
                            "the full count and the system both say {full}; the listing says {}",
                            d.other
                        ),
                    ));
                }
                if agrees {
                    return Ok(match self.written_in_scan(&d.path) {
                        Some(at) => Self::verdict(
                            C_CHANGED,
                            B_WRITTEN,
                            format!("written at {at}; the system now says {truth}"),
                        ),
                        None => Self::verdict(
                            C_STALE,
                            B_STALE,
                            format!("the system says {truth}; the listing says {}", d.other),
                        ),
                    });
                }
                // The system's answer is a later one. A write after the scan
                // began explains why it differs from the reader's.
                if t.mtime >= self.from {
                    return Ok(Self::verdict(
                        C_CHANGED,
                        B_WRITTEN,
                        format!(
                            "written at {} since the reader looked; the system now says {truth}",
                            t.mtime
                        ),
                    ));
                }
                return Err(format!(
                    "the system says {truth} (last written {}, before the scan began); reader {}, walker {}",
                    t.mtime, d.reader, d.other
                ));
            }
            Answer::Gone => "the system says the entry is gone".to_owned(),
            Answer::Unavailable(w) => format!("the system would not say: {w}"),
        };
        // No answer to compare with: only a write inside the scan explains it.
        match self.written_in_scan(&d.path) {
            Some(at) => Ok(Self::verdict(
                C_CHANGED,
                B_WRITTEN,
                format!("written at {at} ({why})"),
            )),
            None => Err(why),
        }
    }

    fn presence(&self, d: &Diff) -> Result<Verdict, String> {
        let reader_has = d.reader == "present";
        if reader_has && d.field == "presence" && SYSTEM_NAMES.contains(&first_part(&d.path)) {
            return Ok(Self::verdict(C_SYSTEM, B_SYSTEM, "reserved record"));
        }
        let ancestor = self.shared_ancestor(&d.path);
        if reader_has {
            // The walker lists nothing below a folder it could not read.
            if let Some(a) = ancestor
                && Self::entry(self.walker, a).is_some_and(|i| i.failed)
            {
                return Ok(Self::verdict(
                    C_WALKER_FAILED,
                    B_WALKER_FAILED,
                    format!("the walker failed on {a:?}"),
                ));
            }
            let t = self.folder_time(ancestor);
            if t >= self.from {
                return Ok(Self::verdict(
                    C_CHANGED,
                    B_FOLDER_WRITTEN,
                    format!(
                        "the folder {} was written at {t}",
                        ancestor.unwrap_or("(root)")
                    ),
                ));
            }
            return Err(format!(
                "the reader has it and the walker's listing of {:?} does not; that folder was last written at {t}, before the scan began",
                ancestor.unwrap_or("(root)")
            ));
        }
        // The walker listed it and the reader does not have it.
        let t = self.folder_time(ancestor);
        match self.ask(&d.path) {
            Answer::Found(_) => Err("the system still has it, and the reader does not".into()),
            Answer::Gone if t >= self.from => Ok(Self::verdict(
                C_CHANGED,
                B_FOLDER_WRITTEN_GONE,
                format!(
                    "the system says it is gone; the folder {} was written at {t}",
                    ancestor.unwrap_or("(root)")
                ),
            )),
            Answer::Gone => Err(format!(
                "the system says it is gone, but the folder {} was last written at {t}, before the scan began",
                ancestor.unwrap_or("(root)")
            )),
            Answer::Unavailable(why) if t >= self.from => Ok(Self::verdict(
                C_CHANGED,
                B_FOLDER_WRITTEN_UNASKED,
                format!(
                    "the folder {} was written at {t} (the system would not say whether the entry is gone: {why})",
                    ancestor.unwrap_or("(root)")
                ),
            )),
            Answer::Unavailable(why) => Err(format!(
                "the system would not say ({why}), and the folder {} was last written at {t}, before the scan began",
                ancestor.unwrap_or("(root)")
            )),
        }
    }

    /// Hard-linked names count their bytes once, as a group. The bytes the
    /// group carries differ: the system is asked about the file (any one of
    /// its names opens it).
    fn group_bytes(&self, d: &Diff) -> Result<Verdict, String> {
        let (Some((rl, ra)), Some((wl, wa))) = (pair(&d.reader), pair(&d.other)) else {
            return Err("group figures that do not read as numbers".into());
        };
        let names = self.group_names(true, &d.path);
        match self.ask(&d.path) {
            Answer::Found(t) => {
                let agrees = rl == t.logical && (ra == t.allocation || Some(ra) == t.held);
                let truth = format!(
                    "the system says ({}, {}) (held {:?}); the listing says ({wl}, {wa})",
                    t.logical, t.allocation, t.held
                );
                if !agrees
                    && rl == t.logical
                    && let Some(full) = self.full_allocation_matches(&names, &t)
                {
                    return Ok(Self::verdict(
                        C_COMPRESSED,
                        B_COMPRESSED,
                        format!("{truth}; the full count of the file is {full}"),
                    ));
                }
                if agrees {
                    let written = names.iter().find_map(|n| self.written_in_scan(n));
                    return Ok(match written {
                        Some(at) => {
                            Self::verdict(C_CHANGED, B_WRITTEN, format!("written at {at}; {truth}"))
                        }
                        None => Self::verdict(C_STALE, B_STALE, truth),
                    });
                }
                if t.mtime >= self.from {
                    return Ok(Self::verdict(
                        C_CHANGED,
                        B_WRITTEN,
                        format!("written at {} since the reader looked; {truth}", t.mtime),
                    ));
                }
                Err(format!("{truth}; the reader says ({rl}, {ra})"))
            }
            other => {
                let why = format!("{other:?}");
                match names.iter().find_map(|n| self.written_in_scan(n)) {
                    Some(at) => Ok(Self::verdict(
                        C_CHANGED,
                        B_WRITTEN,
                        format!("written at {at} ({why})"),
                    )),
                    None => Err(why),
                }
            }
        }
    }

    /// The names of a hard-linked file differ between the trees. A name that
    /// one tree lacks is a presence difference of its own, and is judged as
    /// one; a link made or removed in a folder written during the scan also
    /// explains it.
    fn group(&self, d: &Diff) -> Result<Verdict, String> {
        let mut names = self.group_names(true, &d.path);
        names.extend(self.group_names(false, &d.path));
        names.sort();
        names.dedup();
        let mut last = String::from("no names");
        for name in &names {
            let in_reader = Self::entry(self.reader, name).is_some();
            let in_walker = Self::entry(self.walker, name).is_some();
            if in_reader != in_walker {
                let presence = Diff {
                    path: name.clone(),
                    field: "presence",
                    reader: if in_reader { "present" } else { "missing" }.into(),
                    other: if in_walker { "present" } else { "missing" }.into(),
                };
                match self.presence(&presence) {
                    Ok(v) => {
                        return Ok(Self::verdict(
                            v.class,
                            v.basis,
                            format!("the name {name:?}: {}", v.detail),
                        ));
                    }
                    Err(why) => last = format!("the name {name:?}: {why}"),
                }
            } else {
                let dir = parent_of(name).filter(|p| !p.is_empty());
                let t = self.folder_time(dir);
                if t >= self.from {
                    return Ok(Self::verdict(
                        C_CHANGED,
                        B_FOLDER_WRITTEN,
                        format!(
                            "a folder of the group, {}, was written at {t}",
                            dir.unwrap_or("(root)")
                        ),
                    ));
                }
                last = format!(
                    "every name is in both trees and its folder was not written since the scan began (the names: {})",
                    names.len()
                );
            }
        }
        Err(last)
    }
}

fn tally(out: &mut Outcome, context: &Context<'_>) {
    for a in context.asked.borrow().values() {
        match a {
            Answer::Found(_) => out.found += 1,
            Answer::Gone => out.gone += 1,
            Answer::Unavailable(_) => out.unavailable += 1,
        }
    }
}

/// Sorts `diffs` (from a comparison of `reader` with `walker`).
///
/// `root_time` is the reader's time for the root folder, and `from` the
/// start of the scan window.
pub fn classify_diffs(
    diffs: Vec<Diff>,
    reader: &Tree,
    walker: &Tree,
    root_time: i64,
    from: i64,
    oracle: &dyn Oracle,
    full: Option<&Tree>,
) -> Outcome {
    let context = Context {
        reader,
        walker,
        root_time,
        from,
        oracle,
        full,
        asked: RefCell::new(HashMap::new()),
        reader_groups: OnceCell::new(),
        walker_groups: OnceCell::new(),
    };
    let mut out = Outcome::default();
    for d in diffs {
        let result = if d.field == "mtime" && d.path == SYSTEM_FOLDER {
            system_folder(&context, &d).or_else(|_| context.judge(&d))
        } else {
            context.judge(&d)
        };
        if d.field == "flags"
            && let Answer::Found(t) = context.ask(&d.path)
        {
            let key = format!(
                "tag {:#010x}, backing {:?}, reparse attribute {}",
                t.reparse_tag,
                t.backing,
                t.attributes & ATTR_REPARSE != 0
            );
            *out.reparse_facts.entry(key).or_default() += 1;
        }
        match result {
            Ok(v) => out.explained.push((d, v)),
            Err(why) => out.unexplained.push((d, why)),
        }
    }
    tally(&mut out, &context);
    out
}

/// The root System Volume Information folder: the reader's time is later than
/// the walker's and no later than the system's own answer afterwards.
fn system_folder(context: &Context<'_>, d: &Diff) -> Result<Verdict, String> {
    let is_dir = Context::entry(context.reader, &d.path).is_some_and(|i| i.kind == NodeKind::Dir);
    if !is_dir {
        return Err("not a folder".into());
    }
    let restat = match context.ask(&d.path) {
        Answer::Found(t) => Some(t.mtime),
        _ => None,
    };
    check_system_folder_time(d, restat)?;
    Ok(Context::verdict(
        C_SYSTEM_FOLDER,
        B_SYSTEM_FOLDER,
        format!("system's own answer afterwards: {restat:?}"),
    ))
}

/// Compares two snapshots and sorts what differs.
///
/// `full` is the reader's tree counted in full (every stream), when there is
/// one: it is evidence for files whose bytes are in a named stream.
pub fn classify(
    reader: &Snapshot,
    walker: &Snapshot,
    from: i64,
    oracle: &dyn Oracle,
    full: Option<&Snapshot>,
) -> Outcome {
    let diffs = compare_trees(reader, walker);
    let root_time = reader.root().map_or(i64::MIN, |r| r.mtime());
    let full = full.map(flatten);
    classify_diffs(
        diffs,
        &flatten(reader),
        &flatten(walker),
        root_time,
        from,
        oracle,
        full.as_ref(),
    )
}

/// What the overlay-file audit found.
#[derive(Default, Debug)]
pub struct Audit {
    /// Files the reader marks as backed by the overlay layer (hard-linked
    /// ones once).
    pub files: usize,
    /// Of those, files whose full count equals the system's own allocation.
    pub agree: usize,
    /// Files the system could not be asked about.
    pub unanswered: usize,
    /// Bytes: the full count, the listing-style count, the walker's, and the
    /// system's, over the files the system answered for.
    pub full_bytes: u64,
    pub listing_bytes: u64,
    pub walker_bytes: u64,
    pub system_bytes: u64,
    /// Files whose full count is not the system's allocation for a reason
    /// that holds for this very file (see [`resident_overlay_stream`]), with
    /// the evidence found.
    pub resident: Vec<(Diff, String)>,
    /// Files the reader marks as links not followed that the system reports
    /// with another reparse tag (not the overlay layer's): not overlay files,
    /// so the overlay rule does not apply to them. Each is still compared
    /// with the system's allocation.
    pub other_reparse: Vec<(String, u32)>,
    /// Differences: the reader's full count is not the system's allocation
    /// and nothing explains it, or the system does not say the file is
    /// backed by the overlay layer.
    pub differ: Vec<Diff>,
}

/// The volume's sizes the audit needs.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub cluster_bytes: u64,
    pub record_bytes: u64,
}

/// Whether the reader's full count of 0 clusters for an overlay-backed file,
/// against a larger allocation from the system, is a compressed stream held
/// inside the file's own record. The deciding fact is the record's: the
/// `WofCompressedData` value is resident. Everything is checked for this one
/// file; the first thing that does not hold is the answer (`Err`).
///
/// * the reader's full count holds no cluster for the file;
/// * the system's allocation is above zero and at most one cluster (a larger
///   gap is not a stream of a few hundred bytes, whatever the record says);
/// * the record shows that the main stream holds no cluster: its run list has
///   only holes, and so does its total-allocated field where it has one;
/// * the record is a base record with no attribute list (so no attribute is
///   somewhere else), and holds exactly one `WofCompressedData` value,
///   resident, of at least one byte;
/// * the reader's full length is the system's length of the main stream plus
///   the lengths of the named streams in the record, so the reader saw that
///   stream's bytes and counted them nowhere else.
///
/// A stream the record holds in clusters is the opposite case, an under-count
/// by the reader, and is never explained here.
pub fn resident_overlay_stream(
    full: &Info,
    system: &Truth,
    record: Result<&RecordFacts, &str>,
    geo: Geometry,
) -> Result<String, String> {
    if full.allocated != 0 {
        return Err(format!(
            "the full count holds {} bytes of clusters for it",
            full.allocated
        ));
    }
    if system.allocation == 0 || system.allocation > geo.cluster_bytes {
        return Err(format!(
            "the system's allocation {} is not one cluster ({}) or less",
            system.allocation, geo.cluster_bytes
        ));
    }
    let facts = record.map_err(|why| format!("the file's record is not available: {why}"))?;
    if facts.is_extension {
        return Err("the record is an extension record, not the file's base record".into());
    }
    if facts.has_attribute_list {
        return Err("the record has an attribute list: other records may hold streams".into());
    }
    // The reader's 0 is the figure under audit, so the record has to show it
    // for the main stream: the clusters the stream holds (not what its header
    // allocates, which for a sparse or compressed stream counts holes).
    let mains: Vec<_> = facts.streams.iter().filter(|s| s.name.is_empty()).collect();
    let [main] = mains.as_slice() else {
        return Err(format!(
            "the record holds {} main streams, not exactly one",
            mains.len()
        ));
    };
    if !main.holds_no_clusters() {
        return Err(format!(
            "the record's main stream holds {} clusters (total-allocated field {:?}): the reader's 0 would be an under-count",
            main.held_clusters, main.total_allocated
        ));
    }
    let wof = facts.wof();
    let [stream] = wof.as_slice() else {
        return Err(format!(
            "the record holds {} {WOF_NAME} values, not exactly one",
            wof.len()
        ));
    };
    if !stream.resident {
        return Err(format!(
            "the record holds {WOF_NAME} in clusters (length {}, allocated {}) and the reader counted none: a possible under-count by the reader",
            stream.length, stream.allocated
        ));
    }
    if stream.length == 0 {
        return Err(format!("the record's resident {WOF_NAME} value is empty"));
    }
    let named = facts.named_length();
    if full.logical != system.logical.saturating_add(named) {
        return Err(format!(
            "the reader's full length {} is not the system's length {} plus the record's named streams {named}",
            full.logical, system.logical
        ));
    }
    Ok(format!(
        "{WOF_NAME} is a resident value of {} bytes in the record; the reader counts 0 clusters, the system reports {}",
        stream.length, system.allocation
    ))
}

/// Checks the reader's full count against the system for every file the
/// reader marks as backed by the overlay layer: the bytes such a file holds
/// are in a named stream, so a count that left that stream out would be an
/// under-count, and this is where it would show. Each file is asked about
/// once, by its first name.
pub fn audit_overlay_files(
    reader: &Tree,
    walker: &Tree,
    full: &Tree,
    oracle: &dyn Oracle,
    geo: Geometry,
) -> Audit {
    let mut audit = Audit::default();
    for (path, info) in reader {
        if info.kind != NodeKind::File
            || !info.flags.contains(NodeFlags::NOT_FOLLOWED)
            || info.origin.is_some()
        {
            continue;
        }
        audit.files += 1;
        let text = String::from_utf8_lossy(path).into_owned();
        let Some(f) = full.get(path) else {
            audit.differ.push(Diff {
                path: text,
                field: "overlay file bytes",
                reader: "missing from the full count".into(),
                other: String::new(),
            });
            continue;
        };
        match oracle.ask(&text) {
            Answer::Found(t) if Context::is_overlay_backed(&t) => {
                audit.full_bytes += f.allocated;
                audit.listing_bytes += info.allocated;
                audit.walker_bytes += walker.get(path).map_or(0, |w| w.allocated);
                audit.system_bytes += t.allocation;
                if f.allocated == t.allocation {
                    audit.agree += 1;
                } else {
                    let record = oracle.file_record(&text);
                    let facts_text = match &record {
                        Ok(r) => r.describe(),
                        Err(why) => format!("no record facts: {why}"),
                    };
                    let diff = |why: Option<String>| Diff {
                        path: text.clone(),
                        field: "overlay file bytes",
                        reader: format!("full count {} of length {}", f.allocated, f.logical),
                        other: format!(
                            "system allocation {} (length {}, held {:?}); {facts_text}{}",
                            t.allocation,
                            t.logical,
                            t.held,
                            why.map_or_else(String::new, |w| format!(
                                "; not a stream held in the record: {w}"
                            ))
                        ),
                    };
                    match resident_overlay_stream(
                        f,
                        &t,
                        record.as_ref().map_err(String::as_str),
                        geo,
                    ) {
                        Ok(evidence) => audit.resident.push((diff(None), evidence)),
                        Err(why) => audit.differ.push(diff(Some(why))),
                    }
                }
            }
            // The system reports another kind of reparse point (an app
            // execution alias, say): not an overlay file. Its bytes are still
            // compared with the system's allocation, so a reader figure that
            // disagrees is a difference. The tag is tallied and printed.
            Answer::Found(t) if t.attributes & ATTR_REPARSE != 0 && t.reparse_tag != 0 => {
                audit.other_reparse.push((text.clone(), t.reparse_tag));
                if f.allocated != t.allocation {
                    audit.differ.push(Diff {
                        path: text,
                        field: "reparse point bytes",
                        reader: format!("full count {} of length {}", f.allocated, f.logical),
                        other: format!(
                            "not an overlay file: tag {:#010x}; system allocation {} (length {})",
                            t.reparse_tag, t.allocation, t.logical
                        ),
                    });
                }
            }
            Answer::Found(t) => audit.differ.push(Diff {
                path: text,
                field: "overlay file bytes",
                reader: "marked as a link that was not followed".into(),
                other: format!(
                    "the system names no overlay layer: tag {:#010x} backing {:?}",
                    t.reparse_tag, t.backing
                ),
            }),
            Answer::Gone | Answer::Unavailable(_) => audit.unanswered += 1,
        }
    }
    audit
}

/// The most residuals the audit prints one by one.
const REPORT_LIMIT: usize = 200;

/// Prints the audit.
pub fn report_audit(a: &Audit) {
    println!(
        "files the reader marks as links not followed: {}; of the overlay-backed ones, full count equals the system's allocation for {}, stream held in the file's own record {}; not asked or gone {}; different {}",
        a.files,
        a.agree,
        a.resident.len(),
        a.unanswered,
        a.differ.len()
    );
    // The files that are reparse points of another kind, by tag.
    let mut by_tag: BTreeMap<u32, usize> = BTreeMap::new();
    for (_, tag) in &a.other_reparse {
        *by_tag.entry(*tag).or_default() += 1;
    }
    println!(
        "    not overlay files (the system reports another reparse tag): {}",
        a.other_reparse.len()
    );
    for (tag, n) in &by_tag {
        let example = a
            .other_reparse
            .iter()
            .find(|(_, t)| t == tag)
            .map_or("", |(p, _)| p.as_str());
        println!("        tag {tag:#010x}: {n} files, for example {example:?}");
    }
    // Every residual, with the reader's figures and what the record says.
    for (d, evidence) in a.resident.iter().take(REPORT_LIMIT) {
        println!(
            "    held in the record: {:?}: {} / {}: {evidence}",
            d.path, d.reader, d.other
        );
    }
    println!(
        "    over the files that agree or differ, allocated bytes: full count {}, listing-style count {}, walker {}, system {}",
        a.full_bytes, a.listing_bytes, a.walker_bytes, a.system_bytes
    );
    for d in a.differ.iter().take(REPORT_LIMIT) {
        println!("    {:?}: {} / {}", d.path, d.reader, d.other);
    }
}

/// Prints the outcome: every class with its count and what it rests on, then
/// what is unexplained, with the evidence looked at for each example.
pub fn report(out: &Outcome) {
    let total = out.explained.len() + out.unexplained.len();
    println!(
        "differences between the reader and the walker: {total}; explained {}, unexplained {}",
        out.explained.len(),
        out.unexplained.len()
    );
    println!(
        "the system's answers (one question per path): {} found, {} gone, {} would not say",
        out.found, out.gone, out.unavailable
    );
    let mut classes: BTreeMap<&str, Vec<&(Diff, Verdict)>> = BTreeMap::new();
    for e in &out.explained {
        classes.entry(e.1.class).or_default().push(e);
    }
    for (class, list) in &classes {
        let mut fields: BTreeMap<&str, usize> = BTreeMap::new();
        for (d, _) in list {
            *fields.entry(d.field).or_default() += 1;
        }
        println!("explained: {class}: {} {fields:?}", list.len());
        if let Some((_, v)) = list.first() {
            println!("    because: {}", v.basis);
        }
        let mut shown: BTreeMap<&str, usize> = BTreeMap::new();
        for (d, v) in list {
            let n = shown.entry(d.field).or_default();
            if *n < 4 {
                println!(
                    "    {:?} {}: reader {} / walker {}  [{}]",
                    d.path, d.field, d.reader, d.other, v.detail
                );
                *n += 1;
            }
        }
    }
    if !out.reparse_facts.is_empty() {
        println!(
            "files the reader marks as reparse points that the listing does not (what the system says of each):"
        );
        for (fact, n) in &out.reparse_facts {
            println!("    {n} x {fact}");
        }
    }
    println!("unexplained: {}", out.unexplained.len());
    let mut by_field: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_folder: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
    for (d, why) in &out.unexplained {
        *by_field.entry(d.field).or_default() += 1;
        let top: Vec<&str> = d.path.split('/').take(2).collect();
        *by_folder.entry(top.join("/")).or_default() += 1;
        // The reason without the numbers, so like cases count together.
        let key: String = why
            .chars()
            .filter(|c| !c.is_ascii_digit())
            .take(120)
            .collect();
        *by_reason.entry(key).or_default() += 1;
    }
    println!("    by field: {by_field:?}");
    let mut folders: Vec<_> = by_folder.into_iter().collect();
    folders.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!(
        "    by folder (top 15): {:?}",
        &folders[..folders.len().min(15)]
    );
    let mut reasons: Vec<_> = by_reason.into_iter().collect();
    reasons.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (why, n) in reasons.iter().take(12) {
        println!("    {n} x {why}");
    }
    let mut shown: BTreeMap<&str, usize> = BTreeMap::new();
    for (d, why) in &out.unexplained {
        let n = shown.entry(d.field).or_default();
        if *n < 25 {
            println!(
                "    {:?} {}: reader {} / walker {}  [{why}]",
                d.path, d.field, d.reader, d.other
            );
            *n += 1;
        }
    }
}
