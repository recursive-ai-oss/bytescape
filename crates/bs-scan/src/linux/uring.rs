//! A variant of the Linux walker whose per-entry `statx` calls go through an
//! io_uring, for measuring only (feature `iouring-bench`).
//!
//! Directories are still read with `getdents64`, on the walker's own
//! work-stealing threads: io_uring has no operation for listing a directory.
//! What changes is the examination of the entries a listing gave. Instead of
//! one `statx` system call per name, each worker thread owns a ring and
//! submits the names of a listing buffer in batches of up to the queue depth,
//! then waits for the whole batch. The records written afterwards are the
//! walker's own, from the same code, in the same order.
//!
//! The ring is driven with raw system calls through `libc`, so no crate
//! enters the build. Only `IORING_OP_STATX` is ever submitted; no socket
//! operation is used, and the module does not name one.
//!
//! The kernel handles `IORING_OP_STATX` by handing it to its worker threads
//! (it can block on the filesystem), so this measures whether batching the
//! submissions pays for that hand-off.
#![allow(unsafe_code)]

use super::{Listing, Shared, Task, failure_of, handle_entry_with, list_dir_plain};
use crate::classify::Here;
use crate::dirent;
use crate::pool::Worker;
use crate::scanner::{ScanContext, ScanEnd, ScanError, Scanner};
use bs_tree::{DirKey, ReadErrorKind, ReadFailure};
use std::cell::RefCell;
use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const IORING_OP_STATX: u8 = 21;
const IOSQE_ASYNC: u8 = 1 << 4;
const IORING_ENTER_GETEVENTS: u32 = 1;
const IORING_REGISTER_PROBE: u32 = 8;
const IO_URING_OP_SUPPORTED: u16 = 1;
const IORING_FEAT_SINGLE_MMAP: u32 = 1;
const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;

/// How the ring is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Names submitted per batch (and the ring's size, rounded up to a power
    /// of two by the kernel).
    pub depth: u32,
    /// Set `IOSQE_ASYNC` on every request, which skips the inline attempt and
    /// goes straight to the kernel's worker threads.
    pub force_async: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            depth: 128,
            force_async: false,
        }
    }
}

/// Why the ring cannot be used.
#[derive(Debug)]
pub enum Unavailable {
    /// The system call failed (not built in, disabled by `io_uring_disabled`,
    /// blocked by a sandbox, or out of resources).
    Setup(io::Error),
    /// The kernel has the ring but not `IORING_OP_STATX`, or could not say.
    NoStatx,
    /// Switched off by [`set_force_unavailable`].
    Forced,
}

static FORCE_UNAVAILABLE: AtomicBool = AtomicBool::new(false);
static FORCE_RING_FAILURE: AtomicBool = AtomicBool::new(false);
static SQES: AtomicU64 = AtomicU64::new(0);
static ENTERS: AtomicU64 = AtomicU64::new(0);
static THREAD_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Makes [`probe`] report the ring as unavailable, to test the fallback.
pub fn set_force_unavailable(on: bool) {
    FORCE_UNAVAILABLE.store(on, Ordering::SeqCst);
}

/// Makes every worker thread fail to set up its own ring even when the probe
/// succeeded, to test the per-thread fallback.
pub fn set_force_ring_failure(on: bool) {
    FORCE_RING_FAILURE.store(on, Ordering::SeqCst);
}

/// What the rings did since the last call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `statx` requests submitted.
    pub requests: u64,
    /// `io_uring_enter` calls.
    pub enters: u64,
    /// Worker threads that could not set up a ring and used plain `statx`.
    pub thread_fallbacks: u64,
}

/// Returns the counters and resets them.
pub fn take_stats() -> Stats {
    Stats {
        requests: SQES.swap(0, Ordering::Relaxed),
        enters: ENTERS.swap(0, Ordering::Relaxed),
        thread_fallbacks: THREAD_FALLBACKS.swap(0, Ordering::Relaxed),
    }
}

/// User plus system CPU time the process has used, in milliseconds. It
/// includes the kernel worker threads that serve the ring's requests, which
/// belong to the process.
pub fn process_cpu_ms() -> f64 {
    let mut ru = MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `ru` is a writable `rusage`; `RUSAGE_SELF` is always valid.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    if rc != 0 {
        return 0.0;
    }
    // SAFETY: zero-initialized and, on success, filled by the kernel.
    let ru = unsafe { ru.assume_init() };
    let ms = |v: libc::timeval| v.tv_sec as f64 * 1000.0 + v.tv_usec as f64 / 1000.0;
    ms(ru.ru_utime) + ms(ru.ru_stime)
}

// The kernel's structures, as `linux/io_uring.h` lays them out.

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOffsets,
    cq_off: CqOffsets,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    /// For `STATX`: the address of the `statx` buffer.
    off: u64,
    /// For `STATX`: the address of the path.
    addr: u64,
    /// For `STATX`: the field mask.
    len: u32,
    /// For `STATX`: the `AT_*` flags.
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    pad: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

const _: () = assert!(std::mem::size_of::<Params>() == 120);
const _: () = assert!(std::mem::size_of::<Sqe>() == 64);
const _: () = assert!(std::mem::size_of::<Cqe>() == 16);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProbeOp {
    op: u8,
    resv: u8,
    flags: u16,
    resv2: u32,
}

const PROBE_OPS: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
struct Probe {
    last_op: u8,
    ops_len: u8,
    resv: u16,
    resv2: [u32; 3],
    ops: [ProbeOp; PROBE_OPS],
}

fn setup(entries: u32, params: &mut Params) -> io::Result<OwnedFd> {
    // SAFETY: `params` is a valid, writable `io_uring_params`; the call
    // returns a new descriptor or -1.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            entries,
            std::ptr::from_mut(params),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// Whether the kernel supports `IORING_OP_STATX` here.
pub fn probe() -> Result<(), Unavailable> {
    if FORCE_UNAVAILABLE.load(Ordering::SeqCst) {
        return Err(Unavailable::Forced);
    }
    let mut params = Params::default();
    let fd = setup(2, &mut params).map_err(Unavailable::Setup)?;
    let mut probe = Probe {
        last_op: 0,
        ops_len: 0,
        resv: 0,
        resv2: [0; 3],
        ops: [ProbeOp::default(); PROBE_OPS],
    };
    // SAFETY: `probe` is a writable buffer of the size the kernel is told
    // (`PROBE_OPS` entries after the 16-byte header), and the descriptor is
    // an open ring.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            fd.as_raw_fd(),
            IORING_REGISTER_PROBE,
            std::ptr::from_mut(&mut probe),
            PROBE_OPS as u32,
        )
    };
    if rc < 0 {
        return Err(Unavailable::NoStatx);
    }
    let supported = usize::from(IORING_OP_STATX) < usize::from(probe.ops_len)
        && probe
            .ops
            .get(usize::from(IORING_OP_STATX))
            .is_some_and(|op| op.flags & IO_URING_OP_SUPPORTED != 0);
    if supported {
        Ok(())
    } else {
        Err(Unavailable::NoStatx)
    }
}

/// One ring, owned by one thread.
struct Ring {
    fd: OwnedFd,
    maps: Vec<(*mut libc::c_void, usize)>,
    sqes: *mut Sqe,
    sq_tail: *const std::sync::atomic::AtomicU32,
    sq_mask: u32,
    sq_array: *mut u32,
    cq_head: *const std::sync::atomic::AtomicU32,
    cq_tail: *const std::sync::atomic::AtomicU32,
    cq_mask: u32,
    cqes: *const Cqe,
    entries: u32,
    /// The depth that was asked for.
    requested: u32,
    /// Names per batch: the depth, or the ring's size if that is smaller.
    depth: u32,
}

impl Drop for Ring {
    fn drop(&mut self) {
        for &(ptr, len) in &self.maps {
            // SAFETY: each pair is a mapping this ring made and has not
            // unmapped; nothing uses the pointers after this.
            unsafe {
                libc::munmap(ptr, len);
            }
        }
    }
}

fn map(fd: &OwnedFd, len: usize, offset: i64) -> io::Result<*mut libc::c_void> {
    // SAFETY: a shared mapping of the ring's descriptor at one of the
    // offsets the kernel defines; the result is checked before use.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd.as_raw_fd(),
            offset,
        )
    };
    if ptr == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(ptr)
    }
}

impl Ring {
    fn new(depth: u32) -> io::Result<Ring> {
        if FORCE_RING_FAILURE.load(Ordering::SeqCst) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let mut p = Params::default();
        let fd = setup(depth.max(1), &mut p)?;
        let sq_len = (p.sq_off.array as usize) + (p.sq_entries as usize) * 4;
        let cq_len =
            (p.cq_off.cqes as usize) + (p.cq_entries as usize) * std::mem::size_of::<Cqe>();
        let mut maps: Vec<(*mut libc::c_void, usize)> = Vec::new();
        let single = p.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq_map_len = if single { sq_len.max(cq_len) } else { sq_len };
        let sq_ptr = map(&fd, sq_map_len, IORING_OFF_SQ_RING)?;
        maps.push((sq_ptr, sq_map_len));
        let cq_ptr = if single {
            sq_ptr
        } else {
            match map(&fd, cq_len, IORING_OFF_CQ_RING) {
                Ok(ptr) => {
                    maps.push((ptr, cq_len));
                    ptr
                }
                Err(e) => {
                    unmap_all(&maps);
                    return Err(e);
                }
            }
        };
        let sqes_len = (p.sq_entries as usize) * std::mem::size_of::<Sqe>();
        let sqes = match map(&fd, sqes_len, IORING_OFF_SQES) {
            Ok(ptr) => {
                maps.push((ptr, sqes_len));
                ptr
            }
            Err(e) => {
                unmap_all(&maps);
                return Err(e);
            }
        };
        // SAFETY: the offsets come from the kernel and lie inside the
        // mappings sized from the same parameters; the ring words are 4-byte
        // aligned and shared with the kernel, which is why they are read and
        // written as atomics.
        let ring = unsafe {
            let sq = sq_ptr.cast::<u8>();
            let cq = cq_ptr.cast::<u8>();
            Ring {
                fd,
                sqes: sqes.cast::<Sqe>(),
                sq_tail: sq.add(p.sq_off.tail as usize).cast(),
                sq_mask: *sq.add(p.sq_off.ring_mask as usize).cast::<u32>(),
                sq_array: sq.add(p.sq_off.array as usize).cast(),
                cq_head: cq.add(p.cq_off.head as usize).cast(),
                cq_tail: cq.add(p.cq_off.tail as usize).cast(),
                cq_mask: *cq.add(p.cq_off.ring_mask as usize).cast::<u32>(),
                cqes: cq.add(p.cq_off.cqes as usize).cast(),
                entries: p.sq_entries,
                requested: depth,
                depth: depth.min(p.sq_entries).max(1),
                maps,
            }
        };
        Ok(ring)
    }

    fn enter(&self, to_submit: u32, min_complete: u32) -> io::Result<u32> {
        ENTERS.fetch_add(1, Ordering::Relaxed);
        loop {
            // SAFETY: a plain system call on this ring's descriptor with no
            // signal set (null pointer, size 0).
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_enter,
                    self.fd.as_raw_fd(),
                    to_submit,
                    min_complete,
                    if min_complete > 0 {
                        IORING_ENTER_GETEVENTS
                    } else {
                        0
                    },
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                )
            };
            if rc >= 0 {
                return Ok(rc as u32);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    /// Examines `names` (each a NUL-terminated name inside `dirfd`) with one
    /// `statx` request each and returns every result. At most `self.depth`
    /// names.
    ///
    /// The requests write into buffers owned by this call, so it does not
    /// return until every one has completed; if the ring fails with requests
    /// in flight the process aborts rather than let the kernel write into
    /// freed memory.
    fn stat_names(
        &mut self,
        dirfd: libc::c_int,
        names: &[&CStr],
        force_async: bool,
        out: &mut Vec<io::Result<super::sys::Stat>>,
    ) {
        out.clear();
        let n = names
            .len()
            .min(self.depth as usize)
            .min(self.entries as usize);
        let mut bufs: Vec<MaybeUninit<libc::statx>> = Vec::with_capacity(n);
        bufs.resize_with(n, MaybeUninit::zeroed);
        // SAFETY: the tail is written only by this thread; the kernel reads
        // it with acquire semantics when `io_uring_enter` is called.
        let tail_word = unsafe { &*self.sq_tail };
        let mut tail = tail_word.load(Ordering::Relaxed);
        let flags = if force_async { IOSQE_ASYNC } else { 0 };
        for (i, name) in names.iter().take(n).enumerate() {
            let slot = tail & self.sq_mask;
            let sqe = Sqe {
                opcode: IORING_OP_STATX,
                flags,
                fd: dirfd,
                off: bufs.as_mut_ptr().wrapping_add(i) as u64,
                addr: name.as_ptr() as u64,
                len: super::sys::WANT,
                op_flags: super::sys::FLAGS as u32,
                user_data: i as u64,
                ..Sqe::default()
            };
            // SAFETY: `slot` is below the ring size (masked), so both the
            // entry and its index word lie inside the mappings; the kernel
            // does not read them until the tail is published below.
            unsafe {
                self.sqes.add(slot as usize).write(sqe);
                self.sq_array.add(slot as usize).write(slot);
            }
            tail = tail.wrapping_add(1);
        }
        tail_word.store(tail, Ordering::Release);
        SQES.fetch_add(n as u64, Ordering::Relaxed);

        let mut results = vec![i32::MIN; n];
        let mut submitted = 0u32;
        let mut done = 0usize;
        let total = n as u32;
        // Submit everything and wait for everything in one call when the
        // kernel takes the whole batch at once.
        while done < n {
            let left = total - submitted;
            let wait = (n - done) as u32;
            let r = if left > 0 {
                self.enter(left, wait)
            } else {
                self.enter(0, wait)
            };
            match r {
                Ok(taken) => submitted += taken.min(left),
                Err(e) => {
                    // Requests may be in flight, writing into `bufs`.
                    eprintln!("io_uring_enter failed with requests in flight: {e}");
                    std::process::abort();
                }
            }
            // SAFETY: the completion words are shared with the kernel; the
            // tail is read with acquire so the entries below it are seen, and
            // the head is published with release once they are consumed.
            unsafe {
                let head_word = &*self.cq_head;
                let tail_word = &*self.cq_tail;
                let mut head = head_word.load(Ordering::Relaxed);
                let end = tail_word.load(Ordering::Acquire);
                while head != end {
                    let cqe = self.cqes.add((head & self.cq_mask) as usize).read();
                    if let Some(slot) = usize::try_from(cqe.user_data)
                        .ok()
                        .and_then(|i| results.get_mut(i))
                    {
                        *slot = cqe.res;
                        done += 1;
                    }
                    head = head.wrapping_add(1);
                }
                head_word.store(head, Ordering::Release);
            }
        }
        for (i, res) in results.into_iter().enumerate() {
            if res < 0 {
                out.push(Err(io::Error::from_raw_os_error(res.saturating_neg())));
            } else {
                // SAFETY: the request completed successfully, so the kernel
                // filled the buffer, which was zeroed before in any case.
                let raw = unsafe { bufs[i].assume_init_ref() };
                out.push(Ok(super::sys::convert(raw)));
            }
        }
    }
}

fn unmap_all(maps: &[(*mut libc::c_void, usize)]) {
    for &(ptr, len) in maps {
        // SAFETY: mappings made by `map` and not yet unmapped.
        unsafe {
            libc::munmap(ptr, len);
        }
    }
}

enum Slot {
    Untried,
    Ready(Ring),
    Failed,
}

thread_local! {
    static RING: RefCell<Slot> = const { RefCell::new(Slot::Untried) };
}

/// Runs `f` with this thread's ring, setting it up on first use. `None` when
/// the thread could not have one.
fn with_ring<R>(depth: u32, f: impl FnOnce(&mut Ring) -> R) -> Option<R> {
    RING.with(|cell| {
        let mut slot = cell.borrow_mut();
        if matches!(&*slot, Slot::Ready(r) if r.requested != depth) {
            *slot = Slot::Untried;
        }
        if matches!(&*slot, Slot::Untried) {
            *slot = match Ring::new(depth) {
                Ok(ring) => Slot::Ready(ring),
                Err(_) => {
                    THREAD_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                    Slot::Failed
                }
            };
        }
        match &mut *slot {
            Slot::Ready(ring) => Some(f(ring)),
            _ => None,
        }
    })
}

/// The listing loop with batched examination. See the module comment.
#[allow(clippy::too_many_arguments)]
pub(super) fn list_dir_into(
    shared: &Shared,
    config: Config,
    worker: &mut Worker<'_, '_, Task>,
    dir: &std::sync::Arc<OwnedFd>,
    key: DirKey,
    here: &Here<'_>,
    buf: &mut [u8],
    used: &mut usize,
) -> Listing {
    // A thread without a ring does the walker's own loop.
    if with_ring(config.depth, |_| ()).is_none() {
        return list_dir_plain(shared, worker, dir, key, here, buf, used);
    }
    let mut seen = 0usize;
    let mut results: Vec<io::Result<super::sys::Stat>> = Vec::new();
    loop {
        if worker.cancelled() {
            return Listing::Cancelled;
        }
        if let Some(dirty) = buf.get_mut(..*used) {
            dirty.fill(0);
        }
        *used = 0;
        let filled = match super::sys::getdents(dir.as_fd(), buf) {
            Ok(0) => return Listing::Complete,
            Ok(n) => n,
            Err(e) => return Listing::Failed(failure_of(&e)),
        };
        *used = filled;
        let mut entries = Vec::new();
        let mut damaged = false;
        for item in dirent::entries(&buf[..filled]) {
            match item {
                Ok(entry) => {
                    if !entry.is_dot_or_dotdot() {
                        entries.push(entry);
                    }
                }
                Err(_) => {
                    damaged = true;
                    break;
                }
            }
        }
        let batch_size = with_ring(config.depth, |r| r.depth as usize)
            .unwrap_or(1)
            .max(1);
        for chunk in entries.chunks(batch_size) {
            // Names that can be examined are examined together; the others
            // go through the walker's own handling, which does not look at
            // them.
            let mut names: Vec<&CStr> = Vec::with_capacity(chunk.len());
            let mut index_of: Vec<Option<usize>> = Vec::with_capacity(chunk.len());
            for entry in chunk {
                match CStr::from_bytes_with_nul(entry.name_with_nul) {
                    Ok(c) if entry.padding_is_clean => {
                        index_of.push(Some(names.len()));
                        names.push(c);
                    }
                    _ => index_of.push(None),
                }
            }
            let dirfd = dir.as_raw_fd();
            let ran = with_ring(config.depth, |ring| {
                ring.stat_names(dirfd, &names, config.force_async, &mut results);
            });
            if ran.is_none() {
                results.clear();
            }
            for (entry, at) in chunk.iter().zip(index_of) {
                let looked_up = at.and_then(|i| results.get_mut(i)).map(|slot| {
                    std::mem::replace(slot, Err(io::Error::from(io::ErrorKind::Other)))
                });
                handle_entry_with(shared, worker, dir, key, here, entry, |name_c| {
                    match looked_up {
                        Some(Ok(st)) => Ok(st),
                        // An interrupted request is retried the way the
                        // plain path retries; no result is looked up again.
                        Some(Err(e)) if e.kind() != io::ErrorKind::Interrupted => Err(e),
                        _ => super::sys::stat_at(dir.as_fd(), name_c),
                    }
                });
                seen += 1;
                if seen.is_multiple_of(super::FLUSH_CHECK_EVERY) {
                    worker.flush_if_due();
                    if worker.cancelled() {
                        return Listing::Cancelled;
                    }
                }
            }
        }
        if damaged {
            // The filesystem handed back a damaged listing.
            return Listing::Failed(ReadFailure {
                kind: ReadErrorKind::Corrupt,
                code: 0,
            });
        }
    }
}

/// The walker with its `statx` calls batched through io_uring, falling back
/// to the plain walker when the kernel cannot do it.
#[derive(Debug)]
pub struct IoUringWalker {
    config: Config,
    fell_back: AtomicBool,
}

impl IoUringWalker {
    /// A walker with this ring configuration.
    pub fn new(config: Config) -> IoUringWalker {
        IoUringWalker {
            config,
            fell_back: AtomicBool::new(false),
        }
    }

    /// Whether the most recent scan used the plain walker because the probe
    /// failed.
    pub fn fell_back(&self) -> bool {
        self.fell_back.load(Ordering::SeqCst)
    }
}

impl Scanner for IoUringWalker {
    fn name(&self) -> &'static str {
        "linux-walker-iouring"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        let usable = probe().is_ok();
        self.fell_back.store(!usable, Ordering::SeqCst);
        let variant = usable.then_some(self.config);
        super::scan_with_variant(root, ctx, variant)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::linux::sys;
    use std::os::fd::AsFd;

    /// Each request's result goes with its own name: a name that is not
    /// there fails with its own error and does not disturb its neighbours.
    #[test]
    fn results_stay_with_their_names() {
        if probe().is_err() {
            eprintln!("NOT RUN: io_uring statx unavailable here");
            return;
        }
        let root = sys::open_root(Path::new("/")).unwrap();
        let names: Vec<&CStr> = vec![
            c"usr",
            c"definitely-not-here-1",
            c"proc",
            c"definitely-not-here-2",
            c"usr",
        ];
        for force_async in [false, true] {
            let mut ring = Ring::new(4).unwrap();
            let mut out = Vec::new();
            // Five names, a ring of four: the batch is cut at the depth.
            ring.stat_names(root.as_raw_fd(), &names, force_async, &mut out);
            assert_eq!(out.len(), 4);
            assert!(out[0].is_ok());
            assert_eq!(out[1].as_ref().unwrap_err().kind(), io::ErrorKind::NotFound);
            assert!(out[2].is_ok());
            assert_eq!(out[3].as_ref().unwrap_err().kind(), io::ErrorKind::NotFound);
            let plain = sys::stat_at(root.as_fd(), c"usr").unwrap();
            let got = out[0].as_ref().unwrap();
            assert_eq!(
                (got.ino, got.mode, got.dev),
                (plain.ino, plain.mode, plain.dev)
            );
            // The same ring serves the next batch.
            ring.stat_names(root.as_raw_fd(), &names[4..], force_async, &mut out);
            assert_eq!(out.len(), 1);
            assert!(out[0].is_ok());
        }
    }

    #[test]
    fn an_empty_batch_returns_at_once() {
        if probe().is_err() {
            return;
        }
        let root = sys::open_root(Path::new("/")).unwrap();
        let mut ring = Ring::new(8).unwrap();
        let mut out = vec![Err(io::Error::from(io::ErrorKind::Other))];
        ring.stat_names(root.as_raw_fd(), &[], false, &mut out);
        assert!(out.is_empty());
    }
}
