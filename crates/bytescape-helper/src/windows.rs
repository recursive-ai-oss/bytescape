//! The Windows entry point: connect to the interface's pipe, and check that
//! the process that created it is the interface that launched this helper.
//!
//! The interface creates a pipe with a random name that only its own user can
//! open, starts the helper through the system's elevation prompt, and passes
//! the pipe's name, the one-time launch value and its own process id on the
//! command line. Before the helper says anything it checks that the process
//! which created the pipe is the one it was told about, and that this process
//! is running the interface's program, which sits beside the helper's own.
//! The pipe is opened so that the other side can never act with the helper's
//! rights.

#![allow(unsafe_code)]

use crate::args;
use crate::delete::Judge;
use crate::scan::Host;
use crate::session::{self, Config, Exit};
use crate::{EXIT_PEER, EXIT_USAGE};
use bs_fsops::winpipe::{PipeEnd, process_image_path};
use bs_knowledge::{Environment, Knowledge};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use windows_sys::Win32::Storage::FileSystem::GetLogicalDriveStringsW;
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

/// The name of the interface's program, which sits beside the helper's.
const INTERFACE_EXE: &str = "bytescape.exe";

/// Whether `found` is the interface program beside `own_exe`: the same folder,
/// the same name, compared the way Windows compares paths. Both are resolved
/// first, so a short name or a link does not make a different program look the
/// same, or the same program look different.
pub(crate) fn is_the_interface(own_exe: &Path, found: &Path) -> bool {
    let Some(folder) = own_exe.parent() else {
        return false;
    };
    let expected = folder.join(INTERFACE_EXE);
    let (Ok(expected), Ok(found)) = (
        std::fs::canonicalize(expected),
        std::fs::canonicalize(found),
    ) else {
        return false;
    };
    bs_fsops::winpath::same_path(&expected.to_string_lossy(), &found.to_string_lossy())
}

/// Whether the process at the other end of the pipe is the process this helper
/// was told launched it, and is the interface's program.
fn peer_is_the_interface(pipe: &PipeEnd, parent: u32) -> bool {
    if pipe.server_process_id().ok() != Some(parent) {
        return false;
    }
    let (Ok(own), Ok(image)) = (std::env::current_exe(), process_image_path(parent)) else {
        return false;
    };
    is_the_interface(&own, &image)
}

/// The number of 16-bit units a buffer holds, as the system calls count them.
fn units_of(buffer: &[u16]) -> u32 {
    u32::try_from(buffer.len()).unwrap_or(u32::MAX)
}

fn wide_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// The Windows folder as the system reports it, and every drive root.
fn system_knowledge() -> Knowledge {
    let mut env = Environment::from_lookup(|name| std::env::var_os(name));
    let mut buffer = [0u16; 512];
    // SAFETY: `buffer` is writable for the number of units passed.
    let len = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), units_of(&buffer)) };
    if let Some(units) = buffer.get(..len as usize)
        && len > 0
    {
        // The system's own answer replaces whatever the environment says.
        env = env.with("SystemRoot", wide_to_string(units));
    }
    let mut drives = [0u16; 512];
    // SAFETY: `drives` is writable for the number of units passed.
    let len = unsafe { GetLogicalDriveStringsW(units_of(&drives), drives.as_mut_ptr()) };
    let roots: Vec<PathBuf> = drives
        .get(..len as usize)
        .map(|units| {
            units
                .split(|&u| u == 0)
                .filter(|part| !part.is_empty())
                .map(|part| PathBuf::from(wide_to_string(part)))
                .collect()
        })
        .unwrap_or_default();
    Knowledge::new(&env.with_drive_roots(roots))
}

pub(crate) fn main() -> ExitCode {
    let argv: Vec<_> = std::env::args_os().skip(1).collect();
    let Ok(args) = args::parse(&argv, true) else {
        return ExitCode::from(EXIT_USAGE);
    };
    let (Some(name), Some(parent)) = (args.pipe.as_deref(), args.parent) else {
        return ExitCode::from(EXIT_USAGE);
    };
    let Ok(pipe) = PipeEnd::connect(name) else {
        return ExitCode::from(EXIT_PEER);
    };
    if !peer_is_the_interface(&pipe, parent) {
        return ExitCode::from(EXIT_PEER);
    }
    let config = Config {
        nonce: args.nonce,
        mode: args.mode,
        request_timeout: session::REQUEST_TIMEOUT,
        write_stall: session::WRITE_STALL,
        judge: Judge::new(system_knowledge()),
        host: Host::system(),
        on_stall: Box::new(|| std::process::exit(i32::from(Exit::Idle.code()))),
    };
    let exit = session::run(pipe.clone(), pipe, config);
    ExitCode::from(exit.code())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_the_program_beside_the_helper_is_the_interface() {
        // A stand-in helper and interface in a folder made for the test.
        let base = std::env::temp_dir().join(format!("bs-helper-win-{}", std::process::id()));
        let folder = base.join("app");
        std::fs::create_dir_all(&folder).unwrap();
        let own = folder.join("bytescape-helper.exe");
        let beside = folder.join(INTERFACE_EXE);
        let elsewhere = base.join(INTERFACE_EXE);
        for file in [&own, &beside, &elsewhere] {
            std::fs::write(file, b"x").unwrap();
        }
        assert!(is_the_interface(&own, &beside));
        assert!(is_the_interface(
            &own,
            &PathBuf::from(beside.to_string_lossy().to_uppercase())
        ));
        // Any other program, even one with the same name elsewhere, is not.
        assert!(!is_the_interface(&own, &elsewhere));
        assert!(!is_the_interface(&own, &own));
        assert!(!is_the_interface(&own, Path::new(r"C:\does\not\exist.exe")));
        let _ = std::fs::remove_dir_all(&base);
    }
}
