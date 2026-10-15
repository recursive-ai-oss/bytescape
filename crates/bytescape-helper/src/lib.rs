//! The elevated helper. It starts only when the user asks for a full scan or
//! an elevated delete, accepts requests only from the interface that launched
//! it, reads, deletes only the paths the interface lists, never starts other
//! programs, and exits when its work is done.
//!
//! It reads untrusted disk data and sits on the privilege boundary, so the
//! panicking shortcuts, unchecked arithmetic and unchecked casts are denied in
//! this crate. The threat model is in `docs/security/helper.md`.
//!
//! # One run
//!
//! 1. The command line must match the fixed schema in [`args`].
//! 2. The operating system entry point checks who is on the other end
//!    ([`linux`] or [`windows`]).
//! 3. [`session::run`] reads the hello, checks the launch value, reads one
//!    request and answers it. [`scan`] hosts scans; [`delete`] judges and
//!    deletes.
//! 4. The process exits. It also exits at once when the connection closes, and
//!    when the interface goes quiet.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

pub mod args;
pub mod delete;
#[cfg(target_os = "linux")]
pub mod mounts;
pub mod places;
mod plan;
pub mod scan;
pub mod session;
pub mod volume;

#[cfg(all(test, target_os = "linux"))]
mod testutil;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

use std::process::ExitCode;

/// The exit status when the helper is started wrongly, or on a system it does
/// not support.
pub const EXIT_USAGE: u8 = 2;
/// The exit status when the peer could not be verified.
pub const EXIT_PEER: u8 = 3;
/// The exit status after an internal error.
pub const EXIT_INTERNAL: u8 = 70;

/// Runs the helper as a process: checks the peer, serves one request and
/// returns the exit status.
pub fn run_process() -> ExitCode {
    install_panic_hook();
    #[cfg(target_os = "linux")]
    {
        linux::main()
    }
    #[cfg(windows)]
    {
        windows::main()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        ExitCode::from(EXIT_USAGE)
    }
}

/// A panic ends the whole process at once. Nothing is written about it: a
/// message could carry a path, and a helper with rights must not carry on after
/// a bug.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        std::process::exit(i32::from(EXIT_INTERNAL));
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bytescape-helper");
    }

    #[test]
    fn build_metadata_is_exported() {
        assert_eq!(env!("BYTESCAPE_VERSION"), env!("CARGO_PKG_VERSION"));
        assert!(!env!("BYTESCAPE_BUILD_DATE").is_empty());
        assert!(!env!("BYTESCAPE_TARGET").is_empty());
    }
}
