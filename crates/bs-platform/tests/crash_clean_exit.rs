//! A panic that the program survives is not a crash to report next time, but
//! one it does not survive is. The panic hook is global to a process, so this
//! is a program of its own with one test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bs_platform::crash::{AppInfo, install_in, mark_clean_exit, pending_report_in};
use std::path::PathBuf;

fn reports(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("crash-") && n.ends_with(".txt"))
        .collect();
    names.sort();
    names
}

#[test]
fn a_panic_that_was_caught_is_not_offered_after_a_normal_exit() {
    let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let dir: PathBuf = base.join(format!("bs-platform-clean-exit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A report from an earlier run that really did crash.
    let earlier = dir.join("crash-20200101T000000Z-1-0000.txt");
    std::fs::write(&earlier, "the earlier crash\n").unwrap();

    install_in(
        AppInfo::new("bytescape", "0.1.0", 1_790_000_000, "test-target"),
        dir.clone(),
    );
    let caught = std::panic::catch_unwind(|| {
        panic!("a worker failed and its owner carried on");
    });
    assert!(caught.is_err());

    // The hook cannot know the panic will be caught, so it wrote a report, and
    // while the program is running that report is the newest pending one.
    assert_eq!(reports(&dir).len(), 2, "{:?}", reports(&dir));
    let pending = pending_report_in(&dir).unwrap();
    assert!(pending.render(true).contains("a worker failed"));

    // After a normal exit it is not offered. The earlier crash still is, and
    // nothing was deleted.
    mark_clean_exit();
    let pending = pending_report_in(&dir).unwrap();
    assert!(pending.render(true).contains("the earlier crash"));
    assert_eq!(reports(&dir).len(), 2);
    // Saying it twice changes nothing.
    mark_clean_exit();
    assert!(pending_report_in(&dir).is_some());

    // Only the folder this test made.
    if dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("bs-platform-clean-exit-"))
    {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
