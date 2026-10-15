//! Tests for the tables and the matching.

use super::*;
use crate::lint;
use proptest::prelude::*;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn windows_env() -> Environment {
    Environment::new()
        .with("USERPROFILE", r"C:\Users\alice")
        .with("LOCALAPPDATA", r"C:\Users\alice\AppData\Local")
        .with("APPDATA", r"C:\Users\alice\AppData\Roaming")
        .with("ProgramData", r"C:\ProgramData")
        .with("ProgramFiles", r"C:\Program Files")
        .with("ProgramFiles(x86)", r"C:\Program Files (x86)")
        .with("ProgramFiles(Arm)", r"C:\Program Files (Arm)")
        .with("ProgramW6432", r"C:\Program Files")
        .with("SystemRoot", r"C:\Windows")
        .with("SystemDrive", "C:")
}

fn full_env() -> Environment {
    windows_env().with("HOME", "/home/alice")
}

// The tests name their host, so that they mean the same on every system the
// suite runs on. `full` and `bare` read paths the Windows way, which also
// reads every path that starts with `/` as Linux; `linux_full` and
// `linux_bare` read them the Linux way.

fn full() -> &'static Knowledge {
    static FULL: OnceLock<Knowledge> = OnceLock::new();
    FULL.get_or_init(|| Knowledge::for_host(&full_env(), Host::Windows))
}

fn bare() -> &'static Knowledge {
    static BARE: OnceLock<Knowledge> = OnceLock::new();
    BARE.get_or_init(|| Knowledge::for_host(&Environment::new(), Host::Windows))
}

fn linux_full() -> &'static Knowledge {
    static FULL: OnceLock<Knowledge> = OnceLock::new();
    FULL.get_or_init(|| Knowledge::for_host(&full_env(), Host::Linux))
}

fn linux_bare() -> &'static Knowledge {
    static BARE: OnceLock<Knowledge> = OnceLock::new();
    BARE.get_or_init(|| Knowledge::for_host(&Environment::new(), Host::Linux))
}

/// A Windows-host `Knowledge` that was given some drive roots.
fn roots() -> &'static Knowledge {
    static ROOTS: OnceLock<Knowledge> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let env = full_env().with_drive_roots(["/media/alice/USB", "/mnt/data", r"E:\mount\vol"]);
        Knowledge::for_host(&env, Host::Windows)
    })
}

fn level(k: &Knowledge, path: &str) -> Level {
    k.protection(path).level
}

// ------------------------------------------------------------- the data

#[test]
fn category_numbers_follow_the_table_order() {
    for (index, category) in Category::ALL.iter().enumerate() {
        assert_eq!(usize::from(category.number()), index);
        assert_eq!(Category::from_number(category.number()), Some(*category));
    }
    assert_eq!(Category::from_number(9), None);
    assert_eq!(Category::from_number(255), None);
    let named: [(Category, u8, &str); 9] = [
        (Category::Other, 0, "Other"),
        (Category::Video, 1, "Video"),
        (Category::Images, 2, "Images"),
        (Category::Audio, 3, "Audio"),
        (Category::Documents, 4, "Documents"),
        (Category::Archives, 5, "Archives & disk images"),
        (Category::AppsSystem, 6, "Apps & system"),
        (Category::CodeDev, 7, "Code & dev"),
        (Category::CacheTemp, 8, "Cache & temp"),
    ];
    for (category, number, label) in named {
        assert_eq!(category.number(), number);
        assert_eq!(category.label(), label);
    }
}

#[test]
fn the_crate_has_no_dependencies_of_its_own() {
    let manifest = include_str!("../Cargo.toml");
    assert!(!manifest.contains("[dependencies"), "runtime dependencies");
    assert!(
        !manifest.contains("[build-dependencies"),
        "build dependencies"
    );
    assert!(!manifest.contains("[target."), "target dependencies");
}

#[test]
fn table_sizes() {
    let n = recognition_entries().len();
    assert!((50..=100).contains(&n), "{n} recognition entries");
    assert!(protected_entries().len() >= 40);
    assert!(extension_table().len() >= 200);
}

#[test]
fn extensions_are_sorted_lower_case_and_unique() {
    let table = extension_table();
    for pair in table.windows(2) {
        assert!(pair[0].0 < pair[1].0, "{} then {}", pair[0].0, pair[1].0);
    }
    for (extension, _) in table {
        assert!(
            extension
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
    }
}

fn assert_unique<'a>(table: &str, patterns: impl Iterator<Item = &'a str>) {
    let mut seen = HashSet::new();
    for pattern in patterns {
        assert!(
            seen.insert(pattern.to_lowercase()),
            "{table}: pattern `{pattern}` appears twice"
        );
    }
}

#[test]
fn patterns_are_unique_within_each_table() {
    assert_unique(
        "recognition",
        recognition_entries().iter().map(|e| e.pattern),
    );
    assert_unique("protected", protected_entries().iter().map(|e| e.pattern));
    assert_unique("paths", path_rules().iter().map(|e| e.pattern));
}

fn check_pattern_shape(table: &str, pattern: &str, os: Os) {
    let context = format!("{table}: `{pattern}`");
    // A variable or `~` only at the start.
    // A leading `~` is the home folder: alone, or followed by a separator.
    if let Some(rest) = pattern.strip_prefix('~') {
        assert!(rest.is_empty() || rest.starts_with('/'), "{context}: `~`");
    }
    let percents = pattern.matches('%').count();
    if percents > 0 {
        assert!(pattern.starts_with('%'), "{context}: variable not first");
        assert_eq!(percents, 2, "{context}: variable");
    }
    assert!(
        !pattern.contains('\\') && !pattern.contains("//"),
        "{context}"
    );
    assert!(
        !pattern.ends_with('/') || pattern == "/",
        "{context}: trailing slash"
    );
    // A pattern says which system it is for by how it starts.
    let starts_like_windows = pattern.starts_with('%')
        || pattern.starts_with("?:")
        || pattern.as_bytes().get(1) == Some(&b':');
    let starts_like_linux = pattern.starts_with('/');
    let floating = pattern.starts_with("**/");
    let home = pattern.starts_with('~');
    match os {
        Os::Windows => assert!(starts_like_windows || floating || home, "{context}"),
        Os::Linux => assert!(starts_like_linux || floating || home, "{context}"),
        Os::Any => assert!(floating || home, "{context}: needs a system"),
    }
}

#[test]
fn patterns_have_a_valid_shape() {
    for e in recognition_entries() {
        check_pattern_shape("recognition", e.pattern, e.os);
    }
    for e in protected_entries() {
        check_pattern_shape("protected", e.pattern, e.os);
    }
    for e in path_rules() {
        check_pattern_shape("paths", e.pattern, e.os);
    }
}

#[test]
fn every_variable_a_pattern_uses_is_asked_for() {
    let names: HashSet<&str> = Environment::variable_names().iter().copied().collect();
    for pattern in recognition_entries()
        .iter()
        .map(|e| e.pattern)
        .chain(protected_entries().iter().map(|e| e.pattern))
    {
        if let Some(rest) = pattern.strip_prefix('%') {
            let name = rest.split('%').next().unwrap().to_ascii_uppercase();
            assert!(names.contains(name.as_str()), "{pattern}");
        }
    }
    assert!(names.contains("HOME") && names.contains("USERPROFILE"));
}

// ---------------------------------------------------------- the wording rule

fn all_notes() -> Vec<(&'static str, &'static str)> {
    let mut notes: Vec<(&str, &str)> = recognition_entries()
        .iter()
        .map(|e| (e.pattern, e.note))
        .collect();
    notes.extend(
        protected_entries()
            .iter()
            .filter_map(|e| e.note.map(|n| (e.pattern, n))),
    );
    notes
}

#[test]
fn every_note_passes_the_wording_rule() {
    let mut bad = Vec::new();
    for (pattern, note) in all_notes() {
        let found = lint::check(note);
        if !found.is_empty() {
            bad.push(format!("{pattern}: {note:?} -> {found:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "notes that break the wording rule:\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_note_is_one_short_sentence() {
    for (pattern, note) in all_notes() {
        let context = format!("{pattern}: {note:?}");
        assert!(note.ends_with('.'), "{context}");
        assert!(
            note.chars().next().is_some_and(char::is_uppercase),
            "{context}"
        );
        assert!(note.len() <= 160, "{context}: {} bytes", note.len());
        assert!(note.len() >= 12, "{context}");
        assert!(!note.contains('\n') && note.trim() == note, "{context}");
        // A single sentence: no full stop followed by more text.
        assert!(!note[..note.len() - 1].contains(". "), "{context}");
    }
}

#[test]
fn a_note_that_turns_into_advice_fails_the_lint() {
    // The mutation the rule exists for: add the forbidden advice to a real note.
    let mutations = [
        " Safe to delete.",
        " This is junk.",
        " Reclaimable space.",
        " Clean up to free space.",
        " Freed after a restart.",
        " You can remove it.",
    ];
    for (pattern, note) in all_notes() {
        assert!(
            lint::check(note).is_empty(),
            "{pattern} already breaks the rule"
        );
        for suffix in mutations {
            let mutated = format!("{}{suffix}", note.trim_end_matches('.'));
            assert!(
                !lint::check(&mutated).is_empty(),
                "{pattern}: {mutated:?} was not caught"
            );
        }
        // And in front of it.
        assert!(!lint::check(&format!("Safe to delete: {note}")).is_empty());
    }
}

#[test]
fn notes_do_not_repeat_path_syntax() {
    // The card shows the path itself; a note holds none of the pattern syntax.
    for (pattern, note) in all_notes() {
        for forbidden in ["%", "\\", "C:", "~/"] {
            assert!(!note.contains(forbidden), "{pattern}: {note:?}");
        }
    }
}

// ---------------------------------------------------- fit of the categories

#[test]
fn recognition_categories_fit_the_entries() {
    // Spot table: folders and the category their note describes.
    let spot: [(&str, Category); 24] = [
        ("%LOCALAPPDATA%/Temp", Category::CacheTemp),
        ("%SystemRoot%/WinSxS", Category::AppsSystem),
        ("%SystemDrive%/Windows.old", Category::AppsSystem),
        ("%ProgramFiles%/*", Category::AppsSystem),
        ("%ProgramFiles(x86)%/Steam/steamapps", Category::AppsSystem),
        (
            "%LOCALAPPDATA%/Microsoft/Windows/INetCache",
            Category::CacheTemp,
        ),
        ("%USERPROFILE%/.nuget/packages", Category::CodeDev),
        ("%USERPROFILE%/Downloads", Category::Other),
        ("/var/cache/apt", Category::CacheTemp),
        ("/var/lib/docker", Category::AppsSystem),
        ("/var/lib/libvirt/images", Category::Archives),
        ("/usr", Category::AppsSystem),
        ("/tmp", Category::CacheTemp),
        ("~/.cache", Category::CacheTemp),
        ("~/.cargo/registry", Category::CodeDev),
        ("~/.local/share/Trash", Category::Other),
        ("~/.npm", Category::CacheTemp),
        ("~/VirtualBox VMs", Category::Archives),
        ("**/node_modules", Category::CodeDev),
        ("**/.git", Category::CodeDev),
        ("**/__pycache__", Category::CacheTemp),
        ("**/$Recycle.Bin", Category::AppsSystem),
        ("~/.m2/repository", Category::CodeDev),
        ("~/.wine", Category::AppsSystem),
    ];
    for (pattern, category) in spot {
        let entry = recognition_entries()
            .iter()
            .find(|e| e.pattern == pattern)
            .unwrap_or_else(|| panic!("no entry for {pattern}"));
        assert_eq!(entry.category, category, "{pattern}");
    }
}

#[test]
fn the_cache_and_temp_entries_are_exactly_these() {
    // Adding a place to this color has to be a deliberate act.
    let mut cache: Vec<&str> = recognition_entries()
        .iter()
        .filter(|e| e.category == Category::CacheTemp)
        .map(|e| e.pattern)
        .collect();
    cache.sort_unstable();
    let mut expected = vec![
        "%LOCALAPPDATA%/D3DSCache",
        "%LOCALAPPDATA%/Google/Chrome/User Data/*/Cache",
        "%LOCALAPPDATA%/Microsoft/Edge/User Data/*/Cache",
        "%LOCALAPPDATA%/Microsoft/Windows/Explorer",
        "%LOCALAPPDATA%/Microsoft/Windows/INetCache",
        "%LOCALAPPDATA%/Mozilla/Firefox/Profiles/*/cache2",
        "%LOCALAPPDATA%/Temp",
        "%LOCALAPPDATA%/npm-cache",
        "%LOCALAPPDATA%/pip/Cache",
        "%ProgramData%/Package Cache",
        "%SystemRoot%/Prefetch",
        "%SystemRoot%/Temp",
        "**/__pycache__",
        "/tmp",
        "/var/cache",
        "/var/cache/apt",
        "/var/cache/dnf",
        "/var/cache/pacman/pkg",
        "/var/cache/zypp",
        "/var/tmp",
        "~/.cache",
        "~/.npm",
    ];
    expected.sort_unstable();
    assert_eq!(cache, expected);
}

#[test]
fn folders_never_claim_a_media_category() {
    for e in recognition_entries() {
        assert!(
            !matches!(
                e.category,
                Category::Video | Category::Images | Category::Audio | Category::Documents
            ),
            "{} claims {:?}",
            e.pattern,
            e.category
        );
    }
}

#[test]
fn handoffs_belong_to_their_system() {
    for e in recognition_entries() {
        match e.handoff {
            Some(Handoff::Uninstall | Handoff::DiskCleanup) => {
                assert_eq!(e.os, Os::Windows, "{}", e.pattern);
            }
            Some(Handoff::Package) => assert_eq!(e.os, Os::Linux, "{}", e.pattern),
            _ => {}
        }
    }
    let entry = |pattern: &str| {
        recognition_entries()
            .iter()
            .find(|e| e.pattern == pattern)
            .unwrap()
    };
    assert_eq!(
        entry("%SystemDrive%/Windows.old").handoff,
        Some(Handoff::DiskCleanup)
    );
    assert_eq!(entry("%ProgramFiles%/*").handoff, Some(Handoff::Uninstall));
    assert_eq!(entry("/usr").handoff, Some(Handoff::Package));
    assert_eq!(entry("~/.cache").handoff, None);
}

#[test]
fn protected_entries_have_notes_and_blocked_ones_may_not_need_them() {
    for e in protected_entries() {
        if e.level == Level::Protected {
            assert!(e.note.is_some_and(|n| !n.is_empty()), "{}", e.pattern);
        }
        assert_ne!(e.level, Level::Normal);
    }
}

// ------------------------------------------------------- the blocked list

const BLOCKED_WINDOWS: [&str; 21] = [
    r"C:\Windows",
    r"C:\Windows\System32\config\SAM",
    r"C:\Program Files",
    r"C:\Program Files (x86)",
    r"C:\Program Files (Arm)",
    r"C:\System Volume Information",
    r"D:\System Volume Information\x",
    // The boot loader and recovery, wherever they sit on a drive.
    r"C:\Boot",
    r"C:\Boot\BCD",
    r"c:\BOOT\en-US\bootmgr.exe.mui",
    r"C:\EFI",
    r"D:\EFI\Microsoft\Boot\bootmgfw.efi",
    r"C:\Recovery",
    r"C:\Recovery\WindowsRE\Winre.wim",
    r"E:\Recovery",
    r"C:\bootmgr",
    r"d:\BootMgr",
    r"C:\",
    r"d:\",
    "C:",
    "E:/",
];

const BLOCKED_LINUX: [&str; 22] = [
    "/",
    "/usr",
    "/usr/share/x",
    "/boot",
    // The boot loader partition, wherever the system mounts it.
    "/efi",
    "/efi/EFI",
    "/efi/EFI/systemd",
    "/efi/loader/entries",
    "/boot/efi/EFI",
    "/etc/passwd",
    "/bin",
    "/bin/sh",
    "/sbin",
    "/lib",
    "/lib64",
    "/lib32/x",
    "/var/lib/dpkg/status",
    "/proc/1/mem",
    "/sys/kernel",
    "/dev/null",
    "/proc/self/root/usr",
    "/mnt/c/Windows/System32",
];

#[test]
fn every_listed_os_root_is_blocked_with_and_without_an_environment() {
    for k in [full(), bare()] {
        for path in BLOCKED_WINDOWS.iter().chain(BLOCKED_LINUX.iter()) {
            assert_eq!(level(k, path), Level::Blocked, "{path}");
        }
    }
    for k in [linux_full(), linux_bare()] {
        for path in BLOCKED_LINUX {
            assert_eq!(level(k, path), Level::Blocked, "{path}");
        }
    }
}

#[test]
fn names_that_only_look_like_the_boot_places_are_not_blocked() {
    for k in [full(), bare()] {
        for path in [
            r"C:\Boot2",
            r"C:\Boots\x",
            r"C:\EFIs",
            r"C:\Recovery.txt",
            r"C:\bootmgr.bak",
            r"C:\bootmgr\x",
            r"C:\data\Boot",
            r"C:\data\Recovery\x",
        ] {
            assert!(level(k, path) < Level::Blocked, "{path}");
        }
    }
    for k in [linux_full(), linux_bare()] {
        for path in ["/efi-backup", "/efis/x", "/home/a/efi", "/srv/efi/EFI"] {
            assert!(level(k, path) < Level::Blocked, "{path}");
        }
    }
}

#[test]
fn the_blocked_roots_are_in_the_table() {
    let blocked: HashSet<&str> = protected_entries()
        .iter()
        .filter(|e| e.level == Level::Blocked)
        .map(|e| e.pattern)
        .collect();
    let required = [
        // Windows
        "C:/Windows",
        "C:/Program Files",
        "C:/Program Files (x86)",
        "C:/Program Files (Arm)",
        "%ProgramFiles(Arm)%",
        "?:/System Volume Information",
        "?:",
        "?:/Boot",
        "?:/EFI",
        "?:/Recovery",
        "?:/bootmgr",
        // Linux
        "/",
        "/usr",
        "/boot",
        "/efi",
        "/etc",
        "/bin",
        "/sbin",
        "/lib*",
        "/var/lib/dpkg",
        "/proc",
        "/sys",
        "/dev",
    ];
    for pattern in required {
        assert!(
            blocked.contains(pattern),
            "{pattern} is not blocked in the table"
        );
    }
}

#[test]
fn the_roots_themselves_are_exact_and_the_system_folders_are_subtrees() {
    let entry = |pattern: &str| {
        protected_entries()
            .iter()
            .find(|e| e.pattern == pattern)
            .unwrap()
    };
    assert!(entry("/").exact && entry("?:").exact);
    assert!(entry("C:/Program Files").exact);
    // The boot manager is one file; the other boot and recovery places are
    // folders and blocked with everything in them.
    assert!(entry("?:/bootmgr").exact);
    for pattern in [
        "/usr",
        "/etc",
        "/boot",
        "/efi",
        "/proc",
        "C:/Windows",
        "?:/Boot",
        "?:/EFI",
        "?:/Recovery",
    ] {
        assert!(!entry(pattern).exact, "{pattern}");
    }
}

#[test]
fn every_protected_entry_works_end_to_end() {
    // Turn each pattern into a concrete path and ask for it.
    for (name, k) in [("full", full())] {
        for entry in protected_entries() {
            for path in concrete_paths(entry.pattern, entry.os) {
                let verdict = k.protection(&path);
                assert!(
                    verdict.level >= entry.level,
                    "{name}: {path:?} (from {}) is {:?}, wanted {:?}",
                    entry.pattern,
                    verdict.level,
                    entry.level
                );
                if entry.level == Level::Protected {
                    assert!(verdict.entry.is_some_and(|e| e.note.is_some()), "{path:?}");
                }
            }
        }
    }
}

/// Instances of a pattern as real paths, for the environment of `full_env`.
fn concrete_paths(pattern: &str, os: Os) -> Vec<String> {
    let fill = |text: &str| text.replace("**/", "").replace('*', "x").replace('?', "c");
    let mut out = Vec::new();
    let windows_ok = matches!(os, Os::Windows | Os::Any);
    let linux_ok = matches!(os, Os::Linux | Os::Any);
    if let Some(rest) = pattern.strip_prefix('~') {
        if windows_ok {
            out.push(format!(r"C:\Users\alice{}", fill(&rest.replace('/', "\\"))));
        }
        if linux_ok {
            out.push(format!("/home/alice{}", fill(rest)));
        }
    } else if let Some(rest) = pattern.strip_prefix('%') {
        let (name, tail) = rest.split_once('%').unwrap();
        let value = match name {
            "SystemRoot" => r"C:\Windows",
            "SystemDrive" => "C:",
            "ProgramFiles" | "ProgramW6432" => r"C:\Program Files",
            "ProgramFiles(x86)" => r"C:\Program Files (x86)",
            "ProgramFiles(Arm)" => r"C:\Program Files (Arm)",
            "ProgramData" => r"C:\ProgramData",
            "USERPROFILE" => r"C:\Users\alice",
            "LOCALAPPDATA" => r"C:\Users\alice\AppData\Local",
            "APPDATA" => r"C:\Users\alice\AppData\Roaming",
            other => panic!("test does not know %{other}%"),
        };
        out.push(format!("{value}{}", fill(&tail.replace('/', "\\"))));
    } else if pattern.starts_with('/') {
        out.push(fill(pattern));
    } else if pattern.starts_with("**/") {
        out.push(format!("/home/alice/project/{}", fill(pattern)));
    } else {
        // Windows literal: `C:/Windows`, `?:/Users/*`.
        out.push(fill(pattern).replace('/', "\\"));
    }
    out
}

// ------------------------------------------------- dressed-up and bad paths

#[test]
fn blocked_roots_dressed_up_are_still_blocked() {
    let dressed = [
        // Windows spellings
        r"C:\Windows\.",
        r"C:\Windows\",
        r"C:\Windows\\",
        r"c:\WINDOWS",
        r"C:/Windows",
        r"C:\Windows\System32\..",
        r"C:\Windows\..\Windows",
        r"C:\Users\..\Windows",
        r"\\?\C:\Windows",
        r"\\?\c:\windows\system32",
        r"\\.\C:\Windows",
        r"\??\C:\Windows",
        r"//?/C:/Windows",
        r"C:\Windows::$DATA",
        r"C:\Windows:stream",
        r"C:\Windows.",
        r"C:\Windows ",
        r"C:\Windows. . .",
        r"C:\Windows\...\System32",
        r"\Windows",
        r"\Windows\System32",
        r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\Windows",
        r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\",
        r"C:\PROGRA~1",
        r"C:\PROGRA~2\Foo",
        r"c:\progra~1",
        r"C:\WINDOW~1\x",
        r"C:\Program Files\.",
        r"c:\program files (x86)\",
        r"D:\SYSTEM VOLUME INFORMATION",
        // Windows kind: drive or share roots
        r"C:\.",
        r"C:\Users\..",
        r"\\server\share",
        r"\\server\share\",
        r"\\?\UNC\server\share",
    ];
    for k in [full(), bare()] {
        for path in dressed {
            assert_eq!(level(k, path), Level::Blocked, "{path:?}");
        }
    }
}

#[test]
fn linux_roots_dressed_up_are_still_blocked() {
    let dressed = [
        "//usr",
        "///usr",
        "/usr/",
        "/usr/.",
        "/./usr",
        "/usr//lib",
        "/./usr/./lib/.",
        // `..` at the root stays at the root.
        "/..",
        "/../..",
        "/../usr",
        "/./../etc/passwd",
        "/.",
        "//",
        "/proc/self/cwd/x",
        "/sys/./kernel",
        "/dev/./null",
    ];
    for k in [full(), bare(), linux_full(), linux_bare()] {
        for path in dressed {
            assert_eq!(level(k, path), Level::Blocked, "{path:?}");
        }
    }
}

#[test]
fn a_linux_dotdot_after_a_named_part_is_blocked_because_a_link_may_change_it() {
    // `l -> usr/lib` makes `/home/alice/l/../bin` the folder `/usr/bin`, which
    // no amount of text work can see. Some of these would be harmless if the
    // parts were plain folders; all of them are refused.
    let dotdot = [
        "/usr/../usr",
        "/home/../usr",
        "/usr/lib/../../etc",
        "/tmp/../etc/passwd",
        "/dev/../dev",
        "/home/alice/l/../bin",
        "/home/alice/Documents/..",
        "/home/alice/Documents/../Pictures/x.png",
        "/a/b/../../..",
        "/var/../var/lib/docker",
        "/home/alice/./x/../y",
    ];
    for k in [full(), bare(), linux_full(), linux_bare()] {
        for path in dotdot {
            let verdict = k.protection(path);
            assert_eq!(verdict.level, Level::Blocked, "{path:?} ({:?})", k.host());
            assert_eq!(verdict.reason, Reason::Unresolvable, "{path:?}");
        }
    }
    // Without a `..` the same paths are judged for what they are.
    assert_eq!(level(linux_full(), "/home/alice/bin"), Level::Normal);
    assert_eq!(level(linux_full(), "/home/alice"), Level::Protected);
}

#[test]
fn dotdot_at_the_root_of_a_device_path_is_blocked() {
    // Win32 normalizes every device path except one that starts with exactly
    // `\\?\`, and the root `..` cannot pass is the prefix, so
    // `\\.\D:\..\C:\Windows` is `\\.\C:\Windows`.
    let blocked = [
        r"\\.\D:\..\C:\Windows",
        r"\\.\D:\..\..\C:\Windows",
        r"\\.\D:\..\C:",
        r"\\.\D:\..\C:\Program Files",
        r"\\.\Volume{4d1c2e00-0000-0000-0000-100000000000}\..\C:\Windows",
        r"\\.\D:\..\UNC\localhost\C$\Windows",
        // Other spellings of the prefix are normalized too.
        r"//./D:/../C:/Windows",
        r"\\.\d:\..\c:\windows",
        r"\\.\D:\Data\..\..\C:\Windows",
        r"//?/D:/../C:/Windows",
        r"\??\D:\..\C:\Windows",
        r"\\.\D:\..",
    ];
    for k in [full(), bare(), roots()] {
        for path in blocked {
            let verdict = k.protection(path);
            assert_eq!(verdict.level, Level::Blocked, "{path:?}");
            assert_eq!(verdict.reason, Reason::Unresolvable, "{path:?}");
        }
    }
    // A `..` below the drive is settled by the text, as before.
    assert_eq!(level(full(), r"\\.\C:\Users\..\Windows"), Level::Blocked);
    assert_eq!(level(full(), r"\\.\C:\Windows\..\Data\x"), Level::Normal);
    assert_eq!(level(full(), r"\\.\D:\Data\..\x"), Level::Normal);
}

#[test]
fn windows_dotdot_is_still_settled_by_the_text() {
    // Win32 settles `..` before it looks at the disk, so on Windows the text is
    // the truth.
    for (path, expected) in [
        (r"C:\Users\alice\Documents\..\x", Level::Normal),
        (r"C:\Users\alice\..\..\Windows", Level::Blocked),
        (r"C:\Users\alice\Documents\..", Level::Protected),
        (r"C:\Program Files\Foo\..", Level::Blocked),
    ] {
        assert_eq!(level(full(), path), expected, "{path}");
    }
}

#[test]
fn paths_that_say_nothing_are_blocked() {
    let nothing: [&[u8]; 14] = [
        b"",
        b"usr",
        b"relative/path",
        b"./x",
        b"../x",
        b"~",
        b"~/x",
        b"C:Windows",
        b"C:",
        b"\\\\.\\PhysicalDrive0",
        b"\\\\?\\GLOBALROOT\\Device\\HarddiskVolume1\\Windows",
        b"/usr\0/x",
        b"C:\\Windows\0.txt",
        b"\0",
    ];
    for path in nothing {
        let verdict = full().protection_bytes(path);
        assert_eq!(
            verdict.level,
            Level::Blocked,
            "{:?}",
            String::from_utf8_lossy(path)
        );
    }
    // "C:" is a drive root once spelled out; everything else here is unresolvable.
    assert_eq!(full().protection_bytes(b"").reason, Reason::Unresolvable);
    assert_eq!(full().protection_bytes(b"usr").reason, Reason::Unresolvable);
    assert_eq!(
        full().protection_bytes(b"C:\\PROGRA~1").reason,
        Reason::Alias
    );
    assert_eq!(
        full().protection_bytes(b"\\\\server\\share").reason,
        Reason::Root
    );
    assert_eq!(full().protection_bytes(b"/").reason, Reason::Table);
}

#[test]
fn oversize_paths_are_refused_quickly() {
    let started = Instant::now();
    let huge = format!("/usr/{}", "a/".repeat(200_000));
    assert_eq!(level(full(), &huge), Level::Blocked);
    let huge_windows = format!(r"C:\Users\{}", r"a\".repeat(200_000));
    assert_eq!(level(full(), &huge_windows), Level::Blocked);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn non_utf8_names_are_handled() {
    let mut path = b"/home/alice/".to_vec();
    path.extend_from_slice(&[0xff, 0xfe, b'x']);
    assert_eq!(full().protection_bytes(&path).level, Level::Normal);
    let mut under_usr = b"/usr/".to_vec();
    under_usr.extend_from_slice(&[0xff, 0xfe]);
    assert_eq!(full().protection_bytes(&under_usr).level, Level::Blocked);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let os = std::ffi::OsStr::from_bytes(&under_usr);
        assert_eq!(
            full().protection(std::path::Path::new(os)).level,
            Level::Blocked
        );
    }
}

#[test]
fn near_misses_are_not_blocked() {
    let fine = [
        "/usrx",
        "/usr2/lib",
        "/etc2",
        "/boots/x",
        "/var/lib/dpkg2",
        "/var/lib/dpkgx/y",
        "/dev2",
        "/proc2",
        "/sysx",
        "/home/alice/usr",
        "/home/alice/file.txt",
        "/var/log/syslog",
        "/opt/vendor/app",
        "/mnt/data/x",
        "/mnt/cc/x",
        r"C:\Windowsx",
        r"C:\Windows2\x",
        r"C:\Users\alice\Documents\x.txt",
        r"C:\Program Files\Foo\bar.dll",
        r"C:\Program Filesx",
        r"C:\Data\System Volume Informations",
        r"C:\a~b\x",
        r"D:\Games\Windows\x",
    ];
    for path in fine {
        assert_eq!(level(full(), path), Level::Normal, "{path}");
    }
}

#[test]
fn protected_paths_need_the_stronger_confirmation() {
    let cases = [
        ("/var/lib/docker", "Docker"),
        ("/var/lib/docker/volumes/x/_data", "Docker"),
        ("/var/lib/docker/", "Docker"),
        ("/var/./lib/docker", "Docker"),
        ("/var/lib/snapd/snaps", "snapd"),
        ("/home/alice", "Home folder"),
        ("/var", "Filesystem Hierarchy"),
        ("/root", "root user"),
        (r"C:\Program Files\Foo", "installed program"),
        (r"C:\Program Files (x86)\Foo", "installed 32-bit program"),
        (r"C:\Program Files\WindowsApps\x\y", "Microsoft Store"),
        (r"C:\Users\alice", "profile folder"),
        (r"C:\Users\bob", "user profile"),
        (r"C:\Users", "user profiles"),
        (r"C:\Windows.old", "Previous Windows"),
        (r"C:\Windows.old\Users\x", "Previous Windows"),
        (r"c:\WINDOWS.OLD", "Previous Windows"),
        (r"C:\pagefile.sys", "Paging file"),
        (r"C:\ProgramData", "shared by all users"),
        (r"C:\Users\alice\AppData\Local", "application data"),
    ];
    for (path, word) in cases {
        let verdict = full().protection(path);
        assert_eq!(verdict.level, Level::Protected, "{path}");
        assert_eq!(verdict.reason, Reason::Table, "{path}");
        let note = verdict.entry.and_then(|e| e.note).unwrap_or("");
        assert!(note.contains(word), "{path}: {note:?} lacks {word:?}");
    }
    // Their contents are ordinary unless the entry covers the subtree.
    for path in [
        "/home/alice/Documents/x",
        "/var/log/syslog",
        r"C:\Program Files\Foo\bar.dll",
        r"C:\Users\alice\Desktop\x.txt",
    ] {
        assert_eq!(level(full(), path), Level::Normal, "{path}");
    }
}

#[test]
fn the_current_users_home_is_protected_through_the_environment() {
    let env = Environment::new().with("HOME", "/srv/people/carol");
    let k = Knowledge::new(&env);
    assert_eq!(level(&k, "/srv/people/carol"), Level::Protected);
    assert_eq!(level(&k, "/srv/people/carol/notes.txt"), Level::Normal);
    // Without the environment the same folder is unknown.
    assert_eq!(level(bare(), "/srv/people/carol"), Level::Normal);
}

#[test]
fn the_environment_is_the_only_input() {
    // An environment pointing elsewhere moves the entries with it; a variable
    // that is not set switches its entries off but never the literal ones.
    let env = Environment::new()
        .with("SystemRoot", r"D:\WinNT")
        .with("ProgramFiles", r"E:\Apps");
    let k = Knowledge::for_host(&env, Host::Windows);
    assert_eq!(level(&k, r"D:\WinNT\System32"), Level::Blocked);
    assert_eq!(level(&k, r"E:\Apps"), Level::Blocked);
    assert_eq!(level(&k, r"E:\Apps\Tool"), Level::Protected);
    assert_eq!(level(&k, r"C:\Windows"), Level::Blocked);
    // A value that is not a drive path is ignored, not trusted.
    let odd = Environment::new()
        .with("SystemRoot", "relative")
        .with("HOME", "relative");
    let k = Knowledge::for_host(&odd, Host::Windows);
    assert_eq!(level(&k, r"C:\Windows"), Level::Blocked);
    assert_eq!(level(&k, "/usr"), Level::Blocked);
}

#[test]
fn a_user_profile_variable_names_the_users_folder_on_windows() {
    let k = full();
    assert_eq!(level(k, r"C:\Users\alice"), Level::Protected);
    let rec = k.recognize(std::path::Path::new(r"C:\Users\alice\Downloads"));
    assert!(rec.is_some_and(|e| e.pattern == "%USERPROFILE%/Downloads"));
}

#[test]
fn from_lookup_asks_only_for_the_names_the_tables_use() {
    let mut asked = Vec::new();
    let env = Environment::from_lookup(|name| {
        asked.push(name.to_owned());
        (name == "HOME").then(|| std::ffi::OsString::from("/home/dana"))
    });
    let mut expected: Vec<String> = Environment::variable_names()
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    expected.sort();
    asked.sort();
    assert_eq!(asked, expected);
    assert_eq!(level(&Knowledge::new(&env), "/home/dana"), Level::Protected);
}

// ------------------------------------- other names for places on this machine

#[test]
fn loopback_and_admin_shares_are_judged_as_the_local_place() {
    let blocked = [
        // Loopback and admin-share spellings of the Windows folders.
        r"\\localhost\C$\Windows",
        r"\\127.0.0.1\c$\Program Files",
        r"\\?\UNC\localhost\C$\Windows",
        r"\\.\UNC\localhost\C$\Windows",
        r"\\localhost\ADMIN$\System32",
        // The same through any host name, in every form.
        r"\\LOCALHOST\c$\windows\system32\config\SAM",
        r"\\fileserver\C$\Windows",
        r"\\pc-1.corp.example.com\d$\System Volume Information",
        r"\\fe80--1.ipv6-literal.net\C$\Windows\System32",
        r"\\?\UNC\fileserver\C$\Program Files (x86)",
        r"\\.\UNC\fileserver\C$\Program Files (Arm)",
        r"\??\UNC\fileserver\C$\Windows",
        r"//localhost/C$/Windows",
        r"\\localhost\C$\Windows\.",
        r"\\localhost\C$\Users\..\Windows",
        // `..` cannot climb out of the share, so it cannot be left to the text.
        r"\\localhost\C$\..\Windows\System32",
        r"\\localhost\C$\..\..\Windows\System32",
        r"\\?\UNC\localhost\C$\..\Windows\System32",
        r"\\localhost\C$.\Windows::$DATA",
        r"\\localhost\c$\WINDOWS ",
        // Roots.
        r"\\localhost\C$",
        r"\\localhost\C$\",
        r"\\?\UNC\localhost\d$",
        // The Windows folder through ADMIN$.
        r"\\localhost\ADMIN$",
        r"\\fileserver\admin$\Temp",
        r"\\?\UNC\fileserver\ADMIN$\System32",
        r"\\.\UNC\fileserver\Admin$\System32\drivers",
        // A short name below the share.
        r"\\localhost\C$\PROGRA~1",
        r"\\fileserver\share\WINDOW~1\System32",
    ];
    let protected = [
        r"\\localhost\C$\Users\alice",
        r"\\fileserver\c$\Users\bob",
        r"\\localhost\C$\Program Files\Foo",
        r"\\?\UNC\localhost\C$\Windows.old",
        r"\\localhost\C$\pagefile.sys",
    ];
    for k in [full(), bare(), linux_full(), linux_bare()] {
        for path in blocked {
            // On a Linux host only the paths that start with `/` get this far.
            if k.host() == Host::Linux && !path.starts_with('/') {
                continue;
            }
            assert_eq!(level(k, path), Level::Blocked, "{path:?} ({:?})", k.host());
        }
    }
    for k in [full(), bare()] {
        for path in protected {
            assert_eq!(level(k, path), Level::Protected, "{path:?}");
        }
    }
    // The place itself decides: ordinary folders stay ordinary.
    for path in [
        r"\\localhost\C$\Users\alice\Documents\x.txt",
        r"\\localhost\C$\Data\x",
        r"\\fileserver\share\Windows\System32",
        r"\\fileserver\D$\Games\Windows\x",
        r"\\fileserver\ADMIN\Temp\x",
        r"\\fileserver\CC$\Windows\x",
    ] {
        assert_eq!(level(full(), path), Level::Normal, "{path:?}");
    }
}

#[test]
fn admin_share_follows_the_windows_folder_of_the_environment() {
    let env = Environment::new().with("SystemRoot", r"D:\WinNT");
    let k = Knowledge::for_host(&env, Host::Windows);
    assert_eq!(
        k.system_root,
        Some(vec![b"d:".to_vec(), b"winnt".to_vec()]),
        "the Windows folder is kept as parts"
    );
    assert_eq!(level(&k, r"\\h\ADMIN$\System32"), Level::Blocked);
    assert_eq!(level(&k, r"\\h\ADMIN$"), Level::Blocked);
    // A value that is not a drive path is not kept.
    for odd in ["relative", r"\\server\share", r"\Windows", "/windows"] {
        let env = Environment::new().with("SystemRoot", odd);
        assert_eq!(Knowledge::for_host(&env, Host::Windows).system_root, None);
    }
}

#[test]
fn wsl_shares_are_judged_as_the_linux_path() {
    let blocked = [
        r"\\wsl$\Ubuntu\usr",
        r"\\wsl.localhost\Ubuntu\etc",
        r"\\wsl$\Ubuntu\usr\bin\ls",
        r"\\WSL$\Debian\boot",
        r"\\wsl.localhost\Ubuntu-22.04\var\lib\dpkg\status",
        r"\\?\UNC\wsl$\Ubuntu\usr",
        r"\\.\UNC\wsl.localhost\Ubuntu\etc\passwd",
        r"\\wsl$\Ubuntu\lib64",
        r"\\wsl$\Ubuntu\proc\1",
        r"\\wsl$\Ubuntu",
        r"//wsl$/Ubuntu/usr",
        // A drive of Windows seen from inside the distribution.
        r"\\wsl$\Ubuntu\mnt\c\Windows",
        r"\\wsl$\Ubuntu\mnt\c\windows\System32",
        // A `..` follows the distribution's links.
        r"\\wsl$\Ubuntu\home\alice\l\..\bin",
        r"\\wsl.localhost\Ubuntu\tmp\x\..\y",
    ];
    for k in [full(), bare(), linux_full(), linux_bare()] {
        for path in blocked {
            if k.host() == Host::Linux && !path.starts_with('/') {
                continue;
            }
            assert_eq!(level(k, path), Level::Blocked, "{path:?} ({:?})", k.host());
        }
    }
    let k = full();
    assert_eq!(level(k, r"\\wsl$\Ubuntu\home\alice"), Level::Protected);
    assert_eq!(level(k, r"\\wsl$\Ubuntu\var\lib\docker"), Level::Protected);
    assert_eq!(
        level(k, r"\\wsl$\Ubuntu\home\alice\notes.txt"),
        Level::Normal
    );
    // The distribution tells letter case apart, so `Usr` is another folder.
    assert_eq!(level(k, r"\\wsl$\Ubuntu\Usr\x"), Level::Normal);
    assert_eq!(level(k, r"\\wsl$\Ubuntu\tmp\x"), Level::Normal);
}

#[test]
fn admin_share_spellings_are_never_looser_than_the_drive_path() {
    let hosts = ["localhost", "fileserver", "10.0.0.7", "WSL"];
    let rests = [
        "",
        r"Windows",
        r"Windows\System32",
        r"Program Files",
        r"Program Files\Foo",
        r"Users",
        r"Users\alice",
        r"Users\alice\Documents",
        r"System Volume Information",
        r"Windows.old\x",
        r"ProgramData",
        r"Data\Windows",
    ];
    for host in hosts {
        for rest in rests {
            let drive = if rest.is_empty() {
                "C:\\".to_owned()
            } else {
                format!(r"C:\{rest}")
            };
            let canonical = level(full(), &drive);
            let tail = if rest.is_empty() {
                String::new()
            } else {
                format!(r"\{rest}")
            };
            for form in [
                format!(r"\\{host}\C${tail}"),
                format!(r"\\?\UNC\{host}\c${tail}"),
                format!(r"\\.\UNC\{host}\C${tail}"),
                format!("//{host}/C${}", tail.replace('\\', "/")),
            ] {
                assert!(level(full(), &form) >= canonical, "{form:?} vs {drive:?}");
            }
            // `ADMIN$` is the Windows folder.
            let admin = if tail.is_empty() {
                r"C:\Windows".to_owned()
            } else {
                format!(r"C:\Windows{tail}")
            };
            for form in [
                format!(r"\\{host}\ADMIN${tail}"),
                format!(r"\\?\UNC\{host}\admin${tail}"),
                format!(r"\\.\UNC\{host}\Admin${tail}"),
            ] {
                assert!(
                    level(full(), &form) >= level(full(), &admin),
                    "{form:?} vs {admin:?}"
                );
            }
        }
    }
}

// ---------------------------------------- relative paths on a Linux host

#[test]
fn on_a_linux_host_a_path_without_a_leading_slash_is_unresolvable() {
    // Relative to the working directory, so `C:/../usr` may well be `/usr`.
    let relative = [
        "C:/../usr",
        r"\..\usr",
        "C:/../etc/passwd",
        r"C:\Windows",
        r"c:\Users\alice\Documents\x.txt",
        "C:",
        "usr",
        "relative/path",
        "./x",
        "../x",
        "~",
        "~/x",
        r"x\y",
        r"\\localhost\C$\Windows",
        r"\\?\C:\Windows",
        r"\Users\alice",
        "home/alice/x",
        " /usr",
    ];
    for k in [linux_full(), linux_bare()] {
        for path in relative {
            let verdict = k.protection(path);
            assert_eq!(verdict.level, Level::Blocked, "{path:?}");
            assert_eq!(verdict.reason, Reason::Unresolvable, "{path:?}");
        }
        assert_eq!(k.strictest(["/home/alice/a", "C:/../usr"]), Level::Blocked);
    }
    // The same text on Windows is a drive path, and is judged as one.
    assert_eq!(level(full(), "C:/../usr"), Level::Normal);
    assert_eq!(level(full(), r"\..\usr"), Level::Normal);
    assert_eq!(level(full(), r"C:\Windows"), Level::Blocked);
}

#[cfg(unix)]
#[test]
fn a_unix_build_reads_paths_the_unix_way_by_default() {
    assert_eq!(Host::native(), Host::Linux);
    let k = Knowledge::new(&full_env());
    assert_eq!(k.host(), Host::Linux);
    for path in ["C:/../usr", r"\..\usr", "C:/../etc/passwd"] {
        let verdict = k.protection(path);
        assert_eq!(verdict.level, Level::Blocked, "{path:?}");
        assert_eq!(verdict.reason, Reason::Unresolvable, "{path:?}");
        // The free function uses the same default.
        assert_eq!(protection(path).level, Level::Blocked, "{path:?}");
    }
    assert_eq!(strictest(["/home/alice/a", r"\..\usr"]), Level::Blocked);
}

#[cfg(windows)]
#[test]
fn a_windows_build_reads_paths_the_windows_way_by_default() {
    assert_eq!(Host::native(), Host::Windows);
    let k = Knowledge::new(&full_env());
    assert_eq!(k.host(), Host::Windows);
    assert_eq!(k.protection(r"C:\Windows\System32").level, Level::Blocked);
    assert_eq!(
        k.protection(r"C:\Users\alice\Documents").level,
        Level::Normal
    );
}

// ----------------------------------------------- short names, other roots

#[test]
fn short_names_are_blocked_for_every_root_only_windows_reads() {
    let aliases = [
        // Volume paths and a path rooted at the current drive.
        r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\WINDOW~1",
        r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\PROGRA~1",
        r"\WINDOW~1\System32",
        // More of the same kinds.
        r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\Users\ALICEJ~1\x",
        r"\PROGRA~1",
        r"\Users\ALICEJ~1",
        "/WINDOW~1/System32",
        r"\\host\share\PROGRA~1",
        r"\\?\UNC\host\share\Dir\WINDOW~1",
        r"\\.\UNC\host\C$\PROGRA~1",
        r"//host/share/WINDOW~1",
        r"\\.\C:\PROGRA~1",
        r"C:\PROGRA~1",
    ];
    for path in aliases {
        for k in [full(), bare()] {
            let verdict = k.protection(path);
            assert_eq!(verdict.level, Level::Blocked, "{path:?}");
            assert_eq!(verdict.reason, Reason::Alias, "{path:?}");
        }
    }
    // On a Linux host `/x~1` is an ordinary name, unless it is written with a
    // backslash, which only Windows reads as a separator.
    for k in [linux_full(), linux_bare()] {
        assert_eq!(level(k, "/WINDOW~1/System32"), Level::Normal);
        assert_eq!(level(k, "/srv/data~1/x"), Level::Normal);
        assert_eq!(k.protection(r"/WINDOW~1\System32").reason, Reason::Alias);
        assert_eq!(k.protection(r"//host/share/PROGRA~1").reason, Reason::Alias);
    }
    // Ordinary names that merely look alike.
    for path in [
        r"\\?\Volume{4d1c2e00-0000-0000-0000-100000000000}\Users\alice\PROGRA~1",
        r"\Users\alice\PROGRA~1",
        r"\\host\share\a\b\PROGRA~1",
        r"\\host\SHARE~1\x\y",
    ] {
        assert_ne!(full().protection(path).reason, Reason::Alias, "{path:?}");
    }
}

// --------------------------------------------- a Windows drive seen from Linux

#[test]
fn a_windows_drive_under_mnt_matches_with_windows_case_folding() {
    let blocked = [
        "/mnt/c/Windows",
        "/mnt/c/windows",
        "/mnt/c/WINDOWS/System32",
        "/mnt/C/windows/system32/config/SAM",
        "/mnt/c/program files",
        "/mnt/c/PROGRAM FILES (X86)",
        "/mnt/c/Program Files (Arm)",
        "/mnt/c/program files (arm)",
        "/mnt/d/system volume information",
        "/mnt/d/SYSTEM VOLUME INFORMATION/x",
        "/mnt/c/Windows./System32",
        "/mnt/c/Windows::$DATA",
        "/mnt/c/Windows /x",
        "/mnt/c/PROGRA~1",
        "/mnt/c/",
        "/mnt/z",
    ];
    for k in [full(), bare(), linux_full(), linux_bare()] {
        for path in blocked {
            assert_eq!(level(k, path), Level::Blocked, "{path:?} ({:?})", k.host());
        }
        assert_eq!(level(k, "/mnt/c/PROGRAM FILES/Foo"), Level::Protected);
        assert_eq!(level(k, "/mnt/c/windows.OLD"), Level::Protected);
        assert_eq!(level(k, "/mnt/c/Users"), Level::Protected);
        // Ordinary things on the drive, and things that are not a drive.
        assert_eq!(level(k, "/mnt/c/Data/x.txt"), Level::Normal);
        assert_eq!(level(k, "/mnt/c/Users/alice/Documents"), Level::Normal);
        assert_eq!(level(k, "/mnt/data/windows"), Level::Normal);
        assert_eq!(level(k, "/mnt/data/PROGRAM FILES"), Level::Normal);
        assert_eq!(level(k, "/mnt/cd/windows"), Level::Normal);
        // Another folder called `Windows`: Linux tells the case apart there.
        assert_eq!(level(k, "/srv/WINDOWS/x"), Level::Normal);
        assert_eq!(level(k, "/Mnt/c/windows"), Level::Normal);
    }
}

// ------------------------------------------------------- roots of drives

#[test]
fn the_drive_roots_a_caller_passes_are_blocked_as_exact_paths() {
    let roots = [
        "/media/alice/USB",
        "/mnt/data",
        "/run/media/alice/Backup Disk",
        "/",
        r"D:\",
        r"E:\mount\vol",
        "/media/a*b",
        "relative",
        "",
        r"\\server\share",
    ];
    let build = |host| {
        let env = full_env().with_drive_roots(roots);
        Knowledge::for_host(&env, host)
    };
    for k in [build(Host::Windows), build(Host::Linux)] {
        for path in [
            "/media/alice/USB",
            "/media/alice/USB/",
            "/media/alice/./USB",
            "//media//alice/USB",
            "/mnt/data",
            "/run/media/alice/Backup Disk",
        ] {
            let verdict = k.protection(path);
            assert_eq!(verdict.level, Level::Blocked, "{path:?}");
            assert_eq!(verdict.reason, Reason::Table, "{path:?}");
            assert_eq!(
                verdict.entry.and_then(|e| e.note),
                Some("The root of a drive."),
                "{path:?}"
            );
        }
        // Only the root: what is inside is judged as usual.
        for path in [
            "/media/alice/USB/photos",
            "/media/alice/USB/photos/a.jpg",
            "/media/alice/USBX",
            "/media/alice",
            "/mnt/data/projects",
            "/mnt/data2",
            "/Media/alice/USB",
        ] {
            assert_eq!(level(&k, path), Level::Normal, "{path:?}");
        }
        // No wildcards in a mount point.
        assert_eq!(level(&k, "/media/a*b"), Level::Blocked);
        assert_eq!(level(&k, "/media/axb"), Level::Normal);
    }
    // On Windows a mount point is a folder too, and Windows ignores case.
    let windows = build(Host::Windows);
    for path in [r"E:\mount\vol", r"e:\MOUNT\Vol\", r"\\?\E:\mount\vol\."] {
        assert_eq!(level(&windows, path), Level::Blocked, "{path:?}");
    }
    assert_eq!(level(&windows, r"E:\mount\vol\x"), Level::Normal);
    assert_eq!(level(&windows, r"E:\mount"), Level::Normal);
    // Without the roots the table cannot know.
    for k in [full(), linux_full()] {
        assert_eq!(level(k, "/media/alice/USB"), Level::Normal);
        assert_eq!(level(k, "/mnt/data"), Level::Normal);
    }
    // The roots only add to what is blocked.
    let k = build(Host::Linux);
    assert_eq!(level(&k, "/usr"), Level::Blocked);
    assert_eq!(level(&k, "/home/alice"), Level::Protected);
    assert_eq!(k.strictest(["/home/alice/a", "/mnt/data"]), Level::Blocked);
}

#[test]
fn drive_roots_accept_paths_of_any_kind() {
    use std::path::PathBuf;
    let owned = vec![PathBuf::from("/mnt/one"), PathBuf::from("/mnt/two")];
    let env = Environment::new().with_drive_roots(&owned);
    let k = Knowledge::for_host(&env, Host::Linux);
    assert_eq!(level(&k, "/mnt/one"), Level::Blocked);
    assert_eq!(level(&k, "/mnt/two"), Level::Blocked);
    // Called twice, the lists add up.
    let env = Environment::new()
        .with_drive_roots(["/mnt/one"])
        .with_drive_roots(["/mnt/two"]);
    let k = Knowledge::for_host(&env, Host::Linux);
    assert_eq!(level(&k, "/mnt/one"), Level::Blocked);
    assert_eq!(level(&k, "/mnt/two"), Level::Blocked);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = std::ffi::OsStr::from_bytes(b"/media/\xff\xfe disk");
        let env = Environment::new().with_drive_roots([odd]);
        let k = Knowledge::for_host(&env, Host::Linux);
        assert_eq!(
            k.protection_bytes(b"/media/\xff\xfe disk").level,
            Level::Blocked
        );
        assert_eq!(
            k.protection_bytes(b"/media/\xff\xfe disk/x").level,
            Level::Normal
        );
    }
}

// --------------------------------------------------- Windows on Arm

#[test]
fn program_files_arm_is_treated_like_the_other_program_files_roots() {
    // https://learn.microsoft.com/en-us/windows/arm/faq lists
    // `C:\Program Files (Arm)` next to the other two Program Files folders.
    for k in [full(), bare()] {
        for path in [
            r"C:\Program Files (Arm)",
            r"c:\program files (arm)\",
            r"\\?\C:\Program Files (Arm)",
            r"D:\Program Files (Arm)\.",
        ] {
            let want = if path.starts_with('D') {
                // Only the folder on the usual drive is a fixed entry; the
                // variable entry covers a moved one.
                Level::Normal
            } else {
                Level::Blocked
            };
            assert_eq!(level(k, path), want, "{path:?}");
        }
        for path in [
            r"C:\Program Files (Arm)\Foo",
            r"C:\PROGRAM FILES (ARM)\Foo",
            r"C:\Program Files (Arm)\Foo\.",
            r"\\localhost\C$\Program Files (Arm)\Foo",
        ] {
            assert_eq!(level(k, path), Level::Protected, "{path:?}");
        }
        assert_eq!(
            level(k, r"C:\Program Files (Arm)\Foo\bar.dll"),
            Level::Normal
        );
        assert_eq!(level(k, r"C:\Program Files (Arm)x"), Level::Normal);
    }
    let moved = Environment::new().with("ProgramFiles(Arm)", r"D:\Apps Arm");
    let k = Knowledge::for_host(&moved, Host::Windows);
    assert_eq!(level(&k, r"D:\Apps Arm"), Level::Blocked);
    assert_eq!(level(&k, r"D:\Apps Arm\Foo"), Level::Protected);
    assert_eq!(level(&k, r"D:\Apps Arm\Foo\x"), Level::Normal);
    assert_eq!(level(&k, r"C:\Program Files (Arm)"), Level::Blocked);
    let arm = recognition_entries()
        .iter()
        .find(|e| e.pattern == "%ProgramFiles(Arm)%/*")
        .expect("recognized");
    assert_eq!(arm.handoff, Some(Handoff::Uninstall));
}

// ------------------------------------------------------------- multi-select

#[test]
fn the_strictest_path_decides_a_selection() {
    let k = full();
    let none: [&str; 0] = [];
    assert_eq!(k.strictest(none), Level::Normal);
    assert_eq!(
        k.strictest(["/home/alice/a", "/home/alice/b"]),
        Level::Normal
    );
    assert_eq!(
        k.strictest(["/home/alice/a", "/var/lib/docker"]),
        Level::Protected
    );
    assert_eq!(
        k.strictest(["/var/lib/docker", "/home/alice/a"]),
        Level::Protected
    );
    assert_eq!(
        k.strictest(["/home/alice/a", "/var/lib/docker", "/usr/bin/ls"]),
        Level::Blocked
    );
    assert_eq!(k.strictest(["/usr", "/home/alice/a"]), Level::Blocked);
    assert_eq!(k.strictest(["/home/alice/a", ""]), Level::Blocked);
    let verdict = k.strictest_verdict(["/home/alice/a", "/var/lib/docker", "/var/lib/snapd"]);
    assert_eq!(verdict.entry.map(|e| e.pattern), Some("/var/lib/docker"));
    assert_eq!(strictest(["/home/x", "/etc"]), Level::Blocked);
}

proptest! {
    #[test]
    fn a_selection_is_as_strict_as_its_strictest_member(
        picks in proptest::collection::vec(0usize..12, 0..8),
    ) {
        let pool = [
            "/home/alice/a", "/home/alice", "/usr/bin", "/var/lib/docker", "/tmp/x",
            r"C:\Users\alice\x", r"C:\Windows\x", r"C:\Program Files\App", "/opt/vendor",
            "relative", "/var", "/mnt/data",
        ];
        let chosen: Vec<&str> = picks.iter().map(|&i| pool[i]).collect();
        let expected = chosen.iter().map(|p| level(full(), p)).max().unwrap_or(Level::Normal);
        prop_assert_eq!(full().strictest(chosen.iter()), expected);
        let mut reversed = chosen.clone();
        reversed.reverse();
        prop_assert_eq!(full().strictest(reversed.iter()), expected);
    }
}

// ------------------------------------------------------------- classifying

#[test]
fn extensions_give_categories_whatever_the_case() {
    let k = bare();
    let cases = [
        ("/home/a/movie.MKV", Category::Video),
        ("/home/a/movie.mp4", Category::Video),
        ("C:\\Users\\a\\photo.JpG", Category::Images),
        ("/x/song.flac", Category::Audio),
        ("/x/report.PDF", Category::Documents),
        ("/x/backup.tar.gz", Category::Archives),
        ("/x/disk.iso", Category::Archives),
        ("/x/setup.exe", Category::AppsSystem),
        ("/x/main.rs", Category::CodeDev),
        ("/x/download.crdownload", Category::CacheTemp),
        ("/x/thing.unknownext", Category::Other),
        ("/x/README", Category::Other),
        ("/x/.bashrc", Category::Other),
        ("/x/name.", Category::Other),
        ("/x/a.b.c.mp3", Category::Audio),
        ("/x/dir.mp4/inner", Category::Other),
        ("song.mp3", Category::Audio),
        (r"sub\song.mp3", Category::Audio),
    ];
    for (path, expected) in cases {
        assert_eq!(
            k.classify(std::path::Path::new(path), false),
            expected,
            "{path}"
        );
    }
    // A very long extension is not looked up.
    let long = format!("/x/a.{}", "z".repeat(40));
    assert_eq!(
        k.classify(std::path::Path::new(&long), false),
        Category::Other
    );
    // A folder has no extension.
    assert_eq!(
        k.classify(std::path::Path::new("/x/movies.mp4"), true),
        Category::Other
    );
}

#[test]
fn places_win_over_extensions_and_specific_places_over_broad_ones() {
    let k = full();
    let cases = [
        (
            "/home/alice/.cache/app/readme.pdf",
            false,
            Category::CacheTemp,
        ),
        (
            "/home/alice/proj/node_modules/pkg/logo.png",
            false,
            Category::CodeDev,
        ),
        ("/home/alice/proj/.git", true, Category::CodeDev),
        (
            "/home/alice/proj/.git/objects/ab/cdef",
            false,
            Category::CodeDev,
        ),
        (
            "C:\\Users\\alice\\AppData\\Local\\Temp\\clip.mp4",
            false,
            Category::CacheTemp,
        ),
        (
            "c:\\users\\ALICE\\appdata\\local\\temp\\clip.mp4",
            false,
            Category::CacheTemp,
        ),
        ("C:\\Windows\\WinSxS\\x.txt", false, Category::AppsSystem),
        (
            "/var/lib/docker/overlay2/a/b.mp3",
            false,
            Category::AppsSystem,
        ),
        (
            "/home/alice/.local/share/Steam/steamapps/common/g/asset.png",
            false,
            Category::AppsSystem,
        ),
        ("/tmp/x.pdf", false, Category::CacheTemp),
        (
            "/home/alice/.cargo/registry/src/x/logo.png",
            false,
            Category::CodeDev,
        ),
        (
            "/home/alice/Downloads/archive.tar.gz",
            false,
            Category::Archives,
        ),
        (
            "/home/alice/.config/app/settings.json",
            false,
            Category::CodeDev,
        ),
        (
            "/home/alice/proj/__pycache__/m.cpython-312.pyc",
            false,
            Category::CacheTemp,
        ),
        (
            "/home/alice/proj/.pytest_cache/v/cache/x.png",
            false,
            Category::CacheTemp,
        ),
        (
            "C:\\Program Files\\Foo\\readme.txt",
            false,
            Category::AppsSystem,
        ),
        ("/home/alice/Videos", true, Category::Other),
    ];
    for (path, is_dir, expected) in cases {
        assert_eq!(
            k.classify(std::path::Path::new(path), is_dir),
            expected,
            "{path}"
        );
    }
}

#[test]
fn case_matters_on_linux_and_not_on_windows() {
    let k = full();
    assert_eq!(
        k.classify(std::path::Path::new("/home/alice/.CACHE/x.png"), false),
        Category::Images
    );
    assert_eq!(
        k.classify(
            std::path::Path::new("/home/alice/src/Node_Modules/x.png"),
            false
        ),
        Category::Images
    );
    assert_eq!(
        k.classify(std::path::Path::new(r"C:\src\Node_Modules\x.png"), false),
        Category::CodeDev
    );
    assert_eq!(
        k.classify(std::path::Path::new(r"\\?\C:\Windows\winsxs\x.png"), false),
        Category::AppsSystem
    );
}

#[test]
fn without_an_environment_only_fixed_places_are_known() {
    let k = bare();
    assert_eq!(
        k.classify(std::path::Path::new("/var/lib/docker/x.png"), false),
        Category::AppsSystem
    );
    // `.cache` anywhere is a cache; the home folder is not needed for that.
    assert_eq!(
        k.classify(std::path::Path::new("/home/alice/.cache/x.png"), false),
        Category::CacheTemp
    );
    assert_eq!(
        k.classify(
            std::path::Path::new("/home/alice/.cargo/registry/x.png"),
            false
        ),
        Category::Images
    );
    assert_eq!(
        k.classify(std::path::Path::new("/p/node_modules/x.png"), false),
        Category::CodeDev
    );
    assert_eq!(
        classify(std::path::Path::new("/tmp/x.png"), false),
        Category::CacheTemp
    );
}

// ------------------------------------------------------------- recognizing

#[test]
fn well_known_folders_are_recognized_with_their_notes() {
    let k = full();
    let note = |path: &str| k.recognize(std::path::Path::new(path)).map(|e| e.note);
    assert_eq!(
        note("/var/cache/apt"),
        Some("Package cache managed by APT.")
    );
    assert_eq!(
        note("/var/cache/apt/"),
        Some("Package cache managed by APT.")
    );
    assert_eq!(note("/var/cache/apt/archives"), None);
    assert_eq!(
        note("/home/alice/.cargo/registry"),
        Some("Crate sources and index downloaded by Cargo.")
    );
    assert_eq!(
        note(r"C:\Users\alice\.cargo\registry"),
        Some("Crate sources and index downloaded by Cargo.")
    );
    assert!(note("/home/alice/project/node_modules").is_some());
    assert_eq!(note("/home/alice/project/node_modules/left-pad"), None);
    assert!(note(r"C:\Windows\WinSxS").is_some());
    assert!(note(r"c:\windows\winsxs").is_some());
    assert_eq!(note(r"C:\Windows\WinSxS\x"), None);
    assert!(note("/home/alice/.mozilla").is_some());
    assert!(
        note("/home/alice/Documents").is_none(),
        "Linux has no such entry"
    );
    assert!(note(r"C:\Users\alice\Downloads").is_some());
}

#[test]
fn the_most_specific_entry_wins() {
    let k = full();
    let entry = |path: &str| k.recognize(std::path::Path::new(path)).unwrap();
    // `%ProgramFiles%/*` and `%ProgramFiles%/WindowsApps` both match.
    assert_eq!(
        entry(r"C:\Program Files\WindowsApps").pattern,
        "%ProgramFiles%/WindowsApps"
    );
    assert_eq!(entry(r"C:\Program Files\Foo").pattern, "%ProgramFiles%/*");
    assert_eq!(entry(r"C:\Program Files").pattern, "%ProgramFiles%");
    assert_eq!(
        entry(r"C:\Program Files\Foo").handoff,
        Some(Handoff::Uninstall)
    );
    assert_eq!(entry(r"C:\Windows.old").handoff, Some(Handoff::DiskCleanup));
    assert_eq!(entry("/usr").handoff, Some(Handoff::Package));
    assert_eq!(entry("/opt/vendor").handoff, Some(Handoff::Package));
    assert_eq!(entry("/opt").handoff, None);
    assert_eq!(
        entry(r"C:\Users\alice\AppData\Local\Google\Chrome\User Data\Profile 1\Cache").pattern,
        "%LOCALAPPDATA%/Google/Chrome/User Data/*/Cache"
    );
}

#[test]
fn recognition_needs_the_environment_for_variable_patterns() {
    assert!(recognize(std::path::Path::new("/var/cache/apt")).is_some());
    assert!(recognize(std::path::Path::new(r"C:\Windows\WinSxS")).is_none());
    assert!(recognize(std::path::Path::new("/home/alice/.cache")).is_none());
    assert!(
        full()
            .recognize(std::path::Path::new("/home/alice/.cache"))
            .is_some()
    );
}

// -------------------------------------------------------------- hostile input

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let k = full();
        let _ = k.classify_bytes(&bytes, false);
        let _ = k.classify_bytes(&bytes, true);
        let _ = k.recognize_bytes(&bytes);
        let verdict = k.protection_bytes(&bytes);
        if bytes.contains(&0) {
            prop_assert_eq!(verdict.level, Level::Blocked);
        }
        if bytes.is_empty() {
            prop_assert_eq!(verdict.level, Level::Blocked);
        }
    }

    #[test]
    fn path_like_bytes_never_panic(
        parts in proptest::collection::vec(
            prop_oneof![
                Just(b".".to_vec()), Just(b"..".to_vec()), Just(b"...".to_vec()),
                Just(b"C:".to_vec()), Just(b"?".to_vec()), Just(b"UNC".to_vec()),
                Just(b"Windows".to_vec()), Just(b"usr".to_vec()), Just(b"".to_vec()),
                Just(b"PROGRA~1".to_vec()), Just(b"x:y".to_vec()), Just(b"*".to_vec()),
                Just(b"c$".to_vec()), Just(b"ADMIN$".to_vec()), Just(b"wsl$".to_vec()),
                Just(b"mnt".to_vec()), Just(b"c".to_vec()), Just(b"Volume{x}".to_vec()),
                proptest::collection::vec(any::<u8>(), 0..8),
            ],
            0..20,
        ),
        sep in prop_oneof![Just(b'/'), Just(b'\\')],
        lead in prop_oneof![
            Just(b"".to_vec()), Just(b"/".to_vec()), Just(b"\\\\?\\".to_vec()),
            Just(b"C:\\".to_vec()), Just(b"\\\\h\\".to_vec()), Just(b"\\\\?\\UNC\\h\\".to_vec()),
            Just(b"//".to_vec()), Just(b"\\\\.\\UNC\\wsl$\\d\\".to_vec()),
            Just(b"/mnt/c/".to_vec()), Just(b"\\".to_vec()),
        ],
    ) {
        let mut path = lead;
        for (i, p) in parts.iter().enumerate() {
            if i > 0 { path.push(sep); }
            path.extend_from_slice(p);
        }
        for k in [full(), linux_full(), roots()] {
            let _ = k.classify_bytes(&path, false);
            let _ = k.recognize_bytes(&path);
            let verdict = k.protection_bytes(&path);
            // A Linux host reads only absolute paths.
            if k.host() == Host::Linux && path.first() != Some(&b'/') {
                prop_assert_eq!(verdict.level, Level::Blocked);
            }
        }
    }

    #[test]
    fn a_linux_host_blocks_every_path_that_is_not_absolute(
        bytes in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        prop_assume!(bytes.first() != Some(&b'/'));
        for k in [linux_full(), linux_bare()] {
            let verdict = k.protection_bytes(&bytes);
            prop_assert_eq!(verdict.level, Level::Blocked);
            prop_assert_eq!(verdict.reason, Reason::Unresolvable);
        }
    }

    #[test]
    fn a_linux_dotdot_after_a_named_part_is_always_blocked(
        head in proptest::collection::vec("[a-zA-Z0-9_.-]{1,6}", 0..4),
        name in "[a-zA-Z0-9_-]{1,6}",
        tail in proptest::collection::vec("[a-zA-Z0-9_.-]{1,6}", 0..4),
    ) {
        let path = format!("/{}/{name}/../{}", head.join("/"), tail.join("/"));
        for k in [full(), linux_full(), linux_bare()] {
            prop_assert_eq!(level(k, &path), Level::Blocked, "{}", path);
        }
    }

    #[test]
    fn network_spellings_are_never_looser_than_the_drive_path(
        host in "[a-zA-Z0-9.-]{1,12}",
        letter in "[a-zA-Z]",
        rest in proptest::collection::vec(
            prop_oneof![
                Just("Windows"), Just("windows.old"), Just("Program Files"), Just("Users"),
                Just("System Volume Information"), Just("System32"), Just("alice"), Just("x"),
                Just("."), Just("PROGRA~1"), Just("ProgramData"),
            ],
            0..4,
        ),
        verbatim in prop_oneof![Just(""), Just(r"\\?\UNC\"), Just(r"\\.\UNC\")],
    ) {
        let tail: String = rest.iter().map(|p| format!(r"\{p}")).collect();
        let drive = format!("{letter}:{}", if tail.is_empty() { r"\".to_owned() } else { tail.clone() });
        let canonical = level(full(), &drive);
        let unc = if verbatim.is_empty() {
            format!(r"\\{host}\{letter}${tail}")
        } else {
            format!(r"{verbatim}{host}\{letter}${tail}")
        };
        prop_assert!(level(full(), &unc) >= canonical, "{} vs {}", unc, drive);
        prop_assert!(level(bare(), &unc) >= level(bare(), &drive), "{} vs {}", unc, drive);
        let system = if tail.is_empty() {
            r"C:\Windows".to_owned()
        } else {
            format!(r"C:\Windows{tail}")
        };
        let admin = if verbatim.is_empty() {
            format!(r"\\{host}\ADMIN${tail}")
        } else {
            format!(r"{verbatim}{host}\ADMIN${tail}")
        };
        prop_assert!(level(full(), &admin) >= level(full(), &system), "{} vs {}", admin, system);
    }

    #[test]
    fn drvfs_case_never_changes_the_answer(
        mask in proptest::collection::vec(any::<bool>(), 32),
    ) {
        // `/mnt` is a Linux name and keeps its case; the drive below it does not.
        let original = "c/Windows/System32";
        let mixed: String = original
            .chars()
            .zip(mask.iter().cycle())
            .map(|(c, up)| if *up { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
            .collect();
        let mixed = format!("/mnt/{mixed}");
        for k in [full(), linux_full(), linux_bare()] {
            prop_assert_eq!(level(k, &mixed), Level::Blocked, "{}", mixed);
        }
    }

    #[test]
    fn nothing_under_a_blocked_folder_escapes_its_level(name in "[a-z0-9 _.-]{0,40}") {
        for root in [
            "/usr",
            "/etc",
            "/boot",
            "/efi",
            "/proc",
            "/sys",
            "/dev",
            "/bin",
            r"C:\Windows",
            r"C:\Boot",
            r"C:\EFI",
            r"C:\Recovery",
        ] {
            let sep = if root.starts_with('/') { "/" } else { "\\" };
            let path = format!("{root}{sep}{name}");
            prop_assert_eq!(level(full(), &path), Level::Blocked, "{}", path);
            prop_assert_eq!(level(bare(), &path), Level::Blocked, "{}", path);
        }
    }

    #[test]
    fn a_trailing_dot_component_changes_nothing(
        name in "[a-zA-Z0-9 _.~:$-]{0,30}",
        windows in any::<bool>(),
    ) {
        let (base, dot) = if windows {
            (format!(r"C:\Users\{name}"), r"\.")
        } else {
            (format!("/home/{name}"), "/.")
        };
        let with_dot = format!("{base}{dot}");
        prop_assert_eq!(level(full(), &base), level(full(), &with_dot), "{}", base);
    }

    #[test]
    fn windows_case_never_changes_the_answer(
        mask in proptest::collection::vec(any::<bool>(), 24),
    ) {
        let original = r"C:\Windows\System32";
        let mixed: String = original
            .chars()
            .zip(mask.iter().cycle())
            .map(|(c, up)| if *up { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
            .collect();
        prop_assert_eq!(level(full(), &mixed), Level::Blocked, "{}", mixed);
    }
}

#[test]
fn a_name_deep_in_a_path_is_seen_however_many_parts_come_before_it() {
    let k = full();
    let linux = linux_full();
    // More parts than a path keeps inline, up to far more than any system
    // allows in a path name.
    for depth in [3usize, 47, 48, 49, 100, 5_000, 40_000] {
        let prefix = "/d".repeat(depth);
        // A file below a folder that gives its files a category, found by a
        // pattern that matches anywhere in the path.
        let file = format!("{prefix}/node_modules/x.png");
        assert_eq!(
            k.classify_bytes(file.as_bytes(), false),
            Category::CodeDev,
            "depth {depth}"
        );
        // A note that belongs to the name itself, which must be the last part.
        let folder = format!("{prefix}/node_modules");
        assert!(
            k.recognize_bytes(folder.as_bytes()).is_some(),
            "depth {depth}"
        );
        let inside = format!("{prefix}/node_modules/left-pad");
        assert!(
            k.recognize_bytes(inside.as_bytes()).is_none(),
            "depth {depth}"
        );
        // The same on Windows.
        let windows = format!(r"C:{}\node_modules\x.png", r"\d".repeat(depth));
        assert_eq!(
            k.classify_bytes(windows.as_bytes(), false),
            Category::CodeDev,
            "windows depth {depth}"
        );
        let _ = linux.protection_bytes(file.as_bytes());
    }
}

#[test]
fn very_long_paths_finish_in_bounded_time() {
    let k = full();
    let started = Instant::now();
    let shapes: Vec<Vec<u8>> = vec![
        b"/a".repeat(500_000),
        b"/..".repeat(300_000),
        [b"/".as_slice(), vec![b'a'; 1_000_000].as_slice()].concat(),
        b"C:\\a".repeat(250_000),
        [b"C:\\".as_slice(), vec![b'.'; 1_000_000].as_slice()].concat(),
        vec![b'/'; 1_000_000],
        b"/node_modules".repeat(100_000),
        [b"/".as_slice(), b"lib".repeat(300_000).as_slice()].concat(),
        [b"\\\\h\\C$\\".as_slice(), b"a\\".repeat(200_000).as_slice()].concat(),
        [
            b"\\\\?\\UNC\\".as_slice(),
            b"a\\".repeat(200_000).as_slice(),
        ]
        .concat(),
        [
            b"\\\\wsl$\\d\\".as_slice(),
            b"a\\".repeat(200_000).as_slice(),
        ]
        .concat(),
        [
            b"\\\\h\\ADMIN$\\".as_slice(),
            b"..\\".repeat(200_000).as_slice(),
        ]
        .concat(),
        [b"/mnt/c/".as_slice(), b"a/".repeat(200_000).as_slice()].concat(),
        b"\\\\".repeat(300_000),
        b"c$\\".repeat(300_000),
    ];
    for shape in &shapes {
        for k in [k, linux_full(), roots()] {
            let _ = k.classify_bytes(shape, false);
            let _ = k.recognize_bytes(shape);
            let _ = k.protection_bytes(shape);
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
}

// ------------------------------------------- the lookup agrees with a plain scan

/// The category of a place by trying every rule in turn, with none of the
/// shortcuts `Knowledge` takes.
fn plain_scan_place(env: &Environment, path: &[u8]) -> Option<Category> {
    use crate::matcher::{Split, Style, match_any};
    let split = Split::of(path);
    let mut rules: Vec<(crate::matcher::Compiled, Category)> = Vec::new();
    for rule in path_rules() {
        for c in crate::matcher::compile(rule.pattern, rule.os, false, env) {
            rules.push((c, rule.category));
        }
    }
    for entry in recognition_entries() {
        if entry.inherit && entry.category != Category::Other {
            for c in crate::matcher::compile(entry.pattern, entry.os, false, env) {
                rules.push((c, entry.category));
            }
        }
    }
    let mut best: Option<((usize, usize), Category)> = None;
    for (order, (compiled, category)) in rules.iter().enumerate() {
        let applies = match split.style {
            Style::Windows => compiled.flavor == crate::normalize::Flavor::Windows,
            Style::Linux => compiled.flavor == crate::normalize::Flavor::Linux,
            Style::Relative => compiled.floating,
        };
        if !applies {
            continue;
        }
        if let Some(matched) = match_any(compiled, split.slice(), true, false) {
            let rank = (usize::MAX - matched, order);
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, *category));
            }
        }
    }
    best.map(|(_, c)| c)
}

proptest! {
    #[test]
    fn the_indexed_lookup_agrees_with_a_plain_scan(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("node_modules"), Just(".git"), Just(".cache"), Just(".CACHE"),
                Just("__pycache__"), Just("Node_Modules"), Just("home"), Just("alice"),
                Just("Users"), Just("AppData"), Just("Local"), Just("Temp"), Just("var"),
                Just("lib"), Just("docker"), Just("x.png"), Just("$Recycle.Bin"),
                Just("System Volume Information"), Just(".cargo"), Just("registry"),
                Just("Windows"), Just("WinSxS"), Just("tmp"), Just("a"),
            ],
            0..9,
        ),
        lead in prop_oneof![
            Just("/"), Just("/home/alice/"), Just(r"C:\"), Just(r"c:\users\alice\appdata\local\"),
            Just(""), Just(r"\\?\C:\"), Just("/var/lib/"),
        ],
        windows_sep in any::<bool>(),
    ) {
        let sep = if windows_sep { "\\" } else { "/" };
        let path = format!("{lead}{}", parts.join(sep));
        let env = full_env();
        let expected = plain_scan_place(&env, path.as_bytes());
        prop_assert_eq!(full().place_bytes(path.as_bytes()), expected, "{}", path);
    }
}

#[test]
fn a_folder_that_holds_a_blocked_place_is_found() {
    let k = linux_full();
    let name = |path: &str| k.blocked_below(Path::new(path)).map(|e| e.pattern);
    // The package database folders sit inside `/var` and `/var/lib`, which are
    // only protected.
    assert_eq!(name("/var"), Some("/var/lib/dpkg"));
    assert_eq!(name("/var/lib"), Some("/var/lib/dpkg"));
    // The blocked place itself counts.
    assert_eq!(name("/var/lib/dpkg"), Some("/var/lib/dpkg"));
    assert_eq!(name("/etc"), Some("/etc"));
    // `/mnt` holds the Windows drives that WSL mounts.
    assert!(name("/mnt").is_some());
    // Ordinary folders hold nothing blocked.
    for path in [
        "/home/alice",
        "/home/alice/projects",
        "/var/log",
        "/var/cache",
        "/var/lib/docker",
        "/opt",
        "/opt/tool",
        "/mnt/data",
        "/tmp",
    ] {
        assert_eq!(name(path), None, "{path}");
    }
    // Same-looking names are not the same place.
    assert_eq!(name("/var/lib/dpkgx"), None);
    assert_eq!(name("/var/lib2"), None);
    // Spellings of a path give the answer of the place they name.
    assert_eq!(name("//var"), Some("/var/lib/dpkg"));
    assert_eq!(name("/var/./lib"), Some("/var/lib/dpkg"));
}

#[test]
fn a_windows_folder_that_holds_a_blocked_place_is_found() {
    let k = full();
    let name = |path: &str| k.blocked_below(Path::new(path)).map(|e| e.pattern);
    // A drive root holds the Windows folder.
    assert!(name(r"C:\").is_some());
    assert!(name(r"c:/").is_some());
    assert!(name(r"C:\Windows").is_some());
    assert!(name(r"C:\Program Files").is_some());
    // Ordinary folders on the same drive hold nothing blocked.
    for path in [
        r"C:\Users",
        r"C:\Users\alice",
        r"C:\Users\alice\Documents",
        r"C:\ProgramData",
        r"D:\Data",
        r"C:\Windows.old",
    ] {
        assert_eq!(name(path), None, "{path}");
    }
}

#[test]
fn the_roots_of_other_drives_do_not_make_a_folder_blocked_below() {
    let k = roots();
    // `/mnt/data` is a listed drive root. A folder that holds it is not made
    // blocked by that: a delete does not cross into another mount.
    assert!(k.blocked_below(Path::new("/mnt/data")).is_none());
    assert!(k.blocked_below(Path::new("/media/alice")).is_none());
}
