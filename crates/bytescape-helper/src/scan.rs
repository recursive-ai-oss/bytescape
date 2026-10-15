//! Hosting a scan: choosing the reader, streaming what it finds, and stopping
//! when the interface says so.
//!
//! The helper only reads. The walker applies the same rules as in the
//! interface (links are never followed, other filesystems are never entered,
//! cloud placeholders are never opened). The raw readers are chosen in
//! [`crate::plan`]; this module hosts whichever runs, cuts what it finds into
//! frames, and stops when the interface says so.

use crate::plan::{Chosen, plan_for};
use crate::session::{Event, Exit, Out};
use bs_ipc::{ErrorCode, Message, Progress, ScanDone, ScanEnd, ScanRequest, path_from_wire};
use bs_scan::{
    Boundary, ScanContext, ScanEnd as EngineEnd, ScanError, ScanOptions, ScanProgress, Scanner,
    StatvfsVolume, VolumeInfo,
};
use bs_tree::{Batch, NodeKind, Record};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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
    /// The mount table (Linux).
    pub mountinfo: PathBuf,
}

impl Host {
    pub fn system() -> Self {
        Self {
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
        }
    }
}

/// What a scan of `root` on this computer would use, for the tests that need
/// a real mount table: the reader, why it is not the one asked for, and the
/// mounts that would be entered.
#[doc(hidden)]
pub fn plan_summary(
    requested: bs_ipc::Backend,
    root: &std::path::Path,
    host: &Host,
    requested_mounts: &[u64],
) -> (bs_ipc::Backend, bs_ipc::Fallback, Vec<u64>) {
    let plan = plan_for(requested, root, host, requested_mounts);
    (plan.backend, plan.fallback, plan.mount_ids)
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

/// A scan root: absolute, plain, and a real folder.
fn scan_root(wire: &[u8]) -> Result<PathBuf, &'static str> {
    let path = path_from_wire(wire).map_err(|_| "the volume is not a valid path")?;
    if wire.is_empty() || bs_ipc::wire_has_nul(wire) || !path.is_absolute() {
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
    // Only disks of this computer are read with administrator rights.
    match crate::volume::of_scan_root(&root, host) {
        crate::volume::Verdict::Local => {}
        crate::volume::Verdict::Remote => tell!(
            ErrorCode::RemoteVolume,
            "network volumes are not scanned with administrator rights"
        ),
        crate::volume::Verdict::Unknown => tell!(
            ErrorCode::Refused,
            "the helper cannot tell what kind of volume this is, so it does not scan it"
        ),
    }
    let plan = plan_for(request.backend, &root, host, &request.extra_mount_ids);
    let os_used = StatvfsVolume.used_bytes(&root).ok();
    // Only mounts of the drive's own filesystem are entered, and the user's
    // places come from the request alone.
    let options = ScanOptions {
        boundary: Boundary::with_mount_ids(plan.mount_ids.iter().copied()),
        os_used_bytes: os_used,
        ..ScanOptions::default()
    }
    .with_environment(crate::places::environment(&places));
    let (ctx, batches) = ScanContext::channel(options, 64);
    let ctx = Arc::new(ctx);
    let chosen = Arc::new(Chosen::new(plan));
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
            let (backend, fallback) = chosen.outcome();
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
    use bs_tree::{Category, DirKey, Meta, NodeFlags};

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
}

#[cfg(test)]
mod root_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use bs_ipc::path_to_wire;

    /// What the interface sends for a folder, on whichever system this is.
    /// On Windows the wire form is 16-bit units, so an ordinary path is full
    /// of zero bytes; none of them is a NUL character.
    #[test]
    fn the_wire_form_of_a_real_folder_is_accepted_as_the_volume() {
        let folder = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = scan_root(&path_to_wire(&folder)).unwrap();
        assert!(root.is_dir());
    }

    #[test]
    fn a_volume_that_is_not_a_plain_absolute_real_folder_is_refused() {
        let folder = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let text = folder.to_string_lossy().into_owned();
        let sep = std::path::MAIN_SEPARATOR;
        let wire = |text: &str| path_to_wire(std::path::Path::new(text));
        for (volume, why) in [
            (Vec::new(), "empty"),
            (wire("relative"), "relative"),
            (wire(&format!("{text}{sep}.")), "dot part"),
            (wire(&format!("{text}{sep}..{sep}x")), "dotdot part"),
            (wire(&format!("{text}{sep}no-such-folder-here")), "missing"),
        ] {
            assert!(scan_root(&volume).is_err(), "{why}");
        }
        // A NUL in the path: a zero byte on Linux, a zero unit on Windows.
        let mut with_nul = path_to_wire(&folder);
        with_nul.extend_from_slice(&[0, 0]);
        assert!(scan_root(&with_nul).is_err(), "NUL");
    }

    /// The NTFS reader makes up a folder for files that no folder lists and
    /// marks it, so that nothing is ever done to its contents by path. The
    /// mark has to survive being cut into frames and sent.
    #[test]
    fn a_made_up_folder_is_still_marked_after_it_is_cut_and_sent() {
        use bs_ipc::{Direction, FrameReader, MAX_FRAME_LEN, write_message};
        use bs_tree::{Aggregator, DirKey, Meta, NodeFlags};
        let meta = |flags| Meta {
            logical: 1,
            allocated: 4096,
            flags,
            ..Meta::default()
        };
        // Enough files in the made-up folder that the batch has to be cut.
        let mut batch = Batch::new();
        batch
            .dir(DirKey(0), DirKey::NONE, b"root", &meta(NodeFlags::EMPTY))
            .unwrap();
        batch
            .dir(
                DirKey(1),
                DirKey(0),
                b"orphans",
                &meta(NodeFlags::SYNTHETIC),
            )
            .unwrap();
        batch
            .dir(DirKey(2), DirKey(0), b"real", &meta(NodeFlags::EMPTY))
            .unwrap();
        for i in 0..30_000 {
            let name = format!("{i:0100}");
            batch
                .file(
                    NodeKind::File,
                    DirKey(1),
                    name.as_bytes(),
                    &meta(NodeFlags::EMPTY),
                )
                .unwrap();
        }
        batch
            .file(NodeKind::File, DirKey(2), b"ok", &meta(NodeFlags::EMPTY))
            .unwrap();
        for key in 0..3 {
            batch.listing_done(DirKey(key));
        }
        let parts = split(batch).unwrap();
        assert!(parts.len() > 1, "the batch must have been cut");
        let mut wire = Vec::new();
        for part in parts {
            write_message(&mut wire, &Message::Batch(part)).unwrap();
        }
        let mut reader = FrameReader::new(&wire[..], Direction::ToInterface, MAX_FRAME_LEN);
        let mut tree = Aggregator::new();
        while let Some(message) = reader.read().unwrap() {
            let Message::Batch(part) = message else {
                panic!("only batches were sent");
            };
            assert_eq!(tree.apply(&part).rejected, 0);
        }
        tree.finish();
        let snapshot = tree.snapshot();
        let find = |name: &[u8]| {
            (0..snapshot.len())
                .map(bs_tree::NodeId::from_index)
                .find(|id| snapshot.node(*id).unwrap().name() == name)
                .map(|id| snapshot.node(id).unwrap())
                .unwrap()
        };
        assert!(find(b"orphans").is_synthetic());
        assert!(find(&format!("{:0100}", 7).into_bytes()).is_under_synthetic());
        assert!(find(&format!("{:0100}", 29_999).into_bytes()).is_under_synthetic());
        for real in [&b"root"[..], b"real", b"ok"] {
            assert!(!find(real).is_under_synthetic(), "{real:?}");
        }
    }
}
