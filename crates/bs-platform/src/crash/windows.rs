//! The Windows unhandled-exception filter.
//!
//! It runs when a thread hits an exception nothing handled, such as an access
//! violation, and writes a text report. It writes no memory dump. It always
//! returns "continue searching", so Windows still ends the program the way it
//! would have. It is best effort: a stack overflow leaves too little stack to
//! write anything, and the heap may be damaged.

#![allow(unsafe_code)]

use super::hook;
use super::report::Event;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
};

/// The filter's answer: pass the exception on.
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

/// Set while the filter runs, so an exception inside it does not start it
/// again.
static BUSY: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_filter() {
    // SAFETY: `filter` has the signature the system expects and stays valid
    // for the life of the program. The previous filter, if any, is dropped on
    // purpose: this one decides nothing about what happens next.
    unsafe {
        SetUnhandledExceptionFilter(Some(filter));
    }
}

unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    if BUSY.swap(true, Ordering::SeqCst) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: the system passes a pointer that is valid for the call, or null.
    let pointers = unsafe { info.as_ref() };
    // SAFETY: when the pointers exist, the record they hold is valid for the
    // call, or null.
    let record = pointers.and_then(|pointers| unsafe { pointers.ExceptionRecord.as_ref() });
    if let Some(record) = record {
        hook::write_event(&Event::Exception {
            // The code is a 32-bit pattern; the signed type is only how the
            // system declares it.
            code: record.ExceptionCode as u32,
            address: record.ExceptionAddress as usize as u64,
        });
    }
    BUSY.store(false, Ordering::SeqCst);
    EXCEPTION_CONTINUE_SEARCH
}
