//! Lowering the priority of the calling thread.

#![allow(unsafe_code)]

/// Makes the calling thread yield the processor to ordinary work.
pub(super) fn lower() {
    // SAFETY: `setpriority` with `PRIO_PROCESS` and id 0 changes the niceness
    // of the calling thread on Linux. No memory is involved, and a failure only
    // leaves the priority as it was.
    unsafe {
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}
