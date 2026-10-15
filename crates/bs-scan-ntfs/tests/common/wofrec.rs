//! What a file's own record says about its data streams, for the live
//! comparison.
//!
//! The system's stream listing hides the stream the overlay layer keeps a
//! compressed file's bytes in (`WofCompressedData`), so for the question
//! "does that stream hold clusters, or does it live inside the record?" the
//! record itself is the source that sees it. On a live volume the record is
//! fetched with `FSCTL_GET_NTFS_FILE_RECORD`; this module only reads the
//! bytes it returns, so it runs on every platform against records built by
//! the test kit.
//!
//! The bytes are treated as untrusted: every length and offset is checked
//! against the record before use, the number of attributes visited is capped,
//! and nothing here panics on a damaged record. A record that does not read
//! cleanly is an error, which leaves the difference it was fetched for
//! unexplained.
#![allow(dead_code)]

/// The name of the stream the overlay layer keeps compressed data in.
pub const WOF_NAME: &str = "WofCompressedData";

/// The record's sector size for the update sequence array.
const SECTOR: usize = 512;
/// Attributes visited at most. A record holds a few dozen at the very most.
const MAX_ATTRIBUTES: usize = 256;

const TYPE_DATA: u32 = 0x80;
const TYPE_ATTRIBUTE_LIST: u32 = 0x20;
const END: u32 = 0xFFFF_FFFF;

/// One `$DATA` attribute of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataStream {
    /// Empty for the file's main stream.
    pub name: String,
    /// The value is inside the record.
    pub resident: bool,
    /// The value's length when resident; the stream's real size otherwise.
    pub length: u64,
    /// Bytes of clusters the attribute says it occupies; 0 when resident.
    pub allocated: u64,
}

/// How the update sequence was found in the bytes handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixups {
    /// The sector ends held the check value and were restored from the array.
    Restored,
    /// The sector ends did not hold the check value: the record was already
    /// in its plain form.
    AlreadyPlain,
}

/// What was read from one record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordFacts {
    /// The record's own sequence number.
    pub sequence: u16,
    /// The record's own number, when it says (it is at offset 0x2C).
    pub number: Option<u32>,
    /// The record is an extension record of another file.
    pub is_extension: bool,
    /// The record has an attribute list, so some attributes of the file may
    /// be in other records.
    pub has_attribute_list: bool,
    pub fixups: Fixups,
    /// Every `$DATA` attribute in the record, in order.
    pub streams: Vec<DataStream>,
}

impl RecordFacts {
    /// The attributes named `WofCompressedData`.
    pub fn wof(&self) -> Vec<&DataStream> {
        self.streams.iter().filter(|s| s.name == WOF_NAME).collect()
    }

    /// The summed lengths of the named streams in this record.
    pub fn named_length(&self) -> u64 {
        self.streams
            .iter()
            .filter(|s| !s.name.is_empty())
            .map(|s| s.length)
            .fold(0, u64::saturating_add)
    }

    /// One line for the report.
    pub fn describe(&self) -> String {
        let streams: Vec<String> = self
            .streams
            .iter()
            .map(|s| {
                format!(
                    "{}: {} length {}{}",
                    if s.name.is_empty() { "(main)" } else { &s.name },
                    if s.resident {
                        "resident"
                    } else {
                        "in clusters"
                    },
                    s.length,
                    if s.resident {
                        String::new()
                    } else {
                        format!(", allocated {}", s.allocated)
                    }
                )
            })
            .collect();
        format!(
            "record seq {}{}{}, fixups {:?}; streams [{}]",
            self.sequence,
            if self.is_extension {
                ", extension record"
            } else {
                ""
            },
            if self.has_attribute_list {
                ", has an attribute list"
            } else {
                ""
            },
            self.fixups,
            streams.join("; ")
        )
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Undoes the update sequence on a copy of the record. The check value sits
/// at the start of the array; each sector's last two bytes hold it while the
/// record is stored, and the array holds what they were.
fn apply_fixups(raw: &[u8]) -> Result<(Vec<u8>, Fixups), String> {
    let bad = |why: &str| format!("the record is not readable: {why}");
    if raw.get(..4) != Some(b"FILE") {
        return Err(bad("it does not start with FILE"));
    }
    let usa_at = usize::from(u16_at(raw, 4).ok_or_else(|| bad("short header"))?);
    let usa_count = usize::from(u16_at(raw, 6).ok_or_else(|| bad("short header"))?);
    if usa_count < 2 {
        return Err(bad("the update sequence array is empty"));
    }
    // The array covers the record sector by sector, plus its check value.
    let sectors = usa_count - 1;
    if sectors.checked_mul(SECTOR) != Some(raw.len()) {
        return Err(bad(&format!(
            "{} sectors listed for {} bytes",
            sectors,
            raw.len()
        )));
    }
    let array_end = usa_at
        .checked_add(usa_count.checked_mul(2).ok_or_else(|| bad("array size"))?)
        .ok_or_else(|| bad("array end"))?;
    if usa_at < 0x2A || array_end > raw.len() {
        return Err(bad("the update sequence array is outside the record"));
    }
    let usn = u16_at(raw, usa_at).ok_or_else(|| bad("array"))?;
    let mut out = raw.to_vec();
    let mut matching = 0usize;
    for i in 1..usa_count {
        let tail = i * SECTOR - 2;
        if u16_at(raw, tail) == Some(usn) {
            matching += 1;
        }
    }
    if matching == 0 {
        return Ok((out, Fixups::AlreadyPlain));
    }
    if matching != sectors {
        return Err(bad("only some sector ends hold the check value"));
    }
    for i in 1..usa_count {
        let tail = i * SECTOR - 2;
        let saved = raw
            .get(usa_at + 2 * i..usa_at + 2 * i + 2)
            .ok_or_else(|| bad("array"))?;
        out.get_mut(tail..tail + 2)
            .ok_or_else(|| bad("sector end"))?
            .copy_from_slice(saved);
    }
    Ok((out, Fixups::Restored))
}

fn name_of(attr: &[u8]) -> Result<String, String> {
    let len = usize::from(*attr.get(9).ok_or("short attribute")?);
    if len == 0 {
        return Ok(String::new());
    }
    let at = usize::from(u16_at(attr, 10).ok_or("short attribute")?);
    let bytes = attr
        .get(at..at.checked_add(len * 2).ok_or("name end")?)
        .ok_or("the attribute's name is outside it")?;
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// Reads the data streams of the file record `raw`.
pub fn read_record(raw: &[u8]) -> Result<RecordFacts, String> {
    let (rec, fixups) = apply_fixups(raw)?;
    let bad = |why: &str| format!("the record is not readable: {why}");
    let sequence = u16_at(&rec, 0x10).ok_or_else(|| bad("short header"))?;
    let first = usize::from(u16_at(&rec, 0x14).ok_or_else(|| bad("short header"))?);
    let flags = u16_at(&rec, 0x16).ok_or_else(|| bad("short header"))?;
    let used = usize::try_from(u32_at(&rec, 0x18).ok_or_else(|| bad("short header"))?)
        .map_err(|_| bad("bytes in use"))?;
    let base = u64_at(&rec, 0x20).ok_or_else(|| bad("short header"))?;
    let number = u32_at(&rec, 0x2C);
    if flags & 1 == 0 {
        return Err(bad("the record is not in use"));
    }
    if used > rec.len() || first < 0x30 || first >= used {
        return Err(bad("attributes start outside the used part of the record"));
    }
    let mut facts = RecordFacts {
        sequence,
        number,
        is_extension: base & 0x0000_FFFF_FFFF_FFFF != 0,
        has_attribute_list: false,
        fixups,
        streams: Vec::new(),
    };
    let mut pos = first;
    let mut seen_end = false;
    for _ in 0..MAX_ATTRIBUTES {
        let ty = u32_at(&rec, pos).ok_or_else(|| bad("an attribute header is cut off"))?;
        if ty == END {
            seen_end = true;
            break;
        }
        let len = usize::try_from(u32_at(&rec, pos + 4).ok_or_else(|| bad("cut-off header"))?)
            .map_err(|_| bad("attribute length"))?;
        let end = pos.checked_add(len).ok_or_else(|| bad("attribute end"))?;
        if len < 0x18 || len % 8 != 0 || end > used {
            return Err(bad(&format!(
                "an attribute of length {len} at {pos} does not fit"
            )));
        }
        let attr = rec.get(pos..end).ok_or_else(|| bad("attribute"))?;
        if ty == TYPE_ATTRIBUTE_LIST {
            facts.has_attribute_list = true;
        }
        if ty == TYPE_DATA {
            let resident = *attr.get(8).ok_or_else(|| bad("attribute"))? == 0;
            let name = name_of(attr).map_err(|e| bad(&e))?;
            let stream = if resident {
                let length = u64::from(u32_at(attr, 0x10).ok_or_else(|| bad("resident header"))?);
                let at = usize::from(u16_at(attr, 0x14).ok_or_else(|| bad("resident header"))?);
                let value_end = at
                    .checked_add(usize::try_from(length).map_err(|_| bad("value length"))?)
                    .ok_or_else(|| bad("value end"))?;
                if value_end > attr.len() {
                    return Err(bad("a resident value is outside its attribute"));
                }
                DataStream {
                    name,
                    resident: true,
                    length,
                    allocated: 0,
                }
            } else {
                if attr.len() < 0x40 {
                    return Err(bad("a non-resident header is cut off"));
                }
                let lowest = u64_at(attr, 0x10).ok_or_else(|| bad("non-resident header"))?;
                // Only the first piece of a stream says how long it is.
                let (length, allocated) = if lowest == 0 {
                    (
                        u64_at(attr, 0x30).ok_or_else(|| bad("non-resident header"))?,
                        u64_at(attr, 0x28).ok_or_else(|| bad("non-resident header"))?,
                    )
                } else {
                    (0, 0)
                };
                DataStream {
                    name,
                    resident: false,
                    length,
                    allocated,
                }
            };
            facts.streams.push(stream);
        }
        pos = end;
    }
    if !seen_end {
        return Err(bad("no end marker within the attribute limit"));
    }
    Ok(facts)
}
