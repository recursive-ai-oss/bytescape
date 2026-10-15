//! What a path is, who manages it, and whether it may be deleted.
//!
//! Three tables are compiled into the program from the files under `data/`:
//!
//! - **categories** map file extensions, and a few places, to one of the
//!   colors of the map ([`Category`]);
//! - **recognition** gives well-known folders a short factual note saying what
//!   the folder is and who manages it, with an optional pointer to the tool
//!   that owns it ([`Handoff`]);
//! - **protected** lists the paths that are blocked outright or need a stronger
//!   confirmation before they are deleted ([`Level`]).
//!
//! The notes never judge whether something may be deleted. The tests of this
//! crate enforce that wording rule.
//!
//! # Use
//!
//! Matching needs to know the user's home folder and a few Windows folders
//! (`%LOCALAPPDATA%` and so on), so a [`Knowledge`] is built from an
//! [`Environment`] that the caller fills in. Nothing here reads the process
//! environment, which keeps matching a pure function of its inputs:
//!
//! ```
//! use bs_knowledge::{Environment, Knowledge, Level};
//!
//! let env = Environment::new().with("HOME", "/home/alice");
//! let knowledge = Knowledge::new(&env);
//! assert_eq!(knowledge.protection("//usr/./lib").level, Level::Blocked);
//! ```
//!
//! The free functions [`classify`], [`recognize`], [`protection`] and
//! [`strictest`] use an empty environment. They still know every path that does
//! not depend on a variable, including all the blocked system folders under
//! their usual locations.
//!
//! The crate has no dependencies. The files are turned into Rust tables by a
//! build script, so no parser is linked either.
//!
//! # Category numbers
//!
//! The size tree stores a category as a number. 0 is unclassified and is drawn
//! as Other; 1 to 8 are Video, Images, Audio, Documents, Archives and disk
//! images, Apps and system, Code and dev, Cache and temp. See
//! [`Category::number`].

use std::ffi::OsString;
use std::path::Path;
use std::sync::OnceLock;

mod matcher;
mod normalize;

use matcher::{Compiled, Split, Style, compile_drive_root, match_any};
use normalize::{Flavor, Root};

mod tables {
    use super::*;
    include!(concat!(env!("OUT_DIR"), "/tables.rs"));
}

/// A color of the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
#[repr(u8)]
pub enum Category {
    /// Unclassified; drawn as Other.
    Other = 0,
    Video = 1,
    Images = 2,
    Audio = 3,
    Documents = 4,
    /// Archives and disk images.
    Archives = 5,
    /// Applications and system files.
    AppsSystem = 6,
    /// Code and developer files.
    CodeDev = 7,
    /// Caches and temporary files.
    CacheTemp = 8,
}

impl Category {
    /// Every category, in number order.
    pub const ALL: [Category; 9] = [
        Category::Other,
        Category::Video,
        Category::Images,
        Category::Audio,
        Category::Documents,
        Category::Archives,
        Category::AppsSystem,
        Category::CodeDev,
        Category::CacheTemp,
    ];

    /// The number the size tree stores: 0 for unclassified, 1 to 8 for the
    /// colored categories in table order.
    pub const fn number(self) -> u8 {
        self as u8
    }

    /// The category with this number, if there is one.
    pub const fn from_number(number: u8) -> Option<Category> {
        match number {
            0 => Some(Category::Other),
            1 => Some(Category::Video),
            2 => Some(Category::Images),
            3 => Some(Category::Audio),
            4 => Some(Category::Documents),
            5 => Some(Category::Archives),
            6 => Some(Category::AppsSystem),
            7 => Some(Category::CodeDev),
            8 => Some(Category::CacheTemp),
            _ => None,
        }
    }

    /// The name shown in the legend.
    pub const fn label(self) -> &'static str {
        match self {
            Category::Other => "Other",
            Category::Video => "Video",
            Category::Images => "Images",
            Category::Audio => "Audio",
            Category::Documents => "Documents",
            Category::Archives => "Archives & disk images",
            Category::AppsSystem => "Apps & system",
            Category::CodeDev => "Code & dev",
            Category::CacheTemp => "Cache & temp",
        }
    }
}

/// The system an entry applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Os {
    Any,
    Windows,
    Linux,
}

/// The tool that owns a recognized folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Handoff {
    /// The program's registered uninstaller.
    Uninstall,
    /// Disk Cleanup or the Storage settings.
    DiskCleanup,
    /// The Linux package that installed it.
    Package,
}

/// How firmly a path is guarded against deletion. Ordered from the least to
/// the most strict, so the strictest of several is their maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Level {
    /// Nothing special.
    Normal,
    /// A stronger confirmation that repeats the note, held to confirm.
    Protected,
    /// Never offered for deletion.
    Blocked,
}

/// A recognized folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The pattern as written in the data file.
    pub pattern: &'static str,
    pub os: Os,
    /// The category of the folder, and of what is inside it when `inherit`.
    pub category: Category,
    /// Files inside the folder take its category.
    pub inherit: bool,
    /// What the folder is and who manages it.
    pub note: &'static str,
    pub handoff: Option<Handoff>,
}

/// A blocked or protected path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtectedEntry {
    /// The pattern as written in the data file.
    pub pattern: &'static str,
    pub os: Os,
    pub level: Level,
    /// Only the path itself, not what is below it.
    pub exact: bool,
    /// What the path is and who manages it. Always set for protected entries.
    pub note: Option<&'static str>,
}

/// A place whose contents take a category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathRule {
    pub pattern: &'static str,
    pub os: Os,
    pub category: Category,
}

use tables::{EXTENSIONS, PATH_RULES, PROTECTED, RECOGNITION, VARIABLES};

/// The extension table, sorted by extension.
pub fn extension_table() -> &'static [(&'static str, Category)] {
    EXTENSIONS
}

/// The path rules of the category table.
pub fn path_rules() -> &'static [PathRule] {
    PATH_RULES
}

/// Every recognition entry.
pub fn recognition_entries() -> &'static [Entry] {
    RECOGNITION
}

/// Every protected-path entry.
pub fn protected_entries() -> &'static [ProtectedEntry] {
    PROTECTED
}

/// The system whose way of writing paths a [`Knowledge`] expects.
///
/// A path means different things to different systems: `C:/../usr` is a
/// drive path on Windows, but on Linux it is relative to the working
/// directory, and so says nothing. The protection check reads a path the way
/// the system it runs on does, so it never has to guess which folder a
/// relative name ends up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Host {
    Windows,
    Linux,
}

impl Host {
    /// The system this program was built for.
    pub const fn native() -> Host {
        if cfg!(windows) {
            Host::Windows
        } else {
            Host::Linux
        }
    }
}

/// The values the patterns can refer to.
///
/// Fill it from the process environment, or from anything else, before building
/// a [`Knowledge`]. Names are not case sensitive. A pattern that names a
/// variable which is not set is left out; the main system folders are also
/// listed under their usual drive, so they stay guarded.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    vars: Vec<(String, Vec<u8>)>,
    drive_roots: Vec<Vec<u8>>,
}

impl Environment {
    /// An environment with nothing in it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a variable. `HOME` is the home folder on Linux and `USERPROFILE`
    /// the home folder on Windows; the others are the usual Windows variables
    /// (`LOCALAPPDATA`, `SystemRoot`, ...). An empty value is ignored.
    #[must_use]
    pub fn with(mut self, name: &str, value: impl AsRef<[u8]>) -> Self {
        let value = value.as_ref();
        if value.is_empty() {
            return self;
        }
        let name = name.to_ascii_uppercase();
        self.vars.retain(|(n, _)| *n != name);
        self.vars.push((name, value.to_vec()));
        self
    }

    /// Adds the root folders of the drives of this computer: the mount points
    /// on Linux (`/mnt/data`, `/media/alice/USB`) and the drive letters on
    /// Windows. The tables cannot know where drives are mounted, and the root
    /// of a drive is never to be deleted, so each root given here is blocked
    /// as the path itself (its contents are judged as usual). A root that is
    /// not an absolute path is ignored.
    ///
    /// Callers must pass every drive they know of, for instance the
    /// `mount_point` of each drive and of each of its mounts from
    /// `bs-platform`.
    #[must_use]
    pub fn with_drive_roots<P: AsRef<Path>>(mut self, roots: impl IntoIterator<Item = P>) -> Self {
        for root in roots {
            let bytes = root.as_ref().as_os_str().as_encoded_bytes();
            if !bytes.is_empty() {
                self.drive_roots.push(bytes.to_vec());
            }
        }
        self
    }

    /// Fills in every variable the tables use by asking `lookup`, for example
    /// `Environment::from_lookup(|name| std::env::var_os(name))`. The caller
    /// decides where the values come from; this crate never reads the process
    /// environment itself.
    pub fn from_lookup(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        let mut env = Self::new();
        for name in VARIABLES {
            if let Some(value) = lookup(name) {
                env = env.with(name, value.as_encoded_bytes());
            }
        }
        env
    }

    /// The names that [`Environment::from_lookup`] asks for.
    pub fn variable_names() -> &'static [&'static str] {
        VARIABLES
    }

    pub(crate) fn get(&self, name: &str) -> Option<&[u8]> {
        self.vars
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }
}

/// Why a path got its level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// Nothing applies.
    None,
    /// An entry of the protected table matches. See [`Verdict::entry`].
    Table,
    /// The path is the root of a drive or of a network share.
    Root,
    /// A short (8.3) name is used for one of the top folders. Its target
    /// cannot be told from the text, so it is blocked.
    Alias,
    /// The path is empty, relative, too long, holds a NUL byte, names a
    /// device, or has a `..` that the text cannot settle, so there is no way
    /// to say what it is.
    Unresolvable,
    /// The path is not blocked itself, but a blocked place lies inside it, so
    /// removing it would remove that place too. [`Verdict::entry`] is `None`:
    /// the entry that matched describes the inner place, not this path.
    HoldsBlocked,
}

/// The answer to "may this path be deleted".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub level: Level,
    pub reason: Reason,
    /// The matching entry and its note, for table matches.
    pub entry: Option<&'static ProtectedEntry>,
}

impl Verdict {
    const NORMAL: Verdict = Verdict {
        level: Level::Normal,
        reason: Reason::None,
        entry: None,
    };
}

/// The longest name looked up in the name index.
const MAX_INDEXED_NAME: usize = 32;

/// How many plain names it takes before they are looked up instead of tried.
const INDEX_THRESHOLD: usize = 10;

/// How well a rule matched.
#[derive(Clone, Copy)]
struct Hit {
    /// Parts of the pattern that matched.
    matched: usize,
    /// Wildcard parts in the pattern.
    wild: usize,
    /// Position of the rule in the table, to break ties.
    order: usize,
}

/// A name that may appear anywhere in a path (`**/node_modules`).
#[derive(Clone, Debug)]
struct NameRule<T> {
    /// Lower case for Windows rules.
    name: Box<[u8]>,
    exact: bool,
    order: usize,
    value: T,
}

/// The rules for one system.
#[derive(Clone, Debug)]
struct SystemRules<T> {
    fold: bool,
    /// Everything that is not a plain name.
    general: Vec<(Compiled, usize, T)>,
    /// Plain names, sorted, so a path part can be looked up instead of tried
    /// against every rule.
    names: Vec<NameRule<T>>,
}

impl<T: Copy> SystemRules<T> {
    fn new(fold: bool) -> Self {
        Self {
            fold,
            general: Vec::new(),
            names: Vec::new(),
        }
    }

    /// Moves the plain names into the sorted index, when there are enough of
    /// them for a lookup to be quicker than trying each.
    fn finish(&mut self) {
        let is_plain = |c: &Compiled| {
            c.floating
                && c.parts.len() == 1
                && !c.parts[0].wild
                && c.parts[0].text.len() <= MAX_INDEXED_NAME
        };
        let plain = self.general.iter().filter(|(c, ..)| is_plain(c)).count();
        if plain < INDEX_THRESHOLD {
            return;
        }
        let (named, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.general)
            .into_iter()
            .partition(|(c, ..)| is_plain(c));
        self.general = rest;
        self.names = named
            .into_iter()
            .map(|(c, order, value)| NameRule {
                name: c.parts[0].text.clone(),
                exact: c.exact,
                order,
                value,
            })
            .collect();
        self.names
            .sort_by(|a, b| (&a.name, a.order).cmp(&(&b.name, b.order)));
    }

    fn each(&self, split: &Split<'_>, relative: bool, f: &mut impl FnMut(Hit, T)) {
        for (compiled, order, value) in &self.general {
            if relative && !compiled.floating {
                continue;
            }
            if let Some(matched) = match_any(compiled, split.slice(), true, false) {
                let wild = compiled.wild_parts();
                f(
                    Hit {
                        matched,
                        wild,
                        order: *order,
                    },
                    *value,
                );
            }
        }
        if self.names.is_empty() {
            return;
        }
        let parts = split.slice();
        let mut folded = [0u8; MAX_INDEXED_NAME];
        for (index, part) in parts.iter().enumerate() {
            if part.len() > MAX_INDEXED_NAME {
                continue;
            }
            let key: &[u8] = if self.fold {
                let slot = &mut folded[..part.len()];
                for (out, byte) in slot.iter_mut().zip(*part) {
                    *out = byte.to_ascii_lowercase();
                }
                slot
            } else {
                part
            };
            let start = self.names.partition_point(|n| *n.name < *key);
            for rule in self.names[start..].iter().take_while(|n| *n.name == *key) {
                // An exact rule needs the name to be the last part of a whole path.
                if rule.exact && index + 1 != parts.len() {
                    continue;
                }
                f(
                    Hit {
                        matched: 1,
                        wild: 0,
                        order: rule.order,
                    },
                    rule.value,
                );
            }
        }
    }
}

/// Compiled rules kept apart by the system they are for, so that matching a
/// Windows path never looks at a Linux rule.
#[derive(Clone, Debug)]
struct Rules<T> {
    windows: SystemRules<T>,
    linux: SystemRules<T>,
    count: usize,
}

impl<T: Copy> Rules<T> {
    fn new() -> Self {
        Self {
            windows: SystemRules::new(true),
            linux: SystemRules::new(false),
            count: 0,
        }
    }

    fn push(&mut self, compiled: Compiled, value: T) {
        let order = self.count;
        self.count += 1;
        let system = match compiled.flavor {
            Flavor::Windows => &mut self.windows,
            Flavor::Linux => &mut self.linux,
        };
        system.general.push((compiled, order, value));
    }

    fn finish(&mut self) {
        self.windows.finish();
        self.linux.finish();
    }

    /// Calls `f` for every rule that matches the path.
    fn each(&self, split: &Split<'_>, f: &mut impl FnMut(Hit, T)) {
        match split.style {
            Style::Windows => self.windows.each(split, false, f),
            Style::Linux => self.linux.each(split, false, f),
            // A bare name carries no system; only names anywhere can match.
            Style::Relative => {
                self.windows.each(split, true, f);
                self.linux.each(split, true, f);
            }
        }
    }
}

/// The tables with their patterns resolved for one environment.
#[derive(Clone, Debug)]
pub struct Knowledge {
    host: Host,
    paint: Rules<Category>,
    recognition: Rules<&'static Entry>,
    protected: Vec<(Compiled, &'static ProtectedEntry)>,
    /// The Windows folder of the environment, as parts, for `ADMIN$` shares.
    system_root: Option<Vec<Vec<u8>>>,
}

/// The entry reported for a drive root that the caller listed.
static DRIVE_ROOT: ProtectedEntry = ProtectedEntry {
    pattern: "(drive root)",
    os: Os::Any,
    level: Level::Blocked,
    exact: true,
    note: Some("The root of a drive."),
};

impl Knowledge {
    /// Resolves the tables against `env`, for the system this program runs
    /// on.
    pub fn new(env: &Environment) -> Self {
        Self::for_host(env, Host::native())
    }

    /// Resolves the tables against `env`, reading paths the way `host` does.
    pub fn for_host(env: &Environment, host: Host) -> Self {
        let mut paint = Rules::new();
        for rule in PATH_RULES {
            for compiled in matcher::compile(rule.pattern, rule.os, false, env) {
                paint.push(compiled, rule.category);
            }
        }
        let mut recognition = Rules::new();
        for entry in RECOGNITION {
            for compiled in matcher::compile(entry.pattern, entry.os, true, env) {
                recognition.push(compiled, entry);
            }
            if entry.inherit && entry.category != Category::Other {
                for compiled in matcher::compile(entry.pattern, entry.os, false, env) {
                    paint.push(compiled, entry.category);
                }
            }
        }
        let mut protected = Vec::new();
        for entry in PROTECTED {
            for compiled in matcher::compile(entry.pattern, entry.os, entry.exact, env) {
                protected.push((compiled, entry));
            }
        }
        for root in &env.drive_roots {
            if let Some(compiled) = compile_drive_root(root) {
                protected.push((compiled, &DRIVE_ROOT));
            }
        }
        let system_root = env
            .get("SYSTEMROOT")
            .and_then(|value| normalize::windows(value).ok())
            .filter(|norm| norm.root == Root::Drive)
            .map(|norm| norm.parts().map(<[u8]>::to_vec).collect());
        paint.finish();
        recognition.finish();
        Self {
            host,
            paint,
            recognition,
            protected,
            system_root,
        }
    }

    /// The system whose way of writing paths this follows.
    pub fn host(&self) -> Host {
        self.host
    }

    /// The category of a file or folder, from where it is and what it is
    /// called. A place in the tables wins over the extension; the most specific
    /// place wins over a broader one. Unknown things are [`Category::Other`].
    ///
    /// The path is used as given: `..` is not resolved, and nothing is
    /// allocated. Use [`Knowledge::protection`] when the answer guards a
    /// deletion.
    pub fn classify(&self, path: &Path, is_dir: bool) -> Category {
        self.classify_bytes(path.as_os_str().as_encoded_bytes(), is_dir)
    }

    /// [`Knowledge::classify`] for a path held as bytes.
    pub fn classify_bytes(&self, path: &[u8], is_dir: bool) -> Category {
        let split = Split::of(path);
        if let Some(category) = self.place_of(&split) {
            return category;
        }
        if is_dir {
            return Category::Other;
        }
        extension_category(split.last).unwrap_or(Category::Other)
    }

    /// The category that the place alone gives, without looking at the file
    /// name's extension: `Some` when the path is in or below a place of the
    /// tables. A scanner can ask this once per folder and let the files inside
    /// take the answer, then fall back to [`classify_name`] for the rest.
    pub fn place(&self, path: &Path) -> Option<Category> {
        self.place_bytes(path.as_os_str().as_encoded_bytes())
    }

    /// [`Knowledge::place`] for a path held as bytes.
    pub fn place_bytes(&self, path: &[u8]) -> Option<Category> {
        self.place_of(&Split::of(path))
    }

    fn place_of(&self, split: &Split<'_>) -> Option<Category> {
        // The most parts matched wins; of equals, the earlier rule.
        let mut best: Option<((usize, usize), Category)> = None;
        self.paint.each(split, &mut |hit, category| {
            let rank = (usize::MAX - hit.matched, hit.order);
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, category));
            }
        });
        best.map(|(_, category)| category)
    }

    /// The recognition entry for this folder, if it is a well-known one. Only
    /// the folder itself matches, not what is inside it. When several entries
    /// match, the one with the fewest wildcards and then the most parts wins.
    pub fn recognize(&self, path: &Path) -> Option<&'static Entry> {
        self.recognize_bytes(path.as_os_str().as_encoded_bytes())
    }

    /// [`Knowledge::recognize`] for a path held as bytes.
    pub fn recognize_bytes(&self, path: &[u8]) -> Option<&'static Entry> {
        let split = Split::of(path);
        // The fewest wildcards win, then the most parts, then the earlier entry.
        let mut best: Option<((usize, usize, usize), &'static Entry)> = None;
        self.recognition.each(&split, &mut |hit, entry| {
            let rank = (hit.wild, usize::MAX - hit.matched, hit.order);
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, entry));
            }
        });
        best.map(|(_, entry)| entry)
    }

    /// How firmly this path is guarded against deletion.
    ///
    /// See [`Knowledge::protection_bytes`].
    pub fn protection(&self, path: impl AsRef<Path>) -> Verdict {
        self.protection_bytes(path.as_ref().as_os_str().as_encoded_bytes())
    }

    /// [`Knowledge::protection`] for a path held as bytes.
    ///
    /// The path is reduced to its parts first, so `//usr`, `/usr/./lib`,
    /// `C:\Windows\.`, `c:/WINDOWS`, `\\?\C:\Windows`, a trailing dot or space
    /// and an alternate data stream all give the answer of the path they name.
    /// Paths that name the same place in another way are read as that place
    /// too, and the strictest answer stands: `\\host\C$\Windows` (any host),
    /// `\\host\ADMIN$\...`, `\\wsl$\distro\usr` and `/mnt/c/windows`. Only
    /// those names are known: a share with another name that happens to show a
    /// protected folder cannot be told from its text.
    ///
    /// A `..` at the root of a drive path stays at the root, but two kinds of
    /// root are not guessed at. In a network path a `..` cannot pass the
    /// share, and in a device path other than the exact `\\?\` form (such as
    /// `\\.\D:\..\C:\Windows`) it would swap the drive; both are blocked as
    /// unresolvable.
    ///
    /// The path is read the way [`Knowledge::host`] does. On Linux a path that
    /// does not start with `/` depends on the working directory, so it is
    /// blocked, and so is a `..` after a named part, because a link in that
    /// part changes where the `..` leads (the callers hold canonical paths).
    /// On Windows `..` is settled by the text, as Win32 does. A path that is
    /// empty, too long, holds a NUL byte, names a device, or is relative on
    /// Windows is blocked, because nothing can be said about it. A path that
    /// starts with a separator is read both ways, as Linux and as Windows, and
    /// the stricter answer stands.
    ///
    /// This works on the text alone: links are not followed and short (8.3)
    /// names are not expanded (a short name in a top folder is blocked
    /// instead), so the code that deletes has to check the real target as
    /// well. The table does not know where drives are mounted: the root of a
    /// drive is only blocked here if it was given to
    /// [`Environment::with_drive_roots`], so callers must pass every drive
    /// root.
    pub fn protection_bytes(&self, path: &[u8]) -> Verdict {
        let mut found = Accumulator::new();
        for (index, reading) in self.readings(path).iter().enumerate() {
            match reading {
                Ok(norm) => {
                    let tag = u8::try_from(index).unwrap_or(u8::MAX);
                    self.examine(norm, &mut found, tag);
                }
                Err(_) => found.add_structural(Reason::Unresolvable),
            }
        }
        found.verdict
    }

    /// Every way the path can be read, the path as written first.
    fn readings(&self, path: &[u8]) -> Vec<Result<normalize::Normalized, normalize::Unresolvable>> {
        let mut out = Vec::with_capacity(4);
        if path.first() == Some(&b'/') || self.host == Host::Linux {
            // On Linux a path that does not start with `/` is relative, and
            // this gives the error that says so.
            out.push(normalize::linux(path));
            if path.first() != Some(&b'/') {
                return out;
            }
        }
        out.push(normalize::windows(path));
        // Readings that add places the same path may name. None of them adds
        // another of its own kind, so this ends.
        let mut index = 0;
        while index < out.len() {
            let extra: Vec<_> = match &out[index] {
                Ok(norm) if norm.root == Root::Unc => {
                    normalize::local_readings(norm, self.system_root.as_deref())
                }
                Ok(norm) if norm.flavor == Flavor::Linux => normalize::drive_mounted_in_linux(norm)
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            out.extend(extra);
            index += 1;
        }
        out
    }

    fn examine(&self, norm: &normalize::Normalized, found: &mut Accumulator, reading: u8) {
        // `/x~1` is an ordinary name on Linux, so a path rooted at the current
        // drive only counts as a short name where Windows is the host or the
        // path is written the Windows way.
        let weak = norm.weak_alias && (self.host == Host::Windows || norm.backslash);
        if norm.alias || weak {
            found.add_structural(Reason::Alias);
        }
        match norm.root {
            Root::Unc if norm.depth() <= 2 => found.add_structural(Reason::Root),
            Root::AnyDrive if norm.depth() == 0 => found.add_structural(Reason::Root),
            _ => {}
        }
        let ignore_drive = norm.root == Root::AnyDrive;
        let parts: Vec<&[u8]> = norm.parts().collect();
        for (compiled, entry) in &self.protected {
            if compiled.flavor != norm.flavor {
                continue;
            }
            if let Some(matched) = match_any(compiled, &parts, true, ignore_drive) {
                found.add_entry(entry, matched, reading);
            }
        }
    }

    /// A blocked place that is the folder itself or lies inside it, if there
    /// is one.
    ///
    /// [`Knowledge::protection`] looks at a path and what contains it. A folder
    /// that is only protected can still hold something that is blocked: `/var`
    /// holds the package databases under `/var/lib`, and a drive root holds the
    /// system folders. Deleting such a folder would delete the blocked place
    /// with it, so code that deletes a whole folder asks this too and treats a
    /// hit as blocked.
    ///
    /// The path is read like [`Knowledge::protection`] reads it, with the same
    /// limits. The roots of other drives given to
    /// [`Environment::with_drive_roots`] are not counted, because a delete
    /// does not cross into another mount.
    pub fn blocked_below(&self, path: &Path) -> Option<&'static ProtectedEntry> {
        self.blocked_below_bytes(path.as_os_str().as_encoded_bytes())
    }

    /// [`Knowledge::blocked_below`] for a path held as bytes.
    pub fn blocked_below_bytes(&self, path: &[u8]) -> Option<&'static ProtectedEntry> {
        for reading in self.readings(path) {
            let Ok(norm) = reading else { continue };
            let parts: Vec<&[u8]> = norm.parts().collect();
            let ignore_drive = norm.root == Root::AnyDrive;
            for (compiled, entry) in &self.protected {
                if entry.level == Level::Blocked
                    && !std::ptr::eq(*entry, &DRIVE_ROOT)
                    && compiled.flavor == norm.flavor
                    && matcher::lies_below(compiled, &parts, ignore_drive)
                {
                    return Some(entry);
                }
            }
        }
        None
    }

    /// The strictest level among several paths, as for a multi-selection. An
    /// empty selection is [`Level::Normal`].
    pub fn strictest<P: AsRef<Path>>(&self, paths: impl IntoIterator<Item = P>) -> Level {
        self.strictest_verdict(paths).level
    }

    /// The verdict of the strictest path among several; the first one when
    /// several are equally strict.
    pub fn strictest_verdict<P: AsRef<Path>>(&self, paths: impl IntoIterator<Item = P>) -> Verdict {
        let mut worst = Verdict::NORMAL;
        for path in paths {
            let verdict = self.protection(path);
            if verdict.level > worst.level {
                worst = verdict;
            }
        }
        worst
    }
}

/// Collects the strictest finding over several readings of one path.
struct Accumulator {
    verdict: Verdict,
    /// Parts matched by the entry in `verdict`, and the reading it came from.
    matched: usize,
    reading: u8,
}

impl Accumulator {
    fn new() -> Self {
        Self {
            verdict: Verdict::NORMAL,
            matched: 0,
            reading: 0,
        }
    }

    fn add_structural(&mut self, reason: Reason) {
        if self.verdict.level < Level::Blocked {
            self.verdict = Verdict {
                level: Level::Blocked,
                reason,
                entry: None,
            };
        }
    }

    /// Takes an entry if it is stricter than what is held. Of equally strict
    /// entries from the same reading of the path the more specific one wins;
    /// across readings the first stays.
    fn add_entry(&mut self, entry: &'static ProtectedEntry, matched: usize, reading: u8) {
        let better = entry.level > self.verdict.level
            || (entry.level == self.verdict.level
                && self.verdict.entry.is_some()
                && reading == self.reading
                && matched > self.matched);
        if better {
            self.verdict = Verdict {
                level: entry.level,
                reason: Reason::Table,
                entry: Some(entry),
            };
            self.matched = matched;
            self.reading = reading;
        }
    }
}

/// The category a file name gives by its extension alone. This is the cheap
/// part of [`Knowledge::classify`]: it needs no tables beyond the extension
/// list and makes no allocation.
pub fn classify_name(name: &[u8]) -> Category {
    extension_category(name).unwrap_or(Category::Other)
}

fn extension_category(name: &[u8]) -> Option<Category> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    // `.bashrc` is a name, not an extension; `name.` has none either.
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    let extension = &name[dot + 1..];
    if extension.len() > 15 {
        return None;
    }
    let mut buffer = [0u8; 15];
    let lower = &mut buffer[..extension.len()];
    for (slot, &byte) in lower.iter_mut().zip(extension) {
        *slot = byte.to_ascii_lowercase();
    }
    let lower: &[u8] = lower;
    EXTENSIONS
        .binary_search_by(|(known, _)| known.as_bytes().cmp(lower))
        .ok()
        .map(|index| EXTENSIONS[index].1)
}

fn builtin() -> &'static Knowledge {
    static BUILTIN: OnceLock<Knowledge> = OnceLock::new();
    BUILTIN.get_or_init(|| Knowledge::new(&Environment::new()))
}

/// [`Knowledge::classify`] with an empty environment.
pub fn classify(path: &Path, is_dir: bool) -> Category {
    builtin().classify(path, is_dir)
}

/// [`Knowledge::recognize`] with an empty environment.
pub fn recognize(path: &Path) -> Option<&'static Entry> {
    builtin().recognize(path)
}

/// [`Knowledge::protection`] with an empty environment.
pub fn protection(path: impl AsRef<Path>) -> Verdict {
    builtin().protection(path)
}

/// [`Knowledge::strictest`] with an empty environment.
pub fn strictest<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> Level {
    builtin().strictest(paths)
}

#[cfg(test)]
mod lint;
#[cfg(test)]
mod tests;
