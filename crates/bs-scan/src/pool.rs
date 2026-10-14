//! A small work-stealing pool for directory tasks, shared by the platform
//! walkers.
//!
//! Each worker keeps a private stack of the directories it found (depth first,
//! so memory stays small) and a shared queue other workers steal from. A task
//! moves from the private stack to the shared queue only after the batch that
//! holds its directory record has been sent. That keeps the contract with the
//! tree: no record reaches the applier before the record of its parent
//! directory, so the tree never has to hold records back.
//!
//! Batches are cut by size and by age, not by directory, so a deep chain of
//! tiny directories goes out in a few large batches.
//!
//! A task that panics does not stop the pool from ending: the worker catches
//! the panic, records its message, cancels the scan (every other worker leaves
//! at its next look at the cancel flag, whatever the pending count says) and
//! [`run`] returns the failure as a [`ScanError::Panicked`].

use crate::scanner::{ScanContext, ScanEnd, ScanError, panic_message};
use bs_tree::Batch;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Records after which a worker's batch is sent.
pub(crate) const FLUSH_RECORDS: usize = 4096;
/// Name bytes after which a worker's batch is sent.
pub(crate) const FLUSH_NAME_BYTES: usize = 512 * 1024;
/// Age after which a non-empty batch is sent, so the map fills in live.
pub(crate) const FLUSH_AGE: Duration = Duration::from_millis(40);
/// The shortest time between two moves of work to the shared queues by one
/// worker. Each move sends the worker's batch first, so without a floor a
/// chain of directories with a side branch at every level would go out one
/// directory per batch while other workers are idle.
const PUBLISH_GAP: Duration = Duration::from_micros(500);
/// How long an idle worker sleeps before looking for work again.
const IDLE_WAIT: Duration = Duration::from_millis(2);

pub(crate) struct Pool<'a, T> {
    ctx: &'a ScanContext,
    queues: Vec<Mutex<VecDeque<T>>>,
    /// Tasks created and not yet finished.
    pending: AtomicUsize,
    /// Workers looking for work.
    idle: AtomicUsize,
    sleep: Mutex<()>,
    wake: Condvar,
    /// The message of the first panic in a task, if any.
    panicked: Mutex<Option<String>>,
}

/// Sums kept by a worker between tasks, added to the shared counters once per
/// directory.
#[derive(Default)]
pub(crate) struct Tally {
    pub allocated: u64,
    pub logical: u64,
    pub files: u64,
    pub dirs: u64,
    pub unreadable: u64,
    pub vanished: u64,
}

impl Tally {
    /// Adds an entry's sizes. Sizes come from the disk, so a hostile or
    /// damaged filesystem can report huge ones: the sums saturate.
    pub fn add_bytes(&mut self, allocated: u64, logical: u64) {
        self.allocated = self.allocated.saturating_add(allocated);
        self.logical = self.logical.saturating_add(logical);
    }
}

pub(crate) struct Worker<'p, 'a, T> {
    pool: &'p Pool<'a, T>,
    id: usize,
    local: Vec<T>,
    batch: Batch,
    batch_started: Option<Instant>,
    last_publish: Option<Instant>,
    pub tally: Tally,
}

impl<T> Worker<'_, '_, T> {
    /// The batch being filled.
    pub fn batch(&mut self) -> &mut Batch {
        if self.batch_started.is_none() {
            self.batch_started = Some(Instant::now());
        }
        &mut self.batch
    }

    /// Queues a directory found by the task being processed. It becomes
    /// available to other workers only after the current batch is sent.
    pub fn spawn(&mut self, task: T) {
        self.pool.pending.fetch_add(1, Ordering::AcqRel);
        self.local.push(task);
    }

    pub fn ctx(&self) -> &ScanContext {
        self.pool.ctx
    }

    pub fn cancelled(&self) -> bool {
        self.pool.ctx.is_cancelled()
    }

    fn batch_is_due(&self) -> bool {
        self.batch.len() >= FLUSH_RECORDS
            || self.batch.name_bytes() >= FLUSH_NAME_BYTES
            || self
                .batch_started
                .is_some_and(|at| !self.batch.is_empty() && at.elapsed() >= FLUSH_AGE)
    }

    /// Sends the batch if it is big or old enough. Cheap; call it every so
    /// many entries inside a long directory.
    pub fn flush_if_due(&mut self) {
        if self.batch_is_due() {
            self.flush();
        }
    }

    /// Sends the batch now.
    pub fn flush(&mut self) {
        self.batch_started = None;
        if self.batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.batch);
        if !self.pool.ctx.send(batch) {
            // Nobody is listening: stop.
            self.pool.ctx.cancel();
        }
    }

    fn flush_tally(&mut self) {
        let t = std::mem::take(&mut self.tally);
        self.pool.ctx.count(t.allocated, t.logical, t.files, t.dirs);
        if t.unreadable != 0 || t.vanished != 0 {
            self.pool.ctx.count_problems(t.unreadable, t.vanished);
        }
    }

    /// Moves half of the private stack, oldest first, where other workers can
    /// take it. The batch is sent first: see the module documentation.
    fn publish_some(&mut self) {
        self.flush();
        self.last_publish = Some(Instant::now());
        // Keep at least one for this worker, which is the next it would run.
        let give = self.local.len() / 2;
        if give == 0 {
            return;
        }
        let moved: Vec<T> = self.local.drain(..give).collect();
        if let Ok(mut queue) = self.pool.queues[self.id].lock() {
            queue.extend(moved);
        }
        self.pool.wake.notify_all();
    }

    fn take_task(&mut self) -> Option<T> {
        if let Some(task) = self.local.pop() {
            return Some(task);
        }
        if let Ok(mut queue) = self.pool.queues[self.id].lock()
            && let Some(task) = queue.pop_back()
        {
            return Some(task);
        }
        self.steal()
    }

    fn steal(&mut self) -> Option<T> {
        let n = self.pool.queues.len();
        for offset in 1..n {
            let victim = (self.id + offset) % n;
            let Ok(mut queue) = self.pool.queues[victim].try_lock() else {
                continue;
            };
            let take = queue.len().div_ceil(2);
            if take == 0 {
                continue;
            }
            // Oldest first: they are the shallowest, so the biggest subtrees.
            let mut taken: Vec<T> = queue.drain(..take).collect();
            drop(queue);
            let first = taken.pop();
            // What is left stays private to this worker, deepest last.
            self.local.extend(taken);
            return first;
        }
        None
    }

    fn finish_task(&mut self) {
        self.flush_tally();
        self.flush_if_due();
        // A lone child stays here: it is what this worker runs next, and a
        // chain of directories should not be handed around.
        if self.local.len() >= 2
            && self.pool.idle.load(Ordering::Relaxed) > 0
            && self
                .last_publish
                .is_none_or(|at| at.elapsed() >= PUBLISH_GAP)
        {
            self.publish_some();
        }
        if self.pool.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.pool.wake.notify_all();
        }
    }

    /// Drops every task this worker holds without running it.
    fn discard_local(&mut self) {
        let mut dropped = self.local.len();
        self.local.clear();
        if let Ok(mut queue) = self.pool.queues[self.id].lock() {
            dropped += queue.len();
            queue.clear();
        }
        if dropped > 0 && self.pool.pending.fetch_sub(dropped, Ordering::AcqRel) == dropped {
            self.pool.wake.notify_all();
        }
    }
}

/// Runs `process` on `roots` and everything they spawn, on `threads`
/// threads. Returns when no work is left or the scan is cancelled, or with
/// [`ScanError::Panicked`] if `process` panicked (the scan is then
/// cancelled, and the work already sent stays valid).
pub(crate) fn run<T, F>(
    ctx: &ScanContext,
    threads: usize,
    roots: Vec<T>,
    process: F,
) -> Result<ScanEnd, ScanError>
where
    T: Send,
    F: Fn(&mut Worker<'_, '_, T>, T) + Sync,
{
    let threads = threads.max(1);
    let pool = Pool {
        ctx,
        queues: (0..threads).map(|_| Mutex::new(VecDeque::new())).collect(),
        pending: AtomicUsize::new(roots.len()),
        idle: AtomicUsize::new(0),
        sleep: Mutex::new(()),
        wake: Condvar::new(),
        panicked: Mutex::new(None),
    };
    if let Ok(mut queue) = pool.queues[0].lock() {
        queue.extend(roots);
    }
    ctx.set_threads(threads);

    std::thread::scope(|scope| {
        for id in 0..threads {
            let pool = &pool;
            let process = &process;
            let spawned = std::thread::Builder::new()
                .name(format!("bs-scan-{id}"))
                .spawn_scoped(scope, move || worker_loop(pool, id, process));
            if spawned.is_err() && id == 0 {
                // Without any worker nothing would run; do the work here.
                worker_loop(pool, 0, &process);
                return;
            }
        }
    });

    let panicked = pool
        .panicked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(message) = panicked {
        return Err(ScanError::Panicked(message));
    }
    Ok(if ctx.is_cancelled() {
        ScanEnd::Cancelled
    } else {
        ScanEnd::Completed
    })
}

fn worker_loop<T, F>(pool: &Pool<'_, T>, id: usize, process: &F)
where
    F: Fn(&mut Worker<'_, '_, T>, T),
{
    let mut worker = Worker {
        pool,
        id,
        local: Vec::new(),
        batch: Batch::new(),
        batch_started: None,
        last_publish: None,
        tally: Tally::default(),
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| work(&mut worker, process)));
    if let Err(payload) = outcome {
        // Record why, then stop everyone: a worker that leaves its task
        // unfinished would keep the pending count above zero for good.
        if let Ok(mut slot) = pool.panicked.lock()
            && slot.is_none()
        {
            *slot = Some(panic_message(&*payload));
        }
        pool.ctx.cancel();
        pool.wake.notify_all();
        // Hand over what was found before the panic. This must not panic
        // out of the thread.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            worker.discard_local();
            worker.flush_tally();
            worker.flush();
        }));
    }
}

fn work<T, F>(worker: &mut Worker<'_, '_, T>, process: &F)
where
    F: Fn(&mut Worker<'_, '_, T>, T),
{
    let pool = worker.pool;
    loop {
        if pool.ctx.is_cancelled() {
            worker.discard_local();
            worker.flush_tally();
            worker.flush();
            return;
        }
        match worker.take_task() {
            Some(task) => {
                process(worker, task);
                worker.finish_task();
            }
            None => {
                // Show what this worker found before it waits.
                worker.flush();
                if pool.pending.load(Ordering::Acquire) == 0 {
                    return;
                }
                pool.idle.fetch_add(1, Ordering::AcqRel);
                if let Ok(guard) = pool.sleep.lock() {
                    let _ = pool.wake.wait_timeout(guard, IDLE_WAIT);
                }
                pool.idle.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::ScanOptions;
    use std::sync::atomic::AtomicU64;

    /// A tree where task n spawns n-1 and n-2: count the visits.
    #[test]
    fn every_task_runs_exactly_once() {
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 4);
        let visits = AtomicU64::new(0);
        let end = run(&ctx, 8, vec![22u32], |w, n| {
            visits.fetch_add(1, Ordering::Relaxed);
            if n >= 2 {
                w.spawn(n - 1);
                w.spawn(n - 2);
            }
        });
        // visits(n) = 1 + visits(n-1) + visits(n-2), visits(0)=visits(1)=1.
        let mut fib = (1u64, 1u64);
        for _ in 2..=22 {
            fib = (fib.1, 1 + fib.0 + fib.1);
        }
        assert_eq!(end.unwrap(), ScanEnd::Completed);
        assert_eq!(visits.load(Ordering::Relaxed), fib.1);
    }

    #[test]
    fn cancel_stops_the_pool_and_discards_the_rest() {
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 4);
        let visits = AtomicU64::new(0);
        let end = run(&ctx, 4, vec![40u32], |w, n| {
            if visits.fetch_add(1, Ordering::Relaxed) > 100 {
                w.ctx().cancel();
            }
            if n >= 2 {
                w.spawn(n - 1);
                w.spawn(n - 2);
            }
        });
        assert_eq!(end.unwrap(), ScanEnd::Cancelled);
        assert!(visits.load(Ordering::Relaxed) < 100_000);
    }

    #[test]
    fn a_single_thread_works() {
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 4);
        let visits = AtomicU64::new(0);
        run(&ctx, 1, vec![10u32], |w, n| {
            visits.fetch_add(1, Ordering::Relaxed);
            if n > 0 {
                w.spawn(n - 1);
            }
        })
        .unwrap();
        assert_eq!(visits.load(Ordering::Relaxed), 11);
    }

    #[test]
    fn a_tally_of_huge_sizes_saturates_and_reaches_the_counters_saturated() {
        // Three files of 2^63 - 1 bytes: more than a u64 holds.
        let huge = i64::MAX as u64;
        let mut tally = Tally::default();
        for _ in 0..3 {
            tally.add_bytes(huge, huge);
        }
        assert_eq!((tally.allocated, tally.logical), (u64::MAX, u64::MAX));
        tally.add_bytes(u64::MAX, 1);
        assert_eq!((tally.allocated, tally.logical), (u64::MAX, u64::MAX));

        // Two workers each hand in a saturated tally; the shared counters
        // saturate too instead of wrapping.
        let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 4);
        ctx.count(tally.allocated, tally.logical, 3, 0);
        ctx.count(tally.allocated, tally.logical, 3, 0);
        let p = ctx.progress();
        assert_eq!((p.allocated_bytes, p.logical_bytes), (u64::MAX, u64::MAX));
        assert_eq!(p.files, 6);
    }

    #[test]
    fn a_panicking_task_stops_the_pool_and_is_reported() {
        for threads in [1, 3, 8] {
            let (ctx, _rx) = ScanContext::channel(ScanOptions::default(), 4);
            let (tx, done) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let result = run(&ctx, threads, vec![30u32], |w, n| {
                        if n == 20 {
                            panic!("boom {n}");
                        }
                        if n >= 2 {
                            w.spawn(n - 1);
                            w.spawn(n - 2);
                        }
                    });
                    let _ = tx.send(result);
                });
                let result = done
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the pool ended instead of hanging");
                match result {
                    Err(ScanError::Panicked(m)) => assert_eq!(m, "boom 20"),
                    other => panic!("{threads} threads: {other:?}"),
                }
            });
            assert!(ctx.is_cancelled());
        }
    }
}
