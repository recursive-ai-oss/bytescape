//! NTFS volumes for the tests that run on Windows: a virtual disk made with
//! `diskpart`, formatted NTFS, mounted at a free drive letter, filled with
//! the usual files plus the Windows-only kinds (alternate streams, junctions,
//! links, offline files, compression, sparse files). They change system state
//! (they attach disks), so they run only where
//! `BYTESCAPE_DISPOSABLE_HOST=1` says the machine may be changed, and they
//! fail loudly when asked to run somewhere they cannot.
#![cfg(windows)]
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// True when the test may run; otherwise says `NOT RUN` and returns false.
/// A test that is allowed to run but cannot (no administrator rights) fails.
pub fn allowed(test: &str) -> bool {
    if std::env::var_os("BYTESCAPE_DISPOSABLE_HOST").is_none_or(|v| v != "1") {
        println!("NOT RUN: {test} needs BYTESCAPE_DISPOSABLE_HOST=1 (it attaches virtual disks)");
        return false;
    }
    let admin = Command::new("net")
        .arg("session")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        admin,
        "{test}: BYTESCAPE_DISPOSABLE_HOST is set but this is not an administrator shell"
    );
    true
}

/// The longest any one program here may take. The storage services can wait
/// on each other without end, and a test that hangs hides what it was doing.
const PROGRAM_LIMIT: Duration = Duration::from_secs(600);

pub fn run(cmd: &mut Command) -> String {
    let name = format!("{:?}", cmd.get_program());
    let (status, text) = run_limited(cmd, PROGRAM_LIMIT)
        .unwrap_or_else(|| panic!("{name} did not finish in {PROGRAM_LIMIT:?}"));
    assert!(status.success(), "{name} failed ({status}):\n{text}");
    text
}

/// What became of a program run with a time limit.
pub enum Ran {
    Done(std::process::ExitStatus, String),
    /// Still running at the limit, and now killed.
    TimedOut,
    /// Could not be started or waited for.
    Failed(String),
}

/// Runs a program for at most `limit`. Never panics, so it is safe to call
/// from a `Drop` that runs while a failed test unwinds.
pub fn try_run(cmd: &mut Command, limit: Duration) -> Ran {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Ran::Failed(format!("cannot run {:?}: {e}", cmd.get_program())),
    };
    // Read both streams on threads, so a full pipe cannot stall the program.
    let reader = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = Vec::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_end(&mut s);
            }
            s
        })
    };
    let out_thread = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err_thread = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let begun = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if begun.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Ran::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                return Ran::Failed(format!("cannot wait for {:?}: {e}", cmd.get_program()));
            }
        }
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out_thread.join().unwrap_or_default()),
        String::from_utf8_lossy(&err_thread.join().unwrap_or_default())
    );
    Ran::Done(status, text)
}

/// Runs a program for at most `limit`; `None` if it was still running (and
/// is now killed). Panics if it cannot be run at all.
pub fn run_limited(
    cmd: &mut Command,
    limit: Duration,
) -> Option<(std::process::ExitStatus, String)> {
    match try_run(cmd, limit) {
        Ran::Done(status, text) => Some((status, text)),
        Ran::TimedOut => None,
        Ran::Failed(why) => panic!("{why}"),
    }
}

fn diskpart(script: &str, dir: &Path) -> String {
    let file = dir.join("diskpart.txt");
    fs::write(&file, script).expect("write the diskpart script");
    let text = run(Command::new("diskpart").arg("/s").arg(&file));
    assert!(
        !text.contains("has encountered an error"),
        "diskpart failed:\n{script}\n{text}"
    );
    text
}

/// A virtual disk with one NTFS volume.
pub struct Vhd {
    pub file: PathBuf,
    pub letter: char,
    dir: PathBuf,
    attached: bool,
}

fn free_letter() -> char {
    ('D'..='Z')
        .rev()
        .find(|c| !Path::new(&format!("{c}:\\")).exists())
        .expect("a free drive letter")
}

fn wait_for(path: &Path, what: &str) {
    let begun = Instant::now();
    while fs::metadata(path).is_err() {
        assert!(
            begun.elapsed() < Duration::from_secs(60),
            "{what}: {} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Vhd {
    /// Creates, attaches, partitions, formats (quick) and mounts a disk of
    /// `size_mb`. `cluster` is the allocation unit in bytes, if not the
    /// default.
    pub fn create(name: &str, size_mb: u64, cluster: Option<u32>) -> Vhd {
        // The runner's work drive has room; the system drive does not.
        let base = std::env::var_os("RUNNER_TEMP").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = base.join(format!("bsntfs-{}-{name}", std::process::id()));
        let file = dir.join("disk.vhd");
        let letter = free_letter();
        let unit = cluster.map_or(String::new(), |c| format!(" unit={c}"));
        // The guard exists before the disk does, so a failure after the
        // disk is attached (a partition or format that does not work) still
        // detaches it and removes the files when the guard drops.
        let vhd = Vhd {
            file: file.clone(),
            letter,
            dir: dir.clone(),
            attached: true,
        };
        fs::create_dir_all(&dir).expect("vhd dir");
        diskpart(
            &format!(
                "create vdisk file=\"{f}\" maximum={size_mb} type=expandable\nselect vdisk file=\"{f}\"\nattach vdisk\ncreate partition primary\nformat fs=ntfs quick label=BSFIX{unit}\nassign letter={letter}\n",
                f = file.display()
            ),
            &dir,
        );
        wait_for(&vhd.root(), "the new volume");
        vhd
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}:\\", self.letter))
    }

    /// The raw volume device.
    pub fn raw(&self) -> PathBuf {
        PathBuf::from(format!(r"\\.\{}:", self.letter))
    }

    /// Writes the volume's cached data and metadata to the disk, so that a
    /// raw read sees them.
    pub fn flush(&self) {
        flush_volume(self.letter);
    }

    pub fn detach(&mut self) {
        if self.attached {
            let f = self.file.display();
            run(Command::new("powershell").args([
                "-NoProfile",
                "-Command",
                &format!("Dismount-DiskImage -ImagePath '{f}' | Out-Null"),
            ]));
            self.attached = false;
        }
    }

    /// Attaches the disk again: the volume is a new instance, with nothing
    /// of the file system cached.
    pub fn attach(&mut self) {
        if !self.attached {
            let f = self.file.display();
            let l = self.letter;
            run(Command::new("powershell").args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Mount-DiskImage -ImagePath '{f}' | Out-Null; \
                     $p = Get-DiskImage -ImagePath '{f}' | Get-Disk | Get-Partition | \
                     Where-Object {{ $_.Type -ne 'Reserved' }} | Select-Object -First 1; \
                     if ($p.DriveLetter -ne '{l}') {{ Set-Partition -InputObject $p -NewDriveLetter '{l}' }}"
                ),
            ]));
            self.attached = true;
            wait_for(&self.root(), "the volume");
        }
    }
}

impl Drop for Vhd {
    fn drop(&mut self) {
        // Never panic here: this runs while a failed test unwinds, and a
        // second panic would abort the process and hide the first.
        if self.attached {
            let f = self.file.display();
            let _ = try_run(
                Command::new("powershell").args([
                    "-NoProfile",
                    "-Command",
                    &format!("Dismount-DiskImage -ImagePath '{f}' | Out-Null"),
                ]),
                Duration::from_secs(120),
            );
            self.attached = false;
        }
        if std::env::var_os("KEEP_FIXTURES").is_none() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// A path the Win32 layer takes as it is: long, and with names it would
/// otherwise reinterpret.
pub fn verbatim(root: &Path, rel: &str) -> PathBuf {
    let mut s = OsString::from(r"\\?\");
    s.push(root.as_os_str());
    let mut path = PathBuf::from(s);
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        path.push(part);
    }
    path
}

fn noise(len: u64, seed: u64) -> Vec<u8> {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn text(len: u64) -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog\n";
    (0..len)
        .map(|i| line[(i % line.len() as u64) as usize])
        .collect()
}

#[derive(Clone, Debug)]
pub enum Item {
    Dir(String),
    File(String, u64),
    Sparse(String, u64, Vec<(u64, u64)>),
    /// Written into a directory marked compressed.
    Compressed(String, u64),
    HardLink(String, String),
    /// An alternate data stream: file, stream name, length.
    Stream(String, String, u64),
    /// `mklink /J`: link, target.
    Junction(String, String),
    /// `mklink` (a file symbolic link) or `mklink /D`: link, target.
    Symlink(String, String, bool),
    /// A file marked offline, as a cloud placeholder is.
    Offline(String, u64),
    /// A file whose name is these UTF-16 units (which need not be valid).
    RawName(String, Vec<u16>, u64),
}

pub fn dir(p: &str) -> Item {
    Item::Dir(p.into())
}

pub fn file(p: &str, len: u64) -> Item {
    Item::File(p.into(), len)
}

pub fn link(p: &str, to: &str) -> Item {
    Item::HardLink(p.into(), to.into())
}

pub fn stream(p: &str, name: &str, len: u64) -> Item {
    Item::Stream(p.into(), name.into(), len)
}

pub fn set_mtime(path: &Path, secs: u64) {
    // FILE_WRITE_ATTRIBUTES, and backup semantics so directories open.
    let f = OpenOptions::new()
        .access_mode(0x100)
        .custom_flags(0x0200_0000)
        .share_mode(7)
        .open(path)
        .unwrap_or_else(|e| panic!("open {} to stamp: {e}", path.display()));
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap_or_else(|e| panic!("stamp {}: {e}", path.display()));
}

/// The ordinary spelling of a path, for programs that do not take the
/// verbatim one.
fn plain(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        path.push(part);
    }
    path
}

pub fn write_item(root: &Path, item: &Item, serial: u64) {
    let p = |rel: &str| verbatim(root, rel);
    let q = |rel: &str| plain(root, rel);
    match item {
        Item::Dir(d) => fs::create_dir_all(p(d)).expect("create dir"),
        Item::File(f, len) => fs::write(p(f), noise(*len, serial)).expect("write"),
        Item::Sparse(f, len, islands) => {
            let path = p(f);
            let file = File::create(&path).expect("create sparse");
            drop(file);
            run(Command::new("fsutil")
                .args(["sparse", "setflag"])
                .arg(&path));
            let file = OpenOptions::new().write(true).open(&path).expect("open");
            file.set_len(*len).expect("set_len");
            for (offset, n) in islands {
                file.seek_write(&noise(*n, serial + offset), *offset)
                    .expect("write island");
            }
        }
        Item::Compressed(f, len) => {
            let path = p(f);
            let parent = path.parent().expect("parent").to_path_buf();
            fs::create_dir_all(&parent).expect("create dir");
            let rel_parent = f.rsplit_once('/').map_or("", |(d, _)| d);
            run(Command::new("compact")
                .args(["/c", "/i"])
                .arg(q(rel_parent)));
            let mut file = File::create(&path).expect("create compressed");
            file.write_all(&text(*len)).expect("write");
        }
        Item::HardLink(l, to) => fs::hard_link(p(to), p(l)).expect("hard link"),
        Item::Stream(f, name, len) => {
            let mut path = p(f).into_os_string();
            path.push(format!(":{name}"));
            fs::write(PathBuf::from(path), noise(*len, serial)).expect("write stream");
        }
        Item::Junction(l, to) => {
            let target = p(to);
            run(Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(p(l))
                .arg(target));
        }
        Item::Symlink(l, to, dir) => {
            let mut c = Command::new("cmd");
            c.args(["/c", "mklink"]);
            if *dir {
                c.arg("/D");
            }
            run(c.arg(q(l)).arg(q(to)));
        }
        Item::Offline(f, len) => {
            fs::write(p(f), noise(*len, serial)).expect("write");
            run(Command::new("attrib").arg("+O").arg(q(f)));
        }
        Item::RawName(parent, units, len) => {
            let mut path = p(parent);
            path.push(OsString::from_wide(units));
            fs::write(path, noise(*len, serial)).expect("write");
        }
    }
}

/// Stamps every entry below `root` with a fixed, distinct modification time,
/// deepest first so that no later change moves a directory's time.
pub fn stamp(root: &Path) {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut stack = vec![verbatim(root, "")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // Not the system's own folders, and no link: stamping follows it.
            if name == "System Volume Information" || name.starts_with('$') || kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                // A junction reads as a directory here; its reparse flag says
                // otherwise.
                use std::os::windows::fs::MetadataExt;
                let reparse = entry
                    .metadata()
                    .is_ok_and(|m| m.file_attributes() & 0x400 != 0);
                if reparse {
                    continue;
                }
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    });
    for (i, path) in found.iter().enumerate() {
        set_mtime(path, 1_700_000_000 + (i as u64 % 100_000) * 3 + 1);
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    OsString::from(s).encode_wide().collect()
}

pub fn populate(root: &Path, items: &[Item]) {
    for (i, item) in items.iter().enumerate() {
        write_item(root, item, i as u64 + 1);
    }
    stamp(root);
}

/// Writes a volume's cached data and metadata to its disk, so that a raw
/// read sees them. A best effort: the storage services can be slow to
/// answer, and a volume that is not flushed only shows more recent changes
/// as differences.
pub fn flush_volume(letter: char) {
    let done = run_limited(
        Command::new("powershell").args([
            "-NoProfile",
            "-Command",
            &format!("Write-VolumeCache -DriveLetter {letter}"),
        ]),
        Duration::from_secs(120),
    );
    match done {
        Some((status, _)) if status.success() => {}
        other => eprintln!("could not flush volume {letter}: {other:?}"),
    }
}
