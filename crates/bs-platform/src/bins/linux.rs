//! The freedesktop.org Trash, per drive.

use super::walk::{self, DirStream, Stat, open_dir, open_dir_at, stat_at};
use super::{
    BinControl, BinInfo, BinLocation, BinPolicy, BinProblem, LocationRole, Problems, UnusableReason,
};
use crate::drives::{Drive, DriveId};
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Folders nested deeper than this are not entered. Each level holds one open
/// descriptor.
pub(crate) const MAX_DEPTH: usize = 200;

/// What the Trash depends on that is not in the mount table.
#[derive(Clone, Debug)]
pub(crate) struct Env {
    /// The user the Trash belongs to.
    pub uid: u32,
    /// `$XDG_DATA_HOME`, or `~/.local/share`.
    pub data_home: Option<PathBuf>,
}

impl Env {
    pub fn system() -> Self {
        Self {
            uid: walk::current_uid(),
            data_home: data_home(
                std::env::var_os("XDG_DATA_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            ),
        }
    }
}

/// `$XDG_DATA_HOME` if it is an absolute path, else `$HOME/.local/share`.
pub(crate) fn data_home(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let absolute = |value: Option<&OsStr>| {
        value
            .map(Path::new)
            .filter(|p| p.is_absolute())
            .map(Path::to_path_buf)
    };
    absolute(xdg).or_else(|| absolute(home).map(|h| h.join(".local/share")))
}

/// A Trash folder, as the steps that lead to it from a folder the caller
/// named. Each step is opened without following links when it is measured.
#[derive(Clone, Debug)]
pub(crate) struct Located {
    top: PathBuf,
    steps: Vec<CString>,
    pub location: BinLocation,
}

impl Located {
    fn new(top: &Path, steps: &[&CStr], role: LocationRole) -> Self {
        let mut path = top.to_path_buf();
        for step in steps {
            path.push(OsStr::from_bytes(step.to_bytes()));
        }
        Self {
            top: top.to_path_buf(),
            steps: steps.iter().map(|s| (*s).to_owned()).collect(),
            location: BinLocation { path, role },
        }
    }

    pub(crate) fn open(&self) -> io::Result<std::os::fd::OwnedFd> {
        let mut fd = open_dir(&self.top)?;
        for step in &self.steps {
            fd = open_dir_at(fd.as_raw_fd(), step)?;
        }
        Ok(fd)
    }
}

/// Which Trash folders exist, and what was found out while looking.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub located: Vec<Located>,
    pub problems: Vec<BinProblem>,
}

fn problem_for(path: PathBuf, error: &io::Error) -> BinProblem {
    match error.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => BinProblem::NotFollowed { path },
        _ => BinProblem::Unreadable {
            path,
            kind: error.kind(),
        },
    }
}

/// Finds the Trash folders of `env.uid` on the drive whose mount points are
/// `topdirs`.
///
/// The home Trash belongs to the drive that holds the data folder. The
/// shared `.Trash/$uid` is used only where `.Trash` is a real folder with the
/// sticky bit, as the specification requires, and `.Trash-$uid` where it
/// exists. A folder that is a link, or that someone else owns, is reported and
/// left alone.
pub(crate) fn plan(env: &Env, topdirs: &[PathBuf]) -> Plan {
    let mut plan = Plan::default();
    let uid_name = CString::new(env.uid.to_string()).unwrap_or_default();
    let private_name = CString::new(format!(".Trash-{}", env.uid)).unwrap_or_default();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut devices: Vec<u64> = Vec::new();
    let mut volume: Vec<Located> = Vec::new();

    // A folder that must belong to the user. Gives its status when it may be
    // used.
    let mut accept = |plan: &mut Plan, stat: &Stat, path: &Path| -> bool {
        if stat.is_symlink() {
            plan.problems
                .push(BinProblem::NotFollowed { path: path.into() });
            return false;
        }
        if !stat.is_dir() {
            return false;
        }
        if stat.uid != env.uid {
            plan.problems
                .push(BinProblem::NotOwnedByUser { path: path.into() });
            return false;
        }
        seen.insert((stat.dev, stat.ino))
    };

    for top in topdirs {
        let Ok(top_fd) = open_dir(top) else { continue };
        let fd = top_fd.as_raw_fd();
        let Ok(top_stat) = walk::fstat(fd) else {
            continue;
        };
        devices.push(top_stat.dev);

        // $topdir/.Trash/$uid, when .Trash passes the specification's checks.
        let shared = c".Trash";
        match stat_at(fd, shared) {
            Ok(stat) => {
                let path = top.join(".Trash");
                let reason = if stat.is_symlink() {
                    Some(UnusableReason::Symlink)
                } else if !stat.is_dir() {
                    Some(UnusableReason::NotDirectory)
                } else if !stat.is_sticky() {
                    Some(UnusableReason::NotSticky)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    plan.problems
                        .push(BinProblem::SharedTrashUnusable { path, reason });
                } else {
                    match open_dir_at(fd, shared) {
                        Ok(shared_fd) => match stat_at(shared_fd.as_raw_fd(), &uid_name) {
                            Ok(user) => {
                                let located = Located::new(
                                    top,
                                    &[shared, uid_name.as_c_str()],
                                    LocationRole::SharedVolumeTrash,
                                );
                                if accept(&mut plan, &user, &located.location.path) {
                                    volume.push(located);
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => plan.problems.push(problem_for(
                                path.join(OsStr::from_bytes(uid_name.to_bytes())),
                                &error,
                            )),
                        },
                        Err(error) => plan.problems.push(problem_for(path, &error)),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => plan.problems.push(problem_for(top.join(".Trash"), &error)),
        }

        // $topdir/.Trash-$uid
        match stat_at(fd, &private_name) {
            Ok(stat) => {
                let located =
                    Located::new(top, &[private_name.as_c_str()], LocationRole::VolumeTrash);
                if accept(&mut plan, &stat, &located.location.path) {
                    volume.push(located);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => plan.problems.push(problem_for(
                top.join(OsStr::from_bytes(private_name.to_bytes())),
                &error,
            )),
        }
    }

    // The home Trash belongs to the drive that holds the data folder.
    if let Some(data) = &env.data_home
        && let Ok(data_fd) = open_dir(data)
        && let Ok(data_stat) = walk::fstat(data_fd.as_raw_fd())
        && devices.contains(&data_stat.dev)
    {
        match stat_at(data_fd.as_raw_fd(), c"Trash") {
            Ok(stat) => {
                let located = Located::new(data, &[c"Trash"], LocationRole::HomeTrash);
                if accept(&mut plan, &stat, &located.location.path) {
                    plan.located.push(located);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => plan.problems.push(problem_for(data.join("Trash"), &error)),
        }
    }
    plan.located.extend(volume);
    plan
}

/// The mount points a drive's Trash can be at: every mount that is not just a
/// second view of another.
pub(crate) fn topdirs(drive: &Drive) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = drive
        .mounts
        .iter()
        .filter(|m| !m.covered_by_other_mount)
        .map(|m| m.mount_point.clone())
        .collect();
    if dirs.is_empty() {
        dirs.push(drive.mount_point.clone());
    }
    dirs
}

pub(crate) fn locate_drive(drive: &Drive) -> Vec<BinLocation> {
    plan(&Env::system(), &topdirs(drive))
        .located
        .into_iter()
        .map(|l| l.location)
        .collect()
}

pub(crate) fn measure_drive(drive: &Drive, control: &BinControl) -> BinInfo {
    measure_with(&Env::system(), &topdirs(drive), &drive.id, control)
}

/// Everything counted so far.
#[derive(Default)]
struct Totals {
    allocated: u64,
    logical: u64,
    items: u64,
    shared_files: u64,
    /// Files with several names, by device and inode, so each is counted once.
    linked: HashSet<(u64, u64)>,
}

/// Measures the Trash folders found for `topdirs`.
pub(crate) fn measure_with(
    env: &Env,
    topdirs: &[PathBuf],
    drive: &DriveId,
    control: &BinControl,
) -> BinInfo {
    let plan = plan(env, topdirs);
    let mut problems = Problems::default();
    for problem in plan.problems {
        problems.add(problem);
    }
    let mut totals = Totals::default();
    let mut locations = Vec::new();
    for located in &plan.located {
        if control.is_cancelled() {
            break;
        }
        locations.push(located.location.clone());
        measure_one(located, control, &mut totals, &mut problems);
    }
    if control.is_cancelled() {
        problems.add(BinProblem::Cancelled);
    }
    BinInfo {
        drive: drive.clone(),
        name: super::BinName::Trash,
        locations,
        allocated_bytes: Some(totals.allocated),
        logical_bytes: totals.logical,
        items: totals.items,
        shared_files: totals.shared_files,
        complete: !problems.incomplete,
        problems: problems.list,
        problems_omitted: problems.omitted,
        policy: BinPolicy::default(),
    }
}

const INFO_SUFFIX: &[u8] = b".trashinfo";

fn measure_one(
    located: &Located,
    control: &BinControl,
    totals: &mut Totals,
    problems: &mut Problems,
) {
    let path = &located.location.path;
    let root = match located.open() {
        Ok(fd) => fd,
        // Gone since it was found.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            problems.add(problem_for(path.clone(), &error));
            return;
        }
    };
    let root_fd = root.as_raw_fd();
    let Ok(root_stat) = walk::fstat(root_fd) else {
        problems.add(BinProblem::Unreadable {
            path: path.clone(),
            kind: io::ErrorKind::Other,
        });
        return;
    };

    let mut names: [Vec<Vec<u8>>; 2] = [Vec::new(), Vec::new()];
    let mut stopped = false;
    for (index, folder) in [c"files", c"info"].into_iter().enumerate() {
        let sub = match open_dir_at(root_fd, folder) {
            Ok(fd) => fd,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                problems.add(problem_for(
                    path.join(OsStr::from_bytes(folder.to_bytes())),
                    &error,
                ));
                continue;
            }
        };
        let dir = match DirStream::new(sub) {
            Ok(dir) => dir,
            Err(error) => {
                problems.add(problem_for(
                    path.join(OsStr::from_bytes(folder.to_bytes())),
                    &error,
                ));
                continue;
            }
        };
        let mut walker = Walker {
            control,
            dev: root_stat.dev,
            totals: &mut *totals,
            problems: &mut *problems,
            base: path.join(OsStr::from_bytes(folder.to_bytes())),
            stack: Vec::new(),
            stopped: false,
        };
        walker.walk(dir, 0, Some(&mut names[index]));
        stopped |= walker.stopped;
        if stopped {
            return;
        }
    }

    // The bookkeeping file next to them is emptied with them.
    if let Ok(stat) = stat_at(root_fd, c"directorysizes")
        && !stat.is_dir()
    {
        add_entry(control, totals, &stat);
    }

    let [files, info] = names;
    let present: HashSet<&[u8]> = files.iter().map(Vec::as_slice).collect();
    let mut stems: Vec<&[u8]> = info
        .iter()
        .filter_map(|n| n.strip_suffix(INFO_SUFFIX))
        .collect();
    stems.sort_unstable();
    let described: HashSet<&[u8]> = stems.iter().copied().collect();
    for stem in &stems {
        if present.contains(stem) {
            totals.items += 1;
        } else {
            problems.add(BinProblem::OrphanInfo {
                name: stem.to_vec(),
            });
        }
    }
    let mut loose: Vec<&[u8]> = present
        .iter()
        .copied()
        .filter(|n| !described.contains(n))
        .collect();
    loose.sort_unstable();
    for name in loose {
        problems.add(BinProblem::OrphanFile {
            name: name.to_vec(),
        });
    }
}

/// Adds one entry to the totals. A file with several names adds its bytes the
/// first time only.
fn add_entry(control: &BinControl, totals: &mut Totals, stat: &Stat) {
    let mut counted = true;
    if stat.nlink > 1 && !stat.is_dir() {
        if totals.linked.insert((stat.dev, stat.ino)) {
            totals.shared_files += 1;
        } else {
            counted = false;
        }
    }
    if counted {
        totals.allocated = totals.allocated.saturating_add(stat.allocated);
        if !stat.is_dir() {
            totals.logical = totals.logical.saturating_add(stat.size);
        }
    }
    control.count(if counted { stat.allocated } else { 0 });
}

struct Walker<'a> {
    control: &'a BinControl,
    /// The filesystem the bin is on; folders on others are not entered.
    dev: u64,
    totals: &'a mut Totals,
    problems: &'a mut Problems,
    /// The folder being walked, for naming problems.
    base: PathBuf,
    /// The folders between `base` and the one being read.
    stack: Vec<Vec<u8>>,
    stopped: bool,
}

impl Walker<'_> {
    fn path_of(&self, name: &CStr) -> PathBuf {
        let mut path = self.base.clone();
        for part in &self.stack {
            path.push(OsStr::from_bytes(part));
        }
        path.push(OsStr::from_bytes(name.to_bytes()));
        path
    }

    fn here(&self) -> PathBuf {
        let mut path = self.base.clone();
        for part in &self.stack {
            path.push(OsStr::from_bytes(part));
        }
        path
    }

    /// Reads one folder and everything below it. `top` collects the names of
    /// the entries of this folder when given.
    fn walk(&mut self, mut dir: DirStream, depth: usize, mut top: Option<&mut Vec<Vec<u8>>>) {
        let fd = dir.fd();
        loop {
            if self.control.is_cancelled() {
                self.stopped = true;
                return;
            }
            let name = match dir.next_name() {
                Ok(Some(name)) => name,
                Ok(None) => return,
                Err(error) => {
                    let path = self.here();
                    self.problems.add(problem_for(path, &error));
                    return;
                }
            };
            if let Some(top) = top.as_deref_mut() {
                top.push(name.to_bytes().to_vec());
            }
            let stat = match stat_at(fd, name) {
                Ok(stat) => stat,
                Err(error) => {
                    let path = self.path_of(name);
                    self.problems.add(problem_for(path, &error));
                    continue;
                }
            };
            if !stat.is_dir() {
                add_entry(self.control, self.totals, &stat);
                continue;
            }
            if stat.dev != self.dev {
                let path = self.path_of(name);
                self.problems.add(BinProblem::OtherFilesystem { path });
                continue;
            }
            add_entry(self.control, self.totals, &stat);
            if depth + 1 >= MAX_DEPTH {
                let path = self.path_of(name);
                self.problems.add(BinProblem::TooDeep { path });
                continue;
            }
            match open_dir_at(fd, name).and_then(DirStream::new) {
                Ok(child) => {
                    self.stack.push(name.to_bytes().to_vec());
                    self.walk(child, depth + 1, None);
                    self.stack.pop();
                    if self.stopped {
                        return;
                    }
                }
                Err(error) => {
                    let path = self.path_of(name);
                    self.problems.add(problem_for(path, &error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn our_uid() -> u32 {
        walk::current_uid()
    }

    fn env_for(f: &Fixture) -> Env {
        Env {
            uid: our_uid(),
            data_home: Some(f.path().join("home/.local/share")),
        }
    }

    fn drive_id() -> DriveId {
        DriveId::new("test")
    }

    fn measure(f: &Fixture, env: &Env, tops: &[&str]) -> BinInfo {
        let tops: Vec<PathBuf> = tops.iter().map(|t| f.path().join(t)).collect();
        for top in &tops {
            // The drive's mount point exists, whatever else the test makes.
            if !top.to_string_lossy().contains("does/not/exist") {
                fs::create_dir_all(top).unwrap();
            }
        }
        measure_with(env, &tops, &drive_id(), &BinControl::new())
    }

    /// A mount point inside the fixture, created.
    fn made(f: &Fixture, name: &str) -> PathBuf {
        f.mkdir(name);
        f.path().join(name)
    }

    const TRASHINFO: &str = "[Trash Info]\nPath=/somewhere/x\nDeletionDate=2026-01-02T03:04:05\n";

    /// Creates an item the way a file manager does.
    fn add_item(f: &Fixture, trash: &str, name: &str, contents: &str) {
        f.write(&format!("{trash}/files/{name}"), contents);
        f.write(&format!("{trash}/info/{name}.trashinfo"), TRASHINFO);
    }

    fn sticky(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o1777)).unwrap();
    }

    /// Counts a tree the plain way, without following links: allocated bytes
    /// of everything, apparent sizes of everything but folders, each inode
    /// once.
    fn oracle(dir: &Path) -> (u64, u64) {
        fn go(dir: &Path, seen: &mut HashSet<(u64, u64)>, total: &mut (u64, u64)) {
            let own = fs::symlink_metadata(dir).unwrap();
            total.0 += own.blocks() * 512;
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    go(&path, seen, total);
                } else if meta.nlink() == 1 || seen.insert((meta.dev(), meta.ino())) {
                    total.0 += meta.blocks() * 512;
                    total.1 += meta.len();
                }
            }
        }
        let mut total = (0, 0);
        go(dir, &mut HashSet::new(), &mut total);
        // The folder itself is not part of what is inside it.
        let own = fs::symlink_metadata(dir).unwrap();
        total.0 -= own.blocks() * 512;
        total
    }

    #[test]
    fn data_home_follows_the_xdg_rules() {
        let at = |x: Option<&str>, h: Option<&str>| data_home(x.map(OsStr::new), h.map(OsStr::new));
        assert_eq!(at(Some("/data"), Some("/home/a")), Some("/data".into()));
        assert_eq!(
            at(None, Some("/home/a")),
            Some("/home/a/.local/share".into())
        );
        assert_eq!(
            at(Some("relative"), Some("/home/a")),
            Some("/home/a/.local/share".into())
        );
        assert_eq!(
            at(Some(""), Some("/home/a")),
            Some("/home/a/.local/share".into())
        );
        assert_eq!(at(None, Some("home")), None);
        assert_eq!(at(None, None), None);
    }

    #[test]
    fn a_home_trash_is_measured_and_its_items_counted() {
        let f = Fixture::new("bin-home");
        f.mkdir("vol");
        let trash = "home/.local/share/Trash";
        add_item(&f, trash, "big.bin", &"x".repeat(5000));
        add_item(&f, trash, "small.txt", "hi");
        f.write(&format!("{trash}/files/dir/inner/deep.txt"), "12345");
        f.write(&format!("{trash}/info/dir.trashinfo"), TRASHINFO);
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 3);
        assert_eq!(info.locations.len(), 1);
        assert_eq!(info.locations[0].role, LocationRole::HomeTrash);
        assert_eq!(info.locations[0].path, f.path().join(trash));
        assert!(info.complete, "{:?}", info.problems);
        assert!(info.problems.is_empty(), "{:?}", info.problems);
        // The same figures as a plain count of both folders.
        let files = oracle(&f.path().join(trash).join("files"));
        let infos = oracle(&f.path().join(trash).join("info"));
        assert_eq!(info.allocated_bytes, Some(files.0 + infos.0));
        assert_eq!(info.logical_bytes, files.1 + infos.1);
        // 5000 + 2 + 5 bytes of items, plus three small info files.
        assert_eq!(
            info.logical_bytes,
            5000 + 2 + 5 + 3 * TRASHINFO.len() as u64
        );
        assert_eq!(info.name, super::super::BinName::Trash);
    }

    #[test]
    fn an_empty_or_missing_trash_has_nothing_in_it() {
        let f = Fixture::new("bin-empty");
        f.mkdir("vol");
        let none = measure(&f, &env_for(&f), &["vol"]);
        assert!(none.locations.is_empty());
        assert_eq!(
            (none.items, none.logical_bytes, none.allocated_bytes),
            (0, 0, Some(0))
        );
        assert!(none.complete);
        f.mkdir("home/.local/share/Trash/files");
        f.mkdir("home/.local/share/Trash/info");
        let empty = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(empty.locations.len(), 1);
        assert_eq!((empty.items, empty.logical_bytes), (0, 0));
        assert!(empty.complete && empty.problems.is_empty());
    }

    #[test]
    fn the_shared_trash_needs_a_real_sticky_folder() {
        let f = Fixture::new("bin-shared");
        let uid = our_uid();
        f.mkdir("vol/.Trash");
        sticky(&f.path().join("vol/.Trash"));
        add_item(&f, &format!("vol/.Trash/{uid}"), "a.txt", "aaaa");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 1);
        assert_eq!(info.locations.len(), 1);
        assert_eq!(info.locations[0].role, LocationRole::SharedVolumeTrash);
        assert_eq!(
            info.locations[0].path,
            f.path().join(format!("vol/.Trash/{uid}"))
        );
        assert!(info.problems.is_empty(), "{:?}", info.problems);
    }

    #[test]
    fn a_shared_trash_that_is_not_sticky_is_not_used() {
        let f = Fixture::new("bin-nosticky");
        let uid = our_uid();
        f.mkdir("vol/.Trash");
        fs::set_permissions(
            f.path().join("vol/.Trash"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        add_item(&f, &format!("vol/.Trash/{uid}"), "hidden.txt", "no");
        add_item(&f, &format!("vol/.Trash-{uid}"), "used.txt", "yes");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 1, "only .Trash-uid counts");
        assert_eq!(info.locations.len(), 1);
        assert_eq!(info.locations[0].role, LocationRole::VolumeTrash);
        assert_eq!(
            info.problems,
            vec![BinProblem::SharedTrashUnusable {
                path: f.path().join("vol/.Trash"),
                reason: UnusableReason::NotSticky,
            }]
        );
        // What the specification sets aside is no reason to distrust the rest.
        assert!(info.complete);
    }

    #[test]
    fn a_shared_trash_that_is_a_link_is_not_followed() {
        let f = Fixture::new("bin-symlink");
        let uid = our_uid();
        // The target is a perfectly good sticky folder with items in it.
        f.mkdir("elsewhere");
        sticky(&f.path().join("elsewhere"));
        add_item(
            &f,
            &format!("elsewhere/{uid}"),
            "trap.txt",
            "must not be seen",
        );
        f.symlink("../elsewhere", "vol/.Trash");
        add_item(&f, &format!("vol/.Trash-{uid}"), "ok.txt", "ok");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 1);
        assert_eq!(info.locations.len(), 1);
        assert_eq!(info.locations[0].role, LocationRole::VolumeTrash);
        assert_eq!(
            info.problems,
            vec![BinProblem::SharedTrashUnusable {
                path: f.path().join("vol/.Trash"),
                reason: UnusableReason::Symlink,
            }]
        );
        assert!(
            info.logical_bytes < 100,
            "the link target was counted: {}",
            info.logical_bytes
        );
    }

    #[test]
    fn a_shared_trash_that_is_a_file_is_reported() {
        let f = Fixture::new("bin-notdir");
        f.write("vol/.Trash", "not a folder");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(
            info.problems,
            vec![BinProblem::SharedTrashUnusable {
                path: f.path().join("vol/.Trash"),
                reason: UnusableReason::NotDirectory,
            }]
        );
        assert!(info.locations.is_empty());
    }

    #[test]
    fn the_users_folder_inside_the_shared_trash_must_be_a_real_folder() {
        let f = Fixture::new("bin-userlink");
        let uid = our_uid();
        f.mkdir("vol/.Trash");
        sticky(&f.path().join("vol/.Trash"));
        add_item(&f, "real", "x.txt", "secret");
        f.symlink("../../real", &format!("vol/.Trash/{uid}"));
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert!(info.locations.is_empty());
        assert_eq!(
            info.problems,
            vec![BinProblem::NotFollowed {
                path: f.path().join(format!("vol/.Trash/{uid}"))
            }]
        );
        assert_eq!(info.logical_bytes, 0);
        assert!(!info.complete, "a link stood where the bin should be");
    }

    #[test]
    fn a_trash_folder_of_another_user_is_not_read() {
        let f = Fixture::new("bin-foreign");
        // The same folder, asked for as if the user had another id.
        let other = our_uid() + 1;
        add_item(&f, &format!("vol/.Trash-{other}"), "x.txt", "theirs");
        let mut env = env_for(&f);
        env.uid = other;
        let info = measure(&f, &env, &["vol"]);
        assert!(info.locations.is_empty());
        assert_eq!(
            info.problems,
            vec![BinProblem::NotOwnedByUser {
                path: f.path().join(format!("vol/.Trash-{other}"))
            }]
        );
        assert_eq!(info.logical_bytes, 0);
    }

    #[test]
    fn both_trash_folders_of_a_volume_add_up() {
        let f = Fixture::new("bin-both");
        let uid = our_uid();
        f.mkdir("vol/.Trash");
        sticky(&f.path().join("vol/.Trash"));
        add_item(&f, &format!("vol/.Trash/{uid}"), "a", "1");
        add_item(&f, &format!("vol/.Trash-{uid}"), "b", "22");
        add_item(&f, "home/.local/share/Trash", "c", "333");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 3);
        let roles: Vec<_> = info.locations.iter().map(|l| l.role).collect();
        assert_eq!(
            roles,
            vec![
                LocationRole::HomeTrash,
                LocationRole::SharedVolumeTrash,
                LocationRole::VolumeTrash
            ]
        );
    }

    #[test]
    fn two_mounts_that_show_one_folder_are_measured_once() {
        let f = Fixture::new("bin-twice");
        let uid = our_uid();
        add_item(&f, &format!("vol/.Trash-{uid}"), "a", "1");
        f.symlink("vol", "again");
        let info = measure(&f, &env_for(&f), &["vol", "again"]);
        assert_eq!(info.locations.len(), 1);
        assert_eq!(info.items, 1);
    }

    #[test]
    fn items_need_both_halves_and_orphans_are_reported() {
        let f = Fixture::new("bin-orphans");
        let trash = "home/.local/share/Trash";
        add_item(&f, trash, "whole.txt", "w");
        f.write(&format!("{trash}/info/lost.txt.trashinfo"), TRASHINFO);
        f.write(&format!("{trash}/files/stray.txt"), "stray bytes");
        f.write(&format!("{trash}/files/zzz"), "z");
        f.write(&format!("{trash}/info/not-an-info-file.txt"), "i");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 1);
        assert_eq!(
            info.problems,
            vec![
                BinProblem::OrphanInfo {
                    name: b"lost.txt".to_vec()
                },
                BinProblem::OrphanFile {
                    name: b"stray.txt".to_vec()
                },
                BinProblem::OrphanFile {
                    name: b"zzz".to_vec()
                },
            ]
        );
        // The stray bytes still occupy space, so they are counted.
        // "w", the stray 11 bytes, "z" and "i", and two real info files.
        let want = 1 + 11 + 1 + 1 + 2 * TRASHINFO.len() as u64;
        assert_eq!(info.logical_bytes, want);
        assert!(info.complete, "orphans do not hide any bytes");
    }

    #[test]
    fn links_inside_the_bin_are_counted_as_links() {
        let f = Fixture::new("bin-links");
        let trash = "home/.local/share/Trash";
        // A big folder outside the bin, a link to it inside, and a loop.
        f.write("outside/huge.bin", &"h".repeat(100_000));
        f.mkdir(&format!("{trash}/files"));
        f.mkdir(&format!("{trash}/info"));
        f.symlink("../../../../../outside", &format!("{trash}/files/linked"));
        f.write(&format!("{trash}/info/linked.trashinfo"), TRASHINFO);
        f.mkdir(&format!("{trash}/files/loop"));
        f.symlink("..", &format!("{trash}/files/loop/up"));
        f.write(&format!("{trash}/info/loop.trashinfo"), TRASHINFO);
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 2);
        assert!(
            info.logical_bytes < 1000,
            "followed a link: {}",
            info.logical_bytes
        );
        assert!(info.complete, "{:?}", info.problems);
        let files = oracle(&f.path().join(trash).join("files"));
        let infos = oracle(&f.path().join(trash).join("info"));
        assert_eq!(info.allocated_bytes, Some(files.0 + infos.0));
    }

    #[test]
    fn a_file_with_two_names_is_counted_once_and_flagged() {
        let f = Fixture::new("bin-hardlink");
        let trash = "home/.local/share/Trash";
        add_item(&f, trash, "one", &"d".repeat(10_000));
        fs::hard_link(
            f.path().join(trash).join("files/one"),
            f.path().join(trash).join("files/two"),
        )
        .unwrap();
        f.write(&format!("{trash}/info/two.trashinfo"), TRASHINFO);
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 2);
        assert_eq!(info.shared_files, 1);
        assert_eq!(
            info.logical_bytes,
            10_000 + 2 * TRASHINFO.len() as u64,
            "the shared bytes are counted once"
        );
    }

    #[test]
    fn the_size_cache_is_not_believed() {
        let f = Fixture::new("bin-dirsizes");
        let trash = "home/.local/share/Trash";
        f.write(&format!("{trash}/files/d/x"), "x");
        f.write(&format!("{trash}/info/d.trashinfo"), TRASHINFO);
        let line = "999999999999 1700000000 d\n";
        f.write(&format!("{trash}/directorysizes"), line);
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert!(info.logical_bytes < 1000, "{}", info.logical_bytes);
        // The file itself is emptied with the bin, so it is counted.
        let without = {
            fs::remove_file(f.path().join(trash).join("directorysizes")).unwrap();
            measure(&f, &env_for(&f), &["vol"])
        };
        assert_eq!(
            info.logical_bytes,
            without.logical_bytes + line.len() as u64
        );
    }

    #[test]
    fn odd_names_are_handled() {
        let f = Fixture::new("bin-names");
        let trash = f.path().join("home/.local/share/Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        for raw in [
            b"new\nline".as_slice(),
            b"\xff\xfe bad utf8",
            b"  spaces  ",
            b"-dash",
            b".hidden",
            b"..dots",
            "ünï".as_bytes(),
        ] {
            let name = OsStr::from_bytes(raw);
            fs::write(trash.join("files").join(name), "x").unwrap();
            let mut info_name = raw.to_vec();
            info_name.extend_from_slice(b".trashinfo");
            fs::write(
                trash.join("info").join(OsStr::from_bytes(&info_name)),
                TRASHINFO,
            )
            .unwrap();
        }
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 7);
        assert!(info.problems.is_empty(), "{:?}", info.problems);
    }

    #[test]
    fn unreadable_entries_make_the_figures_incomplete() {
        // The superuser can read everything, so there is nothing to refuse.
        if walk::running_as_superuser() {
            eprintln!("skipped: running as the superuser");
            return;
        }
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let f = Fixture::new("bin-unreadable");
        let trash = "home/.local/share/Trash";
        add_item(&f, trash, "fine", "ok");
        f.write(&format!("{trash}/files/locked/inner"), "hidden");
        f.write(&format!("{trash}/info/locked.trashinfo"), TRASHINFO);
        let locked = f.path().join(trash).join("files/locked");
        let _restore = Restore(locked.clone());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert!(!info.complete);
        assert_eq!(
            info.problems,
            vec![BinProblem::Unreadable {
                path: locked,
                kind: io::ErrorKind::PermissionDenied,
            }]
        );
        // What could be read is still counted, and the folder itself.
        assert_eq!(info.items, 2);
        assert!(info.logical_bytes >= 2);
    }

    #[test]
    fn a_folder_on_another_filesystem_is_not_entered() {
        let f = Fixture::new("bin-dev");
        let trash = "home/.local/share/Trash";
        add_item(&f, trash, "plain", "p");
        f.write(&format!("{trash}/files/sub/inner.txt"), "inside");
        f.write(&format!("{trash}/info/sub.trashinfo"), TRASHINFO);
        let root = f.path().join(trash);
        let located = Located::new(&root, &[], LocationRole::HomeTrash);
        let fd = located.open().unwrap();
        let real_dev = walk::fstat(fd.as_raw_fd()).unwrap().dev;
        let run = |dev: u64| {
            let mut totals = Totals::default();
            let mut problems = Problems::default();
            let control = BinControl::new();
            let dir = DirStream::new(open_dir_at(fd.as_raw_fd(), c"files").unwrap()).unwrap();
            let mut walker = Walker {
                control: &control,
                dev,
                totals: &mut totals,
                problems: &mut problems,
                base: root.join("files"),
                stack: Vec::new(),
                stopped: false,
            };
            walker.walk(dir, 0, None);
            (totals.logical, problems)
        };
        let (all, problems) = run(real_dev);
        assert_eq!(all, 1 + 6);
        assert!(problems.list.is_empty());
        // Pretend the bin is on a different filesystem than its folders: every
        // folder is then "another filesystem" and is left alone.
        let (some, problems) = run(real_dev.wrapping_add(1));
        assert_eq!(some, 1, "only the plain file; the folder is skipped");
        assert_eq!(
            problems.list,
            vec![BinProblem::OtherFilesystem {
                path: root.join("files/sub")
            }]
        );
        assert!(problems.incomplete);
    }

    #[test]
    fn very_deep_folders_stop_at_a_limit_without_overflowing() {
        let f = Fixture::new("bin-deep");
        let trash = f.path().join("home/.local/share/Trash");
        let mut deep = trash.join("files");
        for _ in 0..(MAX_DEPTH + 100) {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("far.txt"), "far").unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert!(!info.complete);
        assert!(
            info.problems
                .iter()
                .any(|p| matches!(p, BinProblem::TooDeep { .. })),
            "{:?}",
            info.problems
        );
        // The names above the limit were counted.
        assert!(info.allocated_bytes.is_some_and(|b| b > 0));
    }

    #[test]
    fn a_cancelled_measurement_says_so_and_stops() {
        let f = Fixture::new("bin-cancel");
        let trash = "home/.local/share/Trash";
        for i in 0..50 {
            add_item(&f, trash, &format!("item{i}"), "data");
        }
        let control = BinControl::new();
        control.cancel();
        let tops = [made(&f, "vol")];
        let info = measure_with(&env_for(&f), &tops, &drive_id(), &control);
        assert!(!info.complete);
        assert!(info.problems.contains(&BinProblem::Cancelled));
        assert_eq!(info.items, 0, "nothing was counted after the stop");
        assert_eq!(control.progress().entries, 0);
    }

    #[test]
    fn progress_follows_the_count() {
        let f = Fixture::new("bin-progress");
        let trash = "home/.local/share/Trash";
        for i in 0..20 {
            add_item(&f, trash, &format!("item{i}"), "data");
        }
        let control = BinControl::new();
        let tops = [made(&f, "vol")];
        let info = measure_with(&env_for(&f), &tops, &drive_id(), &control);
        // Twenty files and twenty info files.
        assert_eq!(control.progress().entries, 40);
        assert_eq!(
            Some(control.progress().allocated_bytes),
            info.allocated_bytes
        );
    }

    #[test]
    fn a_thousand_items_are_counted_exactly() {
        let f = Fixture::new("bin-many");
        let trash = "home/.local/share/Trash";
        for i in 0..1000 {
            add_item(&f, trash, &format!("file-{i:04}.txt"), "0123456789");
        }
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert_eq!(info.items, 1000);
        assert_eq!(info.logical_bytes, 1000 * (10 + TRASHINFO.len() as u64));
        assert!(info.problems.is_empty());
    }

    #[test]
    fn the_home_trash_belongs_only_to_the_drive_that_holds_it() {
        // A second filesystem to put the drive on: /dev/shm is its own tmpfs
        // on systems that have it. Where it is missing or shares the device,
        // the case cannot be built and the test says so.
        let shm = Path::new("/dev/shm");
        let f = Fixture::new("bin-twofs");
        let Ok(shm_meta) = fs::metadata(shm) else {
            eprintln!("skipped: no /dev/shm");
            return;
        };
        if shm_meta.dev() == fs::metadata(f.path()).unwrap().dev() {
            eprintln!("skipped: /dev/shm is on the same filesystem as the temporary folder");
            return;
        }
        let other = shm.join(format!("bs-platform-bins-{}", std::process::id()));
        fs::create_dir_all(&other).unwrap();
        struct Remove(PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                // Only the folder created above, inside /dev/shm.
                if self.0.starts_with("/dev/shm")
                    && self
                        .0
                        .file_name()
                        .is_some_and(|n| n.as_bytes().starts_with(b"bs-platform-bins-"))
                {
                    let _ = fs::remove_dir_all(&self.0);
                }
            }
        }
        let _remove = Remove(other.clone());
        add_item(&f, "home/.local/share/Trash", "mine", "x");
        let env = env_for(&f);
        let here = measure_with(&env, &[made(&f, "vol")], &drive_id(), &BinControl::new());
        let there = measure_with(&env, &[other], &drive_id(), &BinControl::new());
        assert_eq!(here.items, 1);
        assert_eq!(here.locations[0].role, LocationRole::HomeTrash);
        assert!(there.locations.is_empty());
        assert_eq!(there.items, 0);
    }

    #[test]
    fn a_home_trash_that_is_a_link_is_reported_not_followed() {
        let f = Fixture::new("bin-homelink");
        add_item(&f, "bigdisk/Trash", "x", "elsewhere");
        f.symlink("../../../bigdisk/Trash", "home/.local/share/Trash");
        let info = measure(&f, &env_for(&f), &["vol"]);
        assert!(info.locations.is_empty());
        assert_eq!(
            info.problems,
            vec![BinProblem::NotFollowed {
                path: f.path().join("home/.local/share/Trash")
            }]
        );
        assert!(!info.complete);
        assert_eq!(info.logical_bytes, 0);
    }

    #[test]
    fn a_missing_mount_point_is_skipped() {
        let f = Fixture::new("bin-nomount");
        let info = measure(&f, &env_for(&f), &["does/not/exist"]);
        assert!(info.locations.is_empty());
        assert!(info.complete);
    }

    #[test]
    fn drives_with_several_mounts_use_the_uncovered_ones() {
        use crate::drives::{Drive, DriveKind, Mount};
        let mount = |p: PathBuf, covered: bool| Mount {
            mount_point: p,
            mount_id: Some(1),
            root: PathBuf::from("/"),
            covered_by_other_mount: covered,
        };
        let mut drive = Drive {
            id: drive_id(),
            mount_point: PathBuf::from("/a"),
            label: None,
            fs_type: String::new(),
            kind: DriveKind::Fixed,
            read_only: false,
            eligible_for_background_scan: false,
            capacity: None,
            capacity_problem: None,
            mounts: vec![
                mount(PathBuf::from("/a"), false),
                mount(PathBuf::from("/b"), true),
                mount(PathBuf::from("/c"), false),
            ],
        };
        assert_eq!(
            topdirs(&drive),
            vec![PathBuf::from("/a"), PathBuf::from("/c")]
        );
        drive.mounts.clear();
        assert_eq!(topdirs(&drive), vec![PathBuf::from("/a")]);
    }

    #[test]
    fn the_whole_drive_api_runs_on_a_worker() {
        use crate::drives::{Drive, DriveKind};
        let f = Fixture::new("bin-api");
        let uid = our_uid();
        add_item(&f, &format!("vol/.Trash-{uid}"), "a", "12345");
        let drive = Drive {
            id: drive_id(),
            mount_point: f.path().join("vol"),
            label: None,
            fs_type: String::new(),
            kind: DriveKind::Fixed,
            read_only: false,
            eligible_for_background_scan: false,
            capacity: None,
            capacity_problem: None,
            mounts: Vec::new(),
        };
        // `.Trash-uid` of the fixture is found; the user's own home Trash, if
        // it is on this filesystem, is another matter and is not asserted on.
        let located = super::super::locate(&drive);
        assert!(
            located
                .iter()
                .any(|l| l.path == f.path().join(format!("vol/.Trash-{uid}")))
        );
        let mut query = super::super::start(&drive);
        let info = query
            .wait_timeout(std::time::Duration::from_secs(60))
            .expect("finished");
        assert_eq!(info.drive, drive_id());
        assert!(info.items >= 1);
    }

    #[test]
    fn dropping_a_query_cancels_it_without_waiting() {
        use crate::drives::{Drive, DriveKind};
        let f = Fixture::new("bin-drop");
        let drive = Drive {
            id: drive_id(),
            mount_point: f.path().join("vol"),
            label: None,
            fs_type: String::new(),
            kind: DriveKind::Fixed,
            read_only: false,
            eligible_for_background_scan: false,
            capacity: None,
            capacity_problem: None,
            mounts: Vec::new(),
        };
        let query = super::super::start(&drive);
        drop(query);
    }
}
