//! Tests for moving to the Trash, restoring, emptying and deleting on Linux.
//!
//! Every test works inside a fixture folder made for it. Each [`Lab`] limits
//! the service to that folder, points the Trash at a data folder inside it,
//! and gives the protected-path table a home folder and a drive root that are
//! also inside it, so the rules can be exercised without touching the system.
//! Nothing here reads or changes the real Trash.

#![allow(unsafe_code)]

use super::*;
use crate::bins::walk::current_uid;
use crate::drives::{DriveId, DriveKind, Mount};
use crate::testutil::Fixture;
use bs_knowledge::Host;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

struct Lab {
    fx: Fixture,
    uid: u32,
}

impl Lab {
    fn new(label: &str) -> Self {
        let fx = Fixture::new(label);
        let lab = Self {
            fx,
            uid: current_uid(),
        };
        lab.mkdir("data");
        lab.mkdir("work");
        lab
    }

    /// A path inside the fixture. Every change a test makes goes through this.
    fn p(&self, relative: &str) -> PathBuf {
        assert!(
            !relative.starts_with('/') && !relative.split('/').any(|part| part == ".."),
            "{relative}"
        );
        self.fx.path().join(relative)
    }

    fn mkdir(&self, relative: &str) {
        fs::create_dir_all(self.p(relative)).unwrap();
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.p(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.p(relative)).unwrap()
    }

    fn exists(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.p(relative)).is_ok()
    }

    fn knowledge(&self) -> Knowledge {
        // The home folder and a drive root of the fixture stand in for the
        // system's: the first is protected, the second blocked.
        let env = Environment::new()
            .with(
                "HOME",
                self.p("protected-home").as_os_str().as_encoded_bytes(),
            )
            .with_drive_roots([self.p("blocked-root")]);
        Knowledge::for_host(&env, Host::Linux)
    }

    /// The service with the home Trash in the fixture's data folder.
    fn home(&self) -> Disposal {
        Disposal::new(self.knowledge(), Vec::new())
            .restricted_to(self.fx.path())
            .with_trash_env(self.uid, Some(self.p("data")))
            // The home Trash decides for items beside it; this stands in for the
            // volume's mount point in case it does not, so that no test can reach
            // a real mount point.
            .with_topdir(self.fx.path())
    }

    /// The service with no home Trash, and `vol` standing in for the mount
    /// point of every item's volume.
    fn volume(&self, vol: &str) -> Disposal {
        Disposal::new(self.knowledge(), Vec::new())
            .restricted_to(self.fx.path())
            .with_trash_env(self.uid, None)
            .with_topdir(self.p(vol))
    }
}

fn item(lab: &Lab, relative: &str) -> Item {
    let path = lab.p(relative);
    let seen = identity_of(&path).unwrap();
    Item::new(path).seen(seen)
}

fn only(mut outcomes: Vec<Outcome>) -> Outcome {
    assert_eq!(outcomes.len(), 1);
    outcomes.remove(0)
}

fn moved(outcome: Outcome) -> (RestoreToken, u64, u64) {
    match outcome {
        Outcome::MovedToBin {
            token,
            bytes,
            basis,
            shared_files,
        } => {
            assert_eq!(basis, ByteBasis::Allocated);
            (token, bytes, shared_files)
        }
        other => panic!("expected MovedToBin, got {other:?}"),
    }
}

fn allocated(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

fn refused(outcome: &Outcome) -> &RefuseReason {
    match outcome {
        Outcome::Refused { reason } => reason,
        other => panic!("expected Refused, got {other:?}"),
    }
}

fn bytes_of(len: usize) -> String {
    "x".repeat(len)
}

// ---------------------------------------------------------------- Trash

#[test]
fn a_file_goes_to_the_home_trash_and_comes_back() {
    let lab = Lab::new("trash-roundtrip");
    lab.write("work/report.txt", &bytes_of(5000));
    let d = lab.home();
    let expected_bytes = allocated(&lab.p("work/report.txt"));
    let real = fs::canonicalize(lab.p("work/report.txt")).unwrap();
    let before = fs::metadata(&real).unwrap();

    let (token, bytes, shared) = moved(only(d.trash(&[item(&lab, "work/report.txt")], None)));
    assert_eq!(bytes, expected_bytes);
    assert_eq!(shared, 0);
    assert_eq!(token.original_path(), lab.p("work/report.txt"));
    assert!(!lab.exists("work/report.txt"));
    assert_eq!(lab.read("data/Trash/files/report.txt"), bytes_of(5000));

    // The record names the original place, as the specification asks.
    let info = lab.read("data/Trash/info/report.txt.trashinfo");
    assert!(info.starts_with("[Trash Info]\nPath="), "{info}");
    let line = info.lines().find(|l| l.starts_with("Path=")).unwrap();
    assert_eq!(
        trashinfo::decode(line.trim_start_matches("Path=").as_bytes()).unwrap(),
        lab.p("work/report.txt").as_os_str().as_encoded_bytes()
    );
    let date = info
        .lines()
        .find_map(|l| l.strip_prefix("DeletionDate="))
        .unwrap();
    assert_eq!(date.len(), 19, "{date}");
    assert_eq!(&date[10..11], "T");
    // Private to the user, as the specification asks.
    let mode = fs::metadata(lab.p("data/Trash"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);

    // The result is the real place the item is at, with links resolved.
    let RestoreOutcome::Restored { path } = d.restore(&token) else {
        panic!("expected the file to be restored");
    };
    assert_eq!(path, real);
    let after = fs::metadata(&path).unwrap();
    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    assert_eq!(fs::read_to_string(&path).unwrap(), bytes_of(5000));
    assert_eq!(lab.read("work/report.txt"), bytes_of(5000));
    assert!(!lab.exists("data/Trash/files/report.txt"));
    assert!(!lab.exists("data/Trash/info/report.txt.trashinfo"));
}

#[test]
fn a_folder_is_moved_whole_and_the_size_cache_follows() {
    let lab = Lab::new("trash-folder");
    lab.write("work/my dir/a.txt", &bytes_of(9000));
    lab.write("work/my dir/sub/b.txt", "b");
    let d = lab.home();
    let expected = allocated(&lab.p("work/my dir"))
        + allocated(&lab.p("work/my dir/a.txt"))
        + allocated(&lab.p("work/my dir/sub"))
        + allocated(&lab.p("work/my dir/sub/b.txt"));

    let (token, bytes, _) = moved(only(d.trash(&[item(&lab, "work/my dir")], None)));
    assert_eq!(bytes, expected);
    assert!(!lab.exists("work/my dir"));
    assert_eq!(lab.read("data/Trash/files/my dir/sub/b.txt"), "b");
    let sizes = lab.read("data/Trash/directorysizes");
    let fields: Vec<&str> = sizes.trim_end().split(' ').collect();
    assert_eq!(fields.len(), 3, "{sizes}");
    assert_eq!(fields[0], expected.to_string());
    assert_eq!(fields[2], "my%20dir");

    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
    assert_eq!(lab.read("work/my dir/sub/b.txt"), "b");
    assert_eq!(lab.read("data/Trash/directorysizes"), "");
}

#[test]
fn two_items_with_one_name_get_two_names_in_the_trash() {
    let lab = Lab::new("trash-names");
    lab.write("work/one/same.txt", "first");
    lab.write("work/two/same.txt", "second");
    let d = lab.home();
    let outcomes = d.trash(
        &[
            item(&lab, "work/one/same.txt"),
            item(&lab, "work/two/same.txt"),
        ],
        None,
    );
    let (a, _, _) = moved(outcomes[0].clone());
    let (b, _, _) = moved(outcomes[1].clone());
    assert_eq!(lab.read("data/Trash/files/same.txt"), "first");
    assert_eq!(lab.read("data/Trash/files/same.txt.2"), "second");
    // Restoring the second does not mix them up.
    assert!(matches!(d.restore(&b), RestoreOutcome::Restored { .. }));
    assert_eq!(lab.read("work/two/same.txt"), "second");
    assert!(matches!(d.restore(&a), RestoreOutcome::Restored { .. }));
    assert_eq!(lab.read("work/one/same.txt"), "first");
}

#[test]
fn a_name_that_the_trash_already_holds_is_not_overwritten() {
    let lab = Lab::new("trash-taken");
    lab.write("data/Trash/files/f.txt", "old entry");
    lab.write(
        "data/Trash/info/f.txt.trashinfo",
        "[Trash Info]\nPath=/elsewhere\n",
    );
    lab.write("work/f.txt", "new");
    let d = lab.home();
    let (_, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    assert_eq!(lab.read("data/Trash/files/f.txt"), "old entry");
    assert_eq!(lab.read("data/Trash/files/f.txt.2"), "new");
    assert_eq!(
        lab.read("data/Trash/info/f.txt.trashinfo"),
        "[Trash Info]\nPath=/elsewhere\n"
    );
}

#[test]
fn names_with_spaces_newlines_and_invalid_utf8_survive() {
    let lab = Lab::new("trash-odd-names");
    let d = lab.home();
    let names: Vec<std::ffi::OsString> = vec![
        "with space.txt".into(),
        "line\nbreak".into(),
        "100%.txt".into(),
        "é☃.txt".into(),
        std::ffi::OsString::from_vec(vec![b'b', b'a', b'd', 0xff, 0xfe]),
    ];
    for name in names {
        let path = lab.p("work").join(&name);
        fs::write(&path, "data").unwrap();
        let seen = identity_of(&path).unwrap();
        let real = fs::canonicalize(&path).unwrap();
        let (token, _, _) = moved(only(d.trash(&[Item::new(&path).seen(seen)], None)));
        assert!(!path.exists(), "{name:?}");
        let RestoreOutcome::Restored { path: restored } = d.restore(&token) else {
            panic!("expected {name:?} to be restored");
        };
        assert_eq!(restored, real, "{name:?}");
        assert_eq!(identity_of(&restored).unwrap(), seen, "{name:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "data");
    }
}

#[test]
fn a_token_made_for_tests_belongs_to_no_bin_entry() {
    let lab = Lab::new("restore-test-token");
    lab.write("work/f.txt", "still here");
    let token = RestoreToken::for_tests(lab.p("work/f.txt"));
    assert_eq!(token.original_path(), lab.p("work/f.txt"));
    // It can be kept, compared and cloned like a real one, and restoring it
    // touches nothing.
    assert_eq!(token.clone(), token);
    assert_eq!(
        lab.home().restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::NotInBin
        }
    );
    assert_eq!(lab.read("work/f.txt"), "still here");
}

#[test]
fn restoring_never_overwrites() {
    let lab = Lab::new("restore-exists");
    lab.write("work/f.txt", "original");
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    lab.write("work/f.txt", "someone else's file");
    assert_eq!(
        d.restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::OriginalExists
        }
    );
    assert_eq!(lab.read("work/f.txt"), "someone else's file");
    // Still in the Trash, so it can be restored once the way is clear.
    assert_eq!(lab.read("data/Trash/files/f.txt"), "original");
    // Even a link in the way counts.
    fs::remove_file(lab.p("work/f.txt")).unwrap();
    symlink("nowhere", lab.p("work/f.txt")).unwrap();
    assert_eq!(
        d.restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::OriginalExists
        }
    );
}

#[test]
fn restoring_needs_the_original_folder() {
    let lab = Lab::new("restore-no-folder");
    lab.write("work/gone/f.txt", "x");
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/gone/f.txt")], None)));
    fs::remove_dir(lab.p("work/gone")).unwrap();
    assert_eq!(
        d.restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::OriginalFolderMissing
        }
    );
}

#[test]
fn a_token_for_something_no_longer_in_the_trash_is_refused() {
    let lab = Lab::new("restore-gone");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    // Someone empties the Trash.
    fs::remove_file(lab.p("data/Trash/files/f.txt")).unwrap();
    fs::remove_file(lab.p("data/Trash/info/f.txt.trashinfo")).unwrap();
    assert_eq!(
        d.restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::NotInBin
        }
    );
    assert!(!lab.exists("work/f.txt"));
}

#[test]
fn a_token_does_not_restore_a_different_entry_that_took_its_name() {
    let lab = Lab::new("restore-swapped");
    lab.write("work/f.txt", "mine");
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    // The entry is replaced by another file of the same name. The new file is
    // made first so that it cannot be handed the old one's inode number.
    lab.write("data/Trash/files/replacement", "somebody else's");
    fs::rename(
        lab.p("data/Trash/files/replacement"),
        lab.p("data/Trash/files/f.txt"),
    )
    .unwrap();
    assert_eq!(
        d.restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::NotInBin
        }
    );
    assert!(!lab.exists("work/f.txt"));
}

#[test]
fn a_record_edited_to_point_elsewhere_is_not_followed() {
    let lab = Lab::new("restore-tampered");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    lab.write(
        "data/Trash/info/f.txt.trashinfo",
        "[Trash Info]\nPath=/somewhere/else\nDeletionDate=2026-01-01T00:00:00\n",
    );
    assert_eq!(
        d.restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::NotInBin
        }
    );
    assert_eq!(lab.read("data/Trash/files/f.txt"), "x");
}

#[test]
fn restoring_outside_the_restriction_is_refused() {
    let lab = Lab::new("restore-outside");
    let other = Fixture::new("restore-outside-other");
    fs::write(other.path().join("f.txt"), "x").unwrap();
    // A service limited to `other` moves the file, then one limited to `lab`
    // is handed the token.
    let wide = Disposal::new(lab.knowledge(), Vec::new())
        .restricted_to(other.path())
        .with_trash_env(lab.uid, Some(other.path().join("data")));
    fs::create_dir_all(other.path().join("data")).unwrap();
    let path = other.path().join("f.txt");
    let seen = identity_of(&path).unwrap();
    let (token, _, _) = moved(only(wide.trash(&[Item::new(&path).seen(seen)], None)));
    assert_eq!(
        lab.home().restore(&token),
        RestoreOutcome::Refused {
            reason: RefuseReason::OutsideRestriction
        }
    );
    assert!(!path.exists());
}

// ------------------------------------------------- Volume Trash (spec rules)

#[test]
fn a_volume_uses_its_own_trash_with_paths_below_the_mount_point() {
    let lab = Lab::new("vol-private");
    lab.write("vol/docs/a.txt", "a");
    let d = lab.volume("vol");
    let uid = lab.uid;
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "vol/docs/a.txt")], None)));
    let root = format!("vol/.Trash-{uid}");
    assert_eq!(lab.read(&format!("{root}/files/a.txt")), "a");
    let info = lab.read(&format!("{root}/info/a.txt.trashinfo"));
    assert!(info.contains("\nPath=docs/a.txt\n"), "{info}");
    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
    assert_eq!(lab.read("vol/docs/a.txt"), "a");
}

#[test]
fn a_sticky_shared_trash_folder_is_used_when_it_exists() {
    let lab = Lab::new("vol-shared");
    lab.write("vol/a.txt", "a");
    lab.mkdir("vol/.Trash");
    fs::set_permissions(lab.p("vol/.Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
    let d = lab.volume("vol");
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "vol/a.txt")], None)));
    assert_eq!(
        lab.read(&format!("vol/.Trash/{}/files/a.txt", lab.uid)),
        "a"
    );
    assert!(!lab.exists(&format!("vol/.Trash-{}", lab.uid)));
    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
}

#[test]
fn a_shared_trash_folder_without_the_sticky_bit_is_not_used() {
    let lab = Lab::new("vol-not-sticky");
    lab.write("vol/a.txt", "a");
    lab.mkdir("vol/.Trash");
    fs::set_permissions(lab.p("vol/.Trash"), fs::Permissions::from_mode(0o777)).unwrap();
    let d = lab.volume("vol");
    moved(only(d.trash(&[item(&lab, "vol/a.txt")], None)));
    assert_eq!(
        lab.read(&format!("vol/.Trash-{}/files/a.txt", lab.uid)),
        "a"
    );
    assert!(!lab.exists(&format!("vol/.Trash/{}", lab.uid)));
}

#[test]
fn a_shared_trash_folder_that_is_a_link_is_not_followed() {
    let lab = Lab::new("vol-link");
    lab.write("vol/a.txt", "a");
    lab.mkdir("elsewhere");
    fs::set_permissions(lab.p("elsewhere"), fs::Permissions::from_mode(0o1777)).unwrap();
    symlink(lab.p("elsewhere"), lab.p("vol/.Trash")).unwrap();
    let d = lab.volume("vol");
    moved(only(d.trash(&[item(&lab, "vol/a.txt")], None)));
    assert!(!lab.exists(&format!("elsewhere/{}", lab.uid)));
    assert_eq!(
        lab.read(&format!("vol/.Trash-{}/files/a.txt", lab.uid)),
        "a"
    );
}

#[test]
fn a_trash_folder_that_is_a_link_or_a_file_means_the_delete_would_be_permanent() {
    for blocker in ["link", "file"] {
        let lab = Lab::new(&format!("vol-unusable-{blocker}"));
        lab.write("vol/a.txt", "a");
        lab.mkdir("elsewhere");
        let name = format!("vol/.Trash-{}", lab.uid);
        if blocker == "link" {
            symlink(lab.p("elsewhere"), lab.p(&name)).unwrap();
        } else {
            lab.write(&name, "not a folder");
        }
        let d = lab.volume("vol");
        let plan = d.precheck(&[item(&lab, "vol/a.txt")], None);
        assert_eq!(
            plan.entries[0].decision,
            Decision::WouldBePermanent(PermanentReason::NoTrashOnVolume),
            "{blocker}"
        );
        let outcome = only(d.trash(&[item(&lab, "vol/a.txt")], None));
        assert_eq!(
            outcome,
            Outcome::WouldBePermanent {
                reason: PermanentReason::NoTrashOnVolume
            },
            "{blocker}"
        );
        // Not moved, and nothing was written through the link.
        assert_eq!(lab.read("vol/a.txt"), "a");
        assert_eq!(fs::read_dir(lab.p("elsewhere")).unwrap().count(), 0);
    }
}

#[test]
fn a_trash_folder_that_belongs_to_someone_else_is_not_used() {
    let lab = Lab::new("vol-foreign");
    lab.write("vol/a.txt", "a");
    // The folder exists and is owned by the user running the test. For a
    // service acting for the next user id it belongs to someone else.
    let other_uid = lab.uid.wrapping_add(1);
    lab.write(&format!("vol/.Trash-{other_uid}/files/old.txt"), "old");
    let d = Disposal::new(lab.knowledge(), Vec::new())
        .restricted_to(lab.fx.path())
        .with_trash_env(other_uid, None)
        .with_topdir(lab.p("vol"));
    let outcome = only(d.trash(&[item(&lab, "vol/a.txt")], None));
    assert_eq!(
        outcome,
        Outcome::WouldBePermanent {
            reason: PermanentReason::NoTrashOnVolume
        }
    );
    assert_eq!(lab.read("vol/a.txt"), "a");
    assert_eq!(
        fs::read_dir(lab.p(&format!("vol/.Trash-{other_uid}")))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn nothing_is_copied_between_filesystems() {
    // `/dev/shm` is a different filesystem from the fixture's.
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("skipped: /dev/shm is not available");
        return;
    }
    let lab = Lab::new("cross-fs");
    let other = Fixture::new_in(shm, "cross-fs-shm");
    if fs::metadata(other.path()).unwrap().dev() == fs::metadata(lab.fx.path()).unwrap().dev() {
        eprintln!("skipped: /dev/shm is on the same filesystem");
        return;
    }
    fs::write(other.path().join("f.txt"), "x").unwrap();

    // The mount point given for the item's volume is on the other filesystem:
    // the item cannot use it.
    let d = Disposal::new(lab.knowledge(), Vec::new())
        .restricted_to(other.path())
        .with_trash_env(lab.uid, None)
        .with_topdir(lab.fx.path());
    let found = d.resolve(&other.path().join("f.txt")).unwrap();
    assert_eq!(
        d.bin_objection(&other.path().join("f.txt"), &found),
        Some(PermanentReason::NoTrashOnVolume)
    );
    let outcome = d.trash_one(&Item::new(other.path().join("f.txt")), &found);
    assert_eq!(
        outcome,
        Outcome::WouldBePermanent {
            reason: PermanentReason::NoTrashOnVolume
        }
    );
    assert_eq!(fs::read_to_string(other.path().join("f.txt")).unwrap(), "x");

    // A home Trash on the other filesystem is passed over for a volume Trash.
    let d = Disposal::new(lab.knowledge(), Vec::new())
        .restricted_to(other.path())
        .with_trash_env(lab.uid, Some(lab.p("data")))
        .with_topdir(other.path());
    let found = d.resolve(&other.path().join("f.txt")).unwrap();
    let (_, _, _) = moved(d.trash_one(&Item::new(other.path().join("f.txt")), &found));
    assert!(
        other
            .path()
            .join(format!(".Trash-{}/files/f.txt", lab.uid))
            .exists()
    );
    assert!(!lab.exists("data/Trash"));
}

#[test]
fn the_mount_point_of_a_path_is_found_by_the_device_change() {
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("skipped: /dev/shm is not available");
        return;
    }
    let other = Fixture::new_in(shm, "topdir");
    let deep = other.path().join("a/b");
    fs::create_dir_all(&deep).unwrap();
    let dev = fs::metadata(&deep).unwrap().dev();
    let top = linux::topdir_of(&deep, dev);
    // The first folder that is on the filesystem, going up from the item.
    assert_eq!(top, shm);
    // A mount point itself is refused as an item.
    let lab = Lab::new("mount-point");
    let d = lab.home();
    let found = d.resolve(shm).unwrap();
    assert!(found.on_mount_point);
}

// ------------------------------------------------------- Rules and refusals

#[test]
fn a_blocked_item_refuses_the_whole_selection() {
    let lab = Lab::new("blocked-mixed");
    lab.write("work/fine.txt", "x");
    lab.write("work/also fine.txt", "y");
    lab.mkdir("blocked-root");
    let d = lab.home();
    let items = [
        item(&lab, "work/fine.txt"),
        item(&lab, "blocked-root"),
        item(&lab, "work/also fine.txt"),
    ];
    let plan = d.precheck(&items, None);
    assert!(plan.selection_refused);
    assert_eq!(plan.strictest, Level::Blocked);
    assert_eq!(plan.drivers, vec![1]);
    assert!(matches!(
        plan.entries[1].decision,
        Decision::Refused(RefuseReason::Blocked { .. })
    ));
    assert_eq!(
        plan.entries[0].decision,
        Decision::Refused(RefuseReason::SelectionRefused { driver: 1 })
    );

    for outcomes in [d.trash(&items, None), d.delete_permanently(&items, None)] {
        assert!(matches!(
            refused(&outcomes[1]),
            RefuseReason::Blocked { .. }
        ));
        assert_eq!(
            refused(&outcomes[0]),
            &RefuseReason::SelectionRefused { driver: 1 }
        );
        assert_eq!(
            refused(&outcomes[2]),
            &RefuseReason::SelectionRefused { driver: 1 }
        );
    }
    assert_eq!(lab.read("work/fine.txt"), "x");
    assert_eq!(lab.read("work/also fine.txt"), "y");
    assert!(lab.exists("blocked-root"));
    assert!(!lab.exists("data/Trash"));
}

#[test]
fn a_blocked_item_cannot_be_unblocked_by_a_confirmation() {
    let lab = Lab::new("blocked-confirmed");
    lab.mkdir("blocked-root");
    let d = lab.home();
    let confirmation = ProtectedConfirmation::for_paths([lab.p("blocked-root")]);
    let items = [item(&lab, "blocked-root")];
    for outcome in [
        only(d.trash(&items, Some(&confirmation))),
        only(d.delete_permanently(&items, Some(&confirmation))),
    ] {
        assert!(matches!(refused(&outcome), RefuseReason::Blocked { .. }));
    }
    assert!(lab.exists("blocked-root"));
}

#[test]
fn a_protected_item_needs_its_confirmation_and_refuses_the_selection_without_it() {
    let lab = Lab::new("protected");
    lab.write("protected-home/doc.txt", "x");
    lab.write("work/fine.txt", "y");
    let d = lab.home();
    let items = [item(&lab, "work/fine.txt"), item(&lab, "protected-home")];

    let plan = d.precheck(&items, None);
    assert_eq!(plan.strictest, Level::Protected);
    assert_eq!(plan.drivers, vec![1]);
    assert!(plan.selection_refused);
    assert!(plan.entries[1].note.is_some());

    let outcomes = d.trash(&items, None);
    assert!(matches!(
        refused(&outcomes[1]),
        RefuseReason::ProtectedNotConfirmed { note: Some(_) }
    ));
    assert_eq!(
        refused(&outcomes[0]),
        &RefuseReason::SelectionRefused { driver: 1 }
    );
    assert!(lab.exists("protected-home") && lab.exists("work/fine.txt"));

    // A confirmation for another path does not cover it.
    let wrong = ProtectedConfirmation::for_paths([lab.p("work/fine.txt")]);
    let outcomes = d.trash(&items, Some(&wrong));
    assert!(matches!(
        refused(&outcomes[1]),
        RefuseReason::ProtectedNotConfirmed { .. }
    ));
    assert!(lab.exists("protected-home"));

    // The right confirmation lets the whole selection through.
    let right = ProtectedConfirmation::for_paths([lab.p("protected-home")]);
    let outcomes = d.trash(&items, Some(&right));
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, Outcome::MovedToBin { .. }))
    );
    assert!(!lab.exists("protected-home") && !lab.exists("work/fine.txt"));
}

#[test]
fn a_protected_item_is_also_confirmed_for_a_permanent_delete() {
    let lab = Lab::new("protected-permanent");
    lab.write("protected-home/doc.txt", "x");
    let d = lab.home();
    let items = [item(&lab, "protected-home")];
    assert!(matches!(
        refused(&only(d.delete_permanently(&items, None))),
        RefuseReason::ProtectedNotConfirmed { .. }
    ));
    assert!(lab.exists("protected-home/doc.txt"));
    let confirmation = ProtectedConfirmation::for_paths([lab.p("protected-home")]);
    assert!(matches!(
        only(d.delete_permanently(&items, Some(&confirmation))),
        Outcome::Deleted { .. }
    ));
    assert!(!lab.exists("protected-home"));
}

#[test]
fn a_path_through_a_link_into_the_system_is_judged_by_where_it_leads() {
    if crate::bins::walk::running_as_superuser() {
        eprintln!("skipped: the superuser could change the system if this were wrong");
        return;
    }
    let lab = Lab::new("link-to-system");
    // `bin` of the system's own `/usr`, reached through a link in the fixture.
    symlink("/usr", lab.p("work/usr-link")).unwrap();
    let d = lab.home();
    let path = lab.p("work/usr-link/bin");
    let outcome = only(d.trash(&[Item::new(&path)], None));
    assert!(
        matches!(refused(&outcome), RefuseReason::Blocked { .. }),
        "{outcome:?}"
    );
    let outcome = only(d.delete_permanently(&[Item::new(&path)], None));
    assert!(
        matches!(refused(&outcome), RefuseReason::Blocked { .. }),
        "{outcome:?}"
    );
}

#[test]
fn a_link_in_the_path_that_leads_out_of_the_restriction_is_refused() {
    let lab = Lab::new("link-out");
    let other = Fixture::new("link-out-other");
    fs::write(other.path().join("secret.txt"), "keep").unwrap();
    symlink(other.path(), lab.p("work/out")).unwrap();
    let d = lab.home();
    let path = lab.p("work/out/secret.txt");
    for outcome in [
        only(d.trash(&[Item::new(&path)], None)),
        only(d.delete_permanently(&[Item::new(&path)], None)),
    ] {
        assert_eq!(refused(&outcome), &RefuseReason::OutsideRestriction);
    }
    assert_eq!(
        fs::read_to_string(other.path().join("secret.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn nothing_outside_the_restriction_is_touched() {
    let lab = Lab::new("restriction");
    let other = Fixture::new("restriction-other");
    fs::write(other.path().join("f.txt"), "keep").unwrap();
    let d = lab.home();
    let items = [Item::new(other.path().join("f.txt"))];
    assert_eq!(
        refused(&only(d.trash(&items, None))),
        &RefuseReason::OutsideRestriction
    );
    assert_eq!(
        refused(&only(d.delete_permanently(&items, None))),
        &RefuseReason::OutsideRestriction
    );
    assert_eq!(
        fs::read_to_string(other.path().join("f.txt")).unwrap(),
        "keep"
    );
    // The real system is out of reach for the same reason.
    let real = [Item::new("/etc/hostname")];
    // `/etc` is a blocked folder, which is reported ahead of the limit.
    assert!(matches!(
        refused(&only(d.delete_permanently(&real, None))),
        RefuseReason::Blocked { .. }
    ));
}

#[test]
fn a_replaced_item_is_refused() {
    let lab = Lab::new("changed");
    lab.write("work/f.txt", "the file the scan saw");
    let d = lab.home();
    let seen = item(&lab, "work/f.txt");
    // Replace it with a new file of the same name: the new one is created
    // first so that the old inode cannot be handed out again.
    lab.write("work/new.txt", "an imposter");
    fs::rename(lab.p("work/new.txt"), lab.p("work/f.txt")).unwrap();

    for outcome in [
        only(d.trash(std::slice::from_ref(&seen), None)),
        only(d.delete_permanently(std::slice::from_ref(&seen), None)),
    ] {
        assert_eq!(refused(&outcome), &RefuseReason::ChangedSinceScan);
    }
    assert_eq!(lab.read("work/f.txt"), "an imposter");
    assert!(!lab.exists("data/Trash"));
}

#[test]
fn an_item_swapped_after_the_check_is_refused_at_the_moment_of_the_move() {
    let lab = Lab::new("swapped");
    lab.write("work/f.txt", "checked");
    let d = lab.home();
    let seen = item(&lab, "work/f.txt");
    let (plan, resolved) = d.plan(std::slice::from_ref(&seen), None, Mode::Trash);
    assert_eq!(plan.entries[0].decision, Decision::Proceed);
    let planned = resolved[0].clone().unwrap();

    // Between the check and the move the name is pointed at something else. The
    // new link is made before the old file goes, so that it gets another inode.
    lab.write("work/other.txt", "not what was checked");
    symlink("other.txt", lab.p("work/new-link")).unwrap();
    fs::rename(lab.p("work/new-link"), lab.p("work/f.txt")).unwrap();

    for mode in [Mode::Trash, Mode::Permanent] {
        let outcome = d.execute(&seen, &planned, None, mode);
        assert_eq!(
            refused(&outcome),
            &RefuseReason::ChangedSinceScan,
            "{mode:?}"
        );
    }
    assert_eq!(lab.read("work/other.txt"), "not what was checked");
    assert!(lab.exists("work/f.txt"));
}

#[test]
fn items_inside_other_items_and_repeats_are_left_out() {
    let lab = Lab::new("overlap");
    lab.write("work/dir/inner.txt", "x");
    lab.write("work/other.txt", "y");
    let d = lab.home();
    let items = [
        item(&lab, "work/dir/inner.txt"),
        item(&lab, "work/dir"),
        item(&lab, "work/other.txt"),
        item(&lab, "work/other.txt"),
    ];
    let outcomes = d.trash(&items, None);
    assert_eq!(refused(&outcomes[0]), &RefuseReason::Overlaps { with: 1 });
    assert!(matches!(outcomes[1], Outcome::MovedToBin { .. }));
    assert!(matches!(outcomes[2], Outcome::MovedToBin { .. }));
    assert_eq!(refused(&outcomes[3]), &RefuseReason::Overlaps { with: 2 });
    assert_eq!(lab.read("data/Trash/files/dir/inner.txt"), "x");
}

#[test]
fn malformed_paths_are_refused_and_refuse_the_selection() {
    let lab = Lab::new("bad-paths");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    for bad in ["", "relative.txt", "/a/../b", "/a/./b"] {
        let items = [item(&lab, "work/f.txt"), Item::new(bad)];
        let outcomes = d.trash(&items, None);
        assert!(
            matches!(refused(&outcomes[1]), RefuseReason::BadPath(_)),
            "{bad:?}"
        );
        assert_eq!(
            refused(&outcomes[0]),
            &RefuseReason::SelectionRefused { driver: 1 },
            "{bad:?}"
        );
    }
    assert_eq!(lab.read("work/f.txt"), "x");
}

#[test]
fn a_missing_item_is_refused_as_not_found() {
    let lab = Lab::new("missing");
    let d = lab.home();
    let outcome = only(d.trash(&[Item::new(lab.p("work/nothing.txt"))], None));
    assert_eq!(refused(&outcome), &RefuseReason::NotFound);
    let outcome = only(d.delete_permanently(&[Item::new(lab.p("nothing/at/all"))], None));
    assert_eq!(refused(&outcome), &RefuseReason::NotFound);
}

#[test]
fn the_check_changes_nothing() {
    let lab = Lab::new("precheck-readonly");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    let plan = d.precheck(&[item(&lab, "work/f.txt")], None);
    assert_eq!(plan.entries[0].decision, Decision::Proceed);
    assert!(!plan.selection_refused);
    assert!(lab.exists("work/f.txt"));
    // Not even the Trash folder was made.
    assert!(!lab.exists("data/Trash"));
}

// ------------------------------------------------------- Links and hard links

#[test]
fn a_link_is_moved_as_a_link_and_its_target_stays() {
    let lab = Lab::new("trash-link");
    lab.write("work/target/file.txt", "kept");
    symlink("target", lab.p("work/link")).unwrap();
    symlink("missing", lab.p("work/dangling")).unwrap();
    let d = lab.home();
    for name in ["work/link", "work/dangling"] {
        let (token, bytes, _) = moved(only(d.trash(&[item(&lab, name)], None)));
        assert_eq!(
            bytes,
            allocated(&lab.p(&format!("data/Trash/files/{}", &name[5..])))
        );
        assert!(!lab.exists(name));
        assert!(
            fs::symlink_metadata(lab.p(&format!("data/Trash/files/{}", &name[5..])))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
        assert!(
            fs::symlink_metadata(lab.p(name))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert_eq!(lab.read("work/target/file.txt"), "kept");
}

#[test]
fn deleting_a_link_removes_the_link_and_nothing_it_points_to() {
    let lab = Lab::new("delete-link");
    lab.write("work/target/file.txt", "kept");
    symlink("target", lab.p("work/link")).unwrap();
    symlink("/etc/hostname", lab.p("work/system-link")).unwrap();
    let d = lab.home();
    for name in ["work/link", "work/system-link"] {
        let outcome = only(d.delete_permanently(&[item(&lab, name)], None));
        assert!(matches!(outcome, Outcome::Deleted { .. }), "{outcome:?}");
        assert!(!lab.exists(name));
    }
    assert_eq!(lab.read("work/target/file.txt"), "kept");
    assert!(Path::new("/etc/hostname").exists() || !Path::new("/etc").exists());
}

#[test]
fn a_folder_delete_never_goes_through_links_inside_it() {
    let lab = Lab::new("delete-no-descend");
    lab.write("keep/precious.txt", "precious");
    lab.write("work/tree/a.txt", "a");
    lab.write("work/tree/sub/b.txt", "b");
    symlink(lab.p("keep"), lab.p("work/tree/to-keep")).unwrap();
    symlink(lab.p("keep"), lab.p("work/tree/sub/also-to-keep")).unwrap();
    symlink("../../keep/precious.txt", lab.p("work/tree/file-link")).unwrap();
    let d = lab.home();
    let outcome = only(d.delete_permanently(&[item(&lab, "work/tree")], None));
    assert!(matches!(outcome, Outcome::Deleted { .. }), "{outcome:?}");
    assert!(!lab.exists("work/tree"));
    assert_eq!(lab.read("keep/precious.txt"), "precious");
}

#[test]
fn a_folder_with_links_moves_to_the_trash_intact() {
    let lab = Lab::new("trash-folder-with-links");
    lab.write("keep/precious.txt", "precious");
    lab.write("work/tree/a.txt", "a");
    symlink(lab.p("keep"), lab.p("work/tree/to-keep")).unwrap();
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/tree")], None)));
    assert!(
        fs::symlink_metadata(lab.p("data/Trash/files/tree/to-keep"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(lab.read("keep/precious.txt"), "precious");
    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
}

#[test]
fn a_link_in_the_parent_path_is_followed_only_to_find_the_real_place() {
    let lab = Lab::new("parent-link");
    lab.write("work/real/f.txt", "x");
    symlink("real", lab.p("work/alias")).unwrap();
    let d = lab.home();
    let path = lab.p("work/alias/f.txt");
    let seen = identity_of(&path).unwrap();
    let (token, _, _) = moved(only(d.trash(&[Item::new(&path).seen(seen)], None)));
    assert!(!lab.exists("work/real/f.txt"));
    // The token records the real place.
    assert_eq!(token.original_path(), lab.p("work/real/f.txt"));
    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
    assert_eq!(lab.read("work/real/f.txt"), "x");
}

#[test]
fn a_hard_linked_item_is_reported_as_shared() {
    let lab = Lab::new("hardlink");
    lab.write("work/a.txt", &bytes_of(5000));
    fs::hard_link(lab.p("work/a.txt"), lab.p("keep-name.txt")).unwrap();
    let d = lab.home();

    // Trash: the data is only moved, and the report says another name exists.
    let (token, bytes, shared) = moved(only(d.trash(&[item(&lab, "work/a.txt")], None)));
    assert_eq!(shared, 1);
    assert_eq!(bytes, allocated(&lab.p("keep-name.txt")));
    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));

    // Permanent delete of one name gives nothing back.
    let outcome = only(d.delete_permanently(&[item(&lab, "work/a.txt")], None));
    assert_eq!(
        outcome,
        Outcome::Deleted {
            bytes: 0,
            shared_files: 1
        }
    );
    assert_eq!(lab.read("keep-name.txt"), bytes_of(5000));
    // Deleting the last name gives the bytes back.
    let last = allocated(&lab.p("keep-name.txt"));
    let outcome = only(d.delete_permanently(&[item(&lab, "keep-name.txt")], None));
    assert_eq!(
        outcome,
        Outcome::Deleted {
            bytes: last,
            shared_files: 0
        }
    );
}

#[test]
fn names_of_one_file_inside_the_folder_are_counted_once_when_all_go() {
    let lab = Lab::new("hardlink-inside");
    lab.write("work/tree/a.txt", &bytes_of(5000));
    fs::hard_link(lab.p("work/tree/a.txt"), lab.p("work/tree/b.txt")).unwrap();
    lab.write("work/tree/c.txt", "c");
    fs::hard_link(lab.p("work/tree/c.txt"), lab.p("outside-name.txt")).unwrap();
    let d = lab.home();
    let a = allocated(&lab.p("work/tree/a.txt"));
    let dir = allocated(&lab.p("work/tree"));
    let outcome = only(d.delete_permanently(&[item(&lab, "work/tree")], None));
    // `a` and `b` are one file, freed once; `c` lives on under another name.
    assert_eq!(
        outcome,
        Outcome::Deleted {
            bytes: dir + a,
            shared_files: 1
        }
    );
    assert_eq!(lab.read("outside-name.txt"), "c");
}

// ----------------------------------------------------------- Permanent delete

#[test]
fn a_folder_is_deleted_and_the_bytes_are_counted() {
    let lab = Lab::new("delete-tree");
    lab.write("work/tree/a.txt", &bytes_of(10_000));
    lab.write("work/tree/sub/deeper/b.txt", &bytes_of(1));
    lab.mkdir("work/tree/empty");
    let d = lab.home();
    let mut expected = 0;
    for relative in [
        "work/tree",
        "work/tree/a.txt",
        "work/tree/sub",
        "work/tree/sub/deeper",
        "work/tree/sub/deeper/b.txt",
        "work/tree/empty",
    ] {
        expected += allocated(&lab.p(relative));
    }
    let outcome = only(d.delete_permanently(&[item(&lab, "work/tree")], None));
    assert_eq!(
        outcome,
        Outcome::Deleted {
            bytes: expected,
            shared_files: 0
        }
    );
    assert!(!lab.exists("work/tree"));
    assert!(lab.exists("work"));
}

#[test]
fn only_the_listed_items_are_deleted() {
    let lab = Lab::new("delete-listed");
    lab.write("work/a.txt", "a");
    lab.write("work/b.txt", "b");
    lab.write("work/dir/c.txt", "c");
    let d = lab.home();
    let outcomes = d.delete_permanently(&[item(&lab, "work/a.txt"), item(&lab, "work/dir")], None);
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, Outcome::Deleted { .. }))
    );
    assert!(!lab.exists("work/a.txt") && !lab.exists("work/dir"));
    assert_eq!(lab.read("work/b.txt"), "b");
}

#[test]
fn a_partial_failure_is_reported_as_one_and_keeps_going() {
    let lab = Lab::new("delete-partial");
    lab.write("work/tree/ok.txt", "ok");
    lab.write("work/tree/locked/inside.txt", "stuck");
    lab.write("work/tree/z-ok.txt", "ok too");
    // A folder that cannot be changed: the file inside cannot be removed.
    fs::set_permissions(lab.p("work/tree/locked"), fs::Permissions::from_mode(0o500)).unwrap();
    if fs::remove_file(lab.p("work/tree/locked/inside.txt")).is_ok() {
        // Running as a user who may do anything (root): the case cannot occur.
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }
    let d = lab.home();
    let outcome = only(d.delete_permanently(&[item(&lab, "work/tree")], None));
    let Outcome::Failed { error } = &outcome else {
        panic!("a partial delete must not be reported as a success: {outcome:?}");
    };
    assert_eq!(error.kind, io::ErrorKind::PermissionDenied);
    // The siblings were still removed, and the error says how much went.
    assert!(error.removed_entries >= 2, "{error:?}");
    assert!(!lab.exists("work/tree/ok.txt") && !lab.exists("work/tree/z-ok.txt"));
    assert!(lab.exists("work/tree/locked/inside.txt"));
    fs::set_permissions(lab.p("work/tree/locked"), fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_folder_is_not_entered_through_a_changed_device_number() {
    let lab = Lab::new("delete-device");
    lab.write("work/tree/a.txt", "a");
    let parent = fsys::open_dir_following(&lab.p("work")).unwrap();
    let meta = fsys::stat_at(std::os::fd::AsRawFd::as_raw_fd(&parent), c"tree").unwrap();
    let real_dev = meta.dev;
    let mut tally = fsys::Tally::default();
    // As if the item were on a different filesystem than the delete was
    // started on.
    let elsewhere = fsys::Boundary {
        dev: real_dev + 1,
        mnt: meta.mnt,
    };
    fsys::remove_entry(
        std::os::fd::AsRawFd::as_raw_fd(&parent),
        c"tree",
        elsewhere,
        0,
        &mut tally,
    );
    assert!(tally.first_error.is_some());
    assert_eq!(tally.entries, 0);
    assert_eq!(lab.read("work/tree/a.txt"), "a");
}

#[test]
fn very_deep_folders_stop_at_a_limit_without_overflowing_the_stack() {
    let lab = Lab::new("delete-deep");
    // Building it takes many calls; use relative opens so the path stays short.
    let root = fsys::open_dir_following(&lab.p("work")).unwrap();
    fsys::mkdir_at(std::os::fd::AsRawFd::as_raw_fd(&root), c"deep", 0o755).unwrap();
    let mut current =
        crate::bins::walk::open_dir_at(std::os::fd::AsRawFd::as_raw_fd(&root), c"deep").unwrap();
    for _ in 0..300 {
        fsys::mkdir_at(std::os::fd::AsRawFd::as_raw_fd(&current), c"d", 0o755).unwrap();
        current = crate::bins::walk::open_dir_at(std::os::fd::AsRawFd::as_raw_fd(&current), c"d")
            .unwrap();
    }
    drop(current);
    let d = lab.home();
    let outcome = only(d.delete_permanently(&[item(&lab, "work/deep")], None));
    let Outcome::Failed { error } = outcome else {
        panic!("too-deep folders cannot be removed in full");
    };
    assert_eq!(error.kind, io::ErrorKind::InvalidInput);
    assert!(lab.exists("work/deep"));
}

#[test]
fn files_with_odd_names_are_deleted() {
    let lab = Lab::new("delete-odd");
    let d = lab.home();
    for name in [
        "-rf",
        " leading space",
        "trailing space ",
        "new\nline",
        "*",
        "~",
    ] {
        let path = lab.p("work").join(name);
        fs::write(&path, "x").unwrap();
        let seen = identity_of(&path).unwrap();
        let outcome = only(d.delete_permanently(&[Item::new(&path).seen(seen)], None));
        assert!(matches!(outcome, Outcome::Deleted { .. }), "{name:?}");
        assert!(fs::symlink_metadata(&path).is_err(), "{name:?}");
    }
}

// ----------------------------------------------------------------- Emptying

fn drive_at(root: &Path, id: &str) -> Drive {
    Drive {
        id: DriveId::new(id),
        mount_point: root.to_path_buf(),
        label: None,
        fs_type: "ext4".into(),
        kind: DriveKind::Fixed,
        read_only: false,
        eligible_for_background_scan: true,
        capacity: None,
        capacity_problem: None,
        mounts: vec![Mount {
            mount_point: root.to_path_buf(),
            mount_id: Some(1),
            root: PathBuf::from("/"),
            covered_by_other_mount: false,
        }],
    }
}

#[test]
fn emptying_a_drive_clears_that_drives_trash_only() {
    let lab = Lab::new("empty-per-drive");
    lab.write("a/one.txt", &bytes_of(5000));
    lab.write("a/dir/two.txt", "2");
    lab.write("b/three.txt", "3");
    let on_a = lab.volume("a");
    let on_b = lab.volume("b");
    moved(only(on_a.trash(&[item(&lab, "a/one.txt")], None)));
    moved(only(on_a.trash(&[item(&lab, "a/dir")], None)));
    moved(only(on_b.trash(&[item(&lab, "b/three.txt")], None)));
    let uid = lab.uid;
    // Something in the Trash folder that is not an item stays.
    lab.write(&format!("a/.Trash-{uid}/notes.txt"), "not ours to remove");
    let trash_files = |vol: &str| {
        fs::read_dir(lab.p(&format!("{vol}/.Trash-{uid}/files")))
            .unwrap()
            .count()
    };
    assert_eq!(trash_files("a"), 2);

    let drive_a = drive_at(&lab.p("a"), "drive-a");
    match on_a.empty_bin(&drive_a) {
        EmptyOutcome::Emptied { bytes, entries } => {
            assert!(bytes >= 8192, "{bytes}");
            assert!(entries >= 4, "{entries}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(trash_files("a"), 0);
    assert_eq!(
        fs::read_dir(lab.p(&format!("a/.Trash-{uid}/info")))
            .unwrap()
            .count(),
        0
    );
    assert!(!lab.exists(&format!("a/.Trash-{uid}/directorysizes")));
    assert_eq!(
        lab.read(&format!("a/.Trash-{uid}/notes.txt")),
        "not ours to remove"
    );
    // The other drive is untouched.
    assert_eq!(trash_files("b"), 1);
    assert_eq!(lab.read(&format!("b/.Trash-{uid}/files/three.txt")), "3");
}

#[test]
fn emptying_the_trash_does_not_follow_links_inside_it() {
    let lab = Lab::new("empty-links");
    lab.write("keep/precious.txt", "precious");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    symlink(lab.p("keep"), lab.p("data/Trash/files/link-out")).unwrap();
    lab.write(
        "data/Trash/info/link-out.trashinfo",
        "[Trash Info]\nPath=/x\n",
    );
    // `data` is on the drive whose mount point is the fixture.
    let drive = drive_at(lab.fx.path(), "fixture");
    assert!(matches!(d.empty_bin(&drive), EmptyOutcome::Emptied { .. }));
    assert!(!lab.exists("data/Trash/files/link-out"));
    assert_eq!(lab.read("keep/precious.txt"), "precious");
    assert_eq!(fs::read_dir(lab.p("data/Trash/files")).unwrap().count(), 0);
}

#[test]
fn emptying_a_drive_with_no_trash_empties_nothing() {
    let lab = Lab::new("empty-nothing");
    let d = lab.volume("a");
    lab.mkdir("a");
    assert_eq!(
        d.empty_bin(&drive_at(&lab.p("a"), "none")),
        EmptyOutcome::Emptied {
            bytes: 0,
            entries: 0
        }
    );
}

#[test]
fn emptying_is_refused_for_a_trash_outside_the_restriction() {
    let lab = Lab::new("empty-outside");
    let other = Fixture::new("empty-outside-other");
    fs::create_dir_all(other.path().join(format!(".Trash-{}/files", lab.uid))).unwrap();
    fs::write(
        other
            .path()
            .join(format!(".Trash-{}/files/keep.txt", lab.uid)),
        "keep",
    )
    .unwrap();
    let d = lab.volume("a");
    let outcome = d.empty_bin(&drive_at(other.path(), "other"));
    assert_eq!(
        outcome,
        EmptyOutcome::Refused {
            reason: RefuseReason::OutsideRestriction
        }
    );
    assert!(
        other
            .path()
            .join(format!(".Trash-{}/files/keep.txt", lab.uid))
            .exists()
    );
}

// -------------------------------------------------------------------- Guards

#[test]
fn the_labs_trash_is_inside_the_fixture() {
    let lab = Lab::new("guard");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    let TokenInner::Trash(inner) = &token.inner else {
        panic!("not a Trash token");
    };
    assert!(inner.root_for_tests().starts_with(lab.fx.path()));
}

#[test]
fn a_folder_that_cannot_be_changed_makes_the_move_fail_cleanly() {
    let lab = Lab::new("trash-no-permission");
    lab.write("work/locked/f.txt", "x");
    fs::set_permissions(lab.p("work/locked"), fs::Permissions::from_mode(0o500)).unwrap();
    let probe = fs::rename(lab.p("work/locked/f.txt"), lab.p("work/locked/g.txt"));
    if probe.is_ok() {
        eprintln!("skipped: permissions are not enforced for this user");
        fs::set_permissions(lab.p("work/locked"), fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let d = lab.home();
    let outcome = only(d.trash(&[item(&lab, "work/locked/f.txt")], None));
    fs::set_permissions(lab.p("work/locked"), fs::Permissions::from_mode(0o700)).unwrap();
    let Outcome::Failed { error } = outcome else {
        panic!("expected Failed, got {outcome:?}");
    };
    assert_eq!(error.kind, io::ErrorKind::PermissionDenied);
    assert_eq!(lab.read("work/locked/f.txt"), "x");
    // No record is left behind for something that is not in the Trash.
    assert_eq!(fs::read_dir(lab.p("data/Trash/info")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(lab.p("data/Trash/files")).unwrap().count(), 0);
}

#[test]
fn the_roots_of_this_computers_drives_are_blocked() {
    // Reads the real mount table and only looks at paths; nothing is changed.
    let d = Disposal::system().expect("drives");
    let mut checked = 0;
    for drive in d.drives() {
        for root in std::iter::once(&drive.mount_point)
            .chain(drive.mounts.iter().map(|m| &m.mount_point))
            .filter(|root| root.as_path() != Path::new("/"))
        {
            let plan = d.precheck(&[Item::new(root)], None);
            assert_eq!(plan.strictest, Level::Blocked, "{root:?}");
            assert!(plan.selection_refused, "{root:?}");
            checked += 1;
        }
    }
    eprintln!("checked {checked} mount points");
    // The file system root and the system folders too.
    for path in [
        "/",
        "/usr",
        "/usr/bin",
        "/etc",
        "/boot",
        "/proc",
        "/usr/../usr",
        "//usr",
    ] {
        let plan = d.precheck(&[Item::new(path)], None);
        assert_eq!(plan.strictest, Level::Blocked, "{path:?}");
        assert!(plan.selection_refused, "{path:?}");
    }
}

#[test]
fn the_rules_are_applied_again_to_what_is_there_at_the_moment_of_the_move() {
    if crate::bins::walk::running_as_superuser() {
        eprintln!("skipped: the superuser could change the system if this were wrong");
        return;
    }
    let lab = Lab::new("recheck-rules");
    // `work/d/bin` is an ordinary folder when the check is made.
    lab.mkdir("work/d/bin");
    let d = lab.home();
    let path = lab.p("work/d/bin");
    let seen = Item::new(&path);
    let (plan, resolved) = d.plan(std::slice::from_ref(&seen), None, Mode::Trash);
    assert_eq!(plan.entries[0].decision, Decision::Proceed);
    let planned = resolved[0].clone().unwrap();

    // Then `work/d` becomes a link to the system's `/usr`, so that the same
    // path now names the system's own `bin` folder.
    fs::rename(lab.p("work/d"), lab.p("work/d-moved")).unwrap();
    symlink("/usr", lab.p("work/d")).unwrap();

    for mode in [Mode::Trash, Mode::Permanent] {
        let outcome = d.execute(&seen, &planned, None, mode);
        assert!(
            matches!(refused(&outcome), RefuseReason::Blocked { .. }),
            "{mode:?}: {outcome:?}"
        );
    }
    assert!(lab.exists("work/d-moved/bin"));
}

// ------------------------------------------------------------- Mount boundaries

#[test]
fn folders_are_not_entered_when_the_kernel_cannot_tell_mounts_apart() {
    let lab = Lab::new("delete-no-mount-ids");
    lab.write("work/tree/a.txt", "a");
    lab.write("work/file.txt", "f");
    let parent = fsys::open_dir_following(&lab.p("work")).unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&parent);
    let meta = fsys::stat_at(fd, c"tree").unwrap();
    // A kernel before 5.8 gives no mount ID: the boundary then has none.
    let blind = fsys::Boundary {
        dev: meta.dev,
        mnt: None,
    };
    let mut tally = fsys::Tally::default();
    fsys::remove_entry(fd, c"tree", blind, 0, &mut tally);
    assert_eq!(
        tally.first_error.as_ref().map(io::Error::kind),
        Some(io::ErrorKind::Unsupported)
    );
    assert_eq!(lab.read("work/tree/a.txt"), "a");
    // A single file cannot hide a mount, so it can still go.
    let mut tally = fsys::Tally::default();
    fsys::remove_entry(fd, c"file.txt", blind, 0, &mut tally);
    assert!(tally.first_error.is_none());
    assert!(!lab.exists("work/file.txt"));
}

#[test]
fn this_kernel_reports_mount_ids() {
    // The mount-boundary checks depend on it; on a kernel without it they
    // fail closed (see the test above), and this says which case ran.
    let lab = Lab::new("mount-ids");
    let dir = fsys::open_dir_following(lab.fx.path()).unwrap();
    let meta = fsys::fstat(std::os::fd::AsRawFd::as_raw_fd(&dir)).unwrap();
    if meta.mnt.is_none() {
        // Older kernels do not report it, and the checks then refuse rather
        // than guess. Where the test environment is known to be recent, a
        // missing ID is a failure.
        assert!(
            std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
            "this kernel gives no mount IDs"
        );
        eprintln!("note: this kernel gives no mount IDs");
    }
}

/// Binds `source` onto `target`. Only the bind-mount test uses it, inside its
/// own mount namespace.
fn bind(source: &Path, target: &Path) {
    let source = std::ffi::CString::new(source.as_os_str().as_encoded_bytes()).unwrap();
    let target = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: both strings are NUL-terminated and outlive the call; the file
    // system type and data are optional and null for a bind mount.
    let status = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    assert_eq!(status, 0, "bind mount: {}", io::Error::last_os_error());
}

/// Detaches the bind mounts it was given when it goes out of scope, so the
/// fixtures are not walked through them when they are removed.
struct Binds(Vec<PathBuf>);

impl Drop for Binds {
    fn drop(&mut self) {
        for target in self.0.iter().rev() {
            let Ok(target) = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()) else {
                continue;
            };
            // SAFETY: the string is NUL-terminated and outlives the call.
            unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

/// The work of the bind-mount test. It runs only inside the namespace that
/// `bind_mounts_are_never_entered_or_deleted_through` makes, where it may mount.
#[test]
fn bind_mounts_inner() {
    if std::env::var_os("BS_BIND_INNER").is_none() {
        return;
    }
    let lab = Lab::new("bind-inner");
    // The data that must survive lives outside the area the service is limited to.
    let outside = Fixture::new("bind-outside");
    let keep = |name: &str| outside.path().join(name);
    let d = lab.home();
    let mut binds = Binds(Vec::new());

    // 1. A bind mount inside a folder that is deleted.
    lab.write("inside/victim/own.txt", "mine");
    lab.mkdir("inside/victim/sub");
    fs::create_dir_all(keep("one")).unwrap();
    fs::write(keep("one/keep1.txt"), bytes_of(100_000)).unwrap();
    bind(&keep("one"), &lab.p("inside/victim/sub"));
    binds.0.push(lab.p("inside/victim/sub"));
    assert!(
        lab.exists("inside/victim/sub/keep1.txt"),
        "the mount is in place"
    );
    let items = [item(&lab, "inside/victim")];
    let outcome = only(d.delete_permanently(&items, None));
    let Outcome::Failed { error } = &outcome else {
        panic!("a folder holding a mount cannot be deleted whole: {outcome:?}");
    };
    assert_eq!(error.kind, io::ErrorKind::InvalidInput, "{error:?}");
    assert!(
        keep("one/keep1.txt").exists(),
        "data reached through the mount must survive"
    );
    assert_eq!(
        fs::read_to_string(lab.p("inside/victim/sub/keep1.txt"))
            .unwrap()
            .len(),
        100_000
    );

    // 2. The item is itself a bind mount.
    lab.mkdir("inside/victim2");
    fs::create_dir_all(keep("two")).unwrap();
    fs::write(keep("two/keep2.txt"), "two").unwrap();
    bind(&keep("two"), &lab.p("inside/victim2"));
    binds.0.push(lab.p("inside/victim2"));
    let items = [item(&lab, "inside/victim2")];
    assert_eq!(
        d.precheck_permanent(&items, None).entries[0].decision,
        Decision::Refused(RefuseReason::MountPoint)
    );
    assert_eq!(
        refused(&only(d.delete_permanently(&items, None))),
        &RefuseReason::MountPoint
    );
    assert_eq!(
        refused(&only(d.trash(&items, None))),
        &RefuseReason::MountPoint
    );
    assert_eq!(fs::read_to_string(keep("two/keep2.txt")).unwrap(), "two");

    // 3. A folder with a mount in it goes to the Trash without counting the
    // mounted data, and emptying the Trash does not go into the mount.
    lab.write("inside/holder/note.txt", "n");
    lab.mkdir("inside/holder/mnt");
    fs::create_dir_all(keep("three")).unwrap();
    fs::write(keep("three/keep3.txt"), bytes_of(200_000)).unwrap();
    bind(&keep("three"), &lab.p("inside/holder/mnt"));
    binds.0.push(lab.p("inside/holder/mnt"));
    let (_, bytes, _) = moved(only(d.trash(&[item(&lab, "inside/holder")], None)));
    assert!(bytes < 100_000, "mounted data must not be counted: {bytes}");
    binds.0.push(lab.p("data/Trash/files/holder/mnt"));
    assert!(lab.exists("data/Trash/files/holder/mnt/keep3.txt"));
    let outcome = d.empty_bin(&drive_at(lab.fx.path(), "fixture"));
    assert!(
        matches!(outcome, EmptyOutcome::Partial { .. }),
        "the mount cannot be emptied: {outcome:?}"
    );
    assert_eq!(
        fs::read_to_string(keep("three/keep3.txt")).unwrap().len(),
        200_000
    );
    assert!(!lab.exists("data/Trash/files/holder/note.txt"));
}

/// Runs the bind-mount test in a user and mount namespace, where an ordinary
/// user may mount. It needs `unshare` and unprivileged user namespaces; where
/// they are missing it says so and skips, unless `BYTESCAPE_REQUIRE_NAMESPACES`
/// is set, as it should be on CI runners that have them.
#[test]
fn bind_mounts_are_never_entered_or_deleted_through() {
    if std::env::var_os("BS_BIND_INNER").is_some() {
        return;
    }
    let unshare = |extra: &[&str]| {
        let mut command = std::process::Command::new("unshare");
        command
            .args(["--user", "--map-root-user", "--mount"])
            .args(extra);
        command
    };
    let works = unshare(&["true"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !works {
        assert!(
            std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
            "user and mount namespaces are required here but unavailable"
        );
        eprintln!("skipped: unprivileged user and mount namespaces are not available");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let output = unshare(&["--"])
        .arg(&exe)
        .args([
            "--exact",
            "delete::tests::bind_mounts_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("BS_BIND_INNER", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("1 passed"),
        "the inner test did not run: {text}"
    );
}

// ------------------------------------------- Acting on what was checked

#[test]
fn the_action_goes_through_the_folder_that_was_checked() {
    let lab = Lab::new("held-parent");
    lab.write("work/a/f.txt", "x");
    lab.mkdir("work/b");
    // Another name for the same file, in another folder.
    fs::hard_link(lab.p("work/a/f.txt"), lab.p("work/b/f.txt")).unwrap();
    let d = lab.home();
    let seen = item(&lab, "work/a/f.txt");
    let (plan, resolved) = d.plan(std::slice::from_ref(&seen), None, Mode::Trash);
    assert_eq!(plan.entries[0].decision, Decision::Proceed);
    let planned = resolved[0].clone().unwrap();

    // The folder that was checked is moved away and its name now leads to the
    // folder with the other name. The file's identity is the same.
    fs::rename(lab.p("work/a"), lab.p("work/a-moved")).unwrap();
    symlink("b", lab.p("work/a")).unwrap();
    assert_eq!(
        identity_of(&lab.p("work/a/f.txt")).unwrap(),
        planned.identity
    );

    for outcome in [d.trash_one(&seen, &planned), d.delete_one(&seen, &planned)] {
        assert_eq!(refused(&outcome), &RefuseReason::ChangedSinceScan);
    }
    assert_eq!(
        lab.read("work/b/f.txt"),
        "x",
        "the other name must be left alone"
    );
    assert_eq!(lab.read("work/a-moved/f.txt"), "x");
    assert!(!lab.exists("data/Trash"));
}

#[test]
fn an_ordinary_item_is_not_on_a_mount_point() {
    // The positive case, with a real bind mount, is in
    // `bind_mounts_are_never_entered_or_deleted_through`. This one checks that
    // an item sharing its parent's mount ID is not taken for a mount point.
    let lab = Lab::new("mount-point");
    lab.write("work/f.txt", "x");
    let d = lab.home();
    let found = d.resolve(&lab.p("work/f.txt")).unwrap();
    assert!(!found.on_mount_point);
}

// ---------------------------------------------------------- The home Trash

#[test]
fn a_missing_data_folder_is_made_for_the_home_trash() {
    let lab = Lab::new("home-created");
    lab.write("work/f.txt", "x");
    // Neither `.local` nor `share` exists yet.
    let d = Disposal::new(lab.knowledge(), Vec::new())
        .restricted_to(lab.fx.path())
        .with_trash_env(lab.uid, Some(lab.p("fresh/.local/share")))
        .with_topdir(lab.fx.path());
    // The check alone makes nothing.
    let plan = d.precheck(&[item(&lab, "work/f.txt")], None);
    assert_eq!(plan.entries[0].decision, Decision::Proceed);
    assert!(!lab.exists("fresh"));

    let (token, _, _) = moved(only(d.trash(&[item(&lab, "work/f.txt")], None)));
    assert_eq!(lab.read("fresh/.local/share/Trash/files/f.txt"), "x");
    for folder in [
        "fresh/.local",
        "fresh/.local/share",
        "fresh/.local/share/Trash",
    ] {
        let mode = fs::metadata(lab.p(folder)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{folder}");
    }
    assert!(matches!(d.restore(&token), RestoreOutcome::Restored { .. }));
}

// --------------------------------------------- Folders that hold blocked places

#[test]
fn a_protected_folder_that_holds_a_blocked_place_is_blocked_even_when_confirmed() {
    let lab = Lab::new("blocked-below");
    lab.write("work/ordinary/inner.txt", "x");
    let d = lab.home();
    // Only looks at the paths: `/var` is protected and holds the blocked
    // package databases, so a confirmation does not make it deletable.
    for path in ["/var", "/var/lib"] {
        let confirmation = ProtectedConfirmation::for_paths([path]);
        for plan in [
            d.precheck(&[Item::new(path)], Some(&confirmation)),
            d.precheck_permanent(&[Item::new(path)], Some(&confirmation)),
        ] {
            assert_eq!(plan.strictest, Level::Blocked, "{path}");
            assert!(plan.selection_refused, "{path}");
            assert!(
                matches!(
                    plan.entries[0].decision,
                    Decision::Refused(RefuseReason::Blocked { .. })
                ),
                "{path}: {:?}",
                plan.entries[0].decision
            );
        }
    }
    // The reason and the note say what is true of the folder itself, not what
    // the matching entry describes: `/var` is not the package database.
    for path in ["/var", "/var/lib"] {
        let confirmation = ProtectedConfirmation::for_paths([path]);
        let plan = d.precheck(&[Item::new(path)], Some(&confirmation));
        assert_eq!(plan.entries[0].reason, Reason::HoldsBlocked, "{path}");
        assert_eq!(plan.entries[0].note, Some(HOLDS_BLOCKED_NOTE), "{path}");
        assert_eq!(
            plan.entries[0].decision,
            Decision::Refused(RefuseReason::Blocked {
                reason: Reason::HoldsBlocked,
                note: Some(HOLDS_BLOCKED_NOTE),
            }),
            "{path}"
        );
    }
    assert!(!HOLDS_BLOCKED_NOTE.to_lowercase().contains("dpkg"));
    // A place that is blocked itself keeps its own reason and note.
    let plan = d.precheck(&[Item::new("/var/lib/dpkg")], None);
    assert_eq!(plan.entries[0].reason, Reason::Table);
    assert!(
        plan.entries[0].note.is_some_and(|n| n.contains("dpkg")),
        "{:?}",
        plan.entries[0].note
    );
    // An ordinary folder with nothing blocked inside is not affected.
    let plan = d.precheck_permanent(&[item(&lab, "work/ordinary")], None);
    assert_eq!(plan.entries[0].decision, Decision::Proceed);
    assert_eq!(plan.strictest, Level::Normal);
}
