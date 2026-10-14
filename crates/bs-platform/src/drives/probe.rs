//! Running slow operating-system calls without letting them stall the caller.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) type Job<T> = Box<dyn FnOnce() -> T + Send + 'static>;

/// Remembers which probes were abandoned and are still stuck, by key (a mount
/// point, a path).
///
/// A probe that hangs is abandoned after the deadline but its thread lives on
/// until the operating system call returns. Without this record, listing the
/// drives again while a mount is still dead would start one more stuck thread
/// per call. With it, a key whose abandoned probe has not returned is not
/// probed again. A probe that is merely running, inside its deadline, is not
/// recorded: two callers may ask about the same healthy mount at once.
#[derive(Default)]
pub(crate) struct InFlight {
    /// How many abandoned probes of each key are still running. Two callers
    /// can abandon a probe of the same dead key at once; the key stays stuck
    /// until both have returned.
    stuck: Mutex<HashMap<Vec<u8>, usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Abandoned,
    Done,
}

/// The progress of one probe, shared by its thread and the caller waiting
/// for it. Both change it under its lock, so the key is recorded as stuck
/// exactly when the caller gives up on a thread that has not finished.
struct Slot {
    state: Mutex<State>,
}

impl InFlight {
    /// The record shared by every call in this process.
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<InFlight>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(Arc::default))
    }

    fn stuck(&self) -> std::sync::MutexGuard<'_, HashMap<Vec<u8>, usize>> {
        self.stuck.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts tracking a probe. `None` if an abandoned probe for the key is
    /// still stuck.
    fn begin(self: &Arc<Self>, key: Vec<u8>) -> Option<Marker> {
        if self.stuck().contains_key(&key) {
            return None;
        }
        Some(Marker {
            gate: Arc::clone(self),
            key,
            slot: Arc::new(Slot {
                state: Mutex::new(State::Running),
            }),
        })
    }
}

/// Held by the probe thread. When it is dropped, which happens when the thread
/// ends (including when the job panics), a key that had been recorded as stuck
/// is cleared.
struct Marker {
    gate: Arc<InFlight>,
    key: Vec<u8>,
    slot: Arc<Slot>,
}

impl Marker {
    /// The caller gives up on the probe. If the thread has not finished, its
    /// key is recorded as stuck.
    fn abandon(gate: &InFlight, key: &[u8], slot: &Slot) {
        let mut state = slot.state.lock().unwrap_or_else(PoisonError::into_inner);
        if *state == State::Running {
            *state = State::Abandoned;
            *gate.stuck().entry(key.to_vec()).or_insert(0) += 1;
        }
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        let mut state = self
            .slot
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *state == State::Abandoned {
            let mut stuck = self.gate.stuck();
            if let Some(count) = stuck.get_mut(&self.key) {
                *count -= 1;
                if *count == 0 {
                    stuck.remove(&self.key);
                }
            }
        }
        *state = State::Done;
    }
}

/// Runs every job on its own thread and waits for all of them together, for
/// at most `timeout` in total.
///
/// Each job has a key. The result at a position is `None` when that job did
/// not finish in time, when a job with the same key was abandoned earlier and
/// is still stuck (it is not started again), or when its thread could not be
/// started. A job that is still stuck when the deadline passes is abandoned:
/// its thread keeps running until the operating system call returns, and its
/// result is dropped. Nothing else waits on it.
pub(crate) fn run_parallel<T: Send + 'static>(
    gate: &Arc<InFlight>,
    jobs: Vec<(Vec<u8>, Job<T>)>,
    timeout: Duration,
) -> Vec<Option<T>> {
    struct Pending<T> {
        receiver: mpsc::Receiver<T>,
        slot: Arc<Slot>,
        key: Vec<u8>,
    }
    let deadline = Instant::now() + timeout;
    let pending: Vec<Option<Pending<T>>> = jobs
        .into_iter()
        .map(|(key, job)| {
            let marker = gate.begin(key.clone())?;
            let slot = Arc::clone(&marker.slot);
            let (sender, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("bs-drive-probe".to_owned())
                .spawn(move || {
                    let _marker = marker;
                    // The receiver may be gone if the job was abandoned.
                    let _ = sender.send(job());
                })
                .ok()
                .map(|_| Pending {
                    receiver,
                    slot,
                    key,
                })
        })
        .collect();
    pending
        .into_iter()
        .map(|pending| {
            let pending = pending?;
            let left = deadline.saturating_duration_since(Instant::now());
            match pending.receiver.recv_timeout(left) {
                Ok(value) => Some(value),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Marker::abandon(gate, &pending.key, &pending.slot);
                    None
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed<T: Send + 'static>(jobs: Vec<(&str, Job<T>)>) -> Vec<(Vec<u8>, Job<T>)> {
        jobs.into_iter()
            .map(|(k, j)| (k.as_bytes().to_vec(), j))
            .collect()
    }

    #[test]
    fn fast_jobs_all_finish_in_order() {
        let gate = Arc::default();
        let names: Vec<String> = (0..8).map(|i| format!("job{i}")).collect();
        let jobs: Vec<(Vec<u8>, Job<usize>)> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                (
                    name.as_bytes().to_vec(),
                    Box::new(move || i * 2) as Job<usize>,
                )
            })
            .collect();
        let out = run_parallel(&gate, jobs, Duration::from_secs(5));
        assert_eq!(out, (0..8usize).map(|i| Some(i * 2)).collect::<Vec<_>>());
    }

    #[test]
    fn a_stuck_job_is_abandoned_without_holding_up_the_others() {
        let gate = Arc::default();
        let jobs: Vec<(&str, Job<&'static str>)> = vec![
            ("a", Box::new(|| "fast")),
            (
                "b",
                Box::new(|| {
                    thread::sleep(Duration::from_secs(30));
                    "slow"
                }),
            ),
            ("c", Box::new(|| "also fast")),
        ];
        let start = Instant::now();
        let out = run_parallel(&gate, keyed(jobs), Duration::from_millis(150));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(out, vec![Some("fast"), None, Some("also fast")]);
    }

    #[test]
    fn a_stuck_key_is_not_started_again_until_it_returns() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let gate = Arc::default();
        let starts = Arc::new(AtomicUsize::new(0));
        let job = |starts: &Arc<AtomicUsize>| -> Vec<(Vec<u8>, Job<u8>)> {
            let starts = Arc::clone(starts);
            vec![(
                b"/mnt/dead".to_vec(),
                Box::new(move || {
                    starts.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(600));
                    1u8
                }) as Job<u8>,
            )]
        };
        assert_eq!(
            run_parallel(&gate, job(&starts), Duration::from_millis(50)),
            vec![None]
        );
        // Still stuck: no second thread.
        assert_eq!(
            run_parallel(&gate, job(&starts), Duration::from_millis(50)),
            vec![None]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        // Once the call has returned the key can be probed again.
        thread::sleep(Duration::from_millis(900));
        assert_eq!(
            run_parallel(&gate, job(&starts), Duration::from_secs(5)),
            vec![Some(1)]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_key_stays_stuck_until_every_abandoned_probe_of_it_has_returned() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let gate: Arc<InFlight> = Arc::default();
        let starts = Arc::new(AtomicUsize::new(0));
        let probe = |sleep_ms: u64, starts: &Arc<AtomicUsize>| -> Vec<(Vec<u8>, Job<u8>)> {
            let starts = Arc::clone(starts);
            vec![(
                b"/mnt/dead".to_vec(),
                Box::new(move || {
                    starts.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(sleep_ms));
                    1u8
                }) as Job<u8>,
            )]
        };
        // Two callers start at the same moment, so both get to spawn. One
        // probe returns after 300 ms and the other after 1500 ms; both callers
        // give up after 40 ms.
        let callers: Vec<_> = [300u64, 1500]
            .into_iter()
            .map(|ms| {
                let (gate, starts) = (Arc::clone(&gate), Arc::clone(&starts));
                let jobs = probe(ms, &starts);
                thread::spawn(move || run_parallel(&gate, jobs, Duration::from_millis(40)))
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), vec![None]);
        }
        // Both probes ran if the callers really overlapped; otherwise the test
        // has not exercised the case and says so instead of passing quietly.
        assert_eq!(
            starts.load(Ordering::SeqCst),
            2,
            "the two callers did not overlap"
        );

        // After the short probe has returned, the long one is still stuck.
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            run_parallel(&gate, probe(1, &starts), Duration::from_millis(40)),
            vec![None]
        );
        assert_eq!(
            starts.load(Ordering::SeqCst),
            2,
            "no third thread while one is stuck"
        );

        // After the long one returns too, the key is free.
        thread::sleep(Duration::from_millis(1300));
        assert_eq!(
            run_parallel(&gate, probe(1, &starts), Duration::from_secs(5)),
            vec![Some(1)]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn two_callers_may_ask_about_the_same_healthy_key_at_once() {
        let gate: Arc<InFlight> = Arc::default();
        let ask = |gate: Arc<InFlight>| {
            thread::spawn(move || {
                let job: Job<u8> = Box::new(|| {
                    thread::sleep(Duration::from_millis(200));
                    5
                });
                run_parallel(&gate, vec![(b"/".to_vec(), job)], Duration::from_secs(5))
            })
        };
        let (first, second) = (ask(Arc::clone(&gate)), ask(Arc::clone(&gate)));
        assert_eq!(first.join().unwrap(), vec![Some(5)]);
        assert_eq!(second.join().unwrap(), vec![Some(5)]);
    }

    #[test]
    fn a_probe_that_finishes_just_after_the_deadline_leaves_nothing_stuck() {
        let gate: Arc<InFlight> = Arc::default();
        for _ in 0..20 {
            let job: Job<u8> = Box::new(|| {
                thread::sleep(Duration::from_millis(5));
                1
            });
            let _ = run_parallel(
                &gate,
                vec![(b"edge".to_vec(), job)],
                Duration::from_millis(5),
            );
            thread::sleep(Duration::from_millis(50));
            assert!(
                gate.stuck().is_empty(),
                "a finished probe must not stay recorded"
            );
        }
    }

    #[test]
    fn different_keys_do_not_block_each_other() {
        let gate = Arc::default();
        let slow: Vec<(&str, Job<u8>)> = vec![(
            "slow",
            Box::new(|| {
                thread::sleep(Duration::from_millis(500));
                1
            }),
        )];
        assert_eq!(
            run_parallel(&gate, keyed(slow), Duration::from_millis(20)),
            vec![None]
        );
        let other: Vec<(&str, Job<u8>)> = vec![("other", Box::new(|| 2))];
        assert_eq!(
            run_parallel(&gate, keyed(other), Duration::from_secs(5)),
            vec![Some(2)]
        );
    }

    #[test]
    fn a_panicking_job_yields_none_and_frees_its_key() {
        let gate = Arc::default();
        let boom: Vec<(&str, Job<u8>)> = vec![
            ("k", Box::new(|| panic!("probe failed"))),
            ("j", Box::new(|| 7)),
        ];
        assert_eq!(
            run_parallel(&gate, keyed(boom), Duration::from_secs(5)),
            vec![None, Some(7)]
        );
        // The panicked job released its key.
        thread::sleep(Duration::from_millis(100));
        let again: Vec<(&str, Job<u8>)> = vec![("k", Box::new(|| 3))];
        assert_eq!(
            run_parallel(&gate, keyed(again), Duration::from_secs(5)),
            vec![Some(3)]
        );
    }

    #[test]
    fn no_jobs_is_fine() {
        let gate = Arc::default();
        assert!(run_parallel::<u8>(&gate, Vec::new(), Duration::from_millis(10)).is_empty());
    }
}
