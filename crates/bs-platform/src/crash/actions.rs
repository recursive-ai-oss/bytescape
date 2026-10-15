//! A small ring of what the user did last, kept for the crash report.
//!
//! Recording must be cheap enough for the UI thread, so an entry is copied
//! into a fixed slot: no allocation, one short lock.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::Instant;

/// Entries kept; the oldest is overwritten.
pub(crate) const CAPACITY: usize = 32;
/// Longest text kept per entry, in bytes. Longer text is cut at a character
/// boundary.
pub(crate) const MAX_LEN: usize = 96;

#[derive(Clone, Copy)]
struct Slot {
    /// Milliseconds since the start of the program (or of the first entry).
    at_ms: u32,
    len: u8,
    text: [u8; MAX_LEN],
}

struct Ring {
    /// Entries ever recorded; the next goes to `count % CAPACITY`.
    count: u64,
    slots: [Slot; CAPACITY],
}

static RING: Mutex<Ring> = Mutex::new(Ring {
    count: 0,
    slots: [Slot {
        at_ms: 0,
        len: 0,
        text: [0; MAX_LEN],
    }; CAPACITY],
});

static START: OnceLock<Instant> = OnceLock::new();

/// Starts the clock the entries are timed against. Called when the hook is
/// installed so times count from the start of the program.
pub(crate) fn start_clock() {
    let _ = START.get_or_init(Instant::now);
}

fn lock() -> MutexGuard<'static, Ring> {
    // A thread that panicked while holding the lock cannot have left a slot
    // half-written in a way that matters: a slot is plain bytes.
    RING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Remembers that something happened, for the crash report. Keep it short and
/// leave paths out unless they help: text is copied as given (line breaks and
/// other control characters become spaces) and cut at 96 bytes. Only the last
/// 32 entries are kept.
pub fn record_action(action: &str) {
    let at_ms = START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u32::MAX)) as u32;
    let mut text = [0u8; MAX_LEN];
    let mut len = 0usize;
    for character in action.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        let mut buffer = [0u8; 4];
        let bytes = character.encode_utf8(&mut buffer).as_bytes();
        let end = len.saturating_add(bytes.len());
        let Some(target) = text.get_mut(len..end) else {
            break;
        };
        target.copy_from_slice(bytes);
        len = end;
    }
    let mut ring = lock();
    let index = ring.count.checked_rem(CAPACITY as u64).unwrap_or(0) as usize;
    if let Some(slot) = ring.slots.get_mut(index) {
        *slot = Slot {
            at_ms,
            // `len` is at most MAX_LEN, which fits a byte.
            len: len as u8,
            text,
        };
    }
    ring.count = ring.count.wrapping_add(1);
}

/// The entries, oldest first, as `(milliseconds, text)`. With `wait` false, a
/// ring that is being written at that moment gives nothing instead of
/// blocking: the crash hook must never wait on anyone.
pub(crate) fn snapshot(wait: bool) -> Vec<(u32, String)> {
    let ring = if wait {
        lock()
    } else {
        match RING.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Vec::new(),
        }
    };
    let kept = ring.count.min(CAPACITY as u64) as usize;
    let first = ring.count.saturating_sub(kept as u64);
    let mut entries = Vec::with_capacity(kept);
    for n in 0..kept {
        let index = first
            .wrapping_add(n as u64)
            .checked_rem(CAPACITY as u64)
            .unwrap_or(0) as usize;
        if let Some(slot) = ring.slots.get(index) {
            let text = slot.text.get(..usize::from(slot.len)).unwrap_or_default();
            entries.push((slot.at_ms, String::from_utf8_lossy(text).into_owned()));
        }
    }
    entries
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
pub(crate) mod tests {
    use super::*;

    /// The ring is process-wide, so the tests that use it take turns.
    static TURN: Mutex<()> = Mutex::new(());

    pub(crate) fn turn() -> MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn clear() {
        lock().count = 0;
    }

    #[test]
    fn the_last_entries_are_kept_in_order() {
        let _turn = turn();
        clear();
        for n in 0..(CAPACITY + 5) {
            record_action(&format!("step {n}"));
        }
        let got = snapshot(true);
        assert_eq!(got.len(), CAPACITY);
        assert_eq!(got[0].1, "step 5");
        assert_eq!(got[CAPACITY - 1].1, format!("step {}", CAPACITY + 4));
        assert!(got.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    }

    #[test]
    fn a_few_entries_are_all_returned() {
        let _turn = turn();
        clear();
        assert!(snapshot(true).is_empty());
        record_action("one");
        record_action("two");
        let texts: Vec<String> = snapshot(true).into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts, ["one", "two"]);
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let _turn = turn();
        clear();
        // 3-byte characters: 32 of them are 96 bytes, so a 33rd does not fit.
        record_action(&"€".repeat(40));
        record_action(&"x".repeat(500));
        let got = snapshot(true);
        assert_eq!(got[0].1, "€".repeat(32));
        assert_eq!(got[1].1.len(), MAX_LEN);
    }

    #[test]
    fn line_breaks_cannot_forge_report_lines() {
        let _turn = turn();
        clear();
        record_action("a\nBacktrace:\r\n\tb\u{0}c");
        assert_eq!(snapshot(true)[0].1, "a Backtrace:   b c");
    }

    #[test]
    fn a_busy_ring_is_not_waited_for() {
        let _turn = turn();
        clear();
        record_action("kept");
        let held = lock();
        assert!(snapshot(false).is_empty());
        drop(held);
        assert_eq!(snapshot(false).len(), 1);
    }

    #[test]
    fn many_threads_recording_keep_the_ring_sound() {
        let _turn = turn();
        clear();
        let threads: Vec<_> = (0..4)
            .map(|t| {
                std::thread::spawn(move || {
                    for n in 0..2000 {
                        record_action(&format!("t{t} n{n}"));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let got = snapshot(true);
        assert_eq!(got.len(), CAPACITY);
        assert!(
            got.iter()
                .all(|(_, t)| t.starts_with('t') && t.contains(" n"))
        );
    }

    /// What one call costs, printed for the record. The bound is far above
    /// what it takes; it only catches a call that has become expensive.
    #[test]
    fn recording_is_cheap() {
        let _turn = turn();
        clear();
        let calls = 200_000u32;
        let started = Instant::now();
        for n in 0..calls {
            record_action(std::hint::black_box(if n % 2 == 0 {
                "open folder"
            } else {
                "zoom into folder"
            }));
        }
        let per_call = started.elapsed().as_nanos() / u128::from(calls);
        eprintln!("record_action: {per_call} ns per call (test build)");
        assert!(per_call < 20_000, "{per_call} ns");
    }
}
