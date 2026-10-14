//! Path normalization for the protected-path check.
//!
//! The check has to answer for what a path *names*, not for how it is spelled,
//! so paths are reduced to a canonical list of parts first: separators and
//! `.`/`..` are resolved, Windows spellings (`\\?\` prefixes, forward slashes,
//! trailing dots and spaces, alternate data streams, upper case) are undone,
//! and anything that cannot be reduced is reported instead of guessed at.
//!
//! This is text work only. It does not look at the disk, so it cannot follow
//! links or resolve 8.3 short names; the code that deletes has to do that with
//! open handles.

/// Longest path that is examined. Windows allows 32,767 UTF-16 units, which is
/// at most 98,301 bytes of UTF-8; Linux allows 4,096 bytes. Anything longer is
/// refused rather than processed.
pub(crate) const MAX_PATH_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    Windows,
    Linux,
}

/// What the first part of the path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Root {
    /// A drive letter; it is the first part.
    Drive,
    /// Rooted at the current or an unnamed drive (`\Windows`, or a volume
    /// GUID path). The first part is a placeholder for the drive.
    AnyDrive,
    /// `\\server\share`; the first part is a marker, then server and share.
    Unc,
    /// The Linux root; no marker part.
    Linux,
}

/// Why a path could not be reduced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unresolvable {
    /// Empty, too long, or holds a NUL byte.
    Malformed,
    /// Not absolute: it depends on a working directory that is not known.
    Relative,
    /// A device path or other form that names no folder by its letters.
    Device,
    /// A `..` that cannot be settled from the text. On Linux a link before
    /// the `..` changes where it leads; in a network path it could climb out
    /// of the share.
    DotDot,
}

/// A path reduced to its parts.
#[derive(Clone, Debug)]
pub(crate) struct Normalized {
    pub flavor: Flavor,
    pub root: Root,
    /// A short-name alias (`PROGRA~1`) sits in the first two parts below the
    /// root of a form that only Windows reads. Its real target cannot be known
    /// from the text.
    pub alias: bool,
    /// The same, for a path rooted at the current drive (`\x` or `/x`). Whether
    /// that counts is for the caller: on Linux `/x~1` is an ordinary name.
    pub weak_alias: bool,
    /// The path holds a backslash, which only Windows reads as a separator.
    pub backslash: bool,
    /// A `..` removed a part that was there.
    pub popped: bool,
    arena: Vec<u8>,
    ranges: Vec<(usize, usize)>,
    /// The parts as written (not case folded), for readings on a system that
    /// tells letter case apart.
    raw_arena: Vec<u8>,
    raw_ranges: Vec<(usize, usize)>,
}

impl Normalized {
    fn new(flavor: Flavor, root: Root) -> Self {
        Self {
            flavor,
            root,
            alias: false,
            weak_alias: false,
            backslash: false,
            popped: false,
            arena: Vec::new(),
            ranges: Vec::new(),
            raw_arena: Vec::new(),
            raw_ranges: Vec::new(),
        }
    }

    /// Adds a part: `folded` is what is compared, `raw` what was written.
    fn push_both(&mut self, folded: &[u8], raw: &[u8]) {
        let start = self.arena.len();
        self.arena.extend_from_slice(folded);
        self.ranges.push((start, self.arena.len()));
        let start = self.raw_arena.len();
        self.raw_arena.extend_from_slice(raw);
        self.raw_ranges.push((start, self.raw_arena.len()));
    }

    fn push(&mut self, part: &[u8]) {
        self.push_both(part, part);
    }

    fn pop(&mut self) {
        if let Some((start, _)) = self.ranges.pop() {
            self.arena.truncate(start);
        }
        if let Some((start, _)) = self.raw_ranges.pop() {
            self.raw_arena.truncate(start);
        }
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// The parts in order, including the root marker where there is one.
    pub fn parts(&self) -> impl Iterator<Item = &[u8]> + Clone {
        self.ranges.iter().map(|&(s, e)| &self.arena[s..e])
    }

    /// The parts as they were written, with the same stripping of trailing
    /// dots and spaces but no case folding.
    pub fn raw_parts(&self) -> impl Iterator<Item = &[u8]> + Clone {
        self.raw_ranges.iter().map(|&(s, e)| &self.raw_arena[s..e])
    }

    /// Number of parts after the root marker (a Linux root has no marker).
    pub fn depth(&self) -> usize {
        match self.root {
            Root::Linux => self.len(),
            _ => self.len().saturating_sub(1),
        }
    }
}

fn is_separator(b: u8) -> bool {
    b == b'\\' || b == b'/'
}

/// The text before `~` and digits that Windows uses for short names such as
/// `PROGRA~1` or `DOCUME~1.TXT`.
fn looks_like_short_name(part: &[u8]) -> bool {
    let Some(tilde) = part.iter().position(|&b| b == b'~') else {
        return false;
    };
    if tilde == 0 || tilde > 6 {
        return false;
    }
    let digits = part[tilde + 1..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    digits > 0 && matches!(part.get(tilde + 1 + digits), None | Some(b'.'))
}

/// Folds one Windows name part for comparison: ASCII upper case to lower case,
/// plus the three non-ASCII letters whose upper or lower case forms are ASCII
/// letters (dotless i, long s and the Kelvin sign), so that none of them can
/// stand in for a letter of a protected name.
fn fold_windows(part: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < part.len() {
        let rest = &part[i..];
        if rest.starts_with(&[0xC4, 0xB1]) {
            out.push(b'i');
            i += 2;
        } else if rest.starts_with(&[0xC5, 0xBF]) {
            out.push(b's');
            i += 2;
        } else if rest.starts_with(&[0xE2, 0x84, 0xAA]) {
            out.push(b'k');
            i += 3;
        } else {
            out.push(part[i].to_ascii_lowercase());
            i += 1;
        }
    }
}

/// Reduces a path in Windows spelling.
pub(crate) fn windows(path: &[u8]) -> Result<Normalized, Unresolvable> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains(&0) {
        return Err(Unresolvable::Malformed);
    }
    let mut rest = path;
    // `\\?\`, `\\.\` and `\??\` name the same places as the plain forms.
    let mut verbatim = false;
    // Win32 leaves a path alone only when it starts with exactly `\\?\`.
    // Any other spelling of the prefix (`\\.\`, `//?/`, `\??\`) is normalized
    // as if the prefix were the root, so a `..` can remove the drive.
    let normalized_device = !path.starts_with(br"\\?\");
    if rest.len() >= 4
        && is_separator(rest[0])
        && (is_separator(rest[1]) || rest[1] == b'?')
        && matches!(rest[2], b'?' | b'.')
        && is_separator(rest[3])
    {
        rest = &rest[4..];
        verbatim = true;
    }

    let root;
    let mut volume = false;
    if !verbatim && rest.len() >= 2 && is_separator(rest[0]) && is_separator(rest[1]) {
        root = Root::Unc;
    } else if rest.len() >= 2 && rest[0].is_ascii_alphabetic() && rest[1] == b':' {
        match rest.get(2) {
            None => {}
            Some(&b) if is_separator(b) => {}
            // `C:Windows` is relative to that drive's current folder.
            Some(_) => return Err(Unresolvable::Relative),
        }
        root = Root::Drive;
    } else if verbatim {
        let first = rest.split(|&b| is_separator(b)).next().unwrap_or(&[]);
        let mut lower = Vec::new();
        fold_windows(first, &mut lower);
        if lower == b"unc" {
            root = Root::Unc;
        } else if lower.starts_with(b"volume{") {
            root = Root::AnyDrive;
            volume = true;
            rest = &rest[first.len()..];
        } else {
            return Err(Unresolvable::Device);
        }
    } else if !rest.is_empty() && is_separator(rest[0]) {
        root = Root::AnyDrive;
    } else {
        return Err(Unresolvable::Relative);
    }

    let mut out = Normalized::new(Flavor::Windows, root);
    out.backslash = path.contains(&b'\\');
    match root {
        Root::Drive => {
            out.push(&[rest[0].to_ascii_lowercase(), b':']);
            rest = &rest[2..];
        }
        Root::AnyDrive => out.push(b"?:"),
        Root::Unc => {
            out.push(b"\\\\");
            if verbatim {
                // Skip the `UNC` part itself.
                let skip = rest
                    .iter()
                    .position(|&b| is_separator(b))
                    .unwrap_or(rest.len());
                rest = &rest[skip..];
            }
        }
        Root::Linux => {}
    }

    let marker = out.len();
    let mut scratch = Vec::new();
    for raw in rest.split(|&b| is_separator(b)) {
        if raw.is_empty() {
            continue;
        }
        if raw == b"." {
            // As a host or share, a lone dot would vanish and shift the next
            // name into its place, so it cannot be told what it names.
            if root == Root::Unc && out.len() < UNC_FIXED_PARTS {
                return Err(Unresolvable::Device);
            }
            continue;
        }
        if raw == b".." {
            // Climbing out of a network share is not something the text can
            // settle: the host and the share are the first two parts.
            if root == Root::Unc && out.len() <= UNC_FIXED_PARTS {
                return Err(Unresolvable::DotDot);
            }
            // In a normalized device path `D:\..\C:\Windows` is `C:\Windows`:
            // the root `..` cannot pass is the prefix, not the drive. Which
            // device that leaves is not worth guessing at.
            if verbatim && normalized_device && out.len() <= marker {
                return Err(Unresolvable::DotDot);
            }
            if out.len() > marker {
                out.pop();
                out.popped = true;
            }
            continue;
        }
        // An alternate data stream (`name:stream`) is the same file.
        let name = raw.split(|&b| b == b':').next().unwrap_or(raw);
        let end = name
            .iter()
            .rposition(|&b| b != b'.' && b != b' ')
            .map_or(0, |i| i + 1);
        let name = &name[..end];
        if name.is_empty() {
            // The same for a host or share made of dots or spaces.
            if root == Root::Unc && out.len() < UNC_FIXED_PARTS {
                return Err(Unresolvable::Device);
            }
            continue;
        }
        scratch.clear();
        fold_windows(name, &mut scratch);
        out.push_both(&scratch, name);
    }

    // Short names are checked in the first two parts below the root. Where the
    // root is only read by Windows (a drive letter, a share, a volume path)
    // that is certain. A path that starts with a separator is also how Linux
    // paths look, and `~1` is an ordinary ending for names there, so that case
    // is left to the caller.
    let skip = if root == Root::Unc {
        UNC_FIXED_PARTS
    } else {
        1
    };
    let found = out.parts().skip(skip).take(2).any(looks_like_short_name);
    match root {
        Root::AnyDrive if !volume => out.weak_alias = found,
        _ => out.alias = found,
    }
    Ok(out)
}

/// The parts of a network path that are not below the share: the marker, the
/// host and the share.
const UNC_FIXED_PARTS: usize = 3;

/// Reduces a path in Linux spelling. Only absolute paths can be reduced, and
/// `..` is only accepted where it cannot lead anywhere else than where it
/// reads: at the root. After a named part the kernel follows any link in that
/// part before it steps back, so the text alone cannot say where the path
/// ends up.
pub(crate) fn linux(path: &[u8]) -> Result<Normalized, Unresolvable> {
    linux_with(path, true)
}

/// Like [`linux`], but a `..` is settled by the text, as for a value that the
/// caller read from its own environment.
pub(crate) fn linux_text(path: &[u8]) -> Result<Normalized, Unresolvable> {
    linux_with(path, false)
}

fn linux_with(path: &[u8], strict: bool) -> Result<Normalized, Unresolvable> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains(&0) {
        return Err(Unresolvable::Malformed);
    }
    if path[0] != b'/' {
        return Err(Unresolvable::Relative);
    }
    let mut out = Normalized::new(Flavor::Linux, Root::Linux);
    out.backslash = path.contains(&b'\\');
    for raw in path.split(|&b| b == b'/') {
        if raw.is_empty() || raw == b"." {
            continue;
        }
        if raw == b".." {
            // The parent of the root is the root.
            if out.ranges.is_empty() {
                continue;
            }
            if strict {
                return Err(Unresolvable::DotDot);
            }
            out.pop();
            out.popped = true;
            continue;
        }
        out.push(raw);
    }
    Ok(out)
}

/// Names under which Windows shows the files of a WSL distribution as a
/// network share.
const WSL_HOSTS: [&[u8]; 2] = [b"wsl$", b"wsl.localhost"];

/// The places on this machine that a network path may also name, so that the
/// check can treat each of them as the path itself.
///
/// - `\\host\C$\rest` is `C:\rest`, and `\\host\ADMIN$\rest` is the Windows
///   folder plus `rest`, for any host: a name can resolve to this machine, and
///   the text cannot tell.
/// - `\\wsl$\distro\rest` and `\\wsl.localhost\distro\rest` are `/rest` inside
///   the distribution.
///
/// `system_root` is the Windows folder from the environment, as parts.
pub(crate) fn local_readings(
    unc: &Normalized,
    system_root: Option<&[Vec<u8>]>,
) -> Vec<Result<Normalized, Unresolvable>> {
    let mut out = Vec::new();
    if unc.root != Root::Unc || unc.len() < UNC_FIXED_PARTS {
        return out;
    }
    let parts: Vec<&[u8]> = unc.parts().collect();
    let raw: Vec<&[u8]> = unc.raw_parts().collect();
    let (host, share) = (parts[1], parts[2]);
    let below = &parts[UNC_FIXED_PARTS..];

    if WSL_HOSTS.contains(&host) {
        if unc.popped {
            // A `..` inside a distribution follows the distribution's links.
            out.push(Err(Unresolvable::DotDot));
        } else {
            let mut linux = Normalized::new(Flavor::Linux, Root::Linux);
            linux.backslash = unc.backslash;
            for part in &raw[UNC_FIXED_PARTS..] {
                linux.push(part);
            }
            out.push(Ok(linux));
        }
    }
    if share.len() == 2 && share[0].is_ascii_alphabetic() && share[1] == b'$' {
        let drive = [share[0].to_ascii_lowercase(), b':'];
        out.push(Ok(drive_path(&[&drive], below)));
    } else if share == b"admin$" {
        out.push(Ok(drive_path(&[b"c:", b"windows"], below)));
        if let Some(root) = system_root {
            let root: Vec<&[u8]> = root.iter().map(Vec::as_slice).collect();
            out.push(Ok(drive_path(&root, below)));
        }
    }
    out
}

/// A drive path made of already reduced parts.
fn drive_path(head: &[&[u8]], below: &[&[u8]]) -> Normalized {
    let mut out = Normalized::new(Flavor::Windows, Root::Drive);
    out.backslash = true;
    for part in head.iter().chain(below) {
        out.push(part);
    }
    let alias = out.parts().skip(1).take(2).any(looks_like_short_name);
    out.alias = alias;
    out
}

/// The Windows drive that a Linux path shows when a Windows drive is mounted
/// under `/mnt/<letter>`, as WSL does. That filesystem ignores letter case
/// (and trailing dots, streams and short names, like any Windows path), which
/// the Linux reading cannot express, so it is read again as the drive path.
pub(crate) fn drive_mounted_in_linux(
    linux: &Normalized,
) -> Option<Result<Normalized, Unresolvable>> {
    if linux.flavor != Flavor::Linux {
        return None;
    }
    let mut parts = linux.raw_parts();
    if parts.next()? != b"mnt" {
        return None;
    }
    let letter = parts.next()?;
    if letter.len() != 1 || !letter[0].is_ascii_alphabetic() {
        return None;
    }
    let mut text = vec![letter[0], b':', b'\\'];
    for part in parts {
        text.extend_from_slice(part);
        text.push(b'\\');
    }
    Some(windows(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(n: &Normalized) -> Vec<String> {
        n.parts()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    }

    fn win(path: &str) -> Vec<String> {
        parts(&windows(path.as_bytes()).unwrap())
    }

    #[test]
    fn windows_spellings_reduce_to_the_same_parts() {
        let same = [
            r"C:\Windows",
            r"c:\windows",
            r"C:\WINDOWS\",
            r"C:/Windows",
            r"C:\\Windows",
            r"C:\Windows\.",
            r"C:\Windows\System32\..",
            r"C:\Windows\.\.\.",
            r"C:\Users\..\Windows",
            r"C:\Windows.",
            r"C:\Windows ",
            r"C:\Windows. . .",
            r"C:\Windows\...",
            r"C:\Windows::$DATA",
            r"C:\Windows:stream",
            r"C:\Windows::$INDEX_ALLOCATION",
            r"\\?\C:\Windows",
            r"\\.\C:\Windows",
            r"\??\C:\Windows",
            r"\\?\c:\WINDOWS\",
            r"//?/C:/Windows",
            r"C:\Windows\System32\..\..\Windows",
            r"C:\..\..\Windows",
            r"C:\Windows\\\\",
        ];
        for path in same {
            assert_eq!(win(path), vec!["c:", "windows"], "{path:?}");
        }
    }

    #[test]
    fn dotdot_stops_at_the_root() {
        assert_eq!(win(r"C:\.."), vec!["c:"]);
        assert_eq!(win(r"C:\a\..\.."), vec!["c:"]);
        assert_eq!(win(r"C:\"), vec!["c:"]);
        assert_eq!(win("C:"), vec!["c:"]);
        assert_eq!(parts(&linux(b"/..").unwrap()), Vec::<String>::new());
        assert_eq!(parts(&linux(b"/../..").unwrap()), Vec::<String>::new());
        assert_eq!(parts(&linux(b"/./../a").unwrap()), vec!["a"]);
    }

    #[test]
    fn linux_dotdot_after_a_named_part_cannot_be_settled() {
        // A link in `a` decides where `a/..` goes, so the text cannot say.
        for path in [
            b"/a/..".as_slice(),
            b"/a/../..",
            b"/home/alice/l/../bin",
            b"/usr/../usr",
            b"/a/./b/../c",
            b"/a//..",
        ] {
            assert_eq!(
                linux(path).unwrap_err(),
                Unresolvable::DotDot,
                "{:?}",
                String::from_utf8_lossy(path)
            );
        }
        // Values the caller read from its own environment are settled by text.
        assert_eq!(parts(&linux_text(b"/usr/../usr").unwrap()), vec!["usr"]);
        assert_eq!(
            parts(&linux_text(b"/a/../..").unwrap()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_host_or_share_made_of_dots_cannot_shift_the_names_after_it() {
        // Dropped as empty, `...` would turn `C$` into the host.
        for path in [
            r"\\?\UNC\.\ADMIN$\Windows",
            r"\\host\.\Windows",
            r"\\...\C$\Windows\System32",
            r"\\host\...\Windows",
            r"\\?\UNC\. .\C$\Windows\System32",
            r"\\:stream\C$\Windows\System32",
        ] {
            assert_eq!(
                windows(path.as_bytes()).unwrap_err(),
                Unresolvable::Device,
                "{path}"
            );
        }
        // Below the share an empty name is simply left out, as Win32 does.
        assert_eq!(
            parts(&windows(br"\\host\share\...\x").unwrap()),
            vec![r"\\", "host", "share", "x"]
        );
    }

    #[test]
    fn dotdot_at_the_root_of_a_normalized_device_path_cannot_be_settled() {
        for path in [
            r"\\.\D:\..\C:\Windows",
            r"\\.\D:\..",
            r"//./D:/../x",
            r"//?/D:/../x",
            r"\??\D:\..\x",
            r"\\.\Volume{4d1c2e00-0000-0000-0000-100000000000}\..\x",
            r"\\.\D:\a\..\..\x",
        ] {
            assert_eq!(
                windows(path.as_bytes()).unwrap_err(),
                Unresolvable::DotDot,
                "{path}"
            );
        }
        // Below the drive a `..` is settled by the text, and plain drive paths
        // keep `..` at the root.
        assert_eq!(win(r"\\.\D:\a\..\b"), vec!["d:", "b"]);
        assert_eq!(win(r"C:\..\x"), vec!["c:", "x"]);
        assert_eq!(win(r"\\?\D:\x\..\y"), vec!["d:", "y"]);
    }

    #[test]
    fn windows_dotdot_is_settled_by_the_text_like_win32_does() {
        assert_eq!(win(r"C:\Users\..\Windows"), vec!["c:", "windows"]);
        assert_eq!(win(r"\Users\..\Windows"), vec!["?:", "windows"]);
    }

    #[test]
    fn roots_are_told_apart() {
        assert_eq!(windows(br"C:\x").unwrap().root, Root::Drive);
        assert_eq!(windows(br"\x").unwrap().root, Root::AnyDrive);
        assert_eq!(windows(b"/x").unwrap().root, Root::AnyDrive);
        let volume = windows(br"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\Windows").unwrap();
        assert_eq!(volume.root, Root::AnyDrive);
        assert_eq!(parts(&volume), vec!["?:", "windows"]);
        let unc = windows(br"\\server\share\dir").unwrap();
        assert_eq!(unc.root, Root::Unc);
        assert_eq!(unc.depth(), 3);
        let verbatim_unc = windows(br"\\?\UNC\server\share").unwrap();
        assert_eq!(verbatim_unc.root, Root::Unc);
        assert_eq!(verbatim_unc.depth(), 2);
        assert_eq!(windows(b"//server/share").unwrap().root, Root::Unc);
    }

    #[test]
    fn what_cannot_be_reduced_is_reported() {
        let cases: [(&[u8], Unresolvable); 17] = [
            (br"\\host\share\..", Unresolvable::DotDot),
            (br"\\host\..\c$\windows", Unresolvable::DotDot),
            (br"\\?\UNC\host\c$\..\windows", Unresolvable::DotDot),
            (br"\\host\c$\..\..", Unresolvable::DotDot),
            (br"\\host\..", Unresolvable::DotDot),
            (b"", Unresolvable::Malformed),
            (b"C:\\a\0b", Unresolvable::Malformed),
            (b"Windows", Unresolvable::Relative),
            (br"..\Windows", Unresolvable::Relative),
            (b"C:Windows", Unresolvable::Relative),
            (b"C:..", Unresolvable::Relative),
            (br"\\.\PhysicalDrive0", Unresolvable::Device),
            (
                br"\\?\GLOBALROOT\Device\HarddiskVolume1\Windows",
                Unresolvable::Device,
            ),
            (br"\\?\Device\HarddiskVolume1", Unresolvable::Device),
            (br"\\?\pipe\x", Unresolvable::Device),
            (br"\??\", Unresolvable::Device),
            (b"~", Unresolvable::Relative),
        ];
        for (path, expected) in cases {
            assert_eq!(
                windows(path).map(|n| parts(&n)).unwrap_err(),
                expected,
                "{:?}",
                String::from_utf8_lossy(path)
            );
        }
        let too_long = vec![b'a'; MAX_PATH_BYTES + 1];
        assert_eq!(windows(&too_long).unwrap_err(), Unresolvable::Malformed);
        assert_eq!(linux(&too_long).unwrap_err(), Unresolvable::Malformed);
        assert_eq!(linux(b"usr").unwrap_err(), Unresolvable::Relative);
        assert_eq!(linux(b"").unwrap_err(), Unresolvable::Malformed);
        assert_eq!(linux(b"/a\0").unwrap_err(), Unresolvable::Malformed);
    }

    #[test]
    fn short_names_are_flagged_for_every_root_that_only_windows_reads() {
        // Drive letters, volume paths and the part of a network path below the
        // share.
        for path in [
            r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\WINDOW~1",
            r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\PROGRA~1\x",
            r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\Users\ALICEJ~1",
            r"\\host\share\PROGRA~1",
            r"\\host\C$\WINDOW~1\System32",
            r"\\?\UNC\host\share\Dir\PROGRA~2",
            r"\\.\UNC\host\C$\PROGRA~1",
        ] {
            let norm = windows(path.as_bytes()).unwrap();
            assert!(norm.alias, "{path}");
        }
        // Rooted at the current drive: reported apart, because Linux paths
        // look the same.
        for path in [r"\WINDOW~1\System32", "/WINDOW~1/System32", r"\PROGRA~1"] {
            let norm = windows(path.as_bytes()).unwrap();
            assert!(norm.weak_alias && !norm.alias, "{path}");
        }
        assert!(windows(br"\WINDOW~1\x").unwrap().backslash);
        assert!(!windows(b"/WINDOW~1/x").unwrap().backslash);
        // Not a short name, or too deep to matter.
        for path in [
            r"\\host\SHARE~1\x",
            r"\\host\share\a\b\PROGRA~1",
            r"\Users\alice\PROGRA~1",
            r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\a\b\PROGRA~1",
        ] {
            let norm = windows(path.as_bytes()).unwrap();
            assert!(!norm.alias && !norm.weak_alias, "{path}");
        }
    }

    #[test]
    fn network_paths_that_name_a_local_place_are_read_as_that_place() {
        let root = vec![b"d:".to_vec(), b"winnt".to_vec()];
        let read = |path: &str| -> Vec<Vec<String>> {
            let unc = windows(path.as_bytes()).unwrap();
            local_readings(&unc, Some(&root))
                .into_iter()
                .map(|r| parts(&r.unwrap()))
                .collect()
        };
        let forms = [
            r"\\localhost\C$\Windows\System32",
            r"\\127.0.0.1\c$\Windows\System32",
            r"\\any-name-at-all\C$\Windows\System32",
            r"\\?\UNC\localhost\C$\Windows\System32",
            r"\\.\UNC\localhost\C$\Windows\System32",
            r"\??\UNC\localhost\C$\Windows\System32",
            r"//localhost/c$/Windows/System32",
            r"\\localhost\c$.\Windows\.\System32",
        ];
        for form in forms {
            assert_eq!(
                read(form),
                vec![vec!["c:", "windows", "system32"]],
                "{form}"
            );
        }
        assert_eq!(
            read(r"\\h\ADMIN$\System32"),
            vec![
                vec!["c:", "windows", "system32"],
                vec!["d:", "winnt", "system32"]
            ]
        );
        assert_eq!(
            read(r"\\h\admin$"),
            vec![vec!["c:", "windows"], vec!["d:", "winnt"]]
        );
        // Without a Windows folder in the environment only the usual one.
        let unc = windows(br"\\h\ADMIN$\x").unwrap();
        assert_eq!(local_readings(&unc, None).len(), 1);
        // Ordinary shares name nothing local that the text can tell.
        assert!(read(r"\\h\data\x").is_empty());
        assert!(read(r"\\h\CC$\x").is_empty());
        assert!(read(r"\\h\1$\x").is_empty());
        // Not a network path at all.
        assert!(local_readings(&windows(br"C:\x").unwrap(), None).is_empty());
    }

    #[test]
    fn wsl_shares_are_read_as_linux_paths_with_their_case() {
        let linux_parts = |path: &str| -> Vec<Vec<String>> {
            local_readings(&windows(path.as_bytes()).unwrap(), None)
                .into_iter()
                .map(|r| {
                    let norm = r.unwrap();
                    assert_eq!(norm.flavor, Flavor::Linux);
                    parts(&norm)
                })
                .collect()
        };
        for path in [
            r"\\wsl$\Ubuntu\Usr\Lib",
            r"\\WSL$\Ubuntu\Usr\Lib",
            r"\\wsl.localhost\Ubuntu\Usr\Lib",
            r"\\?\UNC\wsl$\Ubuntu\Usr\Lib",
            r"\\.\UNC\wsl.localhost\Ubuntu\Usr\Lib",
            r"//wsl$/Ubuntu/Usr/Lib",
        ] {
            // Linux tells `Usr` and `usr` apart, so the case stays.
            assert_eq!(linux_parts(path), vec![vec!["Usr", "Lib"]], "{path}");
        }
        // A `..` inside a distribution follows its links.
        let popped = windows(br"\\wsl$\Ubuntu\home\alice\l\..\bin").unwrap();
        assert!(popped.popped);
        let readings = local_readings(&popped, None);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].as_ref().unwrap_err(), &Unresolvable::DotDot);
        // Another host named like it is just a host.
        assert!(linux_parts(r"\\wsl\Ubuntu\usr").is_empty());
        assert!(linux_parts(r"\\notwsl$\Ubuntu\usr").is_empty());
    }

    #[test]
    fn a_drive_mounted_in_linux_is_also_read_as_the_drive() {
        let drive = |path: &str| -> Option<Vec<String>> {
            let norm = linux(path.as_bytes()).unwrap();
            drive_mounted_in_linux(&norm).map(|r| parts(&r.unwrap()))
        };
        assert_eq!(drive("/mnt/c"), Some(vec!["c:".into()]));
        assert_eq!(
            drive("/mnt/C/WINDOWS/System32"),
            Some(vec!["c:".into(), "windows".into(), "system32".into()])
        );
        assert_eq!(
            drive("/mnt/d/Program Files./x::$DATA"),
            Some(vec!["d:".into(), "program files".into(), "x".into()])
        );
        let alias = linux(b"/mnt/c/PROGRA~1").unwrap();
        assert!(drive_mounted_in_linux(&alias).unwrap().unwrap().alias);
        // Only a single letter under /mnt, and only /mnt.
        assert_eq!(drive("/mnt/data/Windows"), None);
        assert_eq!(drive("/mnt"), None);
        assert_eq!(drive("/media/c/Windows"), None);
        assert_eq!(drive("/MNT/c/Windows"), None);
        assert_eq!(drive("/mnt/1/Windows"), None);
        assert!(drive_mounted_in_linux(&windows(br"C:\mnt\c").unwrap()).is_none());
    }

    #[test]
    fn short_names_are_flagged_in_the_first_two_parts() {
        for path in [
            r"C:\PROGRA~1",
            r"C:\PROGRA~2\x",
            r"c:\progra~1\Foo",
            r"C:\Users\ALICEJ~1\x",
            r"C:\WINDOW~1",
            r"C:\MYDOCU~1.TXT",
        ] {
            assert!(windows(path.as_bytes()).unwrap().alias, "{path}");
        }
        for path in [
            r"C:\Windows",
            r"C:\Users\alice\PROGRA~1",
            r"C:\a~b",
            r"C:\toolongname~1",
            r"C:\a~",
            r"C:\~1",
        ] {
            assert!(!windows(path.as_bytes()).unwrap().alias, "{path}");
        }
    }

    #[test]
    fn letters_that_fold_to_ascii_cannot_stand_in_for_them() {
        // dotless i, long s, Kelvin sign
        assert_eq!(win("C:\\W\u{131}ndow\u{17f}"), vec!["c:", "windows"]);
        assert_eq!(win("C:\\Program Files"), vec!["c:", "program files"]);
        assert_eq!(win("C:\\Mar\u{212a}"), vec!["c:", "mark"]);
        // Other non-ASCII text is kept as it is.
        assert_eq!(win("C:\\Ünï"), vec!["c:", "\u{dc}n\u{ef}"]);
    }

    #[test]
    fn linux_paths_reduce_without_case_folding() {
        let cases: [(&[u8], &[&str]); 8] = [
            (b"/usr", &["usr"]),
            (b"//usr", &["usr"]),
            (b"/usr/", &["usr"]),
            (b"/../usr", &["usr"]),
            (b"/usr/./lib/.", &["usr", "lib"]),
            (b"///usr///lib//", &["usr", "lib"]),
            (b"/USR", &["USR"]),
            (b"/", &[]),
        ];
        for (path, expected) in cases {
            let got = parts(&linux(path).unwrap());
            assert_eq!(got, expected, "{:?}", String::from_utf8_lossy(path));
        }
        // A backslash is an ordinary character on Linux.
        assert_eq!(parts(&linux(br"/a\b/c").unwrap()), vec![r"a\b", "c"]);
        // Non-UTF-8 names survive.
        assert_eq!(linux(b"/a/\xff\xfe").unwrap().len(), 2);
    }

    #[test]
    fn hostile_input_stays_bounded() {
        let long_parts = [
            b"/a".repeat(60_000),
            b"\\a".repeat(60_000),
            b"/..".repeat(40_000),
        ];
        for path in long_parts {
            let _ = linux(&path);
            let mut windows_path = b"C:".to_vec();
            windows_path.extend_from_slice(&path);
            let _ = windows(&windows_path);
        }
    }
}
