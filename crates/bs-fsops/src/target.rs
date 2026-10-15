//! An item looked up and ready to be judged and deleted.

use crate::{DeleteError, Identity, OpenError};
use std::path::Path;

/// An item that was looked up without following a link at its last name.
///
/// [`Target::real_path`] is the place the item really is, taken from the
/// operating system's record of what was opened, so a link in the middle of
/// the path cannot hide where the item is. Judge that path, then call
/// [`Target::delete`]: the delete acts through what was opened here, and
/// refuses if the item has been replaced or moved since.
#[derive(Debug)]
pub struct Target {
    inner: imp::Inner,
}

impl Target {
    /// Looks the item up. Links in the folders on the way are followed (the
    /// caller named that place); the last name never is.
    pub fn open(path: &Path) -> Result<Target, OpenError> {
        imp::open(path).map(|inner| Target { inner })
    }

    /// Where the item really is. On Windows in plain form (`C:\...`, without
    /// the `\\?\` prefix) unless it has none.
    pub fn real_path(&self) -> &Path {
        self.inner.real_path()
    }

    /// What the item is, for comparing with what the caller saw earlier.
    pub fn identity(&self) -> Identity {
        self.inner.identity()
    }

    /// Whether the name is where another mount starts (Linux), so deleting it
    /// would reach into another filesystem.
    pub fn on_mount_point(&self) -> bool {
        self.inner.on_mount_point()
    }

    /// Deletes the item, and everything below it if it is a folder.
    pub fn delete(&self) -> Result<crate::Removed, DeleteError> {
        self.inner.delete()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::remove::{
        delete_named, fd_path, fstat, identity_of, name_of, open_dir_following, stat_at,
    };
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    #[derive(Debug)]
    pub struct Inner {
        parent: OwnedFd,
        name: CString,
        /// Where the folder really was when it was opened.
        parent_path: PathBuf,
        real: PathBuf,
        identity: Identity,
        on_mount_point: bool,
    }

    fn gone(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::NotFound
    }

    pub fn open(path: &Path) -> Result<Inner, OpenError> {
        let (Some(parent_path), Some(name)) = (path.parent(), name_of(path)) else {
            return Err(OpenError::Cannot(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path has no parent folder or no name",
            )));
        };
        let parent = open_dir_following(parent_path).map_err(|e| {
            if gone(&e) {
                OpenError::NotFound
            } else {
                OpenError::Cannot(e)
            }
        })?;
        let real_parent = fd_path(parent.as_raw_fd()).map_err(OpenError::Cannot)?;
        if !real_parent.is_absolute() {
            return Err(OpenError::Cannot(io::Error::other(
                "the folder has no absolute path",
            )));
        }
        let stat = stat_at(parent.as_raw_fd(), &name).map_err(|e| {
            if gone(&e) {
                OpenError::NotFound
            } else {
                OpenError::Cannot(e)
            }
        })?;
        let parent_stat = fstat(parent.as_raw_fd()).map_err(OpenError::Cannot)?;
        // A bind mount has its source's device number, so the mount ID is the
        // test for "this name is where another mount starts".
        let on_mount_point = parent_stat.dev != stat.dev
            || matches!((parent_stat.mnt, stat.mnt), (Some(a), Some(b)) if a != b);
        Ok(Inner {
            real: real_parent.join(OsStr::from_bytes(name.to_bytes())),
            identity: identity_of(&stat),
            on_mount_point,
            parent,
            name,
            parent_path: real_parent,
        })
    }

    impl Inner {
        pub fn real_path(&self) -> &Path {
            &self.real
        }
        pub fn identity(&self) -> Identity {
            self.identity
        }
        pub fn on_mount_point(&self) -> bool {
            self.on_mount_point
        }
        pub fn delete(&self) -> Result<crate::Removed, DeleteError> {
            // The checks were made for the place the folder was when it was
            // opened; if it has been moved since, they do not apply.
            match fd_path(self.parent.as_raw_fd()) {
                Ok(now) if now == self.parent_path => {}
                Ok(_) => return Err(DeleteError::Replaced),
                Err(error) => return Err(DeleteError::Look(error)),
            }
            delete_named(self.parent.as_raw_fd(), &self.name, self.identity)
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::win::{final_path, identity_of_handle, open_nofollow};
    use std::io;
    use std::path::PathBuf;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

    #[derive(Debug)]
    pub struct Inner {
        path: PathBuf,
        real: PathBuf,
        identity: Identity,
    }

    pub fn open(path: &Path) -> Result<Inner, OpenError> {
        let cannot = |e: io::Error| {
            if e.kind() == io::ErrorKind::NotFound {
                OpenError::NotFound
            } else {
                OpenError::Cannot(e)
            }
        };
        // Opened as itself: a link or junction is looked at, not followed.
        let handle = open_nofollow(path, FILE_READ_ATTRIBUTES).map_err(cannot)?;
        let identity = identity_of_handle(&handle).map_err(OpenError::Cannot)?;
        let real = final_path(&handle).map_err(OpenError::Cannot)?;
        Ok(Inner {
            path: path.to_path_buf(),
            real: PathBuf::from(real),
            identity,
        })
    }

    impl Inner {
        pub fn real_path(&self) -> &Path {
            &self.real
        }
        pub fn identity(&self) -> Identity {
            self.identity
        }
        pub fn on_mount_point(&self) -> bool {
            false
        }
        pub fn delete(&self) -> Result<crate::Removed, DeleteError> {
            crate::win::delete_item(&self.path, self.real.as_os_str(), self.identity)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;

    #[derive(Debug)]
    pub struct Inner;

    pub fn open(_path: &Path) -> Result<Inner, OpenError> {
        Err(OpenError::Cannot(std::io::Error::from(
            std::io::ErrorKind::Unsupported,
        )))
    }

    impl Inner {
        pub fn real_path(&self) -> &Path {
            Path::new("")
        }
        pub fn identity(&self) -> Identity {
            Identity { volume: 0, file: 0 }
        }
        pub fn on_mount_point(&self) -> bool {
            false
        }
        pub fn delete(&self) -> Result<crate::Removed, DeleteError> {
            Err(DeleteError::Look(std::io::Error::from(
                std::io::ErrorKind::Unsupported,
            )))
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::testutil::Fixture;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn an_item_is_found_with_its_real_place_and_identity() {
        let f = Fixture::new("target-open");
        f.write("a/b/file", "12345");
        let target = Target::open(&f.path().join("a/b/file")).unwrap();
        assert_eq!(target.real_path(), f.path().join("a/b/file"));
        let meta = fs::symlink_metadata(f.path().join("a/b/file")).unwrap();
        let identity = target.identity();
        assert_eq!(identity.volume, meta.dev());
        // The inode is in the low 64 bits.
        assert_eq!(identity.file & u128::from(u64::MAX), u128::from(meta.ino()));
        // The creation time, where the filesystem keeps it, is in the high 64.
        if let Ok(created) = meta.created() {
            let nanos = created
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            assert_eq!(identity.file >> 64, nanos);
        } else {
            assert_eq!(identity.file >> 64, 0);
        }
        assert!(!target.on_mount_point());
    }

    #[test]
    fn a_link_at_the_last_name_is_the_item_and_a_link_in_the_way_is_resolved() {
        let f = Fixture::new("target-links");
        f.write("real/file", "x");
        symlink(f.path().join("real"), f.path().join("alias")).unwrap();
        // The last name is not followed: the link is the item.
        let link = Target::open(&f.path().join("alias")).unwrap();
        assert_eq!(link.real_path(), f.path().join("alias"));
        assert_ne!(
            link.identity().file & u128::from(u64::MAX),
            u128::from(fs::metadata(f.path().join("real")).unwrap().ino())
        );
        // A link in the middle is followed, and the real place says so.
        let through = Target::open(&f.path().join("alias/file")).unwrap();
        assert_eq!(through.real_path(), f.path().join("real/file"));
    }

    #[test]
    fn a_missing_item_is_not_found() {
        let f = Fixture::new("target-missing");
        f.mkdir("a");
        assert!(matches!(
            Target::open(&f.path().join("a/nope")),
            Err(OpenError::NotFound)
        ));
        assert!(matches!(
            Target::open(&f.path().join("nope/file")),
            Err(OpenError::NotFound)
        ));
        assert!(matches!(
            Target::open(Path::new("/")),
            Err(OpenError::Cannot(_))
        ));
    }

    #[test]
    fn delete_removes_the_item_and_counts_it() {
        let f = Fixture::new("target-delete");
        f.write("d/x", "1");
        f.write("d/e/y", "22");
        let target = Target::open(&f.path().join("d")).unwrap();
        let removed = target.delete().unwrap();
        assert_eq!(removed.entries, 4);
        assert!(!f.path().join("d").exists());
        assert!(matches!(target.delete(), Err(DeleteError::NotFound)));
    }

    #[test]
    fn an_item_replaced_since_it_was_opened_is_left_alone() {
        let f = Fixture::new("target-replaced");
        f.write("work/file", "old");
        let target = Target::open(&f.path().join("work/file")).unwrap();
        // The replacement is made first, under another name, and renamed over
        // the original: removing the original first would let the filesystem
        // hand its inode to the new file, and the two would look the same.
        f.write("work/new", "new, and not what was judged");
        fs::rename(f.path().join("work/new"), f.path().join("work/file")).unwrap();
        assert!(matches!(target.delete(), Err(DeleteError::Replaced)));
        assert!(f.path().join("work/file").exists());
    }

    #[test]
    fn an_item_keeps_its_identity_when_it_is_changed_and_renamed_but_not_replaced() {
        let f = Fixture::new("target-stable");
        f.write("work/file", "old");
        let before = Target::open(&f.path().join("work/file"))
            .unwrap()
            .identity();
        // Writing, changing the mode and the times, and moving to another name
        // in the same folder do not make it another file.
        fs::write(f.path().join("work/file"), "much longer than before").unwrap();
        fs::set_permissions(
            f.path().join("work/file"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        fs::rename(f.path().join("work/file"), f.path().join("work/renamed")).unwrap();
        let after = Target::open(&f.path().join("work/renamed"))
            .unwrap()
            .identity();
        assert_eq!(before, after);
    }

    #[test]
    fn a_replacement_that_gets_the_same_inode_back_is_still_caught() {
        // Deleting a file and creating another usually hands the inode number
        // on, so the number alone cannot tell the two apart. Try until the
        // filesystem does so (it does at once on ext4); the creation time
        // then has to.
        let f = Fixture::new("target-reuse");
        let mut reused = false;
        for attempt in 0..40 {
            let name = format!("work/file{attempt}");
            f.write(&name, "old");
            let target = Target::open(&f.path().join(&name)).unwrap();
            let old = fs::symlink_metadata(f.path().join(&name)).unwrap();
            fs::remove_file(f.path().join(&name)).unwrap();
            // Filesystem clocks tick every few milliseconds.
            std::thread::sleep(std::time::Duration::from_millis(30));
            f.write(&name, "new, and not what was judged");
            let new = fs::symlink_metadata(f.path().join(&name)).unwrap();
            if new.ino() != old.ino() {
                continue;
            }
            reused = true;
            if old.created().is_err() {
                println!("NOT RUN: this filesystem does not report creation times");
                return;
            }
            assert!(matches!(target.delete(), Err(DeleteError::Replaced)));
            assert!(f.path().join(&name).exists());
            break;
        }
        if !reused {
            println!("NOT RUN: this filesystem did not hand an inode number on");
        }
    }

    #[test]
    fn a_folder_moved_since_it_was_opened_is_not_acted_in() {
        let f = Fixture::new("target-moved");
        f.write("here/work/file", "x");
        let target = Target::open(&f.path().join("here/work/file")).unwrap();
        // The folder that holds the item is moved somewhere else: what was
        // judged was the old place.
        fs::create_dir_all(f.path().join("there")).unwrap();
        fs::rename(f.path().join("here/work"), f.path().join("there/work")).unwrap();
        assert!(matches!(target.delete(), Err(DeleteError::Replaced)));
        assert!(f.path().join("there/work/file").exists());
    }

    #[test]
    fn a_link_swapped_in_for_the_folder_is_not_followed() {
        let f = Fixture::new("target-swapped");
        f.write("work/file", "x");
        f.write("elsewhere/file", "precious");
        let target = Target::open(&f.path().join("work/file")).unwrap();
        // The folder is replaced by a link to another folder, which also has a
        // file of that name. The delete holds the original folder.
        fs::rename(f.path().join("work"), f.path().join("work-away")).unwrap();
        symlink(f.path().join("elsewhere"), f.path().join("work")).unwrap();
        let _ = target.delete();
        assert!(f.path().join("elsewhere/file").exists());
    }
}
