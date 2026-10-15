//! Mounts below the scan's start directory.
//!
//! A mount hides the folder it covers. The walker sees the mount's files
//! there; the trees hold only the empty folder underneath. On the usual
//! distribution layout, where `/home` is a subvolume of its own mounted on
//! the `root` subvolume's folder, reading the `root` tree alone would show
//! `/home` as an empty, complete folder and leave the user's files out of
//! the total. So the reader asks where the mounts are (from the mount table),
//! and for each one decides:
//!
//! * a mount the caller counts as part of the drive and that is a directory
//!   of this same file system is *entered*: its subvolume is read and grafted
//!   at the covered folder, as the walker sees it;
//! * every other mount is *not followed*: the folder is shown as not
//!   followed, never as the contents of what is mounted on it;
//! * a mount that belongs to the drive and cannot be placed is an error, so
//!   that the caller falls back to the walker instead of getting a tree that
//!   looks whole and is not.
//!
//! The mount table is read as the kernel gives it, but treated with the same
//! care as any other input: bounded line and mount counts, checked fields.

use crate::BtrfsError;
use crate::error::{Diagnostics, ProblemKind};
use crate::items::{self, FIRST_FREE, ModeKind};
use crate::model::{Model, Start};
use bs_scan::{Boundary, Placement};
use std::collections::{HashMap, HashSet};

/// The most mounts below the start the reader works with. A system has
/// dozens; a hostile table could list millions.
pub const MAX_MOUNTS: usize = 4096;

/// What is mounted at a mount point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mounted {
    /// Another file system, or another btrfs file system.
    Other,
    /// A directory of this file system: its subvolume and inode.
    Here(Start),
    /// This file system, but the source could not tell which directory.
    HereUnplaced,
}

/// A mount below the start directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountPoint {
    /// The names from the start directory down to the mount point.
    pub path: Vec<Vec<u8>>,
    /// The mount ID, as the mount table and `statx` give it.
    pub mount_id: u64,
    /// The device number, major in the high 32 bits and minor in the low.
    pub device: u64,
    /// What is mounted.
    pub mounted: Mounted,
}

/// What the scan does at a covered folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Graft {
    /// Read the mounted directory instead: its tree number and its position
    /// in the inode table.
    Enter { tree: u32, at: u32 },
    /// Show the folder as not followed.
    NotFollowed,
    /// The mounted directory's tree could not be read.
    Unreadable,
}

/// Where the scan crosses mounts, by the covered folder's position in the
/// inode table.
#[derive(Debug, Default)]
pub(crate) struct Grafts {
    by_inode: HashMap<u32, Graft>,
    pub entered: u64,
    pub not_followed: u64,
}

impl Grafts {
    pub fn get(&self, at: u32) -> Option<Graft> {
        self.by_inode.get(&at).copied()
    }
}

fn placement(m: &MountPoint) -> Placement {
    Placement {
        mount_id: Some(m.mount_id),
        device: m.device,
    }
}

/// The subvolumes whose trees must be read besides the start's and its nested
/// ones: those of the mounts the scan enters. An error when a mount of the
/// drive cannot be read at all.
pub(crate) fn trees_to_read(
    mounts: &[MountPoint],
    boundary: &Boundary,
) -> Result<Vec<u64>, BtrfsError> {
    if mounts.len() > MAX_MOUNTS {
        return Err(BtrfsError::TooLarge(
            "more mounts than the reader works with",
        ));
    }
    let mut trees = Vec::new();
    for m in mounts.iter().filter(|m| boundary.allows(placement(m))) {
        match m.mounted {
            Mounted::Here(start) => {
                if !trees.contains(&start.tree) {
                    trees.push(start.tree);
                }
            }
            Mounted::HereUnplaced => {
                return Err(BtrfsError::MountNotPlaced(
                    "a mount of this file system could not be traced to a subvolume",
                ));
            }
            Mounted::Other => {
                return Err(BtrfsError::MountNotPlaced(
                    "a mount that belongs to the drive is not a directory of this file system",
                ));
            }
        }
    }
    Ok(trees)
}

enum Walk {
    /// The covered folder's position in the inode table.
    Found(u32),
    /// Below a mount that is not entered: nothing to place.
    Hidden,
    /// The trees do not hold the path.
    Missing,
}

fn walk(model: &Model, grafts: &Grafts, start: u32, path: &[Vec<u8>]) -> Walk {
    let (mut tree, mut at) = (0u32, start);
    for name in path {
        // What is below a folder with a mount on it is the mount's.
        match grafts.get(at) {
            Some(Graft::Enter { tree: t, at: a }) => {
                tree = t;
                at = a;
            }
            Some(_) => return Walk::Hidden,
            None => {}
        }
        let Some(rec) = model.inode(at) else {
            return Walk::Missing;
        };
        let Some(entry) = model
            .entries_of(rec)
            .iter()
            .find(|e| model.name_bytes(e) == name.as_slice())
        else {
            return Walk::Missing;
        };
        if entry.stub || (entry.subvolume && !entry.confirmed) {
            // An empty stand-in folder, or an entry the root tree did not
            // place: there is nothing below it to place a mount in.
            return Walk::Missing;
        }
        let next = if entry.subvolume {
            let Some(&t) = model.tree_index.get(&entry.target) else {
                return Walk::Missing;
            };
            tree = t;
            model.find_inode(t, FIRST_FREE)
        } else {
            model.find_inode(tree, entry.target)
        };
        let Some(next) = next else {
            return Walk::Missing;
        };
        if model
            .inode(next)
            .is_none_or(|r| items::mode_kind(r.mode) != Some(ModeKind::Directory))
        {
            return Walk::Missing;
        }
        at = next;
    }
    Walk::Found(at)
}

/// Decides what to do at every mount point, after the trees are loaded.
pub(crate) fn place(
    model: &Model,
    start: u32,
    mounts: &[MountPoint],
    boundary: &Boundary,
    diag: &mut Diagnostics,
) -> Result<Grafts, BtrfsError> {
    let mut order: Vec<&MountPoint> = mounts.iter().collect();
    // Shallow first, so that a mount inside another is placed through it.
    order.sort_by_key(|m| (m.path.len(), m.mount_id));
    // Which of two mounts on one folder shows cannot be told from here (the
    // source is to hand over only the top one). If one of them belongs to
    // the drive, a guess could enter a mount the walker does not see, or
    // skip one it does: refuse, and let the walker answer.
    for (i, m) in order.iter().enumerate() {
        let shares = order
            .iter()
            .enumerate()
            .any(|(j, other)| i != j && other.path == m.path);
        if shares && boundary.allows(placement(m)) {
            return Err(BtrfsError::MountNotPlaced(
                "a mount of the drive shares its folder with another mount",
            ));
        }
    }
    let mut grafts = Grafts::default();
    for m in order {
        let allowed = boundary.allows(placement(m));
        let covered = match walk(model, &grafts, start, &m.path) {
            Walk::Found(a) => a,
            Walk::Hidden => continue,
            Walk::Missing => {
                if allowed {
                    return Err(BtrfsError::MountNotPlaced(
                        "a mount of the drive lies where the trees hold no folder",
                    ));
                }
                diag.note(ProblemKind::MountNotPlaced, 0, 0);
                continue;
            }
        };
        let graft = if allowed {
            let Mounted::Here(target) = m.mounted else {
                return Err(BtrfsError::MountNotPlaced(
                    "a mount of the drive could not be traced to a subvolume",
                ));
            };
            let Some(&t) = model.tree_index.get(&target.tree) else {
                return Err(BtrfsError::MountNotPlaced(
                    "a mount of the drive names a subvolume that was not read",
                ));
            };
            if model
                .trees
                .get(usize::try_from(t).unwrap_or(usize::MAX))
                .is_none_or(|r| !r.ok)
            {
                Graft::Unreadable
            } else {
                let Some(at) = model.find_inode(t, target.inode) else {
                    return Err(BtrfsError::MountNotPlaced(
                        "a mount of the drive names a directory the trees do not hold",
                    ));
                };
                if model
                    .inode(at)
                    .is_none_or(|r| items::mode_kind(r.mode) != Some(ModeKind::Directory))
                {
                    return Err(BtrfsError::MountNotPlaced(
                        "a mount of the drive is not on a directory",
                    ));
                }
                Graft::Enter { tree: t, at }
            }
        } else {
            Graft::NotFollowed
        };
        if grafts.by_inode.insert(covered, graft).is_none() {
            match graft {
                Graft::Enter { .. } => grafts.entered = grafts.entered.saturating_add(1),
                _ => grafts.not_followed = grafts.not_followed.saturating_add(1),
            }
        }
    }
    Ok(grafts)
}

/// One line of the mount table, as far as the reader needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TableEntry {
    pub id: u64,
    /// The mount this one is mounted on. For a mount stacked on another at
    /// the same folder, that other mount.
    pub parent: u64,
    pub device: u64,
    pub mount_point: Vec<u8>,
    pub fs_type: Vec<u8>,
}

/// The bytes of a mount table field with the kernel's octal escapes
/// (`\040` for a space, and so on) undone.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0usize;
    while let Some(&b) = field.get(i) {
        if b == b'\\'
            && let Some(digits) = field.get(i.saturating_add(1)..i.saturating_add(4))
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let value = digits.iter().fold(0u32, |a, d| {
                a.saturating_mul(8)
                    .saturating_add(u32::from(d.saturating_sub(b'0')))
            });
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i = i.saturating_add(4);
                continue;
            }
        }
        out.push(b);
        i = i.saturating_add(1);
    }
    out
}

fn number(field: &[u8]) -> Option<u64> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// Parses `/proc/self/mountinfo`. A line that does not parse is skipped: it
/// cannot be placed, and a mount the reader does not know of is handled the
/// same as one that is not there.
pub(crate) fn parse_table(text: &[u8]) -> Vec<TableEntry> {
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == b"-") else {
            continue;
        };
        if dash < 6 {
            continue;
        }
        let (Some(id), Some(parent), Some(dev), Some(point), Some(fs_type)) = (
            fields.first().and_then(|f| number(f)),
            fields.get(1).and_then(|f| number(f)),
            fields.get(2),
            fields.get(4),
            fields.get(dash.saturating_add(1)),
        ) else {
            continue;
        };
        let mut parts = dev.split(|&b| b == b':');
        let (Some(major), Some(minor)) =
            (parts.next().and_then(number), parts.next().and_then(number))
        else {
            continue;
        };
        let device = (major & 0xffff_ffff) << 32 | (minor & 0xffff_ffff);
        out.push(TableEntry {
            id,
            parent,
            device,
            mount_point: unescape(point),
            fs_type: fs_type.to_vec(),
        });
    }
    out
}

/// Drops the mounts that another mount is stacked on top of at the same
/// folder: only the top one shows there, and it is the top one that a walk
/// sees. The top is told by the mount table's parent IDs (a mount stacked on
/// another has that other as its parent), never by the order of the IDs,
/// which the kernel hands out lowest-free-first and so does not follow the
/// order mounts were made in.
pub(crate) fn without_covered(entries: Vec<TableEntry>) -> Vec<TableEntry> {
    let covered: HashSet<(Vec<u8>, u64)> = entries
        .iter()
        .filter(|top| top.parent != top.id)
        .filter(|top| {
            entries
                .iter()
                .any(|below| below.id == top.parent && below.mount_point == top.mount_point)
        })
        .map(|top| (top.mount_point.clone(), top.parent))
        .collect();
    entries
        .into_iter()
        .filter(|e| !covered.contains(&(e.mount_point.clone(), e.id)))
        .collect()
}

/// The names from `root` down to `point`, when `point` lies strictly below
/// `root`. Both are absolute and normalized, as the mount table and a
/// canonical path are.
pub(crate) fn below(root: &[u8], point: &[u8]) -> Option<Vec<Vec<u8>>> {
    let root = root.strip_suffix(b"/").unwrap_or(root);
    let rest = point.strip_prefix(root)?;
    let rest = rest.strip_prefix(b"/")?;
    let names: Vec<Vec<u8>> = rest
        .split(|&b| b == b'/')
        .filter(|n| !n.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    if names.is_empty() || names.iter().any(|n| n == b"." || n == b"..") {
        return None;
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;

    #[test]
    fn the_mount_table_is_read_with_its_escapes() {
        let text = b"36 35 98:0 / /mnt/with\\040space rw,noatime shared:1 - btrfs /dev/sda1 rw\n\
                     37 35 0:25 / /proc rw - proc proc rw\n\
                     garbage\n\
                     38 35 x:y / /bad rw - ext4 /dev/x rw\n\
                     39 35 8:1 / /a\\134b rw,x master:2 master:3 - btrfs /dev/sda1 rw\n";
        let t = parse_table(text);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].id, 36);
        assert_eq!(t[0].parent, 35);
        assert_eq!(t[0].device, 98 << 32);
        assert_eq!(t[0].mount_point, b"/mnt/with space");
        assert_eq!(t[0].fs_type, b"btrfs");
        assert_eq!(t[1].fs_type, b"proc");
        assert_eq!(t[2].mount_point, b"/a\\b");
        assert_eq!(t[2].device, (8 << 32) | 1);
    }

    #[test]
    fn escapes_that_are_not_escapes_are_kept() {
        assert_eq!(unescape(b"a\\9b\\04"), b"a\\9b\\04");
        assert_eq!(unescape(b"\\777"), b"\\777");
        assert_eq!(unescape(b"\\012x"), b"\nx");
    }

    #[test]
    fn mount_points_are_cut_to_names_below_the_root() {
        assert_eq!(below(b"/", b"/home").unwrap(), vec![b"home".to_vec()]);
        assert_eq!(
            below(b"/mnt/a", b"/mnt/a/x/y").unwrap(),
            vec![b"x".to_vec(), b"y".to_vec()]
        );
        // The root itself, a sibling that shares a prefix, and something above.
        assert!(below(b"/mnt/a", b"/mnt/a").is_none());
        assert!(below(b"/mnt/a", b"/mnt/ab").is_none());
        assert!(below(b"/mnt/a", b"/mnt").is_none());
        assert!(below(b"/mnt/a", b"/mnt/a/../b").is_none());
    }

    fn entry(id: u64, parent: u64, point: &[u8]) -> TableEntry {
        TableEntry {
            id,
            parent,
            device: 0,
            mount_point: point.to_vec(),
            fs_type: b"btrfs".to_vec(),
        }
    }

    fn ids(v: &[TableEntry]) -> Vec<u64> {
        let mut ids: Vec<u64> = v.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn the_mount_on_top_is_told_by_parent_not_by_id_order() {
        // 31 is mounted on 40 at the same folder although its ID is lower:
        // IDs are handed out lowest-free-first.
        let t = without_covered(vec![
            entry(40, 20, b"/home"),
            entry(31, 40, b"/home"),
            entry(41, 20, b"/srv"),
        ]);
        assert_eq!(ids(&t), vec![31, 41]);
        // Three deep.
        let t = without_covered(vec![
            entry(7, 1, b"/a"),
            entry(9, 7, b"/a"),
            entry(8, 9, b"/a"),
        ]);
        assert_eq!(ids(&t), vec![8]);
    }

    #[test]
    fn mounts_that_are_not_stacked_are_all_kept() {
        // Same folder but no parent link between them (they cannot both be
        // told apart by the table): both stay, and placement refuses if one
        // belongs to the drive. A parent on another folder covers nothing.
        let t = without_covered(vec![
            entry(10, 1, b"/x"),
            entry(11, 1, b"/x"),
            entry(12, 10, b"/x/inner"),
            entry(13, 13, b"/self"),
        ]);
        assert_eq!(ids(&t), vec![10, 11, 12, 13]);
    }

    /// Runs inside a private user and mount namespace; outside it has
    /// nothing to do.
    #[test]
    fn inside_the_namespace_the_stacked_mount_on_top_is_found() {
        let Some(dir) = std::env::var_os("BS_STACK_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let (one, two) = (dir.join("one"), dir.join("two"));
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let sh = |script: String| {
            let ok = std::process::Command::new("sh")
                .arg("-ec")
                .arg(script)
                .status()
                .unwrap()
                .success();
            assert!(ok);
        };
        let (o, t) = (one.display(), two.display());
        // Mount IDs are handed out lowest-free-first: freeing the first
        // mount's ID and then stacking on `two` gives the mount on top a
        // lower ID than the one it covers.
        sh(format!(
            "mount -t tmpfs none '{o}'; mount -t tmpfs none '{t}'; umount '{o}'; mount -t tmpfs none '{t}'"
        ));
        let text = std::fs::read("/proc/self/mountinfo").unwrap();
        let here: Vec<TableEntry> = parse_table(&text)
            .into_iter()
            .filter(|e| e.mount_point == two.as_os_str().as_encoded_bytes())
            .collect();
        assert_eq!(here.len(), 2, "{here:?}");
        let (low, high) = (
            here.iter().min_by_key(|e| e.id).unwrap(),
            here.iter().max_by_key(|e| e.id).unwrap(),
        );
        // The mount that was made last has the lower ID and is the one whose
        // parent is the other.
        assert_eq!(
            low.parent, high.id,
            "the ID order is the reverse of the stacking"
        );
        let tops = without_covered(here.clone());
        assert_eq!(tops.len(), 1);
        assert_eq!(tops[0].id, low.id);
    }

    #[test]
    fn mounts_stacked_on_one_folder_are_told_apart_by_parent_in_a_real_table() {
        if std::env::var_os("BS_STACK_DIR").is_some() {
            return;
        }
        let probe = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "true"])
            .output();
        if !matches!(&probe, Ok(o) if o.status.success()) {
            assert!(
                std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
                "BYTESCAPE_REQUIRE_NAMESPACES is set but a user and mount namespace cannot be made"
            );
            eprintln!("NOT RUN: cannot create a user and mount namespace here");
            return;
        }
        let dir = std::env::temp_dir().join(format!("bs-btrfs-stack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        let script = format!(
            "exec '{}' mounts::tests::inside_the_namespace_the_stacked_mount_on_top_is_found --exact --test-threads=1 --nocapture",
            exe.display()
        );
        let out = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c", &script])
            .env("BS_STACK_DIR", &dir)
            .env("RUST_BACKTRACE", "0")
            .output()
            .unwrap();
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "the namespace run failed:\n{text}");
        assert!(
            text.contains("1 passed"),
            "the inner test did not run:\n{text}"
        );
    }

    #[test]
    fn hostile_tables_do_not_panic() {
        for text in [
            &b""[..],
            b"\n\n\n",
            b" - ",
            b"1 2 3:4 / / - x",
            b"99999999999999999999999 1 1:1 / / rw - btrfs a b",
            b"1 1 1:1 / / rw - ",
            &[0xff, 0xfe, b' ', b'-', b' '],
        ] {
            let _ = parse_table(text);
        }
    }
}
