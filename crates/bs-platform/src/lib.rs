//! Platform services: the Recycle Bin and Trash, revealing files in the file
//! manager, terminals, drive lists, installed-program and package lookup,
//! launching the elevated helper, the light and dark, accent and
//! reduced-motion settings, and local crash reports.

#[cfg(target_os = "linux")]
pub mod a11y_bus;
pub mod appearance;
pub mod bins;
pub mod clipboard;
pub mod clock;
pub mod crash;
pub mod delete;
pub mod dialog;
pub mod drives;
pub mod launch;
pub mod software;

pub use clock::local_utc_offset_seconds;

#[cfg(windows)]
mod winreg;

#[cfg(windows)]
mod winshell;

#[cfg(all(test, target_os = "linux"))]
mod testutil;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bs-platform");
    }
}
