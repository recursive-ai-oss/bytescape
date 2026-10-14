//! A panic inside the scan must end it with an error the caller can read, in
//! bounded time, and must not reach the thread that waits for the result.
//! The panic is injected through the open hook, so it happens in a real
//! worker of the real walker. One test per file because the hook is
//! process-wide.
#![cfg(target_os = "linux")]

mod support;

use bs_scan::{ScanError, ScanOptions, Walker};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use support::*;

#[test]
fn a_panic_in_a_worker_ends_the_scan_with_an_error_instead_of_hanging() {
    let fx = Fixture::new("panic");
    for d in 0..40 {
        fx.file(&format!("d{d}/inner/f"), 100);
    }
    fx.file("boom/f", 100);

    for threads in [1, 4] {
        bs_scan::set_before_open_hook(Some(Box::new(|name| {
            if name == b"boom" {
                panic!("injected failure");
            }
        })));
        let handle = bs_scan::start(
            Arc::new(Walker),
            fx.root.clone(),
            ScanOptions {
                threads: Some(threads),
                ..ScanOptions::default()
            },
        );
        // Wait on another thread, so a hang fails the test instead of
        // stalling it.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(handle.wait());
        });
        let result = rx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| panic!("{threads} threads: the scan hung after a panic"));
        bs_scan::set_before_open_hook(None);

        match &result.stats.error {
            Some(ScanError::Panicked(message)) => {
                assert!(message.contains("injected failure"), "{message}");
            }
            other => panic!("{threads} threads: expected ScanError::Panicked, got {other:?}"),
        }
        assert_eq!(result.stats.end, None);
        // What was found is kept, and says it is not the whole story.
        result.snapshot.verify().unwrap();
        assert!(!result.snapshot.scan_complete());
        assert!(!result.snapshot.root().unwrap().is_complete());
        assert_eq!(result.stats.records_parked, 0);
    }
}
