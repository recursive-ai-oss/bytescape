//! The user's places, for the categories of a scan.
//!
//! The helper runs as another user (`/root` is its home under `pkexec`), so
//! the category tables cannot be resolved against its own environment: the
//! results would differ from what the interface shows. The interface sends its
//! places in the scan request, and they are the only source. They are handed
//! to the scan as its [`Environment`]; the helper's own environment is never
//! read for them and never changed.

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
        if place.value.is_empty() || bs_ipc::wire_has_nul(&place.value) || !value.is_absolute() {
            return Err(BadPlace::NotAbsolute);
        }
        out.push((name, value.into_os_string()));
    }
    Ok(out)
}

/// The environment the category tables resolve against: exactly the places of
/// the request, and nothing from the helper's own environment.
pub fn environment(places: &[(&'static str, OsString)]) -> Environment {
    places
        .iter()
        .fold(Environment::new(), |env, (name, value)| {
            env.with(name, value.as_encoded_bytes())
        })
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

    #[test]
    fn the_environment_holds_the_requests_places_and_nothing_else() {
        use bs_knowledge::{Category, Knowledge};
        let home = absolute_text("home");
        // A place that exists only relative to a variable of the request: the
        // user's home on Linux, the local application data folder on Windows.
        let (variable, folder, want) = if cfg!(windows) {
            ("LOCALAPPDATA", format!(r"{home}\Temp"), Category::CacheTemp)
        } else {
            (
                "home",
                format!("{home}/.local/share/Steam/steamapps/game"),
                Category::AppsSystem,
            )
        };
        let category = |env: &Environment| Knowledge::new(env).place_bytes(folder.as_bytes());
        let parsed = parse(&[place(variable, &absolute("home"))]).unwrap();
        // The request's place is the one the tables resolve against.
        assert_eq!(
            category(&environment(&parsed)),
            Some(want),
            "the places of the request are used"
        );
        // No places: nothing resolves, whatever the process's own variables.
        assert_eq!(category(&environment(&[])), None);
    }

    fn absolute_text(text: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{text}")
        } else {
            format!("/{text}")
        }
    }
}
