//! Reading a file's data streams from its record (`common::wofrec`), which is
//! how the live comparison tells a compressed stream held inside the record
//! from one held in clusters. The record is untrusted input: damaged ones
//! must be errors, never panics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::testkit::{NonRes, RecordBuilder, stamp, unstamp};
use common::wofrec::*;

const TYPE_ATTRIBUTE_LIST: u32 = 0x20;

fn builder() -> RecordBuilder {
    RecordBuilder::new(1024, 7)
}

fn main_stream(b: RecordBuilder, size: u64) -> RecordBuilder {
    b.nonres(&NonRes {
        ty: 0x80,
        name: String::new(),
        flags: 0,
        lowest_vcn: 0,
        highest_vcn: 0,
        unit: 0,
        allocated: 0,
        size,
        initialized: 0,
        compressed: None,
        runs: vec![0],
    })
}

#[test]
fn a_resident_overlay_stream_is_read_as_resident_with_its_length() {
    let rec = main_stream(builder(), 6384)
        .data_resident(WOF_NAME, &[7u8; 500])
        .build();
    let f = read_record(&rec).unwrap();
    assert_eq!(f.sequence, 7);
    assert_eq!(f.fixups, Fixups::Restored);
    assert!(!f.is_extension && !f.has_attribute_list);
    let wof = f.wof();
    assert_eq!(wof.len(), 1);
    assert_eq!(
        wof[0],
        &DataStream {
            name: WOF_NAME.into(),
            resident: true,
            length: 500,
            allocated: 0
        }
    );
    assert_eq!(f.named_length(), 500);
    assert!(
        f.describe().contains("resident length 500"),
        "{}",
        f.describe()
    );
}

#[test]
fn a_stream_in_clusters_is_read_as_not_resident_with_its_allocation() {
    let rec = main_stream(builder(), 6384)
        .nonres(&NonRes {
            ty: 0x80,
            name: WOF_NAME.into(),
            flags: 0,
            lowest_vcn: 0,
            highest_vcn: 0,
            unit: 0,
            allocated: 4096,
            size: 3000,
            initialized: 3000,
            compressed: None,
            runs: vec![0x11, 0x01, 0x10, 0],
        })
        .build();
    let f = read_record(&rec).unwrap();
    let wof = f.wof();
    assert_eq!(wof.len(), 1);
    assert_eq!(
        (wof[0].resident, wof[0].length, wof[0].allocated),
        (false, 3000, 4096)
    );
    assert!(f.describe().contains("allocated 4096"), "{}", f.describe());
}

#[test]
fn a_record_already_in_plain_form_is_read_too() {
    let mut rec = main_stream(builder(), 100)
        .data_resident(WOF_NAME, &[1u8; 40])
        .build();
    unstamp(&mut rec);
    let f = read_record(&rec).unwrap();
    assert_eq!(f.fixups, Fixups::AlreadyPlain);
    assert_eq!(f.wof().len(), 1);
}

#[test]
fn attribute_list_and_extension_are_noticed() {
    let rec = builder()
        .resident(TYPE_ATTRIBUTE_LIST, "", &[0u8; 8])
        .data_resident("x", &[1])
        .build();
    let f = read_record(&rec).unwrap();
    assert!(f.has_attribute_list);
    let rec = builder()
        .extension_of(40, 3)
        .data_resident("x", &[1])
        .build();
    assert!(read_record(&rec).unwrap().is_extension);
}

#[test]
fn hostile_records_are_errors_and_never_panics() {
    let good = main_stream(builder(), 6384)
        .data_resident(WOF_NAME, &[7u8; 100])
        .build();
    // Truncations of every length.
    for n in 0..good.len() {
        let _ = read_record(&good[..n]);
    }
    // Every single byte flipped, and every byte set to the extremes.
    for at in 0..good.len() {
        for v in [0x00u8, 0x01, 0x7F, 0x80, 0xFF] {
            let mut r = good.clone();
            r[at] = v;
            let _ = read_record(&r);
        }
    }
    // Specific cases.
    let mut r = good.clone();
    r[0] = b'X';
    assert!(read_record(&r).is_err(), "bad signature");
    let mut plain = good.clone();
    unstamp(&mut plain);
    // An attribute length that runs past the used part.
    let mut r = plain.clone();
    let first = usize::from(u16::from_le_bytes([r[0x14], r[0x15]]));
    r[first + 4..first + 8].copy_from_slice(&0x0010_0000u32.to_le_bytes());
    stamp(&mut r);
    assert!(read_record(&r).is_err(), "length past the record");
    // An attribute that ends past the used part of the record, though still
    // inside the record's bytes.
    let mut r = plain.clone();
    r[0x18..0x1C].copy_from_slice(&((first + 0x20) as u32).to_le_bytes());
    stamp(&mut r);
    assert!(read_record(&r).is_err(), "past the used part");
    // A zero-length attribute must not loop.
    let mut r = plain.clone();
    r[first + 4..first + 8].copy_from_slice(&0u32.to_le_bytes());
    stamp(&mut r);
    assert!(read_record(&r).is_err(), "zero length");
    // A resident value that runs out of its attribute.
    let mut r = plain.clone();
    let mut at = first;
    loop {
        let ty = u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
        let len = u32::from_le_bytes(r[at + 4..at + 8].try_into().unwrap()) as usize;
        if ty == 0x80 && r[at + 8] == 0 {
            r[at + 0x10..at + 0x14].copy_from_slice(&u32::MAX.to_le_bytes());
            break;
        }
        at += len;
    }
    stamp(&mut r);
    assert!(read_record(&r).is_err(), "value outside its attribute");
    // Sectors that do not all hold the check value.
    let mut r = good.clone();
    r[1022] ^= 0xFF;
    assert!(read_record(&r).is_err(), "torn record");
    // Not in use.
    let rec = builder().unused().data_resident("x", &[1]).build();
    assert!(read_record(&rec).is_err());
    // No end marker: the used part ends in the middle of attributes.
    let mut r = plain.clone();
    let used = u32::from_le_bytes(r[0x18..0x1C].try_into().unwrap()) as usize;
    r[used - 8..used - 4].copy_from_slice(&0u32.to_le_bytes());
    stamp(&mut r);
    assert!(read_record(&r).is_err(), "no end marker");
}
