//! The user's places, for the categories of a scan.
//!
//! The helper runs as another user (`/root` is its home under `pkexec`), so
//! the category tables cannot be resolved against its own environment: the
//! results would differ from what the interface shows. The interface sends its
//! places in the scan request, and they are the only source. The scanners read
//! them from the process environment once, as they start, so this replaces
//! those variables with the request's values and clears the rest.

#![allow(unsafe_code)]

use bs_ipc::{Place, path_from_wire};
use bs_knowledge::Environment;
use std::ffi::OsString;

/// Why a place was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadPlace {
    /// The name is not one the tables use.
    UnknownName,
    /// The value is not an absolute path.
    NotAbsolute,
    /// The name is given twice.
    Duplicate,
}

/// Checks the places of a request.
pub fn parse(places: &[Place]) -> Result<Vec<(&'static str, OsString)>, BadPlace> {
    let mut out: Vec<(&'static str, OsString)> = Vec::with_capacity(places.len());
    for place in places {
        let name = Environment::variable_names()
            .iter()
            .copied()
            .find(|known| known.eq_ignore_ascii_case(&place.name))
            .ok_or(BadPlace::UnknownName)?;
        if out.iter().any(|(taken, _)| *taken == name) {
            return Err(BadPlace::Duplicate);
        }
        let value = path_from_wire(&place.value).map_err(|_| BadPlace::NotAbsolute)?;
        if place.value.is_empty() || place.value.contains(&0) || !value.is_absolute() {
            return Err(BadPlace::NotAbsolute);
        }
        out.push((name, value.into_os_string()));
    }
    Ok(out)
}

/// Makes the places the whole environment the tables see: every variable they
/// use is removed, then the requested ones are set.
///
/// Call it before the scan's own threads exist.
pub fn apply(places: &[(&'static str, OsString)]) {
    for name in Environment::variable_names() {
        // SAFETY: nothing else reads or changes the environment while this
        // runs. The helper's other threads only wait on pipes and sleep, and
        // the scan's threads, which read the variables, are started after this
        // returns.
        unsafe { std::env::remove_var(name) };
    }
    for (name, value) in places {
        // SAFETY: as above.
        unsafe { std::env::set_var(name, value) };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use bs_ipc::path_to_wire;
    use std::path::Path;

    fn place(name: &str, value: &[u8]) -> Place {
        Place {
            name: name.into(),
            value: value.to_vec(),
        }
    }

    fn absolute(text: &str) -> Vec<u8> {
        let path = if cfg!(windows) {
            format!("C:\\{text}")
        } else {
            format!("/{text}")
        };
        path_to_wire(Path::new(&path))
    }

    #[test]
    fn known_names_with_absolute_values_are_accepted() {
        let parsed = parse(&[
            place("home", &absolute("home")),
            place("LOCALAPPDATA", &absolute("x")),
        ])
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "HOME");
        assert_eq!(parsed[1].0, "LOCALAPPDATA");
    }

    #[test]
    fn anything_else_is_refused() {
        let ok = absolute("a");
        assert_eq!(parse(&[place("PATH", &ok)]), Err(BadPlace::UnknownName));
        assert_eq!(
            parse(&[place("LD_PRELOAD", &ok)]),
            Err(BadPlace::UnknownName)
        );
        assert_eq!(
            parse(&[place("HOME", &path_to_wire(Path::new("relative")))]),
            Err(BadPlace::NotAbsolute)
        );
        assert_eq!(parse(&[place("HOME", b"")]), Err(BadPlace::NotAbsolute));
        let mut with_nul = ok.clone();
        with_nul.extend_from_slice(&[0; 2]);
        assert_eq!(
            parse(&[place("HOME", &with_nul)]),
            Err(BadPlace::NotAbsolute)
        );
        assert_eq!(
            parse(&[place("HOME", &ok), place("home", &absolute("b"))]),
            Err(BadPlace::Duplicate)
        );
    }
}
