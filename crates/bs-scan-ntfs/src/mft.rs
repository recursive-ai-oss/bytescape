//! Loading the master file table: finding it through its own first record
//! (and its extension records, when it is fragmented past one record),
//! reading it sequentially in large chunks, parsing the chunks on several
//! threads, and joining the pieces of each file.

use crate::boot::Geometry;
use crate::budget::Charge;
use crate::error::{Diagnostics, NtfsError, ProblemKind};
use crate::le::to_usize;
use crate::parse::{
    BaseOut, ChunkOut, ExtOut, FileRec, ListOut, MAX_NAMES_PER_FILE, NONE, NameRec, Parser,
    list_entries,
};
use crate::record::{Body, Peek, attrs, fix_and_parse, peek, split_ref, ty};
use crate::runlist::{self, Run};
use crate::source::ByteSource;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Most bytes read from the table at a time.
const CHUNK_BYTES: u64 = 4 << 20;
/// A chunk, and so each of the chunks in flight, is at most this fraction of
/// the memory limit, so a small limit means small chunks.
const CHUNK_LIMIT_SHARE: u64 = 64;
/// Most extension records the table's own first record may name.
pub(crate) const MAX_MFT_EXTENSIONS: usize = 1024;
/// Largest attribute list that is read from clusters, and the total read for
/// all of them. Lists held outside their record belong to files with
/// hundreds of fragments; a volume has few.
pub(crate) const MAX_LIST_BYTES: u64 = 64 << 20;
pub(crate) const LIST_BUDGET_BYTES: u64 = 256 << 20;
/// Memory one reference of a stored attribute list costs once joined: the
/// pair and a share of the table that holds them.
const REF_BYTES: u64 = 8 + 48;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_memory_bytes: u64,
    pub threads: usize,
}

/// The table as read.
pub(crate) struct Model {
    pub geo: Geometry,
    /// Indexed by record number; a record that is not in use has no
    /// `F_PRESENT`.
    pub files: Vec<FileRec>,
    pub names: Vec<NameRec>,
    pub pool: Vec<u8>,
    pub diag: Diagnostics,
    /// Records known to be lost.
    pub lost: u64,
    /// Bytes of the table that were read.
    pub mft_bytes: u64,
    /// Attribute lists whose extension records could not be checked.
    pub lists_unchecked: u64,
    /// Files that have an attribute list.
    pub lists: u64,
    /// Extension records joined to their files.
    pub extensions_joined: u64,
}

impl Model {
    pub fn name_bytes(&self, n: &NameRec) -> &[u8] {
        let start = to_usize(u64::from(n.off)).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::from(n.len));
        self.pool.get(start..end).unwrap_or(&[])
    }

    pub fn file(&self, rec: u32) -> Option<&FileRec> {
        self.files.get(to_usize(u64::from(rec))?)
    }

    /// The heap the model holds.
    pub fn bytes(&self) -> u64 {
        let files = (self.files.len() as u64).saturating_mul(std::mem::size_of::<FileRec>() as u64);
        let names = (self.names.len() as u64).saturating_mul(std::mem::size_of::<NameRec>() as u64);
        files
            .saturating_add(names)
            .saturating_add(self.pool.len() as u64)
    }
}

enum ReadFail {
    Io(io::Error),
    /// The range lies in a hole of the stream.
    Hole,
    /// The range lies beyond what the stream maps.
    Unmapped,
}

fn cluster_offset(geo: &Geometry, cluster: u64) -> Option<u64> {
    cluster.checked_mul(u64::from(geo.cluster_bytes))
}

/// Reads `buf.len()` bytes of the stream described by `runs` starting at
/// byte `voff` of the stream.
fn read_stream<S: ByteSource + ?Sized>(
    src: &S,
    geo: &Geometry,
    runs: &[Run],
    mut voff: u64,
    mut buf: &mut [u8],
) -> Result<(), ReadFail> {
    let cluster = u64::from(geo.cluster_bytes);
    while !buf.is_empty() {
        let vcn = voff.checked_div(cluster).ok_or(ReadFail::Unmapped)?;
        // The last run starting at or before the cluster.
        let at = runs.partition_point(|r| r.vcn <= vcn);
        let run = at
            .checked_sub(1)
            .and_then(|i| runs.get(i))
            .ok_or(ReadFail::Unmapped)?;
        let run_end = run.vcn.checked_add(run.len).ok_or(ReadFail::Unmapped)?;
        if vcn >= run_end {
            return Err(ReadFail::Unmapped);
        }
        let lcn = run.lcn.ok_or(ReadFail::Hole)?;
        let within = voff
            .checked_sub(cluster_offset(geo, run.vcn).ok_or(ReadFail::Unmapped)?)
            .ok_or(ReadFail::Unmapped)?;
        let run_bytes = cluster_offset(geo, run.len).ok_or(ReadFail::Unmapped)?;
        let avail = run_bytes.checked_sub(within).ok_or(ReadFail::Unmapped)?;
        let take = to_usize(avail).map_or(buf.len(), |a| a.min(buf.len()));
        let phys = cluster_offset(geo, lcn)
            .and_then(|p| p.checked_add(within))
            .ok_or(ReadFail::Unmapped)?;
        let (head, rest) = buf.split_at_mut(take);
        src.read_at(phys, head).map_err(ReadFail::Io)?;
        voff = voff.checked_add(take as u64).ok_or(ReadFail::Unmapped)?;
        buf = rest;
    }
    Ok(())
}

/// Reads an attribute stored in clusters (an attribute list), `size` bytes.
/// `room` is the most memory the read may take beyond what the caller has
/// charged; a list larger than it, or larger than [`MAX_LIST_BYTES`], is
/// refused before anything is allocated.
fn read_stored<S: ByteSource + ?Sized>(
    src: &S,
    geo: &Geometry,
    runs_bytes: &[u8],
    size: u64,
    room: u64,
) -> Result<Vec<u8>, ()> {
    if size > MAX_LIST_BYTES.min(room) {
        return Err(());
    }
    let mut runs = Vec::new();
    runlist::decode(runs_bytes, 0, geo.clusters, |r| runs.push(r)).map_err(|_| ())?;
    // Nothing is allocated for bytes the list's clusters do not hold: the
    // runs lie inside the volume, so this ties the allocation to the size of
    // the device and not only to the field.
    let held = runs
        .iter()
        .filter(|r| r.lcn.is_some())
        .try_fold(0u64, |sum, r| sum.checked_add(r.len))
        .and_then(|clusters| clusters.checked_mul(u64::from(geo.cluster_bytes)))
        .ok_or(())?;
    if size > held {
        return Err(());
    }
    let n = to_usize(size).ok_or(())?;
    let mut data = vec![0u8; n];
    // A hole or a gap in the runs is as unreadable as a failed read; a list
    // that reads as zeros but is meant to say something is caught by the
    // parser.
    read_stream(src, geo, &runs, 0, &mut data).map_err(|_| ())?;
    Ok(data)
}

/// The table's runs, from its first record and any extension records.
struct MftMap {
    runs: Vec<Run>,
    size: u64,
    initialized: u64,
    covered: u64,
}

fn read_record<S: ByteSource + ?Sized>(
    src: &S,
    geo: &Geometry,
    runs: &[Run],
    no: u64,
) -> Result<Vec<u8>, NtfsError> {
    let size = to_usize(u64::from(geo.record_bytes)).ok_or(NtfsError::BadMft("record size"))?;
    let mut buf = vec![0u8; size];
    let at = no
        .checked_mul(u64::from(geo.record_bytes))
        .ok_or(NtfsError::BadMft("record number"))?;
    match read_stream(src, geo, runs, at, &mut buf) {
        Ok(()) => Ok(buf),
        Err(ReadFail::Io(e)) => Err(NtfsError::Io(e)),
        Err(_) => Err(NtfsError::BadMft(
            "a record of the table lies outside the part of it that is mapped",
        )),
    }
}

/// Appends the run list `bytes`, whose first run is virtual cluster `lowest`,
/// to `runs`. The pieces of a table's run list must follow each other without
/// a gap, so `lowest` must be where the runs so far end.
fn append_piece(
    runs: &mut Vec<Run>,
    next: &mut u64,
    lowest: u64,
    bytes: &[u8],
    geo: &Geometry,
) -> Result<(), NtfsError> {
    if lowest != *next {
        return Err(NtfsError::BadMft("the table's run lists do not join up"));
    }
    let t = runlist::decode(bytes, lowest, geo.clusters, |r| runs.push(r))
        .map_err(|_| NtfsError::BadMft("the table's run list is damaged"))?;
    *next = next
        .checked_add(t.covered().ok_or(NtfsError::BadMft("run list overflow"))?)
        .ok_or(NtfsError::BadMft("run list overflow"))?;
    Ok(())
}

/// Whether any two runs that lie on the volume cover the same cluster. A
/// table that maps a cluster twice would be read as several different
/// records at once.
fn runs_overlap(runs: &[Run]) -> bool {
    let mut spans: Vec<(u64, u64)> = runs
        .iter()
        .filter_map(|r| {
            let start = r.lcn?;
            Some((start, start.checked_add(r.len)?))
        })
        .collect();
    spans.sort_unstable();
    let mut end_so_far = 0u64;
    for (start, end) in spans {
        if start < end_so_far {
            return true;
        }
        end_so_far = end_so_far.max(end);
    }
    false
}

fn locate_mft<S: ByteSource + ?Sized>(
    src: &S,
    geo: &Geometry,
    list_room: u64,
) -> Result<MftMap, NtfsError> {
    let size = to_usize(u64::from(geo.record_bytes)).ok_or(NtfsError::BadMft("record size"))?;
    let mut rec0 = vec![0u8; size];
    let at = cluster_offset(geo, geo.mft_lcn).ok_or(NtfsError::BadMft("table position"))?;
    src.read_at(at, &mut rec0)?;
    if peek(&rec0) != Peek::Used {
        return Err(NtfsError::BadMft("the first record is not in use"));
    }
    let header =
        fix_and_parse(&mut rec0).map_err(|_| NtfsError::BadMft("the first record is damaged"))?;
    if header.base.is_some() {
        return Err(NtfsError::BadMft("the first record is not a base record"));
    }
    if header.number.is_some_and(|n| n != 0) {
        return Err(NtfsError::BadMft("the first record is another record"));
    }

    // (lowest VCN, mapping pairs) of every piece of the table's data that is
    // in the first record.
    let mut pieces: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut head: Option<(u64, u64)> = None;
    let mut list: Option<Vec<u8>> = None;
    for item in attrs(&rec0, &header) {
        let a =
            item.map_err(|_| NtfsError::BadMft("an attribute of the first record is damaged"))?;
        match (a.ty, a.body) {
            (ty::DATA, Body::NonResident(n)) if a.is_unnamed() => {
                if n.lowest_vcn == 0 {
                    head = Some((n.size, n.initialized));
                }
                pieces.push((n.lowest_vcn, n.runs.to_vec()));
            }
            (ty::DATA, Body::Resident(_)) if a.is_unnamed() => {
                return Err(NtfsError::BadMft("the table's data is not in clusters"));
            }
            (ty::ATTRIBUTE_LIST, Body::Resident(v)) => list = Some(v.to_vec()),
            (ty::ATTRIBUTE_LIST, Body::NonResident(n)) => {
                list = Some(
                    read_stored(src, geo, n.runs, n.size, list_room)
                        .map_err(|()| NtfsError::BadMft("the table's attribute list"))?,
                );
            }
            _ => {}
        }
    }

    pieces.sort_by_key(|p| p.0);
    let mut runs: Vec<Run> = Vec::new();
    let mut next = 0u64;
    for (lowest, bytes) in &pieces {
        append_piece(&mut runs, &mut next, *lowest, bytes, geo)?;
    }

    // A table fragmented beyond one record keeps the rest of its run list in
    // extension records, which lie in the part of the table already known.
    // They are taken in order of the clusters they describe, each decoded
    // once, so the work is in proportion to the runs and not to the square
    // of the records.
    if let Some(list) = list {
        let mut wanted: BTreeSet<(u64, u64)> = BTreeSet::new();
        let mut too_many = false;
        list_entries(&list, |e| {
            let (r, _) = split_ref(e.reference);
            if e.ty == ty::DATA && !e.named && r != 0 {
                wanted.insert((e.lowest_vcn, r));
                too_many |= wanted.len() > MAX_MFT_EXTENSIONS;
            }
        })
        .map_err(|()| NtfsError::BadMft("the table's attribute list is damaged"))?;
        if too_many {
            return Err(NtfsError::BadMft("too many extension records"));
        }
        for (lowest, r) in wanted {
            let mut buf = read_record(src, geo, &runs, r)?;
            let h = fix_and_parse(&mut buf)
                .map_err(|_| NtfsError::BadMft("an extension record is damaged"))?;
            if h.base.map(|(b, _)| b) != Some(0) {
                return Err(NtfsError::BadMft("an extension record is not the table's"));
            }
            if h.number.is_some_and(|n| u64::from(n) != r) {
                return Err(NtfsError::BadMft("an extension record is another record"));
            }
            let mut found = false;
            for item in attrs(&buf, &h) {
                let a = item.map_err(|_| NtfsError::BadMft("an extension record is damaged"))?;
                if let (ty::DATA, Body::NonResident(n)) = (a.ty, a.body)
                    && a.is_unnamed()
                    && n.lowest_vcn == lowest
                {
                    append_piece(&mut runs, &mut next, n.lowest_vcn, n.runs, geo)?;
                    found = true;
                }
            }
            if !found {
                return Err(NtfsError::BadMft("an extension record lacks its run list"));
            }
        }
    }

    if runs_overlap(&runs) {
        return Err(NtfsError::BadMft("the table's runs overlap"));
    }
    let (size, initialized) = head.ok_or(NtfsError::BadMft("the table has no first run list"))?;
    Ok(MftMap {
        runs,
        size,
        initialized,
        covered: next,
    })
}

struct Job {
    first: u64,
    bytes: Vec<u8>,
    /// Records of this chunk that could not be read, and why.
    lost: Vec<(u64, ProblemKind)>,
}

fn parse_job(parser: &mut Parser<'_>, mut job: Job) -> ChunkOut {
    let mut out = ChunkOut {
        first: job.first,
        ..ChunkOut::default()
    };
    for (rec, kind) in &job.lost {
        out.diag.note(*kind, *rec, 0);
        out.lost = out.lost.saturating_add(1);
    }
    parser.chunk(&mut job.bytes, job.first, &mut out);
    out
}

/// Reads chunk `c` of the table. A stretch that cannot be read as a whole is
/// read record by record, so only the records that fail are lost. `None` when
/// the chunk cannot be addressed at all.
fn read_job<S: ByteSource + ?Sized>(
    src: &S,
    geo: &Geometry,
    runs: &[Run],
    record_bytes: u64,
    records: u64,
    chunk_records: u64,
    c: u64,
) -> Option<Job> {
    let first = c.checked_mul(chunk_records)?;
    let count = chunk_records.min(records.checked_sub(first)?);
    let mut bytes = vec![0u8; to_usize(count.checked_mul(record_bytes)?)?];
    let mut lost = Vec::new();
    let at = first.checked_mul(record_bytes)?;
    if read_stream(src, geo, runs, at, &mut bytes).is_err() {
        let size = to_usize(record_bytes)?;
        for (i, rec) in bytes.chunks_exact_mut(size).enumerate() {
            let no = first.saturating_add(i as u64);
            let from = no.checked_mul(record_bytes)?;
            match read_stream(src, geo, runs, from, rec) {
                Ok(()) => {}
                Err(ReadFail::Hole) => {
                    rec.fill(0);
                    lost.push((no, ProblemKind::MftHole));
                }
                Err(_) => {
                    rec.fill(0);
                    lost.push((no, ProblemKind::MftUnreadable));
                }
            }
        }
    }
    Some(Job { first, bytes, lost })
}

const UNADDRESSABLE: NtfsError = NtfsError::BadMft("a stretch of the table cannot be addressed");
/// The three places the memory limit is enforced give three messages, so a
/// refusal says which stage ran out and the tests can tell them apart.
const OVER_LIMIT_READ: NtfsError =
    NtfsError::TooLarge("the table's names and attributes need more memory than the limit allows");
const OVER_LIMIT_JOIN: NtfsError = NtfsError::TooLarge(
    "joining the table's names and attribute lists needs more memory than the limit allows",
);

/// Reads and parses the table. `Ok(None)` when cancelled.
pub(crate) fn load<S: ByteSource + ?Sized>(
    src: &S,
    geo: Geometry,
    limits: Limits,
    cancelled: &(dyn Fn() -> bool + Sync),
    diag: &mut Diagnostics,
) -> Result<Option<Model>, NtfsError> {
    let limit = limits.max_memory_bytes;
    let map = locate_mft(src, &geo, limit / 4)?;
    let record_bytes = u64::from(geo.record_bytes);
    let mapped = cluster_offset(&geo, map.covered).ok_or(NtfsError::BadMft("size"))?;
    let readable = map.initialized.min(map.size).min(mapped);
    let records = readable
        .checked_div(record_bytes)
        .ok_or(NtfsError::BadMft("record size"))?;
    if records == 0 {
        return Err(NtfsError::BadMft("the table holds no records"));
    }
    if records > u64::from(u32::MAX) {
        return Err(NtfsError::TooLarge("more than 2^32 records"));
    }
    let chunks_wanted = records.div_ceil(CHUNK_BYTES.checked_div(record_bytes).unwrap_or(1).max(1));
    let threads = limits.threads.max(1);
    let parallel = threads > 1 && chunks_wanted > 1;
    // Chunks held at once: those queued for the workers, those being parsed,
    // and the one being read.
    let in_flight = if parallel {
        (threads as u64).saturating_mul(3).saturating_add(1)
    } else {
        1
    };
    let chunk_bytes = (limit / CHUNK_LIMIT_SHARE)
        .checked_div(in_flight)
        .unwrap_or(0)
        .clamp(record_bytes, CHUNK_BYTES.max(record_bytes));
    let chunk_records = chunk_bytes.checked_div(record_bytes).unwrap_or(1).max(1);
    let chunks = records.div_ceil(chunk_records);
    let n_records = to_usize(records).ok_or(NtfsError::TooLarge("record count"))?;

    // What is certain before any record is read: the file table, and the
    // chunks that are in flight.
    let fixed = records
        .saturating_mul(std::mem::size_of::<FileRec>() as u64)
        .saturating_add(
            in_flight
                .saturating_mul(chunk_records)
                .saturating_mul(record_bytes),
        );
    if fixed > limit {
        return Err(NtfsError::TooLarge(
            "the table needs more memory than the limit allows",
        ));
    }
    let charge = Charge::new(limit, fixed);

    let mut outs: Vec<ChunkOut> = Vec::new();
    let mut stopped = false;
    let mut unaddressable = false;
    {
        let geo_ref = &geo;
        let charge_ref = &charge;
        let make_job = |c: u64| -> Option<Job> {
            read_job(
                src,
                geo_ref,
                &map.runs,
                record_bytes,
                records,
                chunk_records,
                c,
            )
        };

        if !parallel {
            let mut parser = Parser::new(geo_ref, charge_ref);
            for c in 0..chunks {
                if cancelled() {
                    stopped = true;
                    break;
                }
                if charge.exceeded() {
                    break;
                }
                let Some(job) = make_job(c) else {
                    unaddressable = true;
                    break;
                };
                outs.push(parse_job(&mut parser, job));
            }
        } else {
            let (job_tx, job_rx) = mpsc::sync_channel::<Job>(threads.saturating_mul(2));
            let job_rx = Arc::new(Mutex::new(job_rx));
            let (out_tx, out_rx) = mpsc::channel::<ChunkOut>();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    let job_rx = Arc::clone(&job_rx);
                    let out_tx = out_tx.clone();
                    scope.spawn(move || {
                        let mut parser = Parser::new(geo_ref, charge_ref);
                        loop {
                            let job = {
                                let guard = job_rx
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                guard.recv()
                            };
                            let Ok(job) = job else { break };
                            if out_tx.send(parse_job(&mut parser, job)).is_err() {
                                break;
                            }
                        }
                    });
                }
                drop(out_tx);
                // Only the workers hold the receiver now, so that if they all
                // stop, sending fails instead of waiting for room forever.
                drop(job_rx);
                for c in 0..chunks {
                    if cancelled() {
                        stopped = true;
                        break;
                    }
                    if charge.exceeded() {
                        break;
                    }
                    let Some(job) = make_job(c) else {
                        unaddressable = true;
                        break;
                    };
                    if job_tx.send(job).is_err() {
                        break;
                    }
                }
                drop(job_tx);
                outs.extend(out_rx.iter());
            });
        }
    }
    if stopped {
        return Ok(None);
    }
    if unaddressable {
        return Err(UNADDRESSABLE);
    }
    if charge.exceeded() {
        return Err(OVER_LIMIT_READ);
    }
    outs.sort_by_key(|o| o.first);
    merge(src, geo, n_records, outs, map.size, diag, &charge).map(Some)
}

fn merge<S: ByteSource + ?Sized>(
    src: &S,
    geo: Geometry,
    n_records: usize,
    outs: Vec<ChunkOut>,
    mft_bytes: u64,
    diag: &mut Diagnostics,
    charge: &Charge,
) -> Result<Model, NtfsError> {
    let total_names: usize = outs.iter().map(|o| o.names.len()).sum();
    let total_pool: usize = outs.iter().map(|o| o.pool.len()).sum();
    let total_exts: usize = outs.iter().map(|o| o.exts.len()).sum();
    // The merge copies every name and extension record while the chunk
    // outputs still exist, so for a moment the read holds both.
    let copy_bytes = (total_names as u64)
        .saturating_mul(std::mem::size_of::<NameRec>() as u64)
        .saturating_add(total_pool as u64)
        .saturating_add((total_exts as u64).saturating_mul(std::mem::size_of::<ExtOut>() as u64));
    if !charge.add(copy_bytes) {
        return Err(OVER_LIMIT_JOIN);
    }
    let mut files: Vec<FileRec> = Vec::new();
    files
        .try_reserve_exact(n_records)
        .map_err(|_| NtfsError::TooLarge("cannot allocate the file table"))?;
    files.resize(n_records, FileRec::default());
    let mut names: Vec<NameRec> = Vec::new();
    let mut pool: Vec<u8> = Vec::new();
    let mut exts: Vec<ExtOut> = Vec::new();
    names
        .try_reserve_exact(total_names)
        .map_err(|_| NtfsError::TooLarge("cannot allocate the name table"))?;
    pool.try_reserve_exact(total_pool)
        .map_err(|_| NtfsError::TooLarge("cannot allocate the name pool"))?;
    exts.try_reserve_exact(total_exts)
        .map_err(|_| NtfsError::TooLarge("cannot allocate the extension table"))?;
    if u32::try_from(total_pool).is_err() || u32::try_from(total_names).is_err() {
        return Err(NtfsError::TooLarge("more names than can be numbered"));
    }

    let mut lost = 0u64;
    let mut lists: HashMap<u32, Vec<(u32, u16)>> = HashMap::new();
    let mut stored: Vec<(u32, u64, Vec<u8>)> = Vec::new();
    let mut broken: Vec<u32> = Vec::new();
    // What the joined lists hold on to, beyond the chunk outputs.
    let mut kept_lists = 0u64;
    for out in outs {
        let nb = u32::try_from(names.len()).map_err(|_| NtfsError::TooLarge("names"))?;
        let pb = u32::try_from(pool.len()).map_err(|_| NtfsError::TooLarge("names"))?;
        let rebase = |i: u32| {
            if i == NONE {
                NONE
            } else {
                i.saturating_add(nb)
            }
        };
        for n in &out.names {
            names.push(NameRec {
                parent: n.parent,
                next: rebase(n.next),
                off: n.off.saturating_add(pb),
                len: n.len,
                ns: n.ns,
            });
        }
        pool.extend_from_slice(&out.pool);
        diag.absorb(&out.diag);
        lost = lost.saturating_add(out.lost);
        for BaseOut { rec, file } in out.bases {
            let mut f = file;
            f.name_head = rebase(f.name_head);
            if let Some(slot) = files.get_mut(to_usize(u64::from(rec)).unwrap_or(usize::MAX)) {
                *slot = f;
            }
        }
        for mut e in out.exts {
            e.name_head = rebase(e.name_head);
            e.name_last = rebase(e.name_last);
            exts.push(e);
        }
        for l in out.lists {
            match l {
                ListOut::Refs(rec, refs) => {
                    kept_lists =
                        kept_lists.saturating_add((refs.len() as u64).saturating_mul(REF_BYTES));
                    lists.insert(rec, refs);
                }
                ListOut::Stored { rec, size, runs } => stored.push((rec, size, runs)),
                ListOut::Broken(rec) => broken.push(rec),
            }
        }
    }

    let mut model = Model {
        geo,
        files,
        names,
        pool,
        diag: Diagnostics::default(),
        lost,
        mft_bytes,
        lists_unchecked: 0,
        lists: 0,
        extensions_joined: 0,
    };
    model.lists = (lists.len() as u64)
        .saturating_add(stored.len() as u64)
        .saturating_add(broken.len() as u64);

    // Attribute lists kept in clusters: read them, within a budget of bytes
    // read and of memory held.
    let mut spent = 0u64;
    for (rec, size, runs) in stored {
        let room = charge
            .limit()
            .saturating_sub(model.bytes())
            .saturating_sub(kept_lists)
            .saturating_sub(
                (exts.capacity() as u64).saturating_mul(std::mem::size_of::<ExtOut>() as u64),
            );
        let within = spent.saturating_add(size) <= LIST_BUDGET_BYTES && size <= room;
        let parsed = if within {
            spent = spent.saturating_add(size);
            read_stored(src, &geo, &runs, size, room)
                .ok()
                .and_then(|bytes| {
                    let mut refs: Vec<(u32, u16)> = Vec::new();
                    list_entries(&bytes, |e| {
                        let (r, s) = split_ref(e.reference);
                        if r != u64::from(rec) {
                            refs.push((u32::try_from(r).unwrap_or(u32::MAX), s));
                        }
                    })
                    .ok()?;
                    refs.sort_unstable();
                    refs.dedup_by_key(|r| r.0);
                    Some(refs)
                })
        } else {
            model
                .diag
                .note(ProblemKind::ReadBudgetExceeded, u64::from(rec), size);
            None
        };
        match parsed {
            Some(refs) => {
                kept_lists =
                    kept_lists.saturating_add((refs.len() as u64).saturating_mul(REF_BYTES));
                if model.bytes().saturating_add(kept_lists) > charge.limit() {
                    return Err(OVER_LIMIT_JOIN);
                }
                lists.insert(rec, refs);
            }
            None => {
                if within {
                    model
                        .diag
                        .note(ProblemKind::AttributeListCorrupt, u64::from(rec), 1);
                }
                broken.push(rec);
            }
        }
    }
    for rec in broken {
        if let Some(f) = model
            .files
            .get_mut(to_usize(u64::from(rec)).unwrap_or(usize::MAX))
        {
            f.flags |= crate::parse::F_DAMAGED;
            model.lists_unchecked = model.lists_unchecked.saturating_add(1);
        }
    }

    // Join extension records to their files, if the file's list names them.
    for e in exts {
        let base = to_usize(e.base).unwrap_or(usize::MAX);
        let idx = u32::try_from(e.base).unwrap_or(u32::MAX);
        let listed = model
            .files
            .get(base)
            .filter(|f| f.present() && f.seq == e.base_seq)
            .and_then(|_| lists.get(&idx))
            .and_then(|refs| {
                refs.binary_search_by_key(&e.rec, |r| r.0)
                    .ok()
                    .and_then(|i| refs.get(i))
            })
            .is_some_and(|&(_, seq)| seq == 0 || seq == e.seq);
        if !listed {
            model
                .diag
                .note(ProblemKind::ExtensionUnlisted, u64::from(e.rec), e.base);
            continue;
        }
        let Some(file) = model.files.get_mut(base) else {
            continue;
        };
        file.c.add(&e.c);
        file.ext_seen = file.ext_seen.saturating_add(1);
        model.extensions_joined = model.extensions_joined.saturating_add(1);
        if e.name_head != NONE {
            // A file has only so many names. What lies past the limit is
            // dropped, and the file says so.
            let total = usize::from(file.name_count).saturating_add(usize::from(e.name_count));
            if total > MAX_NAMES_PER_FILE {
                file.flags |= crate::parse::F_DAMAGED;
                model.diag.note(
                    ProblemKind::TooManyNames,
                    u64::from(idx),
                    MAX_NAMES_PER_FILE as u64,
                );
                continue;
            }
            file.name_count = u16::try_from(total).unwrap_or(u16::MAX);
            if let Some(last) = model
                .names
                .get_mut(to_usize(u64::from(e.name_last)).unwrap_or(usize::MAX))
            {
                last.next = file.name_head;
            }
            file.name_head = e.name_head;
        }
    }
    // Every extension record a list names must have been found.
    for (&rec, refs) in &lists {
        let Some(f) = model
            .files
            .get_mut(to_usize(u64::from(rec)).unwrap_or(usize::MAX))
        else {
            continue;
        };
        let want = refs.len();
        if usize::from(f.ext_seen) < want {
            f.flags |= crate::parse::F_DAMAGED;
            model.diag.note(
                ProblemKind::ExtensionMissing,
                u64::from(rec),
                (want as u64).saturating_sub(u64::from(f.ext_seen)),
            );
        }
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;
    use crate::testkit::{Spec, Volume};

    fn load_all(img: &[u8], threads: usize) -> Model {
        let geo = crate::boot::parse(&img[..512], img.len() as u64).unwrap();
        let mut diag = Diagnostics::default();
        let limits = Limits {
            max_memory_bytes: 1 << 30,
            threads,
        };
        load(img, geo, limits, &|| false, &mut diag)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn records_land_at_their_numbers() {
        let mut v = Volume::new(Spec::default());
        let a = v.file("a.txt", 5, 100);
        let b = v.dir("sub", 5);
        let c = v.file("c.bin", b, 70_000);
        let img = v.finish();
        for threads in [1, 3] {
            let m = load_all(&img, threads);
            for rec in [a, b, c] {
                assert!(m.files[rec as usize].present(), "{rec}");
            }
            assert!(m.files[b as usize].is_dir());
            assert_eq!(m.files[a as usize].c.logical, 100);
            assert_eq!(m.files[a as usize].c.alloc, 0, "resident data");
            assert_eq!(m.files[c as usize].c.logical, 70_000);
            assert_eq!(m.files[c as usize].c.alloc, 18 * 4096);
            assert_eq!(m.lost, 0);
        }
    }

    #[test]
    fn a_fragmented_table_is_followed_run_by_run() {
        let mut v = Volume::new(Spec {
            mft_fragments: 5,
            ..Spec::default()
        });
        let mut recs = Vec::new();
        for i in 0..200 {
            recs.push(v.file(&format!("f{i}"), 5, 10 + i));
        }
        let img = v.finish();
        let m = load_all(&img, 2);
        for (i, rec) in recs.iter().enumerate() {
            assert_eq!(m.files[*rec as usize].c.logical, 10 + i as u64);
        }
    }

    #[test]
    fn a_table_with_its_run_list_in_an_extension_record() {
        let mut v = Volume::new(Spec {
            mft_fragments: 6,
            mft_runs_in_extension: true,
            ..Spec::default()
        });
        let mut recs = Vec::new();
        for i in 0..120 {
            recs.push(v.file(&format!("g{i}"), 5, i));
        }
        let img = v.finish();
        let m = load_all(&img, 1);
        for (i, rec) in recs.iter().enumerate() {
            assert!(m.files[*rec as usize].present());
            assert_eq!(m.files[*rec as usize].c.logical, i as u64);
        }
    }
}
