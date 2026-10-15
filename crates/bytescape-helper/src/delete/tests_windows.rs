//! Tests of the delete judge on Windows. They have not run on a Windows
//! machine when this was written; the Windows runner in CI runs them.
//!
//! Every path a test deletes is made by [`Lab::p`] inside a folder made for the
//! test, and the protected-path table is given a home folder and drive roots
//! inside it, so the rules are exercised without touching the system.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use super::*;
use bs_ipc::path_to_wire;
use bs_knowledge::{Environment, Host};
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Lab {
    root: PathBuf,
}

impl Lab {
    fn new(label: &str) -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "bs-helper-judge-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        // The final path of the folder, in the plain form the judge compares.
        let root = PathBuf::from(bs_fsops::winpath::strip_extended_prefix(
            &fs::canonicalize(&root).unwrap().to_string_lossy(),
        ));
        Self { root }
    }

    fn p(&self, relative: &str) -> PathBuf {
        assert!(!relative.contains(".."), "{relative}");
        self.root.join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.p(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn exists(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.p(relative)).is_ok()
    }

    /// A home folder and a drive root inside the fixture stand in for the
    /// system's: the first is protected, the second blocked.
    fn judge(&self) -> Judge {
        let env = Environment::new()
            .with(
                "USERPROFILE",
                self.p("protected-home").to_string_lossy().as_bytes(),
            )
            .with_drive_roots([self.p("blocked-root")]);
        Judge::new(Knowledge::for_host(&env, Host::Windows))
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let named = self
            .root
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("bs-helper-judge-"));
        if named {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn item(path: &Path) -> DeleteItem {
    DeleteItem {
        path: path_to_wire(path),
        confirmed_protected: false,
        seen: None,
    }
}

fn run(judge: &Judge, items: Vec<DeleteItem>) -> Vec<ItemOutcome> {
    let request = DeleteRequest {
        permanent: true,
        items,
    };
    let mut outcomes = Vec::new();
    judge.run(
        &request,
        || true,
        |_, o| {
            outcomes.push(o);
            true
        },
    );
    outcomes
}

fn refusal(outcome: &ItemOutcome) -> Refusal {
    match outcome {
        ItemOutcome::Refused(reason) => *reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_file_and_a_folder_are_deleted() {
    let lab = Lab::new("basic");
    lab.write("work/a.txt", "hello");
    lab.write("work/d/inner/b", "world");
    lab.write("work/keep", "k");
    let outcomes = run(
        &lab.judge(),
        vec![item(&lab.p("work/a.txt")), item(&lab.p("work/d"))],
    );
    assert!(
        matches!(outcomes[0], ItemOutcome::Deleted { entries: 1, .. }),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[1], ItemOutcome::Deleted { entries: 3, .. }),
        "{outcomes:?}"
    );
    assert!(!lab.exists("work/a.txt") && !lab.exists("work/d"));
    assert!(lab.exists("work/keep"));
}

#[test]
fn a_name_with_an_unpaired_surrogate_never_reaches_its_look_alike() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let lab = Lab::new("surrogate");
    lab.write("work/keep", "k");
    let mut units: Vec<u16> = "a".encode_utf16().collect();
    units.push(0xD800);
    let odd = lab.p("work").join(OsString::from_wide(&units));
    let lookalike = lab.p("work").join("a\u{FFFD}");
    // Only the look-alike exists: asking for the odd name finds nothing, and
    // must not delete its neighbour.
    fs::write(&lookalike, "the file next to it").unwrap();
    let outcomes = run(&lab.judge(), vec![item(&odd)]);
    assert_eq!(refusal(&outcomes[0]), Refusal::NotFound, "{outcomes:?}");
    assert!(lookalike.exists());
    // Both exist: the one that was named goes and the other stays.
    if fs::write(&odd, "the one that was named").is_err() {
        eprintln!("NOT RUN: this filesystem refuses a name with an unpaired surrogate");
        return;
    }
    let outcomes = run(&lab.judge(), vec![item(&odd)]);
    assert!(
        matches!(outcomes[0], ItemOutcome::Deleted { entries: 1, .. }),
        "{outcomes:?}"
    );
    assert!(fs::symlink_metadata(&odd).is_err());
    assert!(lookalike.exists());
    assert!(lab.exists("work/keep"));
}

#[test]
fn a_different_spelling_of_the_case_names_the_same_place() {
    let lab = Lab::new("case");
    lab.write("Work/File.txt", "x");
    let shouted = PathBuf::from(lab.p("Work/File.txt").to_string_lossy().to_uppercase());
    let outcomes = run(&lab.judge(), vec![item(&shouted)]);
    assert!(
        matches!(outcomes[0], ItemOutcome::Deleted { .. }),
        "{outcomes:?}"
    );
}

#[test]
fn a_drive_root_is_blocked_in_every_spelling() {
    let lab = Lab::new("root");
    fs::create_dir_all(lab.p("blocked-root")).unwrap();
    let plain = lab.p("blocked-root").to_string_lossy().into_owned();
    for spelling in [
        plain.clone(),
        format!("{plain}\\"),
        plain.to_uppercase(),
        plain.replace('\\', "/"),
        format!(r"\\?\{plain}"),
        format!("{plain}\\."),
        format!("{plain}\\x\\.."),
    ] {
        let outcomes = run(&lab.judge(), vec![item(Path::new(&spelling))]);
        assert!(
            matches!(refusal(&outcomes[0]), Refusal::Blocked | Refusal::BadPath),
            "{spelling}"
        );
    }
    assert!(lab.exists("blocked-root"));
}

#[test]
fn the_windows_folder_and_its_dressed_up_spellings_are_blocked_without_touching_the_disk() {
    let lab = Lab::new("system");
    let judge = lab.judge();
    for path in [
        r"C:\Windows\bytescape-test-absent",
        r"c:\WINDOWS\bytescape-test-absent",
        r"\\?\C:\Windows\bytescape-test-absent",
        r"C:\Windows\System32\bytescape-test-absent",
        r"C:\Program Files",
        r"C:\System Volume Information\x",
    ] {
        assert_eq!(judge.level_of(Path::new(path)), Level::Blocked, "{path}");
    }
}

#[test]
fn a_protected_path_needs_its_own_confirmation() {
    let lab = Lab::new("protected");
    fs::create_dir_all(lab.p("protected-home/docs")).unwrap();
    lab.write("work/f", "x");
    let judge = lab.judge();
    let outcomes = run(
        &judge,
        vec![item(&lab.p("work/f")), item(&lab.p("protected-home"))],
    );
    assert_eq!(refusal(&outcomes[1]), Refusal::ProtectedNotConfirmed);
    assert_eq!(
        refusal(&outcomes[0]),
        Refusal::SelectionRefused { driver: 1 }
    );
    assert!(lab.exists("work/f") && lab.exists("protected-home/docs"));
    let mut confirmed = item(&lab.p("protected-home"));
    confirmed.confirmed_protected = true;
    let outcomes = run(&judge, vec![confirmed]);
    assert!(
        matches!(outcomes[0], ItemOutcome::Deleted { .. }),
        "{outcomes:?}"
    );
}

#[test]
fn paths_that_make_no_sense_refuse_the_whole_request() {
    let lab = Lab::new("bad");
    lab.write("work/f", "x");
    for bad in [r"relative\path", "", r"C:", r"\\?\C:\a\..\b", r"C:\a\.\b"] {
        let outcomes = run(
            &lab.judge(),
            vec![item(&lab.p("work/f")), item(Path::new(bad))],
        );
        assert_eq!(refusal(&outcomes[1]), Refusal::BadPath, "{bad:?}");
        assert_eq!(
            refusal(&outcomes[0]),
            Refusal::SelectionRefused { driver: 1 }
        );
    }
    assert!(lab.exists("work/f"));
}

#[test]
fn a_link_is_deleted_as_a_link_and_its_target_is_untouched() {
    let lab = Lab::new("link");
    lab.write("outside/precious", "do not touch");
    fs::create_dir_all(lab.p("work")).unwrap();
    // Making a link needs the right to; a runner that lacks it cannot try.
    if std::os::windows::fs::symlink_dir(lab.p("outside"), lab.p("work/link")).is_err() {
        println!("NOT RUN: cannot make a symbolic link here");
        return;
    }
    let outcomes = run(&lab.judge(), vec![item(&lab.p("work/link"))]);
    assert!(
        matches!(outcomes[0], ItemOutcome::Deleted { .. }),
        "{outcomes:?}"
    );
    assert!(!lab.exists("work/link"));
    assert!(lab.exists("outside/precious"));
}

#[test]
fn a_path_that_goes_through_a_link_is_refused_not_followed() {
    let lab = Lab::new("through");
    lab.write("real/file", "x");
    if std::os::windows::fs::symlink_dir(lab.p("real"), lab.p("alias")).is_err() {
        println!("NOT RUN: cannot make a symbolic link here");
        return;
    }
    let outcomes = run(&lab.judge(), vec![item(&lab.p("alias/file"))]);
    assert_eq!(refusal(&outcomes[0]), Refusal::NotRealPath);
    assert!(lab.exists("real/file"));
}
