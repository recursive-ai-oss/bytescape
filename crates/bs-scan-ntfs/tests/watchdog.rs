//! The phase watchdog the long Windows tests use, tested where it can run.

mod common;

use common::watchdog::phase;
use std::time::{Duration, Instant};

#[test]
fn a_phase_that_finishes_returns_its_value() {
    let v = phase("quick", Duration::from_secs(30), || 40 + 2);
    assert_eq!(v, 42);
}

#[test]
fn a_phase_that_hangs_fails_with_its_name_and_does_not_wait_for_it() {
    let begun = Instant::now();
    let caught = std::panic::catch_unwind(|| {
        phase("walk the drive", Duration::from_millis(200), || {
            std::thread::sleep(Duration::from_secs(20));
        });
    });
    let message = caught
        .expect_err("the hung phase must fail")
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(message.contains("walk the drive"), "{message}");
    assert!(message.contains("did not finish"), "{message}");
    assert!(
        begun.elapsed() < Duration::from_secs(10),
        "{:?}",
        begun.elapsed()
    );
}

#[test]
fn a_panic_inside_a_phase_is_passed_on() {
    let caught = std::panic::catch_unwind(|| {
        phase("read", Duration::from_secs(30), || -> u32 {
            panic!("the reader said no");
        })
    });
    let message = caught
        .expect_err("must fail")
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert_eq!(message, "the reader said no");
}
