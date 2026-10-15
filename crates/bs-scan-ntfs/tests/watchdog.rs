//! The phase watchdog the long Windows tests use, tested where it can run.

mod common;

use common::watchdog::{Limits, phase, watch};
use std::sync::atomic::{AtomicU64, Ordering};
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

fn quick(stall_ms: u64, ceiling_ms: u64) -> Limits {
    Limits {
        poll: Duration::from_millis(5),
        report: Duration::from_millis(50),
        stall: Duration::from_millis(stall_ms),
        ceiling: Duration::from_millis(ceiling_ms),
    }
}

fn message_of(caught: Result<(), Box<dyn std::any::Any + Send>>) -> String {
    caught
        .expect_err("the watched phase must fail")
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_watched_phase_that_finishes_returns() {
    let n = AtomicU64::new(0);
    watch("walk", quick(500, 5000), || {
        (n.fetch_add(1, Ordering::Relaxed) < 10).then_some(n.load(Ordering::Relaxed))
    });
}

#[test]
fn a_watched_phase_that_stops_moving_fails_with_its_name_and_where_it_stuck() {
    let begun = Instant::now();
    let caught = std::panic::catch_unwind(|| {
        let n = AtomicU64::new(0);
        watch("walk the drive", quick(200, 60_000), || {
            // Moves for a while, then stops at 7.
            let v = n.fetch_add(1, Ordering::Relaxed).min(7);
            Some(v)
        });
    });
    let message = message_of(caught);
    assert!(message.contains("walk the drive"), "{message}");
    assert!(message.contains("no progress"), "{message}");
    assert!(message.contains("stuck at 7"), "{message}");
    assert!(
        begun.elapsed() < Duration::from_secs(10),
        "{:?}",
        begun.elapsed()
    );
}

#[test]
fn a_slow_phase_that_keeps_moving_is_not_stopped_by_the_stall_limit() {
    // Runs for about 600 ms with a stall limit of 150 ms: every step is
    // inside the limit, the total is not.
    let begun = Instant::now();
    let n = AtomicU64::new(0);
    watch("walk", quick(150, 10_000), || {
        std::thread::sleep(Duration::from_millis(40));
        let v = n.fetch_add(1, Ordering::Relaxed) + 1;
        (begun.elapsed() < Duration::from_millis(600)).then_some(v)
    });
    assert!(begun.elapsed() >= Duration::from_millis(600));
}

#[test]
fn a_phase_that_moves_for_ever_hits_the_ceiling() {
    let caught = std::panic::catch_unwind(|| {
        let n = AtomicU64::new(0);
        watch("walk", quick(1000, 300), || {
            Some(n.fetch_add(1, Ordering::Relaxed))
        });
    });
    let message = message_of(caught);
    assert!(message.contains("did not finish"), "{message}");
}
