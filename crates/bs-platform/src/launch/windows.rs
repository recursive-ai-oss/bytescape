//! Handoffs on Windows. Files, folders, links and uninstallers go through the
//! shell, which applies the user's file associations and any elevation the
//! program asks for itself; see the module table in [`super`].

#![allow(unsafe_code)]

use super::LaunchError;
use super::plan;
use crate::software::{UninstallCommand, quote_arguments};
use crate::winshell::{ComScope, system_directory, wide};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn failed(step: &'static str, code: u32) -> LaunchError {
    LaunchError::Failed {
        step,
        kind: io::ErrorKind::Other,
        code: Some(code as i32),
    }
}

/// The path in the plain form the shell accepts, which must exist.
fn existing_plain(path: &Path) -> Result<String, LaunchError> {
    let text = path
        .to_str()
        .ok_or(LaunchError::BadInput("the path is not text"))?;
    let plain = plan::plain_windows_path(text)?;
    if std::fs::symlink_metadata(&plain).is_err() {
        return Err(LaunchError::BadInput("the path does not exist"));
    }
    Ok(plain)
}

/// Asks the shell to do `verb` (or the default action when `None`) on `file`.
///
/// `file` and `parameters` never go through a command interpreter: the shell
/// starts a program directly, or hands a document or address to its handler.
fn shell_execute(
    step: &'static str,
    verb: Option<&str>,
    file: &str,
    parameters: Option<&str>,
    directory: Option<&str>,
    mask: u32,
) -> Result<(), LaunchError> {
    let _com = ComScope::enter().map_err(|hr| failed(step, hr as u32))?;
    let verb = verb.map(|v| wide(OsStr::new(v)));
    let file = wide(OsStr::new(file));
    let parameters = parameters.map(|p| wide(OsStr::new(p)));
    let directory = directory.map(|d| wide(OsStr::new(d)));
    let pointer = |text: &Option<Vec<u16>>| text.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
    // SAFETY: `SHELLEXECUTEINFOW` is plain data; zero is a valid empty value.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = mask;
    info.lpVerb = pointer(&verb);
    info.lpFile = file.as_ptr();
    info.lpParameters = pointer(&parameters);
    info.lpDirectory = pointer(&directory);
    info.nShow = SW_SHOWNORMAL;
    // SAFETY: `info` is filled in as the call requires, and every string it
    // points to is NUL-terminated and outlives the call.
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        // SAFETY: reads this thread's last error.
        return Err(failed(step, unsafe { GetLastError() }));
    }
    Ok(())
}

pub(crate) fn reveal(path: &Path) -> Result<(), LaunchError> {
    let plain = existing_plain(path)?;
    let _com = ComScope::enter().map_err(|hr| failed("start COM", hr as u32))?;
    let name = wide(OsStr::new(&plain));
    let mut item: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: `name` is NUL-terminated and outlives the call; `item` is
    // writable and receives an item the shell allocates.
    let hr = unsafe {
        SHParseDisplayName(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut item,
            0,
            std::ptr::null_mut(),
        )
    };
    if hr < 0 || item.is_null() {
        return Err(failed("find the item", hr as u32));
    }
    // SAFETY: `item` is a valid absolute item list from the call above; with
    // no children listed the shell selects the item itself.
    let hr = unsafe { SHOpenFolderAndSelectItems(item, 0, std::ptr::null(), 0) };
    // SAFETY: `item` came from `SHParseDisplayName` and is freed once.
    unsafe { ILFree(item) };
    if hr < 0 {
        return Err(failed("show the item", hr as u32));
    }
    Ok(())
}

/// The properties window belongs to the thread that opens it. The shell is
/// asked to finish before the call returns, but call this from a thread that
/// keeps running, such as the interface thread.
pub(crate) fn properties(path: &Path) -> Result<(), LaunchError> {
    let plain = existing_plain(path)?;
    shell_execute(
        "show the properties",
        Some("properties"),
        &plain,
        None,
        None,
        SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
    )
}

pub(crate) fn open(path: &Path) -> Result<(), LaunchError> {
    let plain = existing_plain(path)?;
    shell_execute(
        "open the item",
        None,
        &plain,
        None,
        None,
        SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
    )
}

/// Hands an address that was checked by the caller to the shell.
pub(crate) fn open_link(url: &str) -> Result<(), LaunchError> {
    shell_execute(
        "open the link",
        None,
        url,
        None,
        None,
        SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
    )
}

/// Where Windows Terminal puts its launcher for the current user.
fn windows_terminal() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    if !base.is_absolute() {
        return None;
    }
    let launcher = base.join(r"Microsoft\WindowsApps\wt.exe");
    // The launcher is an app execution alias, which cannot be opened like an
    // ordinary file; looking at the link itself is enough.
    std::fs::symlink_metadata(&launcher)
        .is_ok()
        .then_some(launcher)
}

pub(crate) fn open_terminal(dir: &Path) -> Result<(), LaunchError> {
    let plain = existing_plain(dir)?;
    if !std::fs::metadata(&plain).is_ok_and(|m| m.is_dir()) {
        return Err(LaunchError::BadInput("the folder does not exist"));
    }
    let mask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    if let Some(terminal) = windows_terminal()
        && let Some(terminal) = terminal.to_str()
    {
        let parameters = quote_arguments(&["-d".to_owned(), plain.clone()]);
        if shell_execute(
            "start Windows Terminal",
            None,
            terminal,
            Some(&parameters),
            Some(&plain),
            mask,
        )
        .is_ok()
        {
            return Ok(());
        }
    }
    let system = system_directory().ok_or(LaunchError::ProgramMissing("powershell.exe"))?;
    let powershell = system.join(r"WindowsPowerShell\v1.0\powershell.exe");
    let powershell = powershell
        .to_str()
        .ok_or(LaunchError::ProgramMissing("powershell.exe"))?;
    shell_execute(
        "start PowerShell",
        None,
        powershell,
        Some("-NoLogo"),
        Some(&plain),
        mask,
    )
}

pub(crate) fn open_disk_cleanup(drive: Option<char>) -> Result<(), LaunchError> {
    let mask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    let storage = shell_execute(
        "open Storage settings",
        None,
        "ms-settings:storagesense",
        None,
        None,
        mask,
    );
    if storage.is_ok() {
        return Ok(());
    }
    let system = system_directory().ok_or(LaunchError::ProgramMissing("cleanmgr.exe"))?;
    let cleanmgr = system.join("cleanmgr.exe");
    let cleanmgr = cleanmgr
        .to_str()
        .ok_or(LaunchError::ProgramMissing("cleanmgr.exe"))?;
    let parameters = drive
        .filter(char::is_ascii_alphabetic)
        .map(|letter| format!("/d {}", letter.to_ascii_uppercase()));
    shell_execute(
        "start Disk Cleanup",
        None,
        cleanmgr,
        parameters.as_deref(),
        None,
        mask,
    )
}

pub(crate) fn uninstall(command: &UninstallCommand) -> Result<(), LaunchError> {
    let exe: PathBuf = match command {
        UninstallCommand::Program { exe, .. } => PathBuf::from(exe),
        // Whatever the registered string named, the system's own installer
        // service program is the one that runs.
        UninstallCommand::Msi { .. } => system_directory()
            .ok_or(LaunchError::ProgramMissing("msiexec.exe"))?
            .join("msiexec.exe"),
    };
    let exe_text = exe
        .to_str()
        .ok_or(LaunchError::BadInput("the uninstaller path is not text"))?;
    // The command was checked when it was read, but it is also checked here.
    let exe_text = plan::plain_windows_path(exe_text)?;
    if exe_text.starts_with(r"\\") || !std::fs::metadata(&exe_text).is_ok_and(|m| m.is_file()) {
        return Err(LaunchError::BadInput("the uninstaller is not there"));
    }
    let parameters = quote_arguments(&command.arguments());
    let directory = Path::new(&exe_text)
        .parent()
        .and_then(Path::to_str)
        .map(str::to_owned);
    shell_execute(
        "start the uninstaller",
        None,
        &exe_text,
        (!parameters.is_empty()).then_some(parameters.as_str()),
        directory.as_deref(),
        SEE_MASK_NOASYNC,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// These start real programs on the machine, so they run only where the
    /// environment says the machine is disposable.
    fn disposable_host() -> bool {
        let yes = std::env::var_os("BYTESCAPE_DISPOSABLE_HOST").is_some_and(|v| v == "1");
        if !yes {
            eprintln!("skipped: BYTESCAPE_DISPOSABLE_HOST is not set to 1");
        }
        yes
    }

    #[test]
    fn the_system_folder_is_a_real_absolute_folder() {
        let system = system_directory().expect("system folder");
        assert!(system.is_absolute());
        assert!(system.is_dir());
        assert!(system.join("cmd.exe").is_file());
        assert!(
            system
                .to_string_lossy()
                .to_lowercase()
                .ends_with("system32")
        );
    }

    #[test]
    fn paths_that_are_not_absolute_or_not_there_are_refused() {
        let missing = std::env::temp_dir().join("bytescape-no-such-item-for-handoffs");
        for bad in [
            PathBuf::from(r"relative\file"),
            PathBuf::from("-flag"),
            PathBuf::from(r"\\.\PhysicalDrive0"),
            missing,
        ] {
            assert!(
                matches!(reveal(&bad), Err(LaunchError::BadInput(_))),
                "{bad:?}"
            );
            assert!(
                matches!(properties(&bad), Err(LaunchError::BadInput(_))),
                "{bad:?}"
            );
            assert!(
                matches!(open(&bad), Err(LaunchError::BadInput(_))),
                "{bad:?}"
            );
            assert!(
                matches!(open_terminal(&bad), Err(LaunchError::BadInput(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_uninstaller_that_is_not_there_is_not_started() {
        let command = UninstallCommand::Program {
            exe: r"C:\bytescape-no-such-folder\unins000.exe".into(),
            args: vec!["/S".into()],
        };
        assert!(matches!(uninstall(&command), Err(LaunchError::BadInput(_))));
        let network = UninstallCommand::Program {
            exe: r"\\server\share\unins000.exe".into(),
            args: Vec::new(),
        };
        assert!(matches!(uninstall(&network), Err(LaunchError::BadInput(_))));
    }

    #[test]
    fn a_registered_program_is_started_through_the_shell_with_its_arguments() {
        if !disposable_host() {
            return;
        }
        // `whoami` is a console program that prints and exits.
        let system = system_directory().unwrap();
        let exe = system.join("whoami.exe");
        let command = UninstallCommand::Program {
            exe: exe.to_string_lossy().into_owned(),
            args: vec!["/user".into()],
        };
        uninstall(&command).expect("start whoami");
    }

    #[test]
    fn the_shell_opens_a_file_and_a_folder_for_the_user() {
        if !disposable_host() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("bs-platform-open-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A folder opens in Explorer; no program association is needed.
        open(&dir).expect("open the folder");
        reveal(&dir).expect("show the folder");
        let _ = fs::remove_dir_all(&dir);
    }
}
