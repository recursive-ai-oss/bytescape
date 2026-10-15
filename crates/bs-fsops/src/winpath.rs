//! Windows path comparisons that need no Windows call, so they are tested on
//! every system.

/// Windows compares file names without regard to case.
pub fn fold(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// `\\?\C:\x` as `C:\x` and `\\?\UNC\host\share` as `\\host\share`, which is
/// how the protected-path table reads paths. Other text is unchanged.
pub fn strip_extended_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => path.to_owned(),
    }
}

/// Whether two Windows paths are the same place, apart from case, the `\\?\`
/// prefix and a trailing separator.
pub fn same_path(a: &str, b: &str) -> bool {
    fold(&strip_extended_prefix(a)) == fold(&strip_extended_prefix(b))
}

/// Whether `path` is `root` or lies below it, comparing as Windows does:
/// without regard to case, to `/` against `\`, or to the `\\?\` prefix.
pub fn is_inside(root: &str, path: &str) -> bool {
    let norm = |text: &str| fold(&strip_extended_prefix(text)).replace('/', "\\");
    let root = norm(root);
    let root = root.trim_end_matches('\\');
    let path = norm(path);
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('\\'))
}

// ------------------------------------------------------- Exact (16-bit) forms
//
// Windows file names are sequences of 16-bit units, and a name may hold an
// unpaired surrogate that no `str` can carry. Turning such a name into text
// replaces the surrogate with U+FFFD, which is a different name: a different
// file, perhaps next to it in the same folder. Everything that opens a file or
// decides that two paths are the same place therefore works on the units.

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const QUESTION: u16 = b'?' as u16;
const DOT: u16 = b'.' as u16;
const COLON: u16 = b':' as u16;

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// `path` with letters lower-cased as Windows compares names. A unit that is
/// not part of a valid pair is kept exactly as it is, so two names that differ
/// only in an unpaired surrogate stay different.
pub fn fold_units(path: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(path.len());
    for piece in char::decode_utf16(path.iter().copied()) {
        match piece {
            Ok(c) => {
                let mut buffer = [0u16; 2];
                for lower in c.to_lowercase() {
                    out.extend_from_slice(lower.encode_utf16(&mut buffer));
                }
            }
            Err(error) => out.push(error.unpaired_surrogate()),
        }
    }
    out
}

/// [`strip_extended_prefix`] on 16-bit units.
pub fn strip_extended_prefix_units(path: &[u16]) -> Vec<u16> {
    let unc = units(r"\\?\UNC\");
    if let Some(rest) = path.strip_prefix(unc.as_slice()) {
        let mut out = vec![BACKSLASH, BACKSLASH];
        out.extend_from_slice(rest);
        return out;
    }
    let extended = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];
    match path.strip_prefix(extended.as_slice()) {
        Some(rest) if rest.get(1) == Some(&COLON) => rest.to_vec(),
        _ => path.to_vec(),
    }
}

/// [`same_path`] on 16-bit units.
pub fn same_path_units(a: &[u16], b: &[u16]) -> bool {
    fold_units(&strip_extended_prefix_units(a)) == fold_units(&strip_extended_prefix_units(b))
}

/// [`is_inside`] on 16-bit units.
pub fn is_inside_units(root: &[u16], path: &[u16]) -> bool {
    let norm = |text: &[u16]| -> Vec<u16> {
        fold_units(&strip_extended_prefix_units(text))
            .into_iter()
            .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
            .collect()
    };
    let root = norm(root);
    let end = root
        .iter()
        .rposition(|unit| *unit != BACKSLASH)
        .map_or(0, |last| last + 1);
    let root = &root[..end];
    let path = norm(path);
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.first() == Some(&BACKSLASH))
}

/// The path of the entry `name` inside the folder `parent`, for comparing with
/// the final path of what was opened there.
pub fn child_units(parent: &[u16], name: &[u16]) -> Vec<u16> {
    let end = parent
        .iter()
        .rposition(|unit| *unit != BACKSLASH)
        .map_or(0, |last| last + 1);
    let mut out = parent[..end].to_vec();
    out.push(BACKSLASH);
    out.extend_from_slice(name);
    out
}

/// The form of a path the file calls accept for any length, as a
/// NUL-terminated string: `\\?\` in front and separators as backslashes. A
/// path that already carries a `\\?\` or `\\.\` prefix is kept as it is.
pub fn extended_units(path: &[u16]) -> Vec<u16> {
    let prefixed = matches!(path, [BACKSLASH, BACKSLASH, QUESTION | DOT, BACKSLASH, ..]);
    let mut out = Vec::with_capacity(path.len() + 8);
    if prefixed {
        out.extend_from_slice(path);
    } else {
        let swapped: Vec<u16> = path
            .iter()
            .map(|unit| if *unit == SLASH { BACKSLASH } else { *unit })
            .collect();
        if let Some(rest) = swapped.strip_prefix([BACKSLASH, BACKSLASH].as_slice()) {
            out.extend(units(r"\\?\UNC\"));
            out.extend_from_slice(rest);
        } else {
            out.extend(units(r"\\?\"));
            out.extend_from_slice(&swapped);
        }
    }
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extended_prefix_is_removed_only_where_the_rest_is_a_plain_path() {
        assert_eq!(strip_extended_prefix(r"\\?\C:\x"), r"C:\x");
        assert_eq!(strip_extended_prefix(r"\\?\UNC\h\s\x"), r"\\h\s\x");
        // A volume GUID path has no plain form and is left as it is, so the
        // protected-path table reads it as a device path and refuses it.
        let guid = r"\\?\Volume{1234}\x";
        assert_eq!(strip_extended_prefix(guid), guid);
        assert_eq!(strip_extended_prefix(r"C:\x"), r"C:\x");
    }

    #[test]
    fn containment_ignores_case_prefix_and_slash_direction() {
        assert!(is_inside(r"C:\Users", r"c:/users/me/x"));
        assert!(is_inside(r"C:\Users\", r"\\?\C:\Users"));
        assert!(!is_inside(r"C:\Users", r"C:\Users2"));
        assert!(!is_inside(r"C:\Users\me", r"C:\Users"));
    }

    #[test]
    fn paths_are_compared_without_case_or_prefix() {
        assert!(same_path(r"\\?\C:\Users\Me", r"c:\users\me"));
        assert!(!same_path(r"C:\Users\Me", r"C:\Users\Me2"));
    }

    fn u(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// `text` with one unpaired surrogate (U+D800) where `hole` is.
    fn with_hole(text: &str, hole: char) -> Vec<u16> {
        let mut out = Vec::new();
        for c in text.chars() {
            if c == hole {
                out.push(0xD800);
            } else {
                let mut buffer = [0u16; 2];
                out.extend_from_slice(c.encode_utf16(&mut buffer));
            }
        }
        out
    }

    #[test]
    fn an_unpaired_surrogate_is_not_the_replacement_character() {
        // The file the request names, and the one a lossy conversion would
        // open instead, which can sit next to it in the same folder.
        let sent = with_hole(r"C:\fx\a#", '#');
        let sibling = u("C:\\fx\\a\u{FFFD}");
        assert!(!same_path_units(&sent, &sibling));
        assert!(!same_path_units(&sibling, &sent));
        assert_ne!(extended_units(&sent), extended_units(&sibling));
        // The units go through unchanged.
        let opened = extended_units(&sent);
        assert!(opened.starts_with(&u(r"\\?\C:\fx\a")));
        assert_eq!(opened[opened.len() - 2], 0xD800);
        assert_eq!(opened.last(), Some(&0));
        // The same name is the same place, in any case and with any prefix.
        assert!(same_path_units(&sent, &with_hole(r"\\?\c:\FX\A#", '#')));
        assert!(is_inside_units(&u(r"C:\fx"), &sent));
        assert!(!is_inside_units(&sent, &sibling));
        assert!(!is_inside_units(
            &with_hole(r"C:\fx\a#", '#'),
            &u("C:\\fx\\a\u{FFFD}\\x")
        ));
        assert!(is_inside_units(
            &with_hole(r"C:\fx\a#", '#'),
            &with_hole(r"C:\fx\a#\x", '#')
        ));
    }

    #[test]
    fn case_folding_keeps_what_is_not_a_valid_pair() {
        let mut text = u("ÀB");
        text.push(0xDC00);
        text.push(0xD800);
        let mut want = u("àb");
        want.push(0xDC00);
        want.push(0xD800);
        assert_eq!(fold_units(&text), want);
        // A valid pair outside the basic plane is one letter, not two.
        assert_eq!(fold_units(&u("\u{10400}")), u("\u{10428}"));
    }

    #[test]
    fn the_unit_forms_agree_with_the_text_forms_on_valid_text() {
        let cases = [
            (r"C:\Users\Me", r"c:\users\me"),
            (r"\\?\C:\Users\Me", r"c:\users\me"),
            (r"\\?\UNC\h\s\x", r"\\h\s\x"),
            (r"\\?\Volume{1234}\x", r"\\?\Volume{1234}\x"),
            (r"C:\Users\Me", r"C:\Users\Me2"),
            (r"C:\Users", r"c:/users/me/x"),
            (r"C:\Users\", r"\\?\C:\Users"),
            (r"C:\Users2", r"C:\Users"),
            (r"C:\Users\me", r"C:\Users"),
            (r"", r"C:\x"),
            (r"\\", r"\\x"),
        ];
        for (a, b) in cases {
            assert_eq!(same_path_units(&u(a), &u(b)), same_path(a, b), "{a} {b}");
            assert_eq!(
                is_inside_units(&u(a), &u(b)),
                is_inside(a, b),
                "{a} inside {b}"
            );
            assert_eq!(
                strip_extended_prefix_units(&u(a)),
                u(&strip_extended_prefix(a)),
                "{a}"
            );
        }
    }

    #[test]
    fn a_child_is_its_folder_a_separator_and_the_exact_name() {
        let name = with_hole("a#", '#');
        let mut want = u(r"C:\fx\");
        want.extend_from_slice(&name);
        assert_eq!(child_units(&u(r"C:\fx"), &name), want);
        assert_eq!(child_units(&u(r"C:\fx\"), &name), want);
        // A drive root keeps its separator once.
        assert_eq!(child_units(&u(r"C:\"), &u("x")), u(r"C:\x"));
    }

    #[test]
    fn the_long_form_has_the_right_prefix_for_each_kind_of_path() {
        let nul = |mut text: Vec<u16>| {
            text.push(0);
            text
        };
        assert_eq!(extended_units(&u("C:/a/b")), nul(u(r"\\?\C:\a\b")));
        assert_eq!(
            extended_units(&u(r"\\host\share\x")),
            nul(u(r"\\?\UNC\host\share\x"))
        );
        // Paths that already carry a prefix are not changed.
        for kept in [r"\\?\C:\a/b", r"\\.\pipe\x"] {
            assert_eq!(extended_units(&u(kept)), nul(u(kept)));
        }
    }
}
