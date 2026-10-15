//! Tests of the delete judge. Every test works inside a fixture folder made
//! for it; the protected-path table is given a home folder and drive roots
//! inside that folder, so the rules are exercised without touching the
//! system. Paths are made only by [`Lab::p`], which refuses to leave the
//! fixture.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use super::*;
use crate::testutil::Fixture;
use bs_ipc::{Seen, path_to_wire};
use bs_knowledge::{Environment, Host, Level};
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};

struct Lab {
    fx: Fixture,
}

impl Lab {
    fn new(label: &str) -> Self {
        Self {
            fx: Fixture::new(label),
        }
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

    fn exists(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.p(relative)).is_ok()
    }

    /// The home folder and two drive roots of the fixture stand in for the
    /// system's: the first is protected, the others blocked.
    fn judge(&self) -> Judge {
        let env = Environment::new()
            .with(
                "HOME",
                self.p("protected-home").as_os_str().as_encoded_bytes(),
            )
            .with_drive_roots([self.p("blocked-root"), self.p("holder/inner-root")]);
        Judge::new(Knowledge::for_host(&env, Host::Linux))
    }
}

fn item(path: &Path) -> DeleteItem {
    DeleteItem {
        path: path_to_wire(path),
        confirmed_protected: false,
        seen: None,
    }
}

fn request(items: Vec<DeleteItem>) -> DeleteRequest {
    DeleteRequest {
        permanent: true,
        items,
    }
}

fn run(judge: &Judge, request: &DeleteRequest) -> Vec<ItemOutcome> {
    let mut outcomes = Vec::new();
    let reported = judge.run(
        request,
        || true,
        |_, o| {
            outcomes.push(o);
            true
        },
    );
    assert_eq!(reported, request.items.len());
    outcomes
}

fn refusal(outcome: &ItemOutcome) -> Refusal {
    match outcome {
        ItemOutcome::Refused(reason) => *reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn allocated(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

// ------------------------------------------------------------- Deleting

#[test]
fn a_file_is_deleted_and_the_bytes_are_the_allocated_size() {
    let lab = Lab::new("file");
    lab.write("work/a.bin", &"x".repeat(10_000));
    let bytes = allocated(&lab.p("work/a.bin"));
    let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("work/a.bin"))]));
    assert_eq!(
        outcomes,
        vec![ItemOutcome::Deleted {
            bytes,
            entries: 1,
            shared_files: 0
        }]
    );
    assert!(!lab.exists("work/a.bin"));
    assert!(lab.exists("work"));
}

#[test]
fn a_folder_is_deleted_with_everything_in_it() {
    let lab = Lab::new("folder");
    lab.write("work/d/one", "1");
    lab.write("work/d/sub/two", "22");
    lab.write("work/keep", "k");
    let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("work/d"))]));
    match outcomes[0] {
        ItemOutcome::Deleted { entries, .. } => assert_eq!(entries, 4),
        other => panic!("{other:?}"),
    }
    assert!(!lab.exists("work/d"));
    assert!(lab.exists("work/keep"));
}

#[test]
fn a_link_is_deleted_as_a_link_and_never_followed() {
    let lab = Lab::new("link");
    lab.write("outside/precious", "do not touch");
    lab.mkdir("work/d");
    symlink(lab.p("outside"), lab.p("work/d/to-outside")).unwrap();
    symlink(lab.p("outside"), lab.p("work/direct")).unwrap();
    let outcomes = run(
        &lab.judge(),
        &request(vec![item(&lab.p("work/d")), item(&lab.p("work/direct"))]),
    );
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, ItemOutcome::Deleted { .. }))
    );
    assert!(!lab.exists("work/d") && !lab.exists("work/direct"));
    assert!(lab.exists("outside/precious"));
}

#[test]
fn a_path_that_goes_through_a_link_is_refused_not_followed() {
    let lab = Lab::new("through");
    lab.write("real/file", "x");
    symlink(lab.p("real"), lab.p("alias")).unwrap();
    let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("alias/file"))]));
    assert_eq!(refusal(&outcomes[0]), Refusal::NotRealPath);
    assert!(lab.exists("real/file"));
}

#[test]
fn spellings_of_the_same_place_that_differ_are_not_accepted() {
    let lab = Lab::new("spelling");
    lab.write("work/f", "x");
    // A doubled separator and a trailing one name the same file; the helper
    // acts only on the real spelling.
    let doubled = PathBuf::from(format!("{}//work/f", lab.fx.path().display()));
    let trailing = PathBuf::from(format!("{}/work/f/", lab.fx.path().display()));
    for odd in [doubled, trailing] {
        let outcomes = run(&lab.judge(), &request(vec![item(&odd)]));
        assert_eq!(refusal(&outcomes[0]), Refusal::NotRealPath, "{odd:?}");
        assert!(lab.exists("work/f"));
    }
}

#[test]
fn a_missing_item_is_reported_and_does_not_stop_the_others() {
    let lab = Lab::new("missing");
    lab.write("work/b", "b");
    let outcomes = run(
        &lab.judge(),
        &request(vec![item(&lab.p("work/nope")), item(&lab.p("work/b"))]),
    );
    assert_eq!(refusal(&outcomes[0]), Refusal::NotFound);
    assert!(matches!(outcomes[1], ItemOutcome::Deleted { .. }));
}

#[test]
fn an_item_that_is_not_what_was_seen_is_left_alone() {
    let lab = Lab::new("seen");
    lab.write("work/f", "x");
    let meta = fs::symlink_metadata(lab.p("work/f")).unwrap();
    let mut wrong = item(&lab.p("work/f"));
    wrong.seen = Some(Seen {
        volume: meta.dev(),
        file: u128::from(meta.ino()) + 1,
    });
    let outcomes = run(&lab.judge(), &request(vec![wrong]));
    assert_eq!(refusal(&outcomes[0]), Refusal::ChangedSinceScan);
    assert!(lab.exists("work/f"));
    let mut right = item(&lab.p("work/f"));
    right.seen = Some(Seen {
        volume: meta.dev(),
        file: u128::from(meta.ino()),
    });
    let outcomes = run(&lab.judge(), &request(vec![right]));
    assert!(matches!(outcomes[0], ItemOutcome::Deleted { .. }));
}

#[test]
fn hard_linked_files_report_what_stays() {
    let lab = Lab::new("hardlink");
    lab.write("work/d/a", "data data data");
    fs::hard_link(lab.p("work/d/a"), lab.p("work/other-name")).unwrap();
    let folder_bytes = allocated(&lab.p("work/d"));
    let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("work/d"))]));
    match outcomes[0] {
        ItemOutcome::Deleted {
            bytes,
            shared_files,
            ..
        } => {
            assert_eq!(shared_files, 1);
            // The file keeps its other name, so only the folder's own blocks
            // are given back.
            assert_eq!(bytes, folder_bytes);
        }
        other => panic!("{other:?}"),
    }
    assert!(lab.exists("work/other-name"));
}

// -------------------------------------------------------------- Refusals

#[test]
fn a_request_that_is_not_permanent_deletes_nothing() {
    let lab = Lab::new("not-permanent");
    lab.write("work/f", "x");
    let mut r = request(vec![item(&lab.p("work/f"))]);
    r.permanent = false;
    let outcomes = run(&lab.judge(), &r);
    assert_eq!(refusal(&outcomes[0]), Refusal::NotPermanent);
    assert!(lab.exists("work/f"));
}

#[test]
fn a_blocked_path_is_refused_and_takes_the_whole_request_with_it() {
    let lab = Lab::new("blocked");
    lab.mkdir("blocked-root");
    lab.write("work/f", "x");
    let outcomes = run(
        &lab.judge(),
        &request(vec![item(&lab.p("work/f")), item(&lab.p("blocked-root"))]),
    );
    assert_eq!(refusal(&outcomes[1]), Refusal::Blocked);
    assert_eq!(
        refusal(&outcomes[0]),
        Refusal::SelectionRefused { driver: 1 }
    );
    assert!(lab.exists("work/f") && lab.exists("blocked-root"));
}

#[test]
fn a_blocked_root_is_refused_in_every_spelling_without_touching_the_disk() {
    let lab = Lab::new("dressed");
    lab.mkdir("blocked-root/child");
    let base = lab.fx.path().display().to_string();
    let spellings = [
        format!("{base}/blocked-root"),
        format!("{base}//blocked-root"),
        format!("{base}/blocked-root/"),
        format!("{base}///blocked-root//"),
    ];
    for spelling in spellings {
        let outcomes = run(&lab.judge(), &request(vec![item(Path::new(&spelling))]));
        assert_eq!(refusal(&outcomes[0]), Refusal::Blocked, "{spelling}");
    }
    // Dot parts are not canonical at all.
    for spelling in [
        format!("{base}/./blocked-root"),
        format!("{base}/work/../blocked-root"),
        format!("{base}/blocked-root/."),
        format!("{base}/blocked-root/child/.."),
    ] {
        let outcomes = run(&lab.judge(), &request(vec![item(Path::new(&spelling))]));
        assert_eq!(refusal(&outcomes[0]), Refusal::BadPath, "{spelling}");
    }
    assert!(lab.exists("blocked-root/child"));
}

#[test]
fn what_is_inside_a_drive_root_is_not_blocked_by_the_root() {
    let lab = Lab::new("inside-root");
    lab.write("blocked-root/child", "x");
    let outcomes = run(
        &lab.judge(),
        &request(vec![item(&lab.p("blocked-root/child"))]),
    );
    assert!(matches!(outcomes[0], ItemOutcome::Deleted { .. }));
}

#[test]
fn a_folder_that_holds_a_blocked_place_is_blocked_too() {
    // Judged by the text alone: nothing on the disk is looked at or changed.
    let lab = Lab::new("holds");
    let judge = lab.judge();
    for path in [
        "/var",
        "/var/lib",
        "/var/lib/dpkg",
        "/usr",
        "/usr/share/x",
        "/etc",
        "/boot",
    ] {
        assert_eq!(judge.level_of(Path::new(path)), Level::Blocked, "{path}");
    }
    // A folder beside them is only protected, or not at all.
    assert!(judge.level_of(Path::new("/var/tmp")) < Level::Blocked);
    assert_eq!(judge.level_of(&lab.p("work/f")), Level::Normal);
}

#[test]
fn blocked_places_are_blocked_in_every_spelling() {
    let lab = Lab::new("system-spellings");
    let judge = lab.judge();
    for path in [
        "//usr",
        "/usr/",
        "/usr//lib",
        "///etc",
        "/proc",
        "/sys/kernel",
        "/dev/sda",
        "/bin",
        "/lib64",
    ] {
        assert_eq!(judge.level_of(Path::new(path)), Level::Blocked, "{path}");
    }
}

#[test]
fn the_real_place_is_judged_as_well_as_the_text() {
    let lab = Lab::new("real-place");
    lab.mkdir("holder/inner-root/x");
    // Named through a link, the blocked place does not look blocked as text.
    symlink(lab.p("holder"), lab.p("via")).unwrap();
    let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("via/inner-root"))]));
    assert_eq!(refusal(&outcomes[0]), Refusal::Blocked);
    assert!(lab.exists("holder/inner-root/x"));
}

#[test]
fn a_protected_path_needs_its_own_confirmation() {
    let lab = Lab::new("protected");
    lab.mkdir("protected-home/docs");
    lab.write("work/f", "x");
    let judge = lab.judge();
    // Without the flag: refused, and the other item goes with it.
    let outcomes = run(
        &judge,
        &request(vec![item(&lab.p("work/f")), item(&lab.p("protected-home"))]),
    );
    assert_eq!(refusal(&outcomes[1]), Refusal::ProtectedNotConfirmed);
    assert_eq!(
        refusal(&outcomes[0]),
        Refusal::SelectionRefused { driver: 1 }
    );
    assert!(lab.exists("work/f") && lab.exists("protected-home/docs"));
    // The flag on another item does not count.
    let mut other = item(&lab.p("work/f"));
    other.confirmed_protected = true;
    let outcomes = run(
        &judge,
        &request(vec![other, item(&lab.p("protected-home"))]),
    );
    assert_eq!(refusal(&outcomes[1]), Refusal::ProtectedNotConfirmed);
    assert!(lab.exists("protected-home/docs"));
    // With its own flag it goes.
    let mut confirmed = item(&lab.p("protected-home"));
    confirmed.confirmed_protected = true;
    let outcomes = run(&judge, &request(vec![confirmed]));
    assert!(matches!(outcomes[0], ItemOutcome::Deleted { .. }));
    assert!(!lab.exists("protected-home"));
}

#[test]
fn a_confirmation_never_unblocks_a_blocked_path() {
    let lab = Lab::new("confirm-blocked");
    lab.mkdir("blocked-root");
    let mut it = item(&lab.p("blocked-root"));
    it.confirmed_protected = true;
    let outcomes = run(&lab.judge(), &request(vec![it]));
    assert_eq!(refusal(&outcomes[0]), Refusal::Blocked);
    assert!(lab.exists("blocked-root"));
}

#[test]
fn paths_that_make_no_sense_refuse_the_whole_request() {
    let lab = Lab::new("bad");
    lab.write("work/f", "x");
    let mut nul = item(&lab.p("work/f"));
    nul.path.extend_from_slice(b"\0x");
    let too_long = DeleteItem {
        path: format!("/{}", "a".repeat(bs_fsops::MAX_PATH_BYTES + 1)).into_bytes(),
        confirmed_protected: false,
        seen: None,
    };
    let cases = vec![
        item(Path::new("")),
        item(Path::new("relative/path")),
        item(Path::new("/")),
        nul,
        too_long,
    ];
    for bad in cases {
        let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("work/f")), bad]));
        assert_eq!(refusal(&outcomes[1]), Refusal::BadPath);
        assert_eq!(
            refusal(&outcomes[0]),
            Refusal::SelectionRefused { driver: 1 }
        );
        assert!(lab.exists("work/f"));
    }
}

#[test]
fn names_that_are_not_text_are_handled_by_their_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let lab = Lab::new("bytes");
    let odd = lab
        .p("work")
        .join(std::ffi::OsStr::from_bytes(b"caf\xe9-\xff"));
    fs::create_dir_all(lab.p("work")).unwrap();
    fs::write(&odd, "x").unwrap();
    let outcomes = run(&lab.judge(), &request(vec![item(&odd)]));
    assert!(matches!(outcomes[0], ItemOutcome::Deleted { .. }));
    assert!(fs::symlink_metadata(&odd).is_err());
}

#[test]
fn an_item_inside_another_item_is_left_to_the_outer_one() {
    let lab = Lab::new("overlap");
    lab.write("work/d/f", "x");
    lab.write("work/e", "e");
    let outcomes = run(
        &lab.judge(),
        &request(vec![
            item(&lab.p("work/d/f")),
            item(&lab.p("work/d")),
            item(&lab.p("work/e")),
            item(&lab.p("work/e")),
        ]),
    );
    assert_eq!(refusal(&outcomes[0]), Refusal::Overlaps { with: 1 });
    assert!(matches!(outcomes[1], ItemOutcome::Deleted { .. }));
    assert!(matches!(outcomes[2], ItemOutcome::Deleted { .. }));
    // The same path twice: only the later one is refused.
    assert_eq!(refusal(&outcomes[3]), Refusal::Overlaps { with: 2 });
    assert!(!lab.exists("work/d") && !lab.exists("work/e"));
}

#[test]
fn an_empty_request_has_no_outcomes() {
    let lab = Lab::new("empty");
    assert!(run(&lab.judge(), &request(vec![])).is_empty());
}

#[test]
fn stopping_between_items_leaves_the_rest_alone() {
    let lab = Lab::new("stop");
    lab.write("work/a", "a");
    lab.write("work/b", "b");
    let mut asked = 0;
    let mut outcomes = Vec::new();
    let reported = lab.judge().run(
        &request(vec![item(&lab.p("work/a")), item(&lab.p("work/b"))]),
        || {
            asked += 1;
            asked < 2
        },
        |_, o| {
            outcomes.push(o);
            true
        },
    );
    assert_eq!(reported, 1);
    assert_eq!(outcomes.len(), 1);
    assert!(!lab.exists("work/a") && lab.exists("work/b"));
}

// -------------------------------------------------------------- Failures

#[test]
fn a_failure_part_way_is_reported_as_a_failure_with_what_was_removed() {
    use std::os::unix::fs::PermissionsExt;
    if bs_fsops::walk::running_as_superuser() {
        // The superuser is not stopped by permissions; nothing to show.
        return;
    }
    let lab = Lab::new("partial");
    lab.write("work/d/free", "x");
    lab.write("work/d/locked/inner", "y");
    fs::set_permissions(lab.p("work/d/locked"), fs::Permissions::from_mode(0o500)).unwrap();
    let outcomes = run(&lab.judge(), &request(vec![item(&lab.p("work/d"))]));
    fs::set_permissions(lab.p("work/d/locked"), fs::Permissions::from_mode(0o700)).unwrap();
    match outcomes[0] {
        ItemOutcome::Failed {
            step,
            os_code,
            removed_entries,
            ..
        } => {
            assert_eq!(step, 2);
            assert_eq!(os_code, libc::EACCES);
            assert!(removed_entries >= 1, "the free file was removed");
        }
        other => panic!("a partial delete must not be reported as a success: {other:?}"),
    }
    assert!(lab.exists("work/d/locked/inner"));
}

// ------------------------------------------------------------------ Races

/// A folder swapped for a link to somewhere else while the delete runs must not
/// lead the delete out of the tree. The swap is racy, so the check runs many
/// rounds; the invariant holds whichever way each round goes.
#[test]
fn swapping_a_folder_for_a_link_during_the_delete_never_reaches_outside() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let lab = Lab::new("race");
    let judge = lab.judge();
    for round in 0..40 {
        lab.write("victim/precious-1", "keep");
        lab.write("victim/sub/precious-2", "keep");
        for d in 0..6 {
            for f in 0..30 {
                lab.write(&format!("tree/d{d}/sub/f{f}"), "x");
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let stop = Arc::clone(&stop);
            let victim = lab.p("victim");
            let tree = lab.p("tree");
            std::thread::spawn(move || {
                let mut n = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let d = n % 6;
                    let target = tree.join(format!("d{d}/sub"));
                    let moved = tree.join(format!("d{d}/sub-away"));
                    // Replace the folder with a link to the victim, then put it
                    // back; either step can fail when the delete got there first.
                    if fs::rename(&target, &moved).is_ok() {
                        let _ = symlink(&victim, &target);
                        std::thread::yield_now();
                        let _ = fs::remove_file(&target);
                        let _ = fs::rename(&moved, &target);
                    }
                    n += 1;
                }
            })
        };
        let outcomes = run(&judge, &request(vec![item(&lab.p("tree"))]));
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert!(
            lab.exists("victim/precious-1") && lab.exists("victim/sub/precious-2"),
            "round {round}: the delete reached the victim through a swapped link: {outcomes:?}"
        );
        // Remove what is left, inside the fixture, for the next round.
        let _ = fs::remove_dir_all(lab.p("tree"));
        let _ = fs::remove_dir_all(lab.p("victim"));
    }
}
