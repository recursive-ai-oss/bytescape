//! Hosting a scan: choosing the reader, streaming what it finds, and stopping
//! when the interface says so.
//!
//! The helper only reads. The walker applies the same rules as in the
//! interface (links are never followed, other filesystems are never entered,
//! cloud placeholders are never opened); the raw ext4 reader reads the block
//! device of a mounted filesystem and is used only for a whole filesystem
//! mounted exactly at the requested path.

use crate::session::{Event, Exit, Out};
use bs_ipc::{
    Backend, ErrorCode, Fallback, Message, Progress, ScanDone, ScanEnd, ScanRequest, path_from_wire,
};
use bs_scan::{
    Boundary, ScanContext, ScanEnd as EngineEnd, ScanError, ScanOptions, ScanProgress, Scanner,
    StatvfsVolume, VolumeInfo, Walker,
};
use bs_tree::{Batch, NodeKind, Record};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most a batch frame carries before the batch is split (the frame limit
/// is far higher; this keeps frames quick to write and read).
const BATCH_BUDGET: usize = 512 * 1024;
/// What one record can take on the wire at most, apart from its name.
const RECORD_BUDGET: usize = 96;
/// How often progress is sent.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// What the scan needs to know about this computer.
pub struct Host {
    /// Read the user's places from the request and clear the helper's own
    /// (true for the real helper; false in tests that share a process).
    pub isolate_environment: bool,
    /// The mount table (Linux).
    pub mountinfo: PathBuf,
}

impl Host {
    pub fn system() -> Self {
        Self {
            isolate_environment: true,
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
        }
    }
}

/// A batch that could not be cut into frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitError;

/// Splits `batch` into batches that each fit the frame budget, in order. A
/// batch that fits is returned as it is.
pub fn split(batch: Batch) -> Result<Vec<Batch>, SplitError> {
    let estimate = batch
        .name_bytes()
        .saturating_add(batch.len().saturating_mul(RECORD_BUDGET));
    if estimate <= BATCH_BUDGET {
        return Ok(vec![batch]);
    }
    let mut parts = Vec::new();
    let mut current = Batch::new();
    let mut size = 0usize;
    for record in batch.records() {
        let cost = match record {
            Record::Entry(entry) => {
                let name = batch.name(entry.name).ok_or(SplitError)?;
                match entry.kind {
                    NodeKind::Dir => current.dir(entry.key, entry.parent, name, &entry.meta),
                    kind => current.file(kind, entry.parent, name, &entry.meta),
                }
                .map_err(|_| SplitError)?;
                name.len().saturating_add(RECORD_BUDGET)
            }
            Record::ListingDone { dir } => {
                current.listing_done(*dir);
                RECORD_BUDGET
            }
            Record::Unreadable { dir, failure } => {
                current.unreadable(*dir, *failure);
                RECORD_BUDGET
            }
        };
        size = size.saturating_add(cost);
        if size >= BATCH_BUDGET {
            parts.push(std::mem::take(&mut current));
            size = 0;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

fn progress_of(p: ScanProgress, os_used: Option<u64>) -> Progress {
    Progress {
        allocated_bytes: p.allocated_bytes,
        logical_bytes: p.logical_bytes,
        files: p.files,
        dirs: p.dirs,
        unreadable: p.unreadable,
        vanished: p.vanished,
        batches: p.batches,
        estimated: p.estimated,
        os_used_bytes: os_used.unwrap_or(0),
    }
}

/// What reader runs, and why.
struct Plan {
    backend: Backend,
    fallback: Fallback,
    #[cfg(target_os = "linux")]
    ext4: Option<(PathBuf, bs_scan_ext4::Ext4Options)>,
}

/// The scanner the helper runs: the raw reader when the plan has one, with the
/// walker behind it for a filesystem the raw reader refuses.
struct Chosen {
    plan: Plan,
    /// What actually ran.
    ran: Mutex<(Backend, Fallback)>,
}

impl Scanner for Chosen {
    fn name(&self) -> &'static str {
        "helper"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<EngineEnd, ScanError> {
        self.note(self.plan.backend, self.plan.fallback);
        #[cfg(target_os = "linux")]
        if let Some((device, options)) = &self.plan.ext4 {
            let raw = bs_scan_ext4::Ext4Scanner::new(device.clone(), options.clone());
            match raw.scan(root, ctx) {
                Ok(end) => return Ok(end),
                Err(error) => {
                    // The raw reader only fails before it has sent anything.
                    // If something was sent the walker must not repeat it.
                    if ctx.progress().batches > 0 {
                        return Err(error);
                    }
                    let why = fallback_for(&error);
                    self.note(Backend::Walker, why);
                }
            }
        }
        Walker.scan(root, ctx)
    }
}

impl Chosen {
    fn note(&self, backend: Backend, fallback: Fallback) {
        if let Ok(mut ran) = self.ran.lock() {
            *ran = (backend, fallback);
        }
    }
}

#[cfg(target_os = "linux")]
fn fallback_for(error: &ScanError) -> Fallback {
    use bs_scan_ext4::Ext4Error;
    let ScanError::Root(io) = error else {
        return Fallback::NotSupported;
    };
    match io
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Ext4Error>())
    {
        Some(Ext4Error::TooLarge(_)) => Fallback::TooLarge,
        Some(Ext4Error::PartialMount) => Fallback::PartialMount,
        _ => Fallback::NotSupported,
    }
}

#[cfg(target_os = "linux")]
fn plan_for(requested: Backend, root: &Path, host: &Host) -> Plan {
    let mounts = std::fs::read(&host.mountinfo)
        .map(|text| crate::mounts::parse(&text))
        .unwrap_or_default();
    choose(requested, root, &mounts, &is_block_device_under_dev)
}

/// The device is read as the superuser, so it must be a block device under
/// `/dev` that the mount table names.
#[cfg(target_os = "linux")]
fn is_block_device_under_dev(device: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    device.starts_with("/dev")
        && std::fs::canonicalize(device)
            .ok()
            .filter(|real| real.starts_with("/dev"))
            .and_then(|real| std::fs::metadata(real).ok())
            .is_some_and(|meta| meta.file_type().is_block_device())
}

/// Decides which reader runs for `requested` on `root`, given the mount table.
/// `device_ok` says whether a device named by the table may be read raw.
#[cfg(target_os = "linux")]
fn choose(
    requested: Backend,
    root: &Path,
    mounts: &[crate::mounts::Mount],
    device_ok: &dyn Fn(&Path) -> bool,
) -> Plan {
    use std::os::unix::ffi::OsStrExt;
    let walker = |fallback| Plan {
        backend: Backend::Walker,
        fallback,
        ext4: None,
    };
    match requested {
        Backend::Walker => return walker(Fallback::None),
        Backend::Auto | Backend::Ext4 => {}
        _ => return walker(Fallback::Unavailable),
    }
    // A raw read shows the whole filesystem, so it is used only for a path
    // that is exactly where an ext2/3/4 filesystem is mounted.
    let unsuitable = if requested == Backend::Ext4 {
        Fallback::NotSupported
    } else {
        Fallback::None
    };
    let Some(mount) = crate::mounts::at_exactly(mounts, root.as_os_str().as_bytes()) else {
        return walker(unsuitable);
    };
    if !matches!(mount.fs_type.as_str(), "ext4" | "ext3" | "ext2") {
        return walker(unsuitable);
    }
    let device = PathBuf::from(std::ffi::OsStr::from_bytes(&mount.source));
    if !device_ok(&device) {
        return walker(unsuitable);
    }
    let others = mounts
        .iter()
        .filter(|m| m.id != mount.id)
        .map(|m| PathBuf::from(std::ffi::OsStr::from_bytes(&m.mount_point)));
    let options = bs_scan_ext4::Ext4Options {
        // The mount table's own word for what this mount shows; a mount of part
        // of a filesystem is refused by the reader.
        mount_root: Some(mount.root.clone()),
        ..bs_scan_ext4::Ext4Options::default()
    }
    .with_other_mounts(root, others);
    Plan {
        backend: Backend::Ext4,
        fallback: Fallback::None,
        ext4: Some((device, options)),
    }
}

#[cfg(not(target_os = "linux"))]
fn plan_for(requested: Backend, _root: &Path, _host: &Host) -> Plan {
    Plan {
        backend: Backend::Walker,
        fallback: match requested {
            Backend::Auto | Backend::Walker => Fallback::None,
            _ => Fallback::Unavailable,
        },
    }
}

/// A scan root: absolute, plain, and a real folder.
fn scan_root(wire: &[u8]) -> Result<PathBuf, &'static str> {
    let path = path_from_wire(wire).map_err(|_| "the volume is not a valid path")?;
    if wire.is_empty() || wire.contains(&0) || !path.is_absolute() {
        return Err("the volume is not an absolute path");
    }
    // Dot parts are looked for in the text: path handling quietly drops a `.`.
    let separator = |b: u8| b == b'/' || (cfg!(windows) && b == b'\\');
    if path
        .as_os_str()
        .as_encoded_bytes()
        .split(|&b| separator(b))
        .any(|part| part == b"." || part == b"..")
    {
        return Err("the volume has a . or .. part");
    }
    // Resolved here so that the mount table is compared with the real place.
    let real = std::fs::canonicalize(&path).map_err(|_| "the volume cannot be opened")?;
    if !real.is_dir() {
        return Err("the volume is not a folder");
    }
    Ok(real)
}

/// Serves a scan request.
pub(crate) fn serve<W: Write>(
    request: ScanRequest,
    out: &mut Out<W>,
    events: &Receiver<Event>,
    host: &Host,
) -> Exit {
    macro_rules! tell {
        ($code:expr, $text:expr) => {{
            return match out.error($code, $text) {
                Ok(()) => Exit::Done,
                Err(exit) => exit,
            };
        }};
    }
    let places = match crate::places::parse(&request.places) {
        Ok(places) => places,
        Err(_) => tell!(ErrorCode::BadRequest, "a place in the request is not valid"),
    };
    let root = match scan_root(&request.volume) {
        Ok(root) => root,
        Err(why) => tell!(ErrorCode::BadRequest, why),
    };
    let plan = plan_for(request.backend, &root, host);
    let os_used = StatvfsVolume.used_bytes(&root).ok();
    let options = ScanOptions {
        boundary: Boundary::with_mount_ids(request.extra_mount_ids.iter().copied()),
        os_used_bytes: os_used,
        ..ScanOptions::default()
    };
    if host.isolate_environment {
        crate::places::apply(&places);
    }
    let (ctx, batches) = ScanContext::channel(options, 64);
    let ctx = Arc::new(ctx);
    let chosen = Arc::new(Chosen {
        ran: Mutex::new((plan.backend, plan.fallback)),
        plan,
    });
    let worker = {
        let ctx = Arc::clone(&ctx);
        let chosen = Arc::clone(&chosen);
        let root = root.clone();
        std::thread::Builder::new()
            .name("scan".into())
            .spawn(move || chosen.scan(&root, &ctx))
    };
    let Ok(worker) = worker else {
        tell!(ErrorCode::Internal, "cannot start the scan")
    };

    let mut cancelled = false;
    let mut last_progress = Instant::now();
    loop {
        // The interface closing the connection ends everything at once; a
        // goodbye stops the scan but still gets its answer.
        match events.try_recv() {
            Ok(Event::Message(Message::Bye)) => {
                cancelled = true;
                ctx.cancel();
            }
            Ok(Event::Closed) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                ctx.cancel();
                return Exit::PipeClosed;
            }
            Ok(Event::Message(_)) => {
                ctx.cancel();
                return Exit::Protocol("a message arrived during a scan");
            }
            Ok(Event::Bad(why)) => {
                ctx.cancel();
                return Exit::Protocol(why);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        match batches.recv_timeout(Duration::from_millis(50)) {
            Ok(batch) => {
                if let Err(exit) = send_batch(out, batch) {
                    ctx.cancel();
                    return exit;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if worker.is_finished() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_progress.elapsed() >= PROGRESS_EVERY {
            last_progress = Instant::now();
            let progress = progress_of(ctx.progress(), os_used);
            if let Err(exit) = out.send(&Message::Progress(progress)) {
                ctx.cancel();
                return exit;
            }
        }
    }
    // What the scanner sent before it finished.
    while let Ok(batch) = batches.try_recv() {
        if let Err(exit) = send_batch(out, batch) {
            return exit;
        }
    }
    let result = match worker.join() {
        Ok(result) => result,
        Err(_) => tell!(
            ErrorCode::Internal,
            "the scan stopped after an internal error"
        ),
    };
    match result {
        Ok(end) => {
            let (backend, fallback) = chosen
                .ran
                .lock()
                .map(|ran| *ran)
                .unwrap_or((Backend::Walker, Fallback::None));
            let done = ScanDone {
                backend,
                end: if cancelled || end == EngineEnd::Cancelled {
                    ScanEnd::Cancelled
                } else {
                    ScanEnd::Completed
                },
                fallback,
                progress: progress_of(ctx.progress(), os_used),
            };
            match out.send(&Message::ScanDone(done)) {
                Ok(()) => Exit::Done,
                Err(exit) => exit,
            }
        }
        Err(ScanError::Root(_) | ScanError::NotADirectory(_)) => {
            tell!(ErrorCode::ScanFailed, "the volume cannot be read")
        }
        Err(ScanError::Unsupported) => {
            tell!(ErrorCode::Unsupported, "scanning is not supported here")
        }
        Err(_) => tell!(
            ErrorCode::ScanFailed,
            "the scan stopped after an internal error"
        ),
    }
}

fn send_batch<W: Write>(out: &mut Out<W>, batch: Batch) -> Result<(), Exit> {
    let parts = split(batch).map_err(|SplitError| {
        Exit::Failed(String::from("a batch could not be split for sending"))
    })?;
    for part in parts {
        out.send(&Message::Batch(part))?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    // The tests build a small ext4 image with the system's tool. The helper
    // itself starts nothing.
    #![allow(clippy::disallowed_types, clippy::disallowed_methods)]
    use super::*;
    use crate::mounts::{self, Mount};
    use crate::testutil::Fixture;
    use bs_tree::{Aggregator, Category, DirKey, Meta, NodeFlags};

    // -------------------------------------------------------- Splitting

    fn big_batch(entries: usize, name_len: usize) -> Batch {
        let mut batch = Batch::new();
        let meta = Meta {
            logical: 7,
            allocated: 4096,
            mtime: 1,
            flags: NodeFlags::default(),
            category: Category::OTHER,
            link: None,
            failure: None,
            exclusive: 0,
        };
        batch.dir(DirKey(0), DirKey::NONE, b"root", &meta).unwrap();
        for i in 0..entries {
            let mut name = format!("f{i:08}").into_bytes();
            name.resize(name_len, b'x');
            batch.file(NodeKind::File, DirKey(0), &name, &meta).unwrap();
        }
        batch.listing_done(DirKey(0));
        batch
    }

    #[test]
    fn a_small_batch_is_sent_as_it_is() {
        let batch = big_batch(10, 20);
        let parts = split(batch.clone()).unwrap();
        assert_eq!(parts, vec![batch]);
    }

    #[test]
    fn a_big_batch_is_cut_in_order_and_every_part_fits_a_frame() {
        let batch = big_batch(40_000, 120);
        assert!(batch.name_bytes() + batch.len() * RECORD_BUDGET > BATCH_BUDGET);
        let parts = split(batch.clone()).unwrap();
        assert!(parts.len() > 3);
        let mut again = Vec::new();
        for part in &parts {
            let mut encoded = Vec::new();
            part.encode(&mut encoded).unwrap();
            assert!(
                encoded.len() <= bs_ipc::MAX_FRAME_LEN / 2,
                "{}",
                encoded.len()
            );
            // What was cut decodes again.
            assert_eq!(Batch::decode(&encoded).unwrap(), *part);
            again.extend_from_slice(part.records());
        }
        // Nothing lost, nothing reordered: same records, same names, same order.
        assert_eq!(again.len(), batch.records().len());
        let names_of = |b: &Batch| -> Vec<Vec<u8>> {
            b.records()
                .iter()
                .filter_map(|r| match r {
                    Record::Entry(e) => Some(b.name(e.name).unwrap().to_vec()),
                    _ => None,
                })
                .collect()
        };
        let cut_names: Vec<Vec<u8>> = parts.iter().flat_map(names_of).collect();
        assert_eq!(cut_names, names_of(&batch));
        assert!(matches!(
            again.last(),
            Some(Record::ListingDone { dir }) if *dir == DirKey(0)
        ));
    }

    // ------------------------------------------------ Choosing a reader

    const TABLE: &str = "\
22 1 8:2 / / rw - ext4 /dev/sda2 rw
23 22 8:3 / /data rw - ext4 /dev/sda3 rw
24 22 8:4 /sub /part rw - ext4 /dev/sda4 rw
25 22 0:50 / /btr rw - btrfs /dev/sda5 rw
26 23 8:6 / /data/usb rw - vfat /dev/sdb1 rw
27 22 8:7 / /weird rw - ext4 /tmp/not-a-device rw
";

    fn table() -> Vec<Mount> {
        mounts::parse(TABLE.as_bytes())
    }

    fn anything_under_dev(device: &Path) -> bool {
        device.starts_with("/dev")
    }

    fn choose_for(requested: Backend, root: &str) -> Plan {
        choose(requested, Path::new(root), &table(), &anything_under_dev)
    }

    #[test]
    fn a_whole_ext4_mount_is_read_raw_with_what_the_mount_table_says() {
        let plan = choose_for(Backend::Auto, "/data");
        assert_eq!(
            (plan.backend, plan.fallback),
            (Backend::Ext4, Fallback::None)
        );
        let (device, options) = plan.ext4.unwrap();
        assert_eq!(device, Path::new("/dev/sda3"));
        // Always passed, so a mount of part of a filesystem is refused.
        assert_eq!(options.mount_root.as_deref(), Some(&b"/"[..]));
        // Mounts below it are skipped; mounts elsewhere are not its business.
        assert_eq!(options.skip, vec![b"usb".to_vec()]);
        let (_, options) = choose_for(Backend::Ext4, "/").ext4.unwrap();
        assert!(options.skip.contains(&b"data".to_vec()));
        assert!(options.skip.contains(&b"data/usb".to_vec()));
        assert!(options.skip.contains(&b"btr".to_vec()));
    }

    #[test]
    fn a_mount_of_part_of_a_filesystem_hands_its_root_to_the_reader() {
        let (_, options) = choose_for(Backend::Auto, "/part").ext4.unwrap();
        assert_eq!(options.mount_root.as_deref(), Some(&b"/sub"[..]));
    }

    #[test]
    fn everything_else_is_walked_and_an_explicit_request_says_why() {
        for (requested, root, want) in [
            // Not a mount point, or another kind of filesystem.
            (Backend::Auto, "/data/folder", Fallback::None),
            (Backend::Ext4, "/data/folder", Fallback::NotSupported),
            (Backend::Auto, "/btr", Fallback::None),
            (Backend::Ext4, "/btr", Fallback::NotSupported),
            (Backend::Ext4, "/data/usb", Fallback::NotSupported),
            // A device outside /dev is never read raw.
            (Backend::Ext4, "/weird", Fallback::NotSupported),
            // The walker was asked for.
            (Backend::Walker, "/data", Fallback::None),
            // Readers that do not exist yet.
            (Backend::Ntfs, "/data", Fallback::Unavailable),
            (Backend::Btrfs, "/btr", Fallback::Unavailable),
            (Backend::Xfs, "/data", Fallback::Unavailable),
        ] {
            let plan = choose_for(requested, root);
            assert_eq!(plan.backend, Backend::Walker, "{requested:?} {root}");
            assert_eq!(plan.fallback, want, "{requested:?} {root}");
            assert!(plan.ext4.is_none());
        }
    }

    #[test]
    fn a_device_the_check_refuses_is_not_read() {
        let plan = choose(Backend::Auto, Path::new("/data"), &table(), &|_| false);
        assert_eq!(plan.backend, Backend::Walker);
        assert!(plan.ext4.is_none());
    }

    #[test]
    fn only_block_devices_under_dev_pass_the_real_check() {
        let fx = Fixture::new("device-check");
        fx.write("not-a-device", "x");
        assert!(!is_block_device_under_dev(&fx.p("not-a-device")));
        assert!(!is_block_device_under_dev(Path::new("/etc/passwd")));
        assert!(!is_block_device_under_dev(Path::new("/dev/null")));
        assert!(!is_block_device_under_dev(Path::new("/dev/does-not-exist")));
        assert!(!is_block_device_under_dev(Path::new("/dev/../etc/passwd")));
    }

    // -------------------------------------------- Running the readers

    fn ext4_image(fx: &Fixture) -> Option<PathBuf> {
        fx.write("src/a/one.txt", "1");
        fx.write("src/a/two.txt", "22");
        fx.write("src/three.txt", "333");
        let image = fx.p("disk.img");
        let status = std::process::Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-d"])
            .arg(fx.p("src"))
            .arg(&image)
            .arg("4M")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(image)
    }

    fn run_chosen(plan: Plan, root: &Path) -> (Chosen, Aggregator, ScanEnd2) {
        let chosen = Chosen {
            ran: Mutex::new((plan.backend, plan.fallback)),
            plan,
        };
        let (ctx, batches) = ScanContext::channel(ScanOptions::default(), 1024);
        let end = chosen.scan(root, &ctx).unwrap();
        let mut aggregator = Aggregator::new();
        while let Ok(batch) = batches.try_recv() {
            aggregator.apply(&batch);
        }
        aggregator.finish();
        (chosen, aggregator, end)
    }

    type ScanEnd2 = EngineEnd;

    fn files_below_root(aggregator: &mut Aggregator) -> u64 {
        aggregator
            .publish()
            .root()
            .map_or(0, |r| u64::from(r.file_count()))
    }

    #[test]
    fn the_raw_reader_runs_when_the_plan_has_one() {
        let fx = Fixture::new("raw-ok");
        let Some(image) = ext4_image(&fx) else {
            return println!("NOT RUN: mke2fs is not available");
        };
        let plan = Plan {
            backend: Backend::Ext4,
            fallback: Fallback::None,
            ext4: Some((image, bs_scan_ext4::Ext4Options::default())),
        };
        let (chosen, mut tree, end) = run_chosen(plan, fx.path());
        assert_eq!(end, EngineEnd::Completed);
        assert_eq!(*chosen.ran.lock().unwrap(), (Backend::Ext4, Fallback::None));
        assert_eq!(files_below_root(&mut tree), 3);
    }

    #[test]
    fn a_filesystem_that_needs_too_much_memory_falls_back_to_the_walker() {
        let fx = Fixture::new("raw-too-large");
        let Some(image) = ext4_image(&fx) else {
            return println!("NOT RUN: mke2fs is not available");
        };
        let options = bs_scan_ext4::Ext4Options {
            max_memory_bytes: 1,
            ..bs_scan_ext4::Ext4Options::default()
        };
        let plan = Plan {
            backend: Backend::Ext4,
            fallback: Fallback::None,
            ext4: Some((image, options)),
        };
        // The walker reads the folder that holds the image: the fixture.
        let (chosen, _tree, end) = run_chosen(plan, &fx.p("src"));
        assert_eq!(end, EngineEnd::Completed);
        assert_eq!(
            *chosen.ran.lock().unwrap(),
            (Backend::Walker, Fallback::TooLarge)
        );
    }

    #[test]
    fn a_mount_of_part_of_a_filesystem_and_a_non_ext4_device_fall_back_too() {
        let fx = Fixture::new("raw-refused");
        let Some(image) = ext4_image(&fx) else {
            return println!("NOT RUN: mke2fs is not available");
        };
        let partial = bs_scan_ext4::Ext4Options {
            mount_root: Some(b"/sub".to_vec()),
            ..bs_scan_ext4::Ext4Options::default()
        };
        fx.write("zeros.img", &"\0".repeat(8192));
        for (device, options, want) in [
            (image, partial, Fallback::PartialMount),
            (
                fx.p("zeros.img"),
                bs_scan_ext4::Ext4Options::default(),
                Fallback::NotSupported,
            ),
            (
                fx.p("no-such-device"),
                bs_scan_ext4::Ext4Options::default(),
                Fallback::NotSupported,
            ),
        ] {
            let plan = Plan {
                backend: Backend::Ext4,
                fallback: Fallback::None,
                ext4: Some((device, options)),
            };
            let (chosen, _tree, end) = run_chosen(plan, &fx.p("src"));
            assert_eq!(end, EngineEnd::Completed);
            assert_eq!(*chosen.ran.lock().unwrap(), (Backend::Walker, want));
        }
    }

    #[test]
    fn what_the_raw_reader_reports_maps_to_what_the_interface_is_told() {
        use bs_scan_ext4::Ext4Error;
        let wrap =
            |e: Ext4Error| ScanError::Root(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        assert_eq!(
            fallback_for(&wrap(Ext4Error::TooLarge("x"))),
            Fallback::TooLarge
        );
        assert_eq!(
            fallback_for(&wrap(Ext4Error::PartialMount)),
            Fallback::PartialMount
        );
        assert_eq!(
            fallback_for(&wrap(Ext4Error::NotExt4)),
            Fallback::NotSupported
        );
        assert_eq!(
            fallback_for(&ScanError::Root(std::io::Error::other("x"))),
            Fallback::NotSupported
        );
    }
}
