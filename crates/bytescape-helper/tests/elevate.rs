//! The whole path from the interface's side: `bs_platform::elevate` starts the
//! helper through a stand-in for `pkexec` (which does what `pkexec` does for
//! the helper's checks: it sets `PKEXEC_UID` and runs the program), and the
//! answers come back through the real client.
//!
//! The stand-in does not ask for a password; the prompt itself is the one part
//! of the launch that cannot be tried here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![allow(clippy::cast_possible_truncation)]
#![cfg(target_os = "linux")]

mod common;

use bs_ipc::{Backend, ClientError, ItemOutcome, Refusal, ScanEnd, ScanEvent, ScanRequest};
use bs_platform::elevate::{DeleteTarget, ElevateError, Elevation};
use common::{Fixture, helper_path};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The tests write a script and then run it. A program started by another
/// test at that moment would hold the script open for writing, and the system
/// would refuse to run it ("text file busy"), so the tests take turns.
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    TURN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A stand-in for `pkexec`, made in the fixture: sets `PKEXEC_UID` as the real
/// one does and runs the program it is given.
fn fake_pkexec(fx: &Fixture, body: &str) -> PathBuf {
    let path = fx.p("bin/pkexec");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const PASS_THROUGH: &str = "PKEXEC_UID=$(id -u) exec \"$@\"";

fn elevation(fx: &Fixture, body: &str) -> Elevation {
    Elevation::with_helper(helper_path()).with_pkexec(fake_pkexec(fx, body))
}

fn target(path: &Path) -> DeleteTarget {
    DeleteTarget {
        path: path.to_path_buf(),
        confirmed_protected: false,
        seen: None,
    }
}

#[test]
fn an_elevated_delete_goes_through_launch_handshake_and_answer() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-delete");
    fx.write("work/a", "a");
    fx.write("work/d/b", "b");
    fx.write("work/keep", "k");
    let outcomes = elevation(&fx, PASS_THROUGH)
        .delete(&[target(&fx.p("work/a")), target(&fx.p("work/d"))])
        .unwrap();
    assert!(matches!(
        outcomes[0],
        ItemOutcome::Deleted { entries: 1, .. }
    ));
    assert!(matches!(
        outcomes[1],
        ItemOutcome::Deleted { entries: 2, .. }
    ));
    assert!(!fx.exists("work/a") && !fx.exists("work/d"));
    assert!(fx.exists("work/keep"));
}

#[test]
fn an_elevated_delete_reports_a_refusal_as_a_refusal() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-refuse");
    fx.write("work/a", "a");
    let outcomes = elevation(&fx, PASS_THROUGH)
        .delete(&[target(&fx.p("work/a")), target(&fx.p("work/missing"))])
        .unwrap();
    assert!(matches!(outcomes[0], ItemOutcome::Deleted { .. }));
    assert_eq!(outcomes[1], ItemOutcome::Refused(Refusal::NotFound));
}

#[test]
fn an_elevated_scan_streams_the_tree_and_ends_with_done() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-scan");
    for i in 0..30 {
        fx.write(&format!("tree/d{}/f{i}", i % 5), &"x".repeat(i * 10));
    }
    let mut scan = elevation(&fx, PASS_THROUGH)
        .scan(ScanRequest {
            volume: bs_ipc::path_to_wire(&fx.p("tree")),
            backend: Backend::Walker,
            extra_mount_ids: vec![],
            places: bs_platform::elevate::user_places(),
        })
        .unwrap();
    let mut batches = 0;
    let mut done = None;
    while let Some(event) = scan.next_event().unwrap() {
        match event {
            ScanEvent::Batch(_) => batches += 1,
            ScanEvent::Done(d) => done = Some(d),
            _ => {}
        }
    }
    let done = done.expect("the scan ended with a done event");
    assert!(batches > 0);
    assert_eq!(done.end, ScanEnd::Completed);
    assert_eq!(done.progress.files, 30);
    assert!(scan.next_event().unwrap().is_none());
}

#[test]
fn a_scan_can_be_stopped_from_another_thread() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-cancel");
    for d in 0..400 {
        let dir = fx.p(&format!("big/d{d:03}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            fs::File::create(dir.join(format!("file-with-a-longer-name-{f:03}"))).unwrap();
        }
    }
    let mut scan = elevation(&fx, PASS_THROUGH)
        .scan(ScanRequest {
            volume: bs_ipc::path_to_wire(&fx.p("big")),
            backend: Backend::Walker,
            extra_mount_ids: vec![],
            places: vec![],
        })
        .unwrap();
    let stop = scan.canceller();
    // Nothing has been read, so the helper is blocked writing: still scanning.
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::thread::spawn(move || stop.cancel()).join().unwrap();
    let mut last = None;
    while let Some(event) = scan.next_event().unwrap() {
        last = Some(event);
    }
    match last {
        Some(ScanEvent::Done(done)) => assert_eq!(done.end, ScanEnd::Cancelled),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_dismissed_or_failed_prompt_is_reported_as_declined() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-declined");
    for code in [126, 127] {
        let result = elevation(&fx, &format!("exit {code}")).delete(&[target(&fx.p("anything"))]);
        let error = result.unwrap_err();
        assert!(
            matches!(error.error, ElevateError::Declined),
            "{code}: {error}"
        );
        assert!(error.completed.is_empty());
    }
}

#[test]
fn a_program_that_does_not_speak_the_protocol_is_not_taken_for_the_helper() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-impostor");
    fx.write("work/f", "x");
    // Exits at once, says nothing.
    let error = elevation(&fx, "exit 0")
        .delete(&[target(&fx.p("work/f"))])
        .unwrap_err();
    assert!(matches!(
        error.error,
        ElevateError::Protocol(ClientError::Disconnected | ClientError::Write(_))
    ));
    // Answers with garbage.
    let error = elevation(&fx, "printf 'not a frame at all'; exec cat >/dev/null")
        .delete(&[target(&fx.p("work/f"))])
        .unwrap_err();
    assert!(matches!(error.error, ElevateError::Protocol(_)), "{error}");
    assert!(fx.exists("work/f"));
}

#[test]
fn the_helper_refuses_to_start_when_pkexec_names_another_user() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-wrong-user");
    fx.write("work/f", "x");
    let error = elevation(&fx, "PKEXEC_UID=4242 exec \"$@\"")
        .delete(&[target(&fx.p("work/f"))])
        .unwrap_err();
    assert!(
        matches!(error.error, ElevateError::PeerNotVerified(_)),
        "{error}"
    );
    assert!(fx.exists("work/f"));
}

#[test]
fn a_missing_helper_or_pkexec_is_reported_before_anything_starts() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    let fx = Fixture::new("elevate-missing");
    let no_helper =
        Elevation::with_helper(fx.p("no-such-helper")).with_pkexec(fake_pkexec(&fx, PASS_THROUGH));
    let error = no_helper.delete(&[target(&fx.p("x"))]).unwrap_err();
    assert!(
        matches!(error.error, ElevateError::Unavailable(_)),
        "{error}"
    );
    let no_pkexec = Elevation::with_helper(helper_path()).with_pkexec(fx.p("no-such-pkexec"));
    let error = no_pkexec.delete(&[target(&fx.p("x"))]).unwrap_err();
    assert!(matches!(error.error, ElevateError::Launch(_)), "{error}");
}

#[test]
fn a_delete_asked_of_a_scan_helper_is_never_done() {
    if common::refuses_the_superuser() {
        return;
    }
    let _turn = one_at_a_time();
    // The launch decides what the user authorized; the interface API cannot
    // mix them up, but a hand-made conversation must not either.
    let fx = Fixture::new("elevate-mixed");
    fx.write("work/f", "x");
    let mut helper = common::Launch::new("scan").spawn();
    let mut client = helper.client();
    let failure = client
        .delete(bs_ipc::DeleteRequest {
            permanent: true,
            items: vec![bs_ipc::DeleteItem {
                path: bs_ipc::path_to_wire(&fx.p("work/f")),
                confirmed_protected: false,
                seen: None,
            }],
        })
        .unwrap_err();
    assert!(failure.completed.is_empty());
    assert!(fx.exists("work/f"));
}
