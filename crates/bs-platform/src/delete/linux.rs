//! Trash, restore, empty and delete on Linux.

use super::fsys::{self, Boundary, Tally, fstat, stat_at};
use super::trashinfo;
use super::{
    Disposal, EmptyOutcome, Identity, OpError, Outcome, PermanentReason, RefuseReason,
    ResolveError, Resolved, RestoreOutcome, RestoreToken, TokenInner,
};
use crate::bins::linux::{Env, plan as bin_plan, topdirs};
use crate::bins::walk::{self, open_dir_at};
use crate::drives::Drive;
use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How many names are tried for one item before giving up.
const MAX_NAME_ATTEMPTS: u32 = 1000;

/// What a restore needs to know about an item in the Trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrashToken {
    /// The Trash folder (the one that holds `files/` and `info/`).
    root: PathBuf,
    /// The mount point, for a Trash on a volume: its `.trashinfo` paths are
    /// relative to it.
    topdir: Option<PathBuf>,
    /// The name in `files/`.
    name: Vec<u8>,
    identity: Identity,
}

impl TrashToken {
    #[cfg(test)]
    pub(crate) fn root_for_tests(&self) -> &Path {
        &self.root
    }
}

/// The folder that holds an item, opened once when the item is looked up and
/// kept open until the item is acted on, so the action reaches the folder that
/// was judged and not whatever the path names a moment later.
#[derive(Clone, Debug)]
pub(crate) struct Held {
    parent: Arc<OwnedFd>,
    name: CString,
    /// Where the folder really was when it was opened.
    parent_path: PathBuf,
}

impl Held {
    /// Fails if the folder has been moved since it was opened: the checks were
    /// made for the place it was then.
    fn still_in_place(&self) -> Result<(), Failure> {
        match fsys::fd_path(self.parent.as_raw_fd()) {
            Ok(now) if now == self.parent_path => Ok(()),
            Ok(_) => Err(Failure::Refuse(RefuseReason::ChangedSinceScan)),
            Err(error) => Err(opfail("look at the folder", &error)),
        }
    }
}

pub(crate) fn identity_of(path: &Path) -> io::Result<Identity> {
    let (parent, name, _) = open_parent(path)?;
    let stat = stat_at(parent.as_raw_fd(), &name)?;
    Ok(identity(&stat))
}

fn identity(stat: &walk::Stat) -> Identity {
    Identity {
        volume: stat.dev,
        file: u128::from(stat.ino),
    }
}

/// The folder that holds the item, the item's name in it, and the place the
/// folder really is. Links in the folder's own path are followed (the caller
/// named that place); the item's name never is.
fn open_parent(path: &Path) -> io::Result<(OwnedFd, CString, PathBuf)> {
    let (Some(parent), Some(name)) = (path.parent(), fsys::name_of(path)) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no parent"));
    };
    let fd = fsys::open_dir_following(parent)?;
    let real = fsys::fd_path(fd.as_raw_fd())?;
    Ok((fd, name, real))
}

fn opfail(step: &'static str, error: &io::Error) -> Failure {
    Failure::Fail(OpError::io(step, error))
}

/// Why an item was not moved or deleted.
pub(crate) enum Failure {
    Refuse(RefuseReason),
    Permanent(PermanentReason),
    Fail(OpError),
}

impl From<Failure> for Outcome {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Refuse(reason) => Outcome::Refused { reason },
            Failure::Permanent(reason) => Outcome::WouldBePermanent { reason },
            Failure::Fail(error) => Outcome::Failed { error },
        }
    }
}

/// The highest folder above `start` that is on the device `dev`: the mount
/// point of the filesystem the item is on.
pub(crate) fn topdir_of(start: &Path, dev: u64) -> PathBuf {
    let mut top = start.to_path_buf();
    while let Some(up) = top.parent() {
        match std::fs::metadata(up) {
            Ok(meta) if meta.dev() == dev => top = up.to_path_buf(),
            _ => break,
        }
    }
    top
}

/// A Trash folder that can take an item.
pub(crate) struct Target {
    root: OwnedFd,
    files: OwnedFd,
    info: OwnedFd,
    root_path: PathBuf,
    /// `Some` for a Trash on a volume.
    topdir: Option<PathBuf>,
}

pub(crate) enum Probe {
    Found(Target),
    /// Nothing is there yet, but it can be created.
    WouldCreate,
    Unavailable,
}

/// One folder on the way to a Trash.
enum Step {
    Found(OwnedFd),
    WouldCreate,
    Unusable,
}

/// A real folder of the user's inside `parent`, made if it is missing and
/// `create` is set. A link, a file or somebody else's folder is unusable.
fn user_dir(parent: RawFd, name: &CString, uid: u32, create: bool) -> io::Result<Step> {
    for _ in 0..2 {
        match stat_at(parent, name) {
            Ok(stat) => {
                if stat.is_symlink() || !stat.is_dir() || stat.uid != uid {
                    return Ok(Step::Unusable);
                }
                let fd = open_dir_at(parent, name)?;
                let opened = fstat(fd.as_raw_fd())?;
                return Ok(if opened.dev == stat.dev && opened.ino == stat.ino {
                    Step::Found(fd)
                } else {
                    Step::Unusable
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !fsys::can_create_in(parent) {
                    return Ok(Step::Unusable);
                }
                if !create {
                    return Ok(Step::WouldCreate);
                }
                match fsys::mkdir_at(parent, name, 0o700) {
                    Ok(()) => {}
                    // Someone else made it first: look again.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Step::Unusable)
}

/// The data folder that holds the home Trash, as far as it matters to an item
/// on device `dev`.
enum Home {
    /// The folder exists and is on the item's filesystem.
    Open(OwnedFd),
    /// It does not exist yet but can be made on the item's filesystem.
    WouldCreate,
    /// It is on another filesystem, or cannot be used.
    Elsewhere,
}

/// Opens the data folder. When it is missing it is made (private to the user,
/// as the specification asks) if the nearest folder that exists is on the
/// item's filesystem and can be written to.
fn home_data(data: &Path, dev: u64, create: bool) -> io::Result<Home> {
    match fsys::open_dir_following(data) {
        Ok(fd) => {
            return Ok(if fstat(fd.as_raw_fd())?.dev == dev {
                Home::Open(fd)
            } else {
                Home::Elsewhere
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Ok(Home::Elsewhere),
    }
    let mut ancestor = data.parent();
    while let Some(candidate) = ancestor {
        match fsys::open_dir_following(candidate) {
            Ok(fd) => {
                if fstat(fd.as_raw_fd())?.dev != dev || !fsys::can_create_in(fd.as_raw_fd()) {
                    return Ok(Home::Elsewhere);
                }
                if !create {
                    return Ok(Home::WouldCreate);
                }
                std::os::unix::fs::DirBuilderExt::mode(
                    std::fs::DirBuilder::new().recursive(true),
                    0o700,
                )
                .create(data)?;
                let fd = fsys::open_dir_following(data)?;
                return Ok(if fstat(fd.as_raw_fd())?.dev == dev {
                    Home::Open(fd)
                } else {
                    Home::Elsewhere
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => ancestor = candidate.parent(),
            Err(_) => return Ok(Home::Elsewhere),
        }
    }
    Ok(Home::Elsewhere)
}

/// The Trash folder for an item on device `dev`, whose volume is mounted at
/// `topdir`.
///
/// The home Trash serves the filesystem that holds the data folder. Any other
/// filesystem uses `$topdir/.Trash/$uid` when `.Trash` is a real folder with
/// the sticky bit, and `$topdir/.Trash-$uid` otherwise.
pub(crate) fn locate(env: &Env, dev: u64, topdir: &Path, create: bool) -> io::Result<Probe> {
    let uid_name = CString::new(env.uid.to_string()).unwrap_or_default();
    if let Some(data) = &env.data_home {
        match home_data(data, dev, create)? {
            Home::Open(data_fd) => {
                return finish(
                    user_dir(
                        data_fd.as_raw_fd(),
                        &CString::from(c"Trash"),
                        env.uid,
                        create,
                    )?,
                    data.join("Trash"),
                    None,
                    env.uid,
                    create,
                );
            }
            Home::WouldCreate => return Ok(Probe::WouldCreate),
            Home::Elsewhere => {}
        }
    }

    let top_fd = fsys::open_dir_following(topdir)?;
    if fstat(top_fd.as_raw_fd())?.dev != dev {
        return Ok(Probe::Unavailable);
    }
    // $topdir/.Trash/$uid, only when .Trash passes the specification's checks.
    let shared = CString::from(c".Trash");
    match stat_at(top_fd.as_raw_fd(), &shared) {
        Ok(stat) if stat.is_dir() && !stat.is_symlink() && stat.is_sticky() => {
            let shared_fd = open_dir_at(top_fd.as_raw_fd(), &shared)?;
            // The folder inside it is the user's own.
            let step = user_dir(shared_fd.as_raw_fd(), &uid_name, env.uid, create)?;
            if !matches!(step, Step::Unusable) {
                return finish(
                    step,
                    topdir.join(".Trash").join(uid_name.to_str().unwrap_or("0")),
                    Some(topdir.to_path_buf()),
                    env.uid,
                    create,
                );
            }
        }
        Ok(_) | Err(_) => {}
    }
    let private = CString::new(format!(".Trash-{}", env.uid)).unwrap_or_default();
    finish(
        user_dir(top_fd.as_raw_fd(), &private, env.uid, create)?,
        topdir.join(private.to_str().unwrap_or(".Trash")),
        Some(topdir.to_path_buf()),
        env.uid,
        create,
    )
}

/// Opens `files/` and `info/` inside a Trash folder.
fn finish(
    step: Step,
    root_path: PathBuf,
    topdir: Option<PathBuf>,
    uid: u32,
    create: bool,
) -> io::Result<Probe> {
    let root = match step {
        Step::Found(fd) => fd,
        Step::WouldCreate => return Ok(Probe::WouldCreate),
        Step::Unusable => return Ok(Probe::Unavailable),
    };
    let mut halves = Vec::new();
    for name in [c"files", c"info"] {
        match user_dir(root.as_raw_fd(), &CString::from(name), uid, create)? {
            Step::Found(fd) => halves.push(fd),
            Step::WouldCreate => return Ok(Probe::WouldCreate),
            Step::Unusable => return Ok(Probe::Unavailable),
        }
    }
    let info = halves.pop().ok_or_else(|| io::Error::other("no info"))?;
    let files = halves.pop().ok_or_else(|| io::Error::other("no files"))?;
    Ok(Probe::Found(Target {
        root,
        files,
        info,
        root_path,
        topdir,
    }))
}

impl Disposal {
    pub(crate) fn resolve(&self, path: &Path) -> Result<Resolved, ResolveError> {
        let (parent, name, real_parent) = open_parent(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ResolveError::NotFound
            } else {
                ResolveError::CannotResolve
            }
        })?;
        let stat = match stat_at(parent.as_raw_fd(), &name) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ResolveError::NotFound);
            }
            Err(_) => return Err(ResolveError::CannotResolve),
        };
        let parent_stat = fstat(parent.as_raw_fd()).map_err(|_| ResolveError::CannotResolve)?;
        if !real_parent.is_absolute() {
            return Err(ResolveError::CannotResolve);
        }
        // A bind mount has its source's device number, so the mount ID is the
        // test for "this name is where another mount starts".
        let on_mount_point = parent_stat.dev != stat.dev
            || matches!((parent_stat.mnt, stat.mnt), (Some(a), Some(b)) if a != b);
        Ok(Resolved {
            real: real_parent.join(std::ffi::OsStr::from_bytes(name.to_bytes())),
            identity: identity(&stat),
            on_mount_point,
            held: Held {
                parent: Arc::new(parent),
                name,
                parent_path: real_parent,
            },
        })
    }

    fn topdir_for(&self, real: &Path, dev: u64) -> PathBuf {
        if let Some(top) = &self.topdir_override {
            return top.clone();
        }
        topdir_of(real.parent().unwrap_or(real), dev)
    }

    /// Whether the Trash can take the item, without making anything.
    pub(crate) fn bin_objection(&self, _path: &Path, found: &Resolved) -> Option<PermanentReason> {
        let dev = found.identity.volume;
        let top = self.topdir_for(&found.real, dev);
        match locate(&self.trash_env, dev, &top, false) {
            Ok(Probe::Found(_) | Probe::WouldCreate) => None,
            Ok(Probe::Unavailable) | Err(_) => Some(PermanentReason::NoTrashOnVolume),
        }
    }

    pub(crate) fn trash_one(&self, item: &super::Item, now: &Resolved) -> Outcome {
        match self.try_trash(item, now) {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }

    fn try_trash(&self, _item: &super::Item, now: &Resolved) -> Result<Outcome, Failure> {
        // Act through the folder that was checked, not through the path again.
        now.held.still_in_place()?;
        let parent_fd = now.held.parent.as_raw_fd();
        let name = &now.held.name;
        let stat = stat_at(parent_fd, name).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Failure::Refuse(RefuseReason::NotFound),
            _ => opfail("look at the item", &e),
        })?;
        if identity(&stat) != now.identity {
            return Err(Failure::Refuse(RefuseReason::ChangedSinceScan));
        }
        let dev = stat.dev;
        let real = now
            .held
            .parent_path
            .join(std::ffi::OsStr::from_bytes(name.to_bytes()));

        let top = self.topdir_for(&real, dev);
        let target = match locate(&self.trash_env, dev, &top, true)
            .map_err(|e| opfail("find the Trash", &e))?
        {
            Probe::Found(target) => target,
            Probe::WouldCreate | Probe::Unavailable => {
                return Err(Failure::Permanent(PermanentReason::NoTrashOnVolume));
            }
        };

        // What the record says: the full path for the home Trash, the path
        // below the volume's mount point for a volume's.
        let recorded: Vec<u8> = match &target.topdir {
            None => real.as_os_str().as_bytes().to_vec(),
            Some(top) => real
                .strip_prefix(top)
                .map_err(|_| Failure::Refuse(RefuseReason::CannotResolve))?
                .as_os_str()
                .as_bytes()
                .to_vec(),
        };

        // Measured before the move: after it the item is out of reach of the
        // folder it came from.
        let mut tally = Tally::default();
        fsys::measure_entry(parent_fd, name, Boundary::of(&stat), 0, &mut tally);
        tally.finish();
        let bytes = tally.allocated;
        let shared = tally.kept_by_other_names;

        let base = name.to_bytes();
        let mut placed: Option<Vec<u8>> = None;
        for attempt in 1..=MAX_NAME_ATTEMPTS {
            let unique = trashinfo::candidate_name(base, attempt);
            let mut info_name = unique.clone();
            info_name.extend_from_slice(b".trashinfo");
            let info_c =
                fsys::cstring(info_name.clone()).map_err(|e| opfail("name the record", &e))?;
            let unique_c =
                fsys::cstring(unique.clone()).map_err(|e| opfail("name the item", &e))?;

            // The record comes first and is created exclusively, so two
            // programs never claim the same name.
            let mut record = match fsys::create_new_at(target.info.as_raw_fd(), &info_c, 0o600) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(opfail("create the record", &error)),
            };
            let text = trashinfo::format_info(&recorded, &fsys::local_date_now());
            if let Err(error) = record
                .write_all(text.as_bytes())
                .and_then(|()| record.flush())
            {
                let _ = fsys::unlink_at(target.info.as_raw_fd(), &info_c, false);
                return Err(opfail("write the record", &error));
            }
            drop(record);

            // An entry that is not ours already holds the name.
            if stat_at(target.files.as_raw_fd(), &unique_c).is_ok() {
                let _ = fsys::unlink_at(target.info.as_raw_fd(), &info_c, false);
                continue;
            }
            match fsys::rename_noreplace(parent_fd, name, target.files.as_raw_fd(), &unique_c) {
                Ok(()) => {
                    placed = Some(unique);
                    break;
                }
                Err(error) => {
                    let _ = fsys::unlink_at(target.info.as_raw_fd(), &info_c, false);
                    match error.raw_os_error() {
                        Some(libc::EEXIST | libc::ENOTEMPTY) => continue,
                        Some(libc::EXDEV) => {
                            return Err(Failure::Permanent(PermanentReason::CrossesFilesystem));
                        }
                        _ if error.kind() == io::ErrorKind::NotFound => {
                            return Err(Failure::Refuse(RefuseReason::NotFound));
                        }
                        _ => return Err(opfail("move to the Trash", &error)),
                    }
                }
            }
        }
        let Some(unique) = placed else {
            return Err(Failure::Fail(OpError {
                step: "find a free name in the Trash",
                kind: io::ErrorKind::AlreadyExists,
                code: None,
                removed_bytes: 0,
                removed_entries: 0,
            }));
        };
        let unique_c = fsys::cstring(unique.clone()).map_err(|e| opfail("name the item", &e))?;
        let mut info_name = unique.clone();
        info_name.extend_from_slice(b".trashinfo");
        let info_c = fsys::cstring(info_name).map_err(|e| opfail("name the record", &e))?;

        // The rename moved whatever the name held at that instant. If that is
        // not what was checked, put it back.
        let moved = stat_at(target.files.as_raw_fd(), &unique_c)
            .map_err(|e| opfail("check the moved item", &e))?;
        if identity(&moved) != now.identity {
            let back = fsys::rename_noreplace(target.files.as_raw_fd(), &unique_c, parent_fd, name);
            let _ = fsys::unlink_at(target.info.as_raw_fd(), &info_c, false);
            return Err(match back {
                Ok(()) => Failure::Refuse(RefuseReason::ChangedSinceScan),
                Err(error) => opfail("put back an item that was replaced during the move", &error),
            });
        }

        if moved.is_dir() {
            // The size cache is optional; a failure to update it does not undo
            // the move.
            let mtime = fsys::open_file_at(target.info.as_raw_fd(), &info_c)
                .map_or_else(|_| fsys::now_secs(), |f| fsys::mtime_secs(&f));
            let _ = update_sizes(&target, |old| {
                trashinfo::sizes_with(old, bytes, mtime, &unique)
            });
        }

        Ok(Outcome::MovedToBin {
            token: RestoreToken {
                original: real,
                inner: TokenInner::Trash(TrashToken {
                    root: target.root_path.clone(),
                    topdir: target.topdir.clone(),
                    name: unique,
                    identity: now.identity,
                }),
            },
            bytes,
            basis: super::ByteBasis::Allocated,
            shared_files: shared,
        })
    }

    pub(crate) fn delete_one(&self, _item: &super::Item, now: &Resolved) -> Outcome {
        // Act through the folder that was checked, not through the path again.
        if let Err(failure) = now.held.still_in_place() {
            return failure.into();
        }
        let parent = &now.held.parent;
        let name = &now.held.name;
        let stat = match stat_at(parent.as_raw_fd(), name) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Outcome::Refused {
                    reason: RefuseReason::NotFound,
                };
            }
            Err(error) => {
                return Outcome::Failed {
                    error: OpError::io("look at the item", &error),
                };
            }
        };
        if identity(&stat) != now.identity {
            return Outcome::Refused {
                reason: RefuseReason::ChangedSinceScan,
            };
        }
        let mut tally = Tally::default();
        fsys::remove_entry(parent.as_raw_fd(), name, Boundary::of(&stat), 0, &mut tally);
        tally.finish();
        match tally.first_error.take() {
            None => Outcome::Deleted {
                bytes: tally.allocated,
                shared_files: tally.kept_by_other_names,
            },
            Some(error) => Outcome::Failed {
                error: OpError {
                    removed_bytes: tally.allocated,
                    removed_entries: tally.entries,
                    ..OpError::io("delete", &error)
                },
            },
        }
    }

    pub(crate) fn restore_one(&self, token: &RestoreToken) -> RestoreOutcome {
        let TokenInner::Trash(trash) = &token.inner else {
            return RestoreOutcome::Refused {
                reason: RefuseReason::NotInBin,
            };
        };
        match self.try_restore(token, trash) {
            Ok(path) => RestoreOutcome::Restored { path },
            Err(Failure::Refuse(reason)) => RestoreOutcome::Refused { reason },
            Err(Failure::Permanent(_)) => RestoreOutcome::Refused {
                reason: RefuseReason::NotInBin,
            },
            Err(Failure::Fail(error)) => RestoreOutcome::Failed { error },
        }
    }

    fn try_restore(&self, token: &RestoreToken, trash: &TrashToken) -> Result<PathBuf, Failure> {
        let missing = |e: io::Error, step: &'static str| {
            if e.kind() == io::ErrorKind::NotFound {
                Failure::Refuse(RefuseReason::NotInBin)
            } else {
                opfail(step, &e)
            }
        };
        let root =
            fsys::open_dir_following(&trash.root).map_err(|e| missing(e, "open the Trash"))?;
        let root_stat = fstat(root.as_raw_fd()).map_err(|e| opfail("open the Trash", &e))?;
        if root_stat.uid != self.trash_env.uid {
            return Err(Failure::Refuse(RefuseReason::NotInBin));
        }
        let files = open_dir_at(root.as_raw_fd(), &CString::from(c"files"))
            .map_err(|e| missing(e, "open the Trash"))?;
        let info = open_dir_at(root.as_raw_fd(), &CString::from(c"info"))
            .map_err(|e| missing(e, "open the Trash"))?;
        let unique = fsys::cstring(trash.name.clone()).map_err(|e| opfail("name the item", &e))?;
        let mut info_name = trash.name.clone();
        info_name.extend_from_slice(b".trashinfo");
        let info_c = fsys::cstring(info_name).map_err(|e| opfail("name the record", &e))?;

        // The record must say what the token says: the token alone is not
        // allowed to point at an arbitrary place.
        let record = fsys::open_file_at(info.as_raw_fd(), &info_c)
            .and_then(|f| fsys::read_limited(f, trashinfo::MAX_INFO_BYTES))
            .map_err(|e| missing(e, "read the record"))?
            .ok_or(Failure::Refuse(RefuseReason::NotInBin))?;
        let recorded = trashinfo::parse_info_path(&record)
            .map(fsys::path_from)
            .ok_or(Failure::Refuse(RefuseReason::NotInBin))?;
        let recorded = match &trash.topdir {
            Some(top) if recorded.is_relative() => top.join(recorded),
            _ => recorded,
        };
        if recorded != token.original {
            return Err(Failure::Refuse(RefuseReason::NotInBin));
        }

        let stat =
            stat_at(files.as_raw_fd(), &unique).map_err(|e| missing(e, "look at the item"))?;
        if identity(&stat) != trash.identity {
            return Err(Failure::Refuse(RefuseReason::NotInBin));
        }

        let (Some(parent_path), Some(name)) =
            (token.original.parent(), fsys::name_of(&token.original))
        else {
            return Err(Failure::Refuse(RefuseReason::NotInBin));
        };
        let parent = fsys::open_dir_following(parent_path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                Failure::Refuse(RefuseReason::OriginalFolderMissing)
            } else {
                opfail("open the original folder", &e)
            }
        })?;
        if stat_at(parent.as_raw_fd(), &name).is_ok() {
            return Err(Failure::Refuse(RefuseReason::OriginalExists));
        }
        match fsys::rename_noreplace(files.as_raw_fd(), &unique, parent.as_raw_fd(), &name) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                return Err(Failure::Refuse(RefuseReason::OriginalExists));
            }
            Err(error) => return Err(opfail("move back", &error)),
        }
        let _ = fsys::unlink_at(info.as_raw_fd(), &info_c, false);
        if stat.is_dir() {
            let target = Target {
                root,
                files,
                info,
                root_path: trash.root.clone(),
                topdir: trash.topdir.clone(),
            };
            let _ = update_sizes(&target, |old| trashinfo::sizes_without(old, &trash.name));
        }
        Ok(token.original.clone())
    }

    pub(crate) fn empty_one(&self, drive: &Drive) -> EmptyOutcome {
        let topdirs = topdirs(drive);
        let located = bin_plan(&self.trash_env, &topdirs).located;
        if located
            .iter()
            .any(|p| !self.within_restriction(&p.location.path))
        {
            return EmptyOutcome::Refused {
                reason: RefuseReason::OutsideRestriction,
            };
        }
        let mut tally = Tally::default();
        for place in &located {
            let root = match place.open() {
                Ok(fd) => fd,
                Err(error) => {
                    tally.fail(error);
                    continue;
                }
            };
            let bound = match fstat(root.as_raw_fd()) {
                Ok(stat) => Boundary::of(&stat),
                Err(error) => {
                    tally.fail(error);
                    continue;
                }
            };
            for folder in [c"files", c"info"] {
                let folder = CString::from(folder);
                match open_dir_at(root.as_raw_fd(), &folder) {
                    Ok(dir) => empty_folder(dir, bound, &mut tally),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => tally.fail(error),
                }
            }
            // The size cache goes with the items it describes.
            match fsys::unlink_at(root.as_raw_fd(), &CString::from(c"directorysizes"), false) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => tally.fail(error),
            }
        }
        tally.finish();
        match tally.first_error.take() {
            None => EmptyOutcome::Emptied {
                bytes: tally.allocated,
                entries: tally.entries,
            },
            Some(error) => EmptyOutcome::Partial {
                bytes: tally.allocated,
                entries: tally.entries,
                error: OpError::io("empty the Trash", &error),
            },
        }
    }
}

/// Removes everything inside a folder, keeping the folder.
fn empty_folder(dir: OwnedFd, bound: Boundary, tally: &mut Tally) {
    let mut stream = match walk::DirStream::new(dir) {
        Ok(stream) => stream,
        Err(error) => return tally.fail(error),
    };
    let mut names = Vec::new();
    loop {
        match stream.next_name() {
            Ok(Some(name)) => names.push(name.to_owned()),
            Ok(None) => break,
            Err(error) => {
                tally.fail(error);
                break;
            }
        }
    }
    let fd = stream.fd();
    for name in &names {
        fsys::remove_entry(fd, name, bound, 0, tally);
    }
}

/// Rewrites the size cache of a Trash folder: reads it, applies `change`, and
/// puts the result in place with a rename, so a reader never sees half of it.
fn update_sizes(target: &Target, change: impl FnOnce(&[u8]) -> Vec<u8>) -> io::Result<()> {
    let root = target.root.as_raw_fd();
    let name = CString::from(c"directorysizes");
    let old = match fsys::open_file_at(root, &name) {
        Ok(file) => fsys::read_limited(file, trashinfo::MAX_SIZES_BYTES)?
            .ok_or_else(|| io::Error::other("directorysizes is too large"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let new = change(&old);
    let temp = CString::new(format!("directorysizes.bytescape-{}", std::process::id()))
        .map_err(io::Error::other)?;
    let _ = fsys::unlink_at(root, &temp, false);
    let mut file = fsys::create_new_at(root, &temp, 0o600)?;
    let written = file.write_all(&new).and_then(|()| file.flush());
    drop(file);
    if let Err(error) = written {
        let _ = fsys::unlink_at(root, &temp, false);
        return Err(error);
    }
    // A plain rename replaces the old cache in one step.
    if let Err(error) = fsys::rename_replace(root, &temp, &name) {
        let _ = fsys::unlink_at(root, &temp, false);
        return Err(error);
    }
    Ok(())
}
