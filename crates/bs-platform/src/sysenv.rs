//! The values the protected-path table refers to, for this computer.

use bs_knowledge::Environment;

/// The variables of the protected-path table (`HOME`, `SystemRoot`,
/// `ProgramFiles`, ...).
///
/// On Windows they come from the system's own answers and never from the
/// process environment, so the interface and the elevated helper build the
/// same table from the same facts and cannot disagree about which folders are
/// the operating system's (see `bs_fsops::folders`). A folder the system does
/// not give is left out. Elsewhere they come from the process environment,
/// which is the user's own.
pub fn system_environment() -> Environment {
    #[cfg(windows)]
    {
        bs_fsops::folders::trusted(&bs_fsops::folders::System)
            .into_iter()
            .fold(Environment::new(), |env, (name, value)| {
                env.with(name, value)
            })
    }
    #[cfg(not(windows))]
    {
        Environment::from_lookup(|name| std::env::var_os(name))
    }
}

#[cfg(all(test, windows))]
mod tests {
    #![allow(unsafe_code)]
    use super::*;
    use bs_knowledge::{Knowledge, Level};
    use std::path::Path;

    fn level(env: &Environment, path: &str) -> Level {
        Knowledge::new(env).protection(Path::new(path)).level
    }

    #[test]
    fn the_process_environment_does_not_shape_the_table_on_windows() {
        let names = ["SystemRoot", "ProgramFiles", "ProgramData", "USERPROFILE"];
        let old: Vec<_> = names.iter().map(std::env::var_os).collect();
        let clean = system_environment();
        for name in names {
            // SAFETY: CI runs these tests one at a time, and nothing here
            // starts threads that read these variables.
            unsafe { std::env::set_var(name, r"Z:\planted\x") };
        }
        let planted = system_environment();
        for (name, value) in names.iter().zip(old) {
            // SAFETY: as above.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        for path in [
            r"Z:\planted\x",
            r"Z:\planted\x\y",
            r"C:\Windows",
            r"C:\Program Files",
        ] {
            assert_eq!(level(&planted, path), level(&clean, path), "{path}");
        }
        assert_eq!(level(&planted, r"Z:\planted\x"), Level::Normal);
    }
}
