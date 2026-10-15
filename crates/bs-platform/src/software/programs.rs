//! Matching a folder to the installed program that lives in it (Windows).
//!
//! A program's registry entry names its `InstallLocation`. A folder matches
//! when it is that location, or lies inside it. Locations that are too
//! general to name one program (a drive root, the Windows folder, the
//! `Program Files` roots, a user profile) are ignored, so a stray entry cannot
//! make a whole drive look like one program.

use super::uninstall::parse_uninstall_string;
use super::{Program, Relation};
use bs_knowledge::{Knowledge, Level};
use std::path::{Path, PathBuf};

/// What an uninstall entry in the registry says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Installed {
    pub name: String,
    pub publisher: Option<String>,
    /// `InstallLocation`, as stored.
    pub location: String,
    /// `UninstallString`, as stored.
    pub uninstall_string: Option<String>,
}

struct Entry {
    /// The location's folder names, compared without regard to case.
    parts: Vec<String>,
    program: Program,
}

pub(crate) struct ProgramIndex {
    entries: Vec<Entry>,
}

/// A path as comparable folder names: no `\\?\` prefix, quotes or trailing
/// separators; `/` read as `\`; lower case. It must start with a drive letter.
fn parts_of(path: &str) -> Option<Vec<String>> {
    let trimmed = path.trim().trim_matches('"').trim();
    let plain = trimmed
        .strip_prefix(r"\\?\")
        .unwrap_or(trimmed)
        .replace('/', "\\");
    if plain.contains('\0') {
        return None;
    }
    let parts: Vec<String> = plain
        .split('\\')
        .filter(|part| !part.is_empty())
        .map(|part| part.chars().flat_map(char::to_lowercase).collect())
        .collect();
    let first = parts.first()?;
    let drive = first.len() == 2
        && first.as_bytes()[0].is_ascii_alphabetic()
        && first.as_bytes()[1] == b':';
    // `..` and `.` would make the comparison depend on resolution.
    let odd = parts.iter().any(|p| p == ".." || p == ".");
    (drive && !odd && !plain.starts_with(r"\\")).then_some(parts)
}

/// Whether a location names a program's own folder.
fn specific_enough(parts: &[String], path: &str, knowledge: &Knowledge) -> bool {
    if parts.len() < 2 {
        return false;
    }
    if knowledge.protection(Path::new(path)).level == Level::Blocked {
        return false;
    }
    // A profile root and the shared data folder hold many programs' files.
    let second = parts[1].as_str();
    !((second == "users" && parts.len() <= 3) || (second == "programdata" && parts.len() <= 2))
}

impl ProgramIndex {
    pub fn new(installed: Vec<Installed>, knowledge: &Knowledge) -> Self {
        let mut entries = Vec::new();
        for item in installed {
            let Some(parts) = parts_of(&item.location) else {
                continue;
            };
            let location = parts_to_text(&item.location);
            if item.name.trim().is_empty() || !specific_enough(&parts, &location, knowledge) {
                continue;
            }
            let uninstall = item
                .uninstall_string
                .as_deref()
                .and_then(|text| parse_uninstall_string(text).ok());
            entries.push(Entry {
                parts,
                program: Program {
                    name: item.name.trim().to_owned(),
                    publisher: item.publisher.filter(|p| !p.trim().is_empty()),
                    location: PathBuf::from(location),
                    relation: Relation::Exact,
                    uninstall,
                },
            });
        }
        Self { entries }
    }

    /// The program that lives in `folder`.
    ///
    /// - A program whose location is the folder is an exact match. When two
    ///   programs with different names share the location, none is named.
    /// - Otherwise the program with the deepest location that holds the folder
    ///   is returned with [`Relation::Inside`], under the same rule for ties.
    pub fn find(&self, folder: &str) -> Option<Program> {
        let parts = parts_of(folder)?;
        let mut best: Option<(usize, Vec<&Entry>)> = None;
        for entry in &self.entries {
            if !parts.starts_with(&entry.parts) {
                continue;
            }
            let depth = entry.parts.len();
            match &mut best {
                Some((d, found)) if *d == depth => found.push(entry),
                Some((d, _)) if *d > depth => {}
                _ => best = Some((depth, vec![entry])),
            }
        }
        let (depth, found) = best?;
        let first = found[0];
        if found.iter().any(|e| e.program.name != first.program.name) {
            return None;
        }
        // Prefer an entry that has an uninstaller we can run.
        let chosen = found
            .iter()
            .find(|e| e.program.uninstall.is_some())
            .unwrap_or(&first);
        let mut program = chosen.program.clone();
        program.relation = if depth == parts.len() {
            Relation::Exact
        } else {
            Relation::Inside
        };
        Some(program)
    }
}

/// The location as text without quotes and without the `\\?\` prefix.
fn parts_to_text(path: &str) -> String {
    let trimmed = path.trim().trim_matches('"').trim();
    trimmed
        .strip_prefix(r"\\?\")
        .unwrap_or(trimmed)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_knowledge::{Environment, Host};

    fn knowledge() -> Knowledge {
        Knowledge::for_host(&Environment::new(), Host::Windows)
    }

    fn installed(name: &str, location: &str, uninstall: Option<&str>) -> Installed {
        Installed {
            name: name.into(),
            publisher: Some("Maker".into()),
            location: location.into(),
            uninstall_string: uninstall.map(Into::into),
        }
    }

    fn index(items: Vec<Installed>) -> ProgramIndex {
        ProgramIndex::new(items, &knowledge())
    }

    #[test]
    fn a_folder_is_matched_to_the_program_installed_in_it() {
        let idx = index(vec![
            installed(
                "Foo",
                r"C:\Program Files\Foo\",
                Some(r#""C:\Program Files\Foo\unins000.exe" /SILENT"#),
            ),
            installed("Bar", r"D:\Apps\Bar", None),
        ]);
        let foo = idx.find(r"C:\Program Files\Foo").unwrap();
        assert_eq!(foo.name, "Foo");
        assert_eq!(foo.publisher.as_deref(), Some("Maker"));
        assert_eq!(foo.relation, Relation::Exact);
        assert_eq!(foo.location, PathBuf::from(r"C:\Program Files\Foo"));
        assert!(foo.uninstall.is_some());

        // Case, a trailing separator, forward slashes and the extended prefix
        // do not matter.
        for spelling in [
            r"c:\program files\foo",
            r"C:\Program Files\Foo\",
            r"\\?\C:\Program Files\Foo",
            "C:/Program Files/Foo",
        ] {
            assert_eq!(idx.find(spelling).unwrap().name, "Foo", "{spelling}");
        }
        let bar = idx.find(r"D:\Apps\Bar").unwrap();
        assert_eq!(bar.name, "Bar");
        assert!(bar.uninstall.is_none());
    }

    #[test]
    fn a_folder_inside_a_program_is_marked_as_inside() {
        let idx = index(vec![installed("Foo", r"C:\Program Files\Foo", None)]);
        let found = idx.find(r"C:\Program Files\Foo\data\cache").unwrap();
        assert_eq!(found.relation, Relation::Inside);
        assert_eq!(found.name, "Foo");
        // A sibling whose name only starts the same is not inside.
        assert_eq!(idx.find(r"C:\Program Files\Foobar"), None);
        assert_eq!(idx.find(r"C:\Program Files"), None);
        assert_eq!(idx.find(r"C:\Program Files\Fo"), None);
        // The parent of the location is not the program.
        assert_eq!(idx.find(r"C:\"), None);
    }

    #[test]
    fn the_deepest_location_wins() {
        let idx = index(vec![
            installed("Suite", r"C:\Apps\Suite", None),
            installed("Suite Plugin", r"C:\Apps\Suite\Plugins\One", None),
        ]);
        assert_eq!(idx.find(r"C:\Apps\Suite").unwrap().name, "Suite");
        let inner = idx.find(r"C:\Apps\Suite\Plugins\One").unwrap();
        assert_eq!(inner.name, "Suite Plugin");
        assert_eq!(inner.relation, Relation::Exact);
        let deeper = idx.find(r"C:\Apps\Suite\Plugins\One\x").unwrap();
        assert_eq!(deeper.name, "Suite Plugin");
        assert_eq!(deeper.relation, Relation::Inside);
        assert_eq!(idx.find(r"C:\Apps\Suite\Plugins").unwrap().name, "Suite");
    }

    #[test]
    fn two_different_programs_in_one_folder_name_neither() {
        let idx = index(vec![
            installed("One", r"C:\Shared\Tools", None),
            installed("Two", r"C:\Shared\Tools", None),
        ]);
        assert_eq!(idx.find(r"C:\Shared\Tools"), None);
        assert_eq!(idx.find(r"C:\Shared\Tools\bin"), None);
        // The same program listed twice (for example in both registry views)
        // is one program, and the entry with an uninstaller is used.
        let idx = index(vec![
            installed("One", r"C:\Shared\Tools", None),
            installed("One", r"C:\Shared\Tools", Some(r"C:\Shared\Tools\u.exe")),
        ]);
        assert!(idx.find(r"C:\Shared\Tools").unwrap().uninstall.is_some());
    }

    #[test]
    fn locations_too_general_to_name_a_program_are_ignored() {
        let idx = index(vec![
            installed("Whole drive", r"C:\", None),
            installed("Drive letter", "C:", None),
            installed("Windows", r"C:\Windows", None),
            installed("Windows system", r"C:\Windows\System32", None),
            installed("Program Files", r"C:\Program Files", None),
            installed("Program Files x86", r"C:\Program Files (x86)", None),
            installed("Users", r"C:\Users", None),
            installed("Profile", r"C:\Users\me", None),
            installed("Shared data", r"C:\ProgramData", None),
            installed("Network", r"\\server\share\app", None),
            installed("Relative", r"app\folder", None),
            installed("Dots", r"C:\Apps\..\Windows", None),
            installed("Empty", "", None),
            installed("", r"C:\Apps\Unnamed", None),
        ]);
        for path in [
            r"C:\",
            r"C:\Windows",
            r"C:\Windows\System32",
            r"C:\Windows\System32\drivers",
            r"C:\Program Files",
            r"C:\Program Files\Anything",
            r"C:\Users",
            r"C:\Users\me",
            r"C:\ProgramData",
            r"C:\Apps\Unnamed",
            r"\\server\share\app",
        ] {
            assert_eq!(idx.find(path), None, "{path}");
        }
        // Below a profile, a program's own folder is specific.
        let idx = index(vec![installed(
            "Local app",
            r"C:\Users\me\AppData\Local\Programs\App",
            None,
        )]);
        assert!(
            idx.find(r"C:\Users\me\AppData\Local\Programs\App")
                .is_some()
        );
    }

    #[test]
    fn an_uninstall_string_that_cannot_be_used_still_names_the_program() {
        let idx = index(vec![installed(
            "Odd",
            r"C:\Apps\Odd",
            Some("cmd.exe /c rd /s /q C:\\Apps\\Odd"),
        )]);
        let odd = idx.find(r"C:\Apps\Odd").unwrap();
        assert_eq!(odd.name, "Odd");
        assert!(odd.uninstall.is_none());
    }

    #[test]
    fn odd_input_does_not_panic() {
        let idx = index(vec![installed("Foo", r"C:\Apps\Foo", None)]);
        for path in ["", "\0", r"C:\Apps\Foo\..", "é☃", r"\\?\", "C:", "/"] {
            let _ = idx.find(path);
        }
        assert_eq!(idx.find(r"C:\Apps\Foo\..\Foo"), None);
    }
}
