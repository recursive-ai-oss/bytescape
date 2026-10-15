//! Where crash reports are kept, and how.
//!
//! Reports are plain text files in one folder in the user's own application
//! data. Each is written under a temporary name and then moved into place, so
//! a reader never sees half a report, and each is readable by its owner only.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use super::{Report, date};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Reports kept; older ones are removed when the program starts.
pub(crate) const MAX_REPORTS: usize = 10;
/// Most bytes read back from one report file.
const READ_LIMIT: u64 = 128 * 1024;
/// Most directory entries looked at.
const MAX_ENTRIES: usize = 10_000;
/// A temporary file this old was left by a program that died while writing.
const STALE_TEMPORARY: Duration = Duration::from_secs(24 * 3600);
/// How many names are tried before giving up on a clash.
const NAME_ATTEMPTS: u32 = 16;

const TEMPORARY_PREFIX: &str = ".tmp-";

/// `$XDG_STATE_HOME/bytescape/crashes`, or `~/.local/state/bytescape/crashes`.
/// Both arguments must already be absolute paths.
#[cfg(any(not(windows), test))]
pub(crate) fn unix_dir(state_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = match (state_home, home) {
        (Some(state), _) => state,
        (None, Some(home)) => home.join(".local").join("state"),
        (None, None) => return None,
    };
    Some(base.join("bytescape").join("crashes"))
}

/// `%LOCALAPPDATA%\Bytescape\crashes`, or the same place under the profile.
/// Both arguments must already be absolute paths.
#[cfg(any(windows, test))]
pub(crate) fn windows_dir(
    local_app_data: Option<PathBuf>,
    profile: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = match (local_app_data, profile) {
        (Some(local), _) => local,
        (None, Some(profile)) => profile.join("AppData").join("Local"),
        (None, None) => return None,
    };
    Some(base.join("Bytescape").join("crashes"))
}

/// An environment variable that holds an absolute path.
fn absolute_env(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

/// The folder reports go to on this system.
pub(crate) fn default_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows_dir(absolute_env("LOCALAPPDATA"), absolute_env("USERPROFILE"))
    }
    #[cfg(not(windows))]
    {
        unix_dir(absolute_env("XDG_STATE_HOME"), absolute_env("HOME"))
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        // The folder is inside the user's own profile, which other accounts
        // cannot read.
        fs::create_dir_all(dir)
    }
}

fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Writes `text` as a new report in `dir` and returns its path. `now` is Unix
/// seconds and `sequence` tells apart reports made in the same second. An
/// existing report is never replaced.
pub(crate) fn write_report(dir: &Path, text: &str, now: i64, sequence: u32) -> io::Result<PathBuf> {
    create_private_dir(dir)?;
    let stem = format!(
        "crash-{}-{}-{:04}",
        date::format_compact(now),
        std::process::id(),
        sequence
    );
    let temporary = dir.join(format!("{TEMPORARY_PREFIX}{stem}.txt"));
    let mut file = match create_private_file(&temporary) {
        // Left over from a program with the same process number.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary)?;
            create_private_file(&temporary)?
        }
        other => other?,
    };
    let written = file.write_all(text.as_bytes()).and_then(|()| file.flush());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let result = move_into_place(&temporary, dir, &stem);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Gives the finished file its real name without ever replacing a file that
/// is already there.
fn move_into_place(temporary: &Path, dir: &Path, stem: &str) -> io::Result<PathBuf> {
    for attempt in 0..NAME_ATTEMPTS {
        let name = if attempt == 0 {
            format!("{stem}.txt")
        } else {
            format!("{stem}-{attempt}.txt")
        };
        let target = dir.join(name);
        match fs::hard_link(temporary, &target) {
            Ok(()) => {
                let _ = fs::remove_file(temporary);
                return Ok(target);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                // A file system without links: a rename does the same job,
                // and the check keeps it from replacing anything.
                if !target.exists() {
                    fs::rename(temporary, &target)?;
                    return Ok(target);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the crash report",
    ))
}

fn is_report_name(name: &str) -> bool {
    name.starts_with("crash-")
        && name.ends_with(".txt")
        && name.is_ascii()
        && !name.contains(['/', '\\'])
}

/// The reports in `dir`, oldest first. Only regular files count: a link in
/// this folder is not followed.
pub(crate) fn list(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut reports: Vec<PathBuf> = entries
        .flatten()
        .take(MAX_ENTRIES)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| entry.file_name().to_str().is_some_and(is_report_name))
        .map(|entry| entry.path())
        .collect();
    reports.sort();
    reports
}

/// The file whose existence means the report was seen.
fn seen_marker(report: &Path) -> PathBuf {
    report.with_extension("seen")
}

/// Reads a report back. Text that is not valid UTF-8 is read with its bad
/// bytes replaced, and a file larger than the limit is read up to it.
pub(crate) fn read(path: &Path) -> io::Result<Report> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(READ_LIMIT)
        .read_to_end(&mut bytes)?;
    Ok(Report {
        path: path.to_path_buf(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// The newest report that has not been marked as seen.
pub(crate) fn pending(dir: &Path) -> Option<Report> {
    list(dir)
        .into_iter()
        .rev()
        .filter(|path| !seen_marker(path).exists())
        .find_map(|path| read(&path).ok())
}

/// Marks `report`, and every report older than it, as seen. An older report
/// is one the user would otherwise be told about next time, as if it were the
/// last crash.
pub(crate) fn acknowledge(report: &Path) -> io::Result<()> {
    let mut first_error = None;
    let mut mark = |path: &Path| {
        let marker = seen_marker(path);
        match create_private_file(&marker) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    };
    mark(report);
    if let Some(dir) = report.parent() {
        for older in list(dir).iter().filter(|path| path.as_path() < report) {
            mark(older);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Keeps the newest `keep` reports. Also removes the markers of reports that
/// are gone and temporary files left by a crash during a write.
pub(crate) fn prune(dir: &Path, keep: usize) {
    let reports = list(dir);
    let surplus = reports.len().saturating_sub(keep);
    for old in reports.iter().take(surplus) {
        let _ = fs::remove_file(old);
        let _ = fs::remove_file(seen_marker(old));
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(MAX_ENTRIES) {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let orphan_marker = name.starts_with("crash-")
            && name.ends_with(".seen")
            && !path.with_extension("txt").exists();
        let stale_temporary = name.starts_with(TEMPORARY_PREFIX)
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .is_ok_and(|age| age > STALE_TEMPORARY)
                });
        if orphan_marker || stale_temporary {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::crash::tests::Scratch;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn folders_on_each_system() {
        let p = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            unix_dir(p("/state"), p("/home/a")),
            p("/state/bytescape/crashes")
        );
        assert_eq!(
            unix_dir(None, p("/home/a")),
            p("/home/a/.local/state/bytescape/crashes")
        );
        assert_eq!(unix_dir(None, None), None);
        assert_eq!(
            windows_dir(p("/local"), p("/profile")),
            p("/local/Bytescape/crashes")
        );
        assert_eq!(
            windows_dir(None, p("/profile")),
            p("/profile/AppData/Local/Bytescape/crashes")
        );
        assert_eq!(windows_dir(None, None), None);
    }

    #[test]
    fn a_report_is_written_whole_under_its_final_name() {
        let tmp = Scratch::new("store-write");
        let dir = tmp.path().join("a").join("crashes");
        let path = write_report(&dir, "hello\n", 1_800_000_000, 7).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("crash-20270115T080000Z-{}-0007.txt", std::process::id())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        // No temporary file is left behind.
        assert_eq!(names(&dir).len(), 1, "{:?}", names(&dir));
        assert_eq!(list(&dir), [path]);
    }

    #[cfg(unix)]
    #[test]
    fn the_folder_and_file_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = Scratch::new("store-private");
        let dir = tmp.path().join("state").join("bytescape").join("crashes");
        let path = write_report(&dir, "x", 0, 0).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // Folders made on the way are private too.
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
    }

    #[test]
    fn a_report_is_never_replaced() {
        let tmp = Scratch::new("store-clash");
        let first = write_report(tmp.path(), "first", 5, 1).unwrap();
        let second = write_report(tmp.path(), "second", 5, 1).unwrap();
        let third = write_report(tmp.path(), "third", 5, 1).unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_eq!(fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(fs::read_to_string(&second).unwrap(), "second");
        assert_eq!(fs::read_to_string(&third).unwrap(), "third");
        assert_eq!(list(tmp.path()).len(), 3);
        assert!(names(tmp.path()).iter().all(|n| !n.starts_with(".tmp-")));
    }

    #[test]
    fn a_failed_write_leaves_nothing_and_says_so() {
        let tmp = Scratch::new("store-fail");
        // A file where the folder should be.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "x").unwrap();
        assert!(write_report(&blocker.join("crashes"), "x", 0, 0).is_err());
        assert_eq!(names(tmp.path()), ["blocker"]);
    }

    #[test]
    fn a_half_written_temporary_file_is_not_a_report() {
        let tmp = Scratch::new("store-partial");
        fs::write(
            tmp.path().join(".tmp-crash-20270101T000000Z-1-0000.txt"),
            "par",
        )
        .unwrap();
        assert!(list(tmp.path()).is_empty());
        assert!(pending(tmp.path()).is_none());
        // A fresh one may belong to a program that is writing right now.
        prune(tmp.path(), MAX_REPORTS);
        assert_eq!(names(tmp.path()).len(), 1);
        // One from a day ago does not.
        let old = tmp.path().join(".tmp-crash-20200101T000000Z-1-0000.txt");
        let file = fs::File::create(&old).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3 * 24 * 3600))
            .unwrap();
        drop(file);
        prune(tmp.path(), MAX_REPORTS);
        assert_eq!(
            names(tmp.path()),
            [".tmp-crash-20270101T000000Z-1-0000.txt"]
        );
    }

    #[test]
    fn only_reports_are_listed_and_links_are_not_followed() {
        let tmp = Scratch::new("store-list");
        fs::write(tmp.path().join("crash-1.txt"), "a").unwrap();
        fs::write(tmp.path().join("crash-2.txt"), "b").unwrap();
        fs::write(tmp.path().join("notes.txt"), "c").unwrap();
        fs::write(tmp.path().join("crash-3.log"), "d").unwrap();
        fs::create_dir(tmp.path().join("crash-4.txt")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", tmp.path().join("crash-5.txt")).unwrap();
        let listed: Vec<String> = list(tmp.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(listed, ["crash-1.txt", "crash-2.txt"]);
        assert!(list(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn old_reports_are_pruned_and_the_newest_kept() {
        let tmp = Scratch::new("store-prune");
        for n in 0..15 {
            let path = tmp.path().join(format!("crash-{n:02}.txt"));
            fs::write(&path, format!("report {n}")).unwrap();
            fs::write(seen_marker(&path), "").unwrap();
        }
        // A marker whose report is gone.
        fs::write(tmp.path().join("crash-99.seen"), "").unwrap();
        prune(tmp.path(), MAX_REPORTS);
        let left = names(tmp.path());
        let reports: Vec<&String> = left.iter().filter(|n| n.ends_with(".txt")).collect();
        assert_eq!(reports.len(), 10);
        assert_eq!(reports[0], "crash-05.txt");
        assert_eq!(reports[9], "crash-14.txt");
        // The markers of the removed reports went with them, and the orphan.
        let markers: Vec<&String> = left.iter().filter(|n| n.ends_with(".seen")).collect();
        assert_eq!(markers.len(), 10, "{left:?}");
        assert!(!left.contains(&"crash-99.seen".to_owned()));
        assert!(!left.contains(&"crash-00.seen".to_owned()));
    }

    #[test]
    fn pruning_a_missing_folder_is_harmless() {
        let tmp = Scratch::new("store-prune-missing");
        prune(&tmp.path().join("none"), 3);
        prune(tmp.path(), 0);
    }

    #[test]
    fn the_newest_unseen_report_is_pending_and_acknowledging_keeps_the_file() {
        let tmp = Scratch::new("store-pending");
        assert!(pending(tmp.path()).is_none());
        for n in 1..=3 {
            fs::write(
                tmp.path().join(format!("crash-{n}.txt")),
                format!("report {n}"),
            )
            .unwrap();
        }
        let newest = pending(tmp.path()).unwrap();
        assert_eq!(newest.text, "report 3");
        acknowledge(&newest.path).unwrap();
        // The file is still there, and the older reports were superseded.
        assert!(newest.path.exists());
        assert!(pending(tmp.path()).is_none());
        assert_eq!(list(tmp.path()).len(), 3);
        // A later crash is pending again.
        fs::write(tmp.path().join("crash-4.txt"), "report 4").unwrap();
        assert_eq!(pending(tmp.path()).unwrap().text, "report 4");
        // Acknowledging twice is fine.
        acknowledge(&newest.path).unwrap();
        assert_eq!(pending(tmp.path()).unwrap().text, "report 4");
    }

    #[test]
    fn acknowledging_an_old_report_leaves_newer_ones_pending() {
        let tmp = Scratch::new("store-ack-old");
        for n in 1..=3 {
            fs::write(tmp.path().join(format!("crash-{n}.txt")), format!("r{n}")).unwrap();
        }
        acknowledge(&tmp.path().join("crash-2.txt")).unwrap();
        assert_eq!(pending(tmp.path()).unwrap().text, "r3");
    }

    #[test]
    fn a_report_that_is_not_text_is_read_with_replacements() {
        let tmp = Scratch::new("store-bytes");
        fs::write(tmp.path().join("crash-1.txt"), b"ok \xFF\xFE bytes \xC3").unwrap();
        let report = pending(tmp.path()).unwrap();
        assert_eq!(report.text, "ok \u{FFFD}\u{FFFD} bytes \u{FFFD}");
    }

    #[test]
    fn a_huge_file_is_read_only_up_to_the_limit() {
        let tmp = Scratch::new("store-huge");
        fs::write(tmp.path().join("crash-1.txt"), vec![b'x'; 1_000_000]).unwrap();
        assert_eq!(pending(tmp.path()).unwrap().text.len() as u64, READ_LIMIT);
    }
}
