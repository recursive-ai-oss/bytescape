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
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // These tests never depend on how fast the machine is. A probe that must be
    // "stuck" is held on a channel until the test lets it go, and a test waits
    // for an outcome by polling for it with a generous limit, so a slow runner
    // makes a test slower, never wrong.

    fn keyed<T: Send + 'static>(jobs: Vec<(&str, Job<T>)>) -> Vec<(Vec<u8>, Job<T>)> {
        jobs.into_iter()
            .map(|(k, j)| (k.as_bytes().to_vec(), j))
            .collect()
    }

    /// A job that returns `value` only after the returned sender is used or
    /// dropped. Dropping it also lets the job go, so a failing test leaves no
    /// thread waiting.
    fn held<T: Send + 'static>(value: T) -> (Job<T>, mpsc::Sender<()>) {
        let (release, wait) = mpsc::channel::<()>();
        let job: Job<T> = Box::new(move || {
            let _ = wait.recv();
            value
        });
        (job, release)
    }

    /// Waits, at most a minute, for something that is certain to happen.
    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let limit = Instant::now() + Duration::from_secs(60);
        while !condition() {
            assert!(Instant::now() < limit, "gave up waiting for {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A probe that finishes at once and counts how often it was started.
    fn quick(starts: &Arc<AtomicUsize>, value: u8) -> Job<u8> {
        let starts = Arc::clone(starts);
        Box::new(move || {
            starts.fetch_add(1, Ordering::SeqCst);
            value
        })
    }

    /// An answer that comes at once, so it can only be missing if the probe was
    /// not started.
    const PLENTY: Duration = Duration::from_secs(300);

    fn stuck_count(gate: &InFlight, key: &[u8]) -> Option<usize> {
        gate.stuck().get(key).copied()
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
        let out = run_parallel(&gate, jobs, Duration::from_secs(60));
        assert_eq!(out, (0..8usize).map(|i| Some(i * 2)).collect::<Vec<_>>());
    }

    #[test]
    fn a_stuck_job_is_abandoned_without_holding_up_the_others() {
        let gate = Arc::default();
        let (slow, release) = held("slow");
        let jobs: Vec<(&str, Job<&'static str>)> = vec![
            ("a", Box::new(|| "fast")),
            ("b", slow),
            ("c", Box::new(|| "also fast")),
        ];
        // The deadline is shared. It only has to be long enough for two
        // instant jobs; the held one cannot finish before it.
        let started = Instant::now();
        let out = run_parallel(&gate, keyed(jobs), Duration::from_secs(2));
        assert_eq!(out, vec![Some("fast"), None, Some("also fast")]);
        assert!(started.elapsed() < Duration::from_secs(60));
        drop(release);
    }

    #[test]
    fn a_stuck_key_is_not_started_again_until_it_returns() {
        let gate: Arc<InFlight> = Arc::default();
        let starts = Arc::new(AtomicUsize::new(0));
        let key = b"/mnt/dead".to_vec();
        let (job, release) = held(1u8);
        assert_eq!(
            run_parallel(&gate, vec![(key.clone(), job)], Duration::from_millis(50)),
            vec![None]
        );
        assert_eq!(stuck_count(&gate, &key), Some(1));
        // Still stuck: the probe is not started, so the answer is "none" at
        // once even though this one would have finished instantly.
        assert_eq!(
            run_parallel(&gate, vec![(key.clone(), quick(&starts, 2))], PLENTY),
            vec![None]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        // Once the call has returned the key can be probed again.
        release.send(()).unwrap();
        wait_until("the stuck probe to end", || gate.stuck().is_empty());
        assert_eq!(
            run_parallel(&gate, vec![(key, quick(&starts, 2))], PLENTY),
            vec![Some(2)]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_key_stays_stuck_until_every_abandoned_probe_of_it_has_returned() {
        let gate: Arc<InFlight> = Arc::default();
        let starts = Arc::new(AtomicUsize::new(0));
        let key = b"/mnt/dead".to_vec();
        // Two probes of one key are both started before either is abandoned:
        // all jobs of a call are started first, and only then waited for.
        let (first, release_first) = held(1u8);
        let (second, release_second) = held(2u8);
        let out = run_parallel(
            &gate,
            vec![(key.clone(), first), (key.clone(), second)],
            Duration::from_millis(50),
        );
        assert_eq!(out, vec![None, None]);
        assert_eq!(stuck_count(&gate, &key), Some(2));

        // After one returns, the other still holds the key.
        release_first.send(()).unwrap();
        wait_until("the first probe to end", || {
            stuck_count(&gate, &key) == Some(1)
        });
        assert_eq!(
            run_parallel(&gate, vec![(key.clone(), quick(&starts, 3))], PLENTY),
            vec![None]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0, "no third thread");

        // After the second returns too, the key is free.
        release_second.send(()).unwrap();
        wait_until("the second probe to end", || gate.stuck().is_empty());
        assert_eq!(
            run_parallel(&gate, vec![(key, quick(&starts, 3))], PLENTY),
            vec![Some(3)]
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn two_callers_may_ask_about_the_same_healthy_key_at_once() {
        let gate: Arc<InFlight> = Arc::default();
        // Each probe waits for the other to be running, so the test passes only
        // if both were allowed to run at the same time. If the second were
        // refused, the first would wait for it until the deadline.
        let meeting = Arc::new(Barrier::new(2));
        let ask = |gate: Arc<InFlight>, meeting: Arc<Barrier>| {
            thread::spawn(move || {
                let job: Job<u8> = Box::new(move || {
                    meeting.wait();
                    5
                });
                run_parallel(&gate, vec![(b"/".to_vec(), job)], PLENTY)
            })
        };
        let first = ask(Arc::clone(&gate), Arc::clone(&meeting));
        let second = ask(Arc::clone(&gate), meeting);
        assert_eq!(first.join().unwrap(), vec![Some(5)]);
        assert_eq!(second.join().unwrap(), vec![Some(5)]);
        assert!(gate.stuck().is_empty());
    }

    #[test]
    fn a_probe_that_finishes_just_after_the_deadline_leaves_nothing_stuck() {
        // The two orders in which "the caller gives up" and "the probe ends"
        // can meet, each made to happen exactly.
        let gate: Arc<InFlight> = Arc::default();

        // The probe ended first and the caller gives up a moment later: there
        // is nothing to record.
        let marker = gate.begin(b"edge".to_vec()).expect("not stuck");
        let slot = Arc::clone(&marker.slot);
        drop(marker);
        Marker::abandon(&gate, b"edge", &slot);
        assert!(gate.stuck().is_empty(), "a finished probe was recorded");

        // The caller gave up first and the probe ends a moment later: the key
        // is recorded, then cleared.
        let marker = gate.begin(b"edge".to_vec()).expect("not stuck");
        let slot = Arc::clone(&marker.slot);
        Marker::abandon(&gate, b"edge", &slot);
        assert_eq!(stuck_count(&gate, b"edge"), Some(1));
        drop(marker);
        assert!(gate.stuck().is_empty(), "the ended probe stayed recorded");

        // The same through real threads: the probe is held past a short
        // deadline, then let go.
        let (job, release) = held(1u8);
        let out = run_parallel(
            &gate,
            vec![(b"edge".to_vec(), job)],
            Duration::from_millis(20),
        );
        assert_eq!(out, vec![None]);
        assert_eq!(stuck_count(&gate, b"edge"), Some(1));
        release.send(()).unwrap();
        wait_until("the probe to end", || gate.stuck().is_empty());
    }

    #[test]
    fn different_keys_do_not_block_each_other() {
        let gate = Arc::default();
        let (slow, release) = held(1u8);
        let slow: Vec<(&str, Job<u8>)> = vec![("slow", slow)];
        assert_eq!(
            run_parallel(&gate, keyed(slow), Duration::from_millis(20)),
            vec![None]
        );
        let other: Vec<(&str, Job<u8>)> = vec![("other", Box::new(|| 2))];
        assert_eq!(run_parallel(&gate, keyed(other), PLENTY), vec![Some(2)]);
        drop(release);
    }

    /// Ends the thread the way a panic does, but without the panic message and
    /// backtrace, which can take seconds to print on a loaded machine.
    fn unwind() -> ! {
        std::panic::resume_unwind(Box::new("probe failed"))
    }

    #[test]
    fn a_panicking_job_yields_none_and_frees_its_key() {
        let gate: Arc<InFlight> = Arc::default();
        // A job that panics yields nothing and does not disturb the others.
        let boom: Vec<(&str, Job<u8>)> = vec![("k", Box::new(|| unwind())), ("j", Box::new(|| 7))];
        assert_eq!(
            run_parallel(&gate, keyed(boom), PLENTY),
            vec![None, Some(7)]
        );
        assert!(gate.stuck().is_empty());

        // One that panics while its caller has already given up still frees
        // the key when its thread ends.
        let (release_job, release) = {
            let (release, wait) = mpsc::channel::<()>();
            let job: Job<u8> = Box::new(move || {
                let _ = wait.recv();
                unwind()
            });
            (job, release)
        };
        let out = run_parallel(
            &gate,
            vec![(b"k".to_vec(), release_job)],
            Duration::from_millis(20),
        );
        assert_eq!(out, vec![None]);
        assert_eq!(stuck_count(&gate, b"k"), Some(1));
        release.send(()).unwrap();
        wait_until("the panicked probe to free its key", || {
            gate.stuck().is_empty()
        });
        let again: Vec<(&str, Job<u8>)> = vec![("k", Box::new(|| 3))];
        assert_eq!(run_parallel(&gate, keyed(again), PLENTY), vec![Some(3)]);
    }

    #[test]
    fn no_jobs_is_fine() {
        let gate = Arc::default();
        assert!(run_parallel::<u8>(&gate, Vec::new(), Duration::from_millis(10)).is_empty());
    }
}
