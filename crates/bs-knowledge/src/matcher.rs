//! Pattern compilation and matching.
//!
//! Matching works on parts of a path (the text between separators) and never
//! allocates, so the scanner can classify millions of files cheaply. Patterns
//! are compiled once, when a [`Knowledge`](crate::Knowledge) is built, because
//! that is when the environment (home folder, `%LOCALAPPDATA%` and so on) is
//! known.

use crate::normalize::{self, Flavor, Root};
use crate::{Environment, Os};

/// One part of a compiled pattern.
#[derive(Clone, Debug)]
pub(crate) struct Part {
    /// Lower case for Windows patterns.
    pub text: Box<[u8]>,
    /// Holds `*` or `?`.
    pub wild: bool,
}

/// A pattern ready to match.
#[derive(Clone, Debug)]
pub(crate) struct Compiled {
    pub flavor: Flavor,
    /// `**/name`: the parts may appear anywhere in the path.
    pub floating: bool,
    /// Match the folder itself only, not what is below it.
    pub exact: bool,
    pub parts: Box<[Part]>,
    /// The first part is a drive (`c:` or `?:`), which paths on an unnamed
    /// drive do not have.
    pub starts_with_drive: bool,
}

impl Compiled {
    pub fn wild_parts(&self) -> usize {
        self.parts.iter().filter(|p| p.wild).count()
    }

    pub fn fold(&self) -> bool {
        self.flavor == Flavor::Windows
    }
}

fn make_part(text: &[u8], fold: bool) -> Part {
    let mut kept = Vec::with_capacity(text.len());
    if fold {
        kept.extend(text.iter().map(u8::to_ascii_lowercase));
    } else {
        kept.extend_from_slice(text);
    }
    Part {
        wild: kept.iter().any(|&b| b == b'*' || b == b'?'),
        text: kept.into_boxed_slice(),
    }
}

/// Compiles one pattern for the systems it applies to. Gives nothing for a
/// system where the pattern cannot be placed, for instance because the
/// environment lacks a variable it names.
pub(crate) fn compile(pattern: &str, os: Os, exact: bool, env: &Environment) -> Vec<Compiled> {
    let flavors: &[Flavor] = match os {
        Os::Any => &[Flavor::Windows, Flavor::Linux],
        Os::Windows => &[Flavor::Windows],
        Os::Linux => &[Flavor::Linux],
    };
    flavors
        .iter()
        .filter_map(|&flavor| compile_for(pattern, flavor, exact, env))
        .collect()
}

/// A drive root given by the caller: the path itself, nothing below it. The
/// text is taken as it is, with no wildcards.
pub(crate) fn compile_drive_root(root: &[u8]) -> Option<Compiled> {
    let (flavor, norm) = if root.first() == Some(&b'/') {
        (Flavor::Linux, normalize::linux_text(root).ok()?)
    } else {
        (Flavor::Windows, normalize::windows(root).ok()?)
    };
    // The roots of drives and shares are blocked by what they are; this adds
    // the places that look like ordinary folders (a mount point, or a volume
    // mounted in a folder).
    if matches!(norm.root, Root::Unc | Root::AnyDrive) || (norm.depth() == 0) {
        return None;
    }
    let parts: Vec<Part> = norm
        .parts()
        .map(|p| Part {
            wild: false,
            text: p.into(),
        })
        .collect();
    Some(Compiled {
        flavor,
        floating: false,
        exact: true,
        parts: parts.into(),
        starts_with_drive: flavor == Flavor::Windows,
    })
}

fn compile_for(pattern: &str, flavor: Flavor, exact: bool, env: &Environment) -> Option<Compiled> {
    let fold = flavor == Flavor::Windows;
    let mut parts: Vec<Part> = Vec::new();

    if let Some(rest) = pattern.strip_prefix("**/") {
        for piece in rest.split('/').filter(|p| !p.is_empty()) {
            parts.push(make_part(piece.as_bytes(), fold));
        }
        if parts.is_empty() {
            return None;
        }
        return Some(Compiled {
            flavor,
            floating: true,
            exact,
            parts: parts.into(),
            starts_with_drive: false,
        });
    }

    // Anchored: the head is a variable, the home folder, or literal.
    let (head, tail): (Option<&[u8]>, &str) = if let Some(rest) = pattern.strip_prefix('~') {
        let home = match flavor {
            Flavor::Windows => env.get("USERPROFILE"),
            Flavor::Linux => env.get("HOME"),
        }?;
        (Some(home), rest)
    } else if let Some(rest) = pattern.strip_prefix('%') {
        let (name, tail) = rest.split_once('%')?;
        (Some(env.get(name)?), tail)
    } else {
        (None, pattern)
    };

    let mut starts_with_drive = false;
    if let Some(head) = head {
        // The value is a path in the style of the system it was read from.
        let norm = match flavor {
            Flavor::Windows => normalize::windows(head).ok()?,
            Flavor::Linux => normalize::linux_text(head).ok()?,
        };
        if flavor == Flavor::Windows && norm.root != Root::Drive {
            return None;
        }
        starts_with_drive = flavor == Flavor::Windows;
        for p in norm.parts() {
            parts.push(Part {
                wild: false,
                text: p.into(),
            });
        }
    } else if flavor == Flavor::Linux {
        if !pattern.starts_with('/') {
            return None;
        }
    } else {
        // A literal Windows pattern starts with a drive part such as `C:`.
        let first = pattern.split('/').next().unwrap_or("").as_bytes();
        if !(first.len() == 2 && first[1] == b':') {
            return None;
        }
        starts_with_drive = true;
    }
    for piece in tail.split('/').filter(|p| !p.is_empty()) {
        parts.push(make_part(piece.as_bytes(), fold));
    }
    // `/` alone is the Linux root: no parts at all.
    if parts.is_empty() && !(flavor == Flavor::Linux && pattern == "/") {
        return None;
    }
    Some(Compiled {
        flavor,
        floating: false,
        exact,
        parts: parts.into(),
        starts_with_drive,
    })
}

#[inline]
fn eq_part(pattern: &[u8], text: &[u8], fold: bool) -> bool {
    if pattern.len() != text.len() {
        return false;
    }
    if fold {
        pattern
            .iter()
            .zip(text)
            .all(|(&p, &t)| p == t.to_ascii_lowercase())
    } else {
        pattern == text
    }
}

/// `*` matches any run of bytes within a part, `?` any one byte.
fn glob(pattern: &[u8], text: &[u8], fold: bool) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let byte = if fold {
            text[t].to_ascii_lowercase()
        } else {
            text[t]
        };
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == byte) {
            p += 1;
            t += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[inline]
fn part_matches(part: &Part, text: &[u8], fold: bool) -> bool {
    if part.wild {
        glob(&part.text, text, fold)
    } else {
        eq_part(&part.text, text, fold)
    }
}

/// Matches an anchored pattern against the parts of a path. Gives the number
/// of parts matched. `ignore_drive` is set for paths on an unnamed drive.
pub(crate) fn match_anchored(c: &Compiled, path: &[&[u8]], ignore_drive: bool) -> Option<usize> {
    let wanted = c.parts.len();
    if path.len() < wanted || (c.exact && path.len() != wanted) {
        return None;
    }
    let fold = c.fold();
    for (index, (part, text)) in c.parts.iter().zip(path).enumerate() {
        let skip = index == 0 && ignore_drive && c.starts_with_drive;
        if !skip && !part_matches(part, text, fold) {
            return None;
        }
    }
    Some(wanted)
}

/// Matches a floating pattern: its parts in a row, anywhere in the path (for
/// an exact pattern, at the end). `complete` is false when `path` is only the
/// start of a longer path, in which case an end cannot be matched.
pub(crate) fn match_floating(c: &Compiled, path: &[&[u8]], complete: bool) -> Option<usize> {
    let wanted = c.parts.len();
    if path.len() < wanted {
        return None;
    }
    let fold = c.fold();
    let fits = |start: usize| {
        c.parts
            .iter()
            .zip(&path[start..])
            .all(|(part, text)| part_matches(part, text, fold))
    };
    if c.exact {
        (complete && fits(path.len() - wanted)).then_some(wanted)
    } else {
        (0..=path.len() - wanted).any(fits).then_some(wanted)
    }
}

/// Matches a pattern against the parts of a path.
pub(crate) fn match_any(
    c: &Compiled,
    path: &[&[u8]],
    complete: bool,
    ignore_drive: bool,
) -> Option<usize> {
    if c.floating {
        match_floating(c, path, complete)
    } else {
        match_anchored(c, path, ignore_drive)
    }
}

/// The most parts of a raw path that are matched against patterns. Longer
/// paths are matched on their first parts only, which is enough for every
/// pattern (they have at most a handful of parts) except a name deeper than
/// this, which is then not seen.
pub(crate) const MAX_PARTS: usize = 48;

/// The first parts of a raw path and its last part.
pub(crate) struct Split<'a> {
    pub style: Style,
    pub parts: [&'a [u8]; MAX_PARTS],
    pub count: usize,
    /// `parts` holds the whole path.
    pub complete: bool,
    pub last: &'a [u8],
}

impl<'a> Split<'a> {
    pub fn of(path: &'a [u8]) -> Self {
        let (style, iter) = split(path);
        let mut out = Split {
            style,
            parts: [&[]; MAX_PARTS],
            count: 0,
            complete: true,
            last: &[],
        };
        for part in iter {
            out.last = part;
            if out.count < MAX_PARTS {
                out.parts[out.count] = part;
                out.count += 1;
            } else {
                out.complete = false;
            }
        }
        out
    }

    pub fn slice(&self) -> &[&'a [u8]] {
        &self.parts[..self.count]
    }
}

// ------------------------------------------------- raw paths, no allocation

/// How a raw path is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Windows,
    Linux,
    /// Not absolute: there is nothing to say which system wrote it.
    Relative,
}

/// The parts of a raw path, without resolving anything.
#[derive(Clone)]
pub(crate) struct Parts<'a> {
    rest: &'a [u8],
    windows: bool,
}

impl<'a> Iterator for Parts<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let windows = self.windows;
        let is_sep = move |b: &u8| *b == b'/' || (windows && *b == b'\\');
        loop {
            let start = self.rest.iter().position(|b| !is_sep(b))?;
            let rest = &self.rest[start..];
            let end = rest.iter().position(is_sep).unwrap_or(rest.len());
            let part = &rest[..end];
            self.rest = &rest[end..];
            if part != b"." {
                return Some(part);
            }
        }
    }
}

/// Works out how a raw path is written and returns its parts.
pub(crate) fn split(path: &[u8]) -> (Style, Parts<'_>) {
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let mut rest = path;
    let mut verbatim = false;
    if rest.len() >= 4
        && is_sep(rest[0])
        && (is_sep(rest[1]) || rest[1] == b'?')
        && matches!(rest[2], b'?' | b'.')
        && is_sep(rest[3])
    {
        rest = &rest[4..];
        verbatim = true;
    }
    let drive = rest.len() >= 2
        && rest[0].is_ascii_alphabetic()
        && rest[1] == b':'
        && rest.get(2).is_none_or(|&b| is_sep(b));
    // `//usr` is more likely a Linux path with a doubled slash than a share
    // on a server called `usr`; only a backslash pair starts a share here.
    let unc = rest.len() >= 2 && rest[0] == b'\\' && is_sep(rest[1]);
    let style = if verbatim || drive || unc || rest.first() == Some(&b'\\') {
        Style::Windows
    } else if rest.first() == Some(&b'/') {
        Style::Linux
    } else {
        Style::Relative
    };
    let windows = style != Style::Linux;
    (style, Parts { rest, windows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globs(pattern: &str, text: &str) -> bool {
        glob(pattern.as_bytes(), text.as_bytes(), false)
    }

    #[test]
    fn glob_table() {
        let cases = [
            ("lib*", "lib", true),
            ("lib*", "lib64", true),
            ("lib*", "libx32", true),
            ("lib*", "li", false),
            ("lib*", "xlib", false),
            ("*", "", true),
            ("*", "anything", true),
            ("?", "a", true),
            ("?", "", false),
            ("?", "ab", false),
            ("?:", "c:", true),
            ("?:", "cc:", false),
            ("a*b*c", "aXbYc", true),
            ("a*b*c", "aXbY", false),
            ("a*b*c", "abcabc", true),
            ("**", "x", true),
            ("*a", "baba", true),
            ("*a", "babab", false),
            ("", "", true),
            ("", "a", false),
        ];
        for (pattern, text, expected) in cases {
            assert_eq!(globs(pattern, text), expected, "{pattern:?} {text:?}");
        }
    }

    #[test]
    fn glob_is_not_exponential() {
        let text = "a".repeat(100_000);
        let started = std::time::Instant::now();
        assert!(!glob(b"*a*a*a*a*a*a*a*b", text.as_bytes(), false));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn raw_paths_are_styled_and_split() {
        let cases: [(&str, Style, &[&str]); 12] = [
            ("/usr/lib", Style::Linux, &["usr", "lib"]),
            ("//usr//lib/", Style::Linux, &["usr", "lib"]),
            ("/a/./b", Style::Linux, &["a", "b"]),
            (r"/a\b/c", Style::Linux, &[r"a\b", "c"]),
            (r"C:\Users\a", Style::Windows, &["C:", "Users", "a"]),
            ("C:/Users/a", Style::Windows, &["C:", "Users", "a"]),
            (r"\\?\C:\Users", Style::Windows, &["C:", "Users"]),
            (
                r"\\server\share\x",
                Style::Windows,
                &["server", "share", "x"],
            ),
            (r"\Users", Style::Windows, &["Users"]),
            ("C:", Style::Windows, &["C:"]),
            ("name.txt", Style::Relative, &["name.txt"]),
            (r"sub\name.txt", Style::Relative, &["sub", "name.txt"]),
        ];
        for (path, style, parts) in cases {
            let (got_style, got_parts) = split(path.as_bytes());
            let got: Vec<String> = got_parts
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            assert_eq!(got_style, style, "{path}");
            assert_eq!(got, parts, "{path}");
        }
        // `C:name` is not a drive.
        assert_eq!(split(b"C:name").0, Style::Relative);
    }
}
