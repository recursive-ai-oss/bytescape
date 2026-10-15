//! Turning messages into frame bodies and back.
//!
//! A frame body is `version (u8) | kind (u8) | fields`. Integers are
//! little-endian. The decoder is total: any bytes give a message or an error,
//! and nothing is allocated before the data that justifies it has been seen.

use crate::msg::{
    Backend, DeleteItem, DeleteRequest, ErrorCode, ErrorReport, Fallback, Hello, ItemOutcome,
    Message, Place, Progress, Refusal, ScanDone, ScanEnd, ScanRequest, Seen,
};
use crate::wire::{Reader, Writer};
use crate::{
    DecodeError, EncodeError, FRAME_VERSION, MAX_DELETE_ITEMS, MAX_ERROR_TEXT, MAX_EXTRA_MOUNTS,
    MAX_PLACE_NAME, MAX_PLACE_VALUE, MAX_PLACES, MAX_WIRE_PATH,
};
use bs_tree::Batch;

const K_HELLO: u8 = 1;
const K_SCAN: u8 = 2;
const K_DELETE: u8 = 3;
const K_BYE: u8 = 4;
const K_BATCH: u8 = 16;
const K_PROGRESS: u8 = 17;
const K_SCAN_DONE: u8 = 18;
const K_ERROR: u8 = 19;
const K_DELETE_ITEM: u8 = 20;
const K_DELETE_DONE: u8 = 21;

/// The smallest an encoded delete item can be: a path length, the flags and
/// the empty `seen`.
const MIN_DELETE_ITEM: usize = 4 + 1 + 1;

/// Appends the body of `message` (version, kind, fields) to `out`.
pub fn encode_body(message: &Message, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let start = out.len();
    let result = encode_into(message, out);
    if result.is_err() {
        out.truncate(start);
    }
    result
}

fn encode_into(message: &Message, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    let mut w = Writer::new(out);
    w.u8(FRAME_VERSION);
    match message {
        Message::Hello(hello) => {
            w.u8(K_HELLO);
            w.u16(hello.version);
            w.raw(&hello.nonce);
        }
        Message::Scan(scan) => {
            w.u8(K_SCAN);
            if scan.volume.len() > MAX_WIRE_PATH
                || scan.extra_mount_ids.len() > MAX_EXTRA_MOUNTS
                || scan.places.len() > MAX_PLACES
            {
                return Err(EncodeError::TooLarge);
            }
            w.bytes(&scan.volume)?;
            w.u8(scan.backend.code());
            w.count(scan.extra_mount_ids.len())?;
            for id in &scan.extra_mount_ids {
                w.u64(*id);
            }
            w.count(scan.places.len())?;
            for place in &scan.places {
                if place.name.len() > MAX_PLACE_NAME || place.value.len() > MAX_PLACE_VALUE {
                    return Err(EncodeError::TooLarge);
                }
                w.bytes(place.name.as_bytes())?;
                w.bytes(&place.value)?;
            }
        }
        Message::Delete(delete) => {
            w.u8(K_DELETE);
            if delete.items.len() > MAX_DELETE_ITEMS {
                return Err(EncodeError::TooLarge);
            }
            w.bool(delete.permanent);
            w.count(delete.items.len())?;
            for item in &delete.items {
                if item.path.len() > MAX_WIRE_PATH {
                    return Err(EncodeError::TooLarge);
                }
                w.bytes(&item.path)?;
                w.bool(item.confirmed_protected);
                match item.seen {
                    None => w.u8(0),
                    Some(seen) => {
                        w.u8(1);
                        w.u64(seen.volume);
                        w.u128(seen.file);
                    }
                }
            }
        }
        Message::Bye => w.u8(K_BYE),
        Message::Batch(batch) => {
            w.u8(K_BATCH);
            batch.encode(w.vec()).map_err(|_| EncodeError::TooLarge)?;
        }
        Message::Progress(progress) => {
            w.u8(K_PROGRESS);
            put_progress(&mut w, progress);
        }
        Message::ScanDone(done) => {
            w.u8(K_SCAN_DONE);
            w.u8(done.backend.code());
            w.u8(match done.end {
                ScanEnd::Completed => 0,
                ScanEnd::Cancelled => 1,
            });
            w.u8(match done.fallback {
                Fallback::None => 0,
                Fallback::Unavailable => 1,
                Fallback::TooLarge => 2,
                Fallback::PartialMount => 3,
                Fallback::NotSupported => 4,
                Fallback::MountNotPlaced => 5,
            });
            put_progress(&mut w, &done.progress);
        }
        Message::Error(report) => {
            w.u8(K_ERROR);
            w.u8(report.code.code());
            // The text is cut at a character boundary, never refused.
            let text = truncate_text(&report.message, MAX_ERROR_TEXT);
            w.bytes(text.as_bytes())?;
        }
        Message::DeleteItem { index, outcome } => {
            w.u8(K_DELETE_ITEM);
            w.u32(*index);
            put_outcome(&mut w, outcome);
        }
        Message::DeleteDone { items } => {
            w.u8(K_DELETE_DONE);
            w.u32(*items);
        }
    }
    Ok(())
}

fn truncate_text(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or("")
}

fn put_progress(w: &mut Writer<'_>, p: &Progress) {
    w.u64(p.allocated_bytes);
    w.u64(p.logical_bytes);
    w.u64(p.files);
    w.u64(p.dirs);
    w.u64(p.unreadable);
    w.u64(p.vanished);
    w.u64(p.batches);
    w.u64(p.estimated);
    w.u64(p.os_used_bytes);
}

fn get_progress(r: &mut Reader<'_>) -> Result<Progress, DecodeError> {
    Ok(Progress {
        allocated_bytes: r.u64()?,
        logical_bytes: r.u64()?,
        files: r.u64()?,
        dirs: r.u64()?,
        unreadable: r.u64()?,
        vanished: r.u64()?,
        batches: r.u64()?,
        estimated: r.u64()?,
        os_used_bytes: r.u64()?,
    })
}

fn put_outcome(w: &mut Writer<'_>, outcome: &ItemOutcome) {
    match *outcome {
        ItemOutcome::Deleted {
            bytes,
            entries,
            shared_files,
        } => {
            w.u8(1);
            w.u64(bytes);
            w.u64(entries);
            w.u64(shared_files);
        }
        ItemOutcome::Refused(reason) => {
            w.u8(2);
            let (code, extra) = match reason {
                Refusal::Blocked => (1, None),
                Refusal::ProtectedNotConfirmed => (2, None),
                Refusal::SelectionRefused { driver } => (3, Some(driver)),
                Refusal::BadPath => (4, None),
                Refusal::NotFound => (5, None),
                Refusal::CannotResolve => (6, None),
                Refusal::ChangedSinceScan => (7, None),
                Refusal::MountPoint => (8, None),
                Refusal::Overlaps { with } => (9, Some(with)),
                Refusal::NotRealPath => (10, None),
                Refusal::NotPermanent => (11, None),
                Refusal::RemoteVolume => (12, None),
            };
            w.u8(code);
            w.u32(extra.unwrap_or(0));
        }
        ItemOutcome::Failed {
            step,
            os_code,
            removed_bytes,
            removed_entries,
        } => {
            w.u8(3);
            w.u8(step);
            w.i32(os_code);
            w.u64(removed_bytes);
            w.u64(removed_entries);
        }
    }
}

fn get_outcome(r: &mut Reader<'_>) -> Result<ItemOutcome, DecodeError> {
    match r.u8()? {
        1 => Ok(ItemOutcome::Deleted {
            bytes: r.u64()?,
            entries: r.u64()?,
            shared_files: r.u64()?,
        }),
        2 => {
            let code = r.u8()?;
            let extra = r.u32()?;
            let refusal = match code {
                1 => Refusal::Blocked,
                2 => Refusal::ProtectedNotConfirmed,
                3 => Refusal::SelectionRefused { driver: extra },
                4 => Refusal::BadPath,
                5 => Refusal::NotFound,
                6 => Refusal::CannotResolve,
                7 => Refusal::ChangedSinceScan,
                8 => Refusal::MountPoint,
                9 => Refusal::Overlaps { with: extra },
                10 => Refusal::NotRealPath,
                11 => Refusal::NotPermanent,
                12 => Refusal::RemoteVolume,
                _ => return Err(DecodeError::BadValue),
            };
            Ok(ItemOutcome::Refused(refusal))
        }
        3 => Ok(ItemOutcome::Failed {
            step: r.u8()?,
            os_code: r.i32()?,
            removed_bytes: r.u64()?,
            removed_entries: r.u64()?,
        }),
        _ => Err(DecodeError::BadValue),
    }
}

/// Which way a frame travels. A receiver decodes only the kinds that can come
/// its way, so a helper never spends memory on a batch the interface sends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Interface to helper: hello, scan, delete, bye.
    ToHelper,
    /// Helper to interface: hello, bye and everything the helper answers with.
    ToInterface,
}

/// Reads one frame body.
pub fn decode_body(body: &[u8], direction: Direction) -> Result<Message, DecodeError> {
    let mut r = Reader::new(body);
    let version = r.u8()?;
    if version != FRAME_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    let kind = r.u8()?;
    let allowed = match direction {
        Direction::ToHelper => matches!(kind, K_HELLO | K_SCAN | K_DELETE | K_BYE),
        Direction::ToInterface => matches!(kind, K_HELLO | K_BYE | K_BATCH..=K_DELETE_DONE),
    };
    if !allowed {
        return Err(DecodeError::UnexpectedKind(kind));
    }
    let message = match kind {
        K_HELLO => Message::Hello(Hello {
            version: r.u16()?,
            nonce: r.nonce()?,
        }),
        K_SCAN => {
            let volume = r.bytes(MAX_WIRE_PATH)?.to_vec();
            let backend = Backend::from_code(r.u8()?).ok_or(DecodeError::BadValue)?;
            let mounts = r.count(8, MAX_EXTRA_MOUNTS)?;
            let mut extra_mount_ids = Vec::with_capacity(mounts);
            for _ in 0..mounts {
                extra_mount_ids.push(r.u64()?);
            }
            let places_count = r.count(8, MAX_PLACES)?;
            let mut places = Vec::with_capacity(places_count);
            for _ in 0..places_count {
                let name = std::str::from_utf8(r.bytes(MAX_PLACE_NAME)?)
                    .map_err(|_| DecodeError::BadValue)?
                    .to_owned();
                let value = r.bytes(MAX_PLACE_VALUE)?.to_vec();
                places.push(Place { name, value });
            }
            Message::Scan(ScanRequest {
                volume,
                backend,
                extra_mount_ids,
                places,
            })
        }
        K_DELETE => {
            let permanent = r.bool()?;
            let count = r.count(MIN_DELETE_ITEM, MAX_DELETE_ITEMS)?;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let path = r.bytes(MAX_WIRE_PATH)?.to_vec();
                let confirmed_protected = r.bool()?;
                let seen = match r.u8()? {
                    0 => None,
                    1 => Some(Seen {
                        volume: r.u64()?,
                        file: r.u128()?,
                    }),
                    _ => return Err(DecodeError::BadValue),
                };
                items.push(DeleteItem {
                    path,
                    confirmed_protected,
                    seen,
                });
            }
            Message::Delete(DeleteRequest { permanent, items })
        }
        K_BYE => Message::Bye,
        K_BATCH => {
            let rest = r.take(r.remaining())?;
            Message::Batch(Batch::decode(rest).map_err(DecodeError::Batch)?)
        }
        K_PROGRESS => Message::Progress(get_progress(&mut r)?),
        K_SCAN_DONE => {
            let backend = Backend::from_code(r.u8()?).ok_or(DecodeError::BadValue)?;
            let end = match r.u8()? {
                0 => ScanEnd::Completed,
                1 => ScanEnd::Cancelled,
                _ => return Err(DecodeError::BadValue),
            };
            let fallback = match r.u8()? {
                0 => Fallback::None,
                1 => Fallback::Unavailable,
                2 => Fallback::TooLarge,
                3 => Fallback::PartialMount,
                4 => Fallback::NotSupported,
                5 => Fallback::MountNotPlaced,
                _ => return Err(DecodeError::BadValue),
            };
            Message::ScanDone(ScanDone {
                backend,
                end,
                fallback,
                progress: get_progress(&mut r)?,
            })
        }
        K_ERROR => {
            let code = ErrorCode::from_code(r.u8()?).ok_or(DecodeError::BadValue)?;
            let message = String::from_utf8_lossy(r.bytes(MAX_ERROR_TEXT)?).into_owned();
            Message::Error(ErrorReport { code, message })
        }
        K_DELETE_ITEM => Message::DeleteItem {
            index: r.u32()?,
            outcome: get_outcome(&mut r)?,
        },
        K_DELETE_DONE => Message::DeleteDone { items: r.u32()? },
        other => return Err(DecodeError::UnknownKind(other)),
    };
    r.finish()?;
    Ok(message)
}
