//! Tests of the protocol: every message round-trips, every malformed frame is
//! refused without a panic or a large allocation, and the client checks the
//! order of what the helper sends.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![allow(clippy::cast_possible_truncation)]

use super::*;
use bs_tree::{Batch, Category, DirKey, Meta, NodeFlags};
use proptest::prelude::*;
use std::io::{Cursor, Read, Write};

const NONCE: [u8; NONCE_LEN] = [7; NONCE_LEN];

fn batch() -> Batch {
    let mut b = Batch::new();
    let meta = Meta {
        logical: 10,
        allocated: 4096,
        mtime: 5,
        exclusive: 0,
        flags: NodeFlags::default(),
        category: Category::OTHER,
        link: None,
        failure: None,
    };
    b.dir(DirKey(0), DirKey::NONE, b"root", &meta).unwrap();
    b.file(bs_tree::NodeKind::File, DirKey(0), b"a", &meta)
        .unwrap();
    b.listing_done(DirKey(0));
    b
}

fn progress() -> Progress {
    Progress {
        allocated_bytes: 1,
        logical_bytes: 2,
        files: 3,
        dirs: 4,
        unreadable: 5,
        vanished: 6,
        batches: 7,
        estimated: 8,
        os_used_bytes: 9,
    }
}

fn samples() -> Vec<Message> {
    vec![
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            nonce: NONCE,
        }),
        Message::Scan(ScanRequest {
            volume: b"/".to_vec(),
            backend: Backend::Ext4,
            extra_mount_ids: vec![1, 2, u64::MAX],
            places: vec![Place {
                name: "HOME".into(),
                value: b"/home/a".to_vec(),
            }],
        }),
        Message::Delete(DeleteRequest {
            permanent: true,
            items: vec![
                DeleteItem {
                    path: b"/x/y".to_vec(),
                    confirmed_protected: true,
                    seen: Some(Seen {
                        volume: 3,
                        file: u128::MAX,
                    }),
                },
                DeleteItem {
                    path: vec![0xff, 0xfe],
                    confirmed_protected: false,
                    seen: None,
                },
            ],
        }),
        Message::Bye,
        Message::Batch(batch()),
        Message::Progress(progress()),
        Message::ScanDone(ScanDone {
            backend: Backend::Walker,
            end: ScanEnd::Cancelled,
            fallback: Fallback::TooLarge,
            progress: progress(),
        }),
        Message::Error(ErrorReport {
            code: ErrorCode::Refused,
            message: "no".into(),
        }),
        Message::DeleteItem {
            index: 4,
            outcome: ItemOutcome::Deleted {
                bytes: 1,
                entries: 2,
                shared_files: 3,
            },
        },
        Message::DeleteItem {
            index: 5,
            outcome: ItemOutcome::Refused(Refusal::Overlaps { with: 9 }),
        },
        Message::DeleteItem {
            index: 6,
            outcome: ItemOutcome::Failed {
                step: 2,
                os_code: -13,
                removed_bytes: 7,
                removed_entries: 8,
            },
        },
        Message::DeleteDone { items: 7 },
    ]
}

fn direction_of(message: &Message) -> Direction {
    match message {
        Message::Hello(_) | Message::Bye => Direction::ToHelper,
        Message::Scan(_) | Message::Delete(_) => Direction::ToHelper,
        _ => Direction::ToInterface,
    }
}

fn frame(message: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    write_message(&mut out, message).unwrap();
    out
}

fn body(message: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    encode_body(message, &mut out).unwrap();
    out
}

fn scan_request() -> ScanRequest {
    ScanRequest {
        volume: b"/".to_vec(),
        backend: Backend::Auto,
        extra_mount_ids: vec![],
        places: vec![],
    }
}

#[test]
fn every_message_round_trips_through_a_frame() {
    for message in samples() {
        let bytes = frame(&message);
        let mut reader =
            FrameReader::new(Cursor::new(bytes), direction_of(&message), MAX_FRAME_LEN);
        assert_eq!(reader.read().unwrap(), Some(message.clone()), "{message:?}");
        assert!(reader.read().unwrap().is_none(), "clean end");
    }
}

#[test]
fn the_length_prefix_counts_everything_after_itself() {
    let bytes = frame(&Message::Bye);
    assert_eq!(bytes, [2, 0, 0, 0, FRAME_VERSION, 4]);
}

#[test]
fn hello_travels_both_ways() {
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        nonce: NONCE,
    });
    let b = body(&hello);
    assert_eq!(decode_body(&b, Direction::ToHelper).unwrap(), hello);
    assert_eq!(decode_body(&b, Direction::ToInterface).unwrap(), hello);
}

#[test]
fn the_helper_decodes_only_requests() {
    // A tree batch can be large; a helper must not even try to decode one.
    for message in samples() {
        let b = body(&message);
        let allowed = matches!(
            message,
            Message::Hello(_) | Message::Scan(_) | Message::Delete(_) | Message::Bye
        );
        let result = decode_body(&b, Direction::ToHelper);
        assert_eq!(result.is_ok(), allowed, "{message:?}");
        if !allowed {
            assert!(matches!(result, Err(DecodeError::UnexpectedKind(_))));
        }
    }
    // The interface does not accept requests as answers, either.
    for message in [
        Message::Scan(scan_request()),
        Message::Delete(DeleteRequest {
            permanent: true,
            items: vec![],
        }),
    ] {
        assert!(decode_body(&body(&message), Direction::ToInterface).is_err());
    }
}

#[test]
fn an_unknown_version_is_refused() {
    let mut b = body(&Message::Bye);
    b[0] = FRAME_VERSION + 1;
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::UnsupportedVersion(FRAME_VERSION + 1))
    );
    b[0] = 0;
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::UnsupportedVersion(0))
    );
}

#[test]
fn an_unknown_kind_is_refused() {
    for kind in [0u8, 5, 15, 22, 255] {
        let b = [FRAME_VERSION, kind];
        assert!(decode_body(&b, Direction::ToHelper).is_err(), "{kind}");
        assert!(decode_body(&b, Direction::ToInterface).is_err(), "{kind}");
    }
}

#[test]
fn bad_lengths_are_refused_before_the_body_is_read() {
    // A reader that fails the test if anything past the prefix is requested.
    struct PrefixOnly {
        prefix: Vec<u8>,
        pos: usize,
    }
    impl Read for PrefixOnly {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            assert!(self.pos < self.prefix.len(), "read past the length prefix");
            let n = buf.len().min(self.prefix.len() - self.pos);
            buf[..n].copy_from_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    for declared in [0u32, 1, MAX_REQUEST_FRAME_LEN as u32 + 1, u32::MAX, 1 << 31] {
        let mut reader = FrameReader::new(
            PrefixOnly {
                prefix: declared.to_le_bytes().to_vec(),
                pos: 0,
            },
            Direction::ToHelper,
            MAX_REQUEST_FRAME_LEN,
        );
        match reader.read() {
            Err(ReadError::BadLength(n)) => assert_eq!(n, declared),
            other => panic!("{declared}: {other:?}"),
        }
    }
}

#[test]
fn a_reader_never_goes_over_the_hard_maximum_whatever_limit_it_is_given() {
    let declared = (MAX_FRAME_LEN + 1) as u32;
    let mut reader = FrameReader::new(
        Cursor::new(declared.to_le_bytes().to_vec()),
        Direction::ToInterface,
        usize::MAX,
    );
    assert!(matches!(reader.read(), Err(ReadError::BadLength(_))));
}

#[test]
fn a_frame_that_claims_more_than_arrives_is_an_error_not_a_hang() {
    let mut bytes = (100_000u32).to_le_bytes().to_vec();
    bytes.extend_from_slice(&[FRAME_VERSION, 4]);
    let mut reader = FrameReader::new(Cursor::new(bytes), Direction::ToHelper, MAX_FRAME_LEN);
    assert!(matches!(reader.read(), Err(ReadError::Io(_))));
    // A prefix cut in the middle is also an error, but nothing at all is a
    // clean end.
    let mut cut = FrameReader::new(Cursor::new(vec![2, 0]), Direction::ToHelper, 100);
    assert!(matches!(cut.read(), Err(ReadError::Io(_))));
    let mut empty = FrameReader::new(Cursor::new(Vec::new()), Direction::ToHelper, 100);
    assert!(empty.read().unwrap().is_none());
}

#[test]
fn counts_are_checked_against_the_data_before_anything_is_reserved() {
    // A delete request that claims the maximum number of items and has none.
    let mut b = vec![FRAME_VERSION, 3, 1];
    b.extend_from_slice(&(MAX_DELETE_ITEMS as u32).to_le_bytes());
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::Truncated)
    );
    // One more than the maximum is a different error.
    let mut b = vec![FRAME_VERSION, 3, 1];
    b.extend_from_slice(&(MAX_DELETE_ITEMS as u32 + 1).to_le_bytes());
    b.extend_from_slice(&vec![0u8; 100_000]);
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::TooLong)
    );
    // A path length above the limit.
    let mut b = vec![FRAME_VERSION, 3, 1];
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&((MAX_WIRE_PATH + 1) as u32).to_le_bytes());
    b.extend_from_slice(&vec![b'a'; MAX_WIRE_PATH + 8]);
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::TooLong)
    );
    // A scan request that claims 256 mount ids with none after it.
    let mut b = vec![FRAME_VERSION, 2];
    b.extend_from_slice(&0u32.to_le_bytes());
    b.push(0);
    b.extend_from_slice(&(MAX_EXTRA_MOUNTS as u32).to_le_bytes());
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn trailing_bytes_and_bad_flags_are_refused() {
    for message in samples() {
        let mut b = body(&message);
        b.push(0);
        let result = decode_body(&b, direction_of(&message));
        assert!(result.is_err(), "{message:?}");
    }
    // A boolean that is neither 0 nor 1.
    let delete = Message::Delete(DeleteRequest {
        permanent: true,
        items: vec![],
    });
    let mut b = body(&delete);
    b[2] = 2;
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::BadValue)
    );
    // An unknown backend.
    let mut b = body(&Message::Scan(scan_request()));
    // version, kind, volume length (4) + the one-byte volume, then the backend
    b[2 + 4 + 1] = 99;
    assert_eq!(
        decode_body(&b, Direction::ToHelper),
        Err(DecodeError::BadValue)
    );
}

#[test]
fn limits_are_enforced_when_sending_too() {
    let item = |path: Vec<u8>| DeleteItem {
        path,
        confirmed_protected: false,
        seen: None,
    };
    let too_long = Message::Delete(DeleteRequest {
        permanent: true,
        items: vec![item(vec![b'a'; MAX_WIRE_PATH + 1])],
    });
    let mut out = Vec::new();
    assert!(write_message(&mut out, &too_long).is_err());
    assert!(out.is_empty(), "nothing is written for a refused message");
    let too_many = Message::Delete(DeleteRequest {
        permanent: true,
        items: vec![item(b"/a".to_vec()); MAX_DELETE_ITEMS + 1],
    });
    assert!(write_message(&mut out, &too_many).is_err());
    assert!(out.is_empty());
    // An error text is cut, not refused.
    let long_text = Message::Error(ErrorReport {
        code: ErrorCode::Internal,
        message: "\u{e9}".repeat(MAX_ERROR_TEXT),
    });
    let bytes = frame(&long_text);
    let mut reader = FrameReader::new(Cursor::new(bytes), Direction::ToInterface, MAX_FRAME_LEN);
    match reader.read().unwrap() {
        Some(Message::Error(report)) => {
            assert!(report.message.len() <= MAX_ERROR_TEXT);
            assert!(report.message.chars().all(|c| c == '\u{e9}'));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_batch_frame_over_the_maximum_is_refused_when_sending() {
    let mut big = Batch::new();
    let meta = Meta::default();
    let name = vec![b'n'; 255];
    big.dir(DirKey(0), DirKey::NONE, b"r", &meta).unwrap();
    for _ in 0..(MAX_FRAME_LEN / 300 + 100) {
        big.file(bs_tree::NodeKind::File, DirKey(0), &name, &meta)
            .unwrap();
    }
    let mut out = Vec::new();
    assert!(write_message(&mut out, &Message::Batch(big)).is_err());
}

#[test]
fn paths_cross_the_wire_unchanged() {
    let path = std::path::PathBuf::from("/a/b c/\u{e9}");
    assert_eq!(path_from_wire(&path_to_wire(&path)).unwrap(), path);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/x/\xff\xfe"));
        assert_eq!(path_to_wire(odd), b"/x/\xff\xfe");
        assert_eq!(path_from_wire(b"/x/\xff\xfe").unwrap(), odd);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    // Whatever the bytes are, decoding ends with a message or an error.
    #[test]
    fn arbitrary_bodies_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = decode_body(&bytes, Direction::ToHelper);
        let _ = decode_body(&bytes, Direction::ToInterface);
    }

    // Bodies that start like a real message reach the field parsers.
    #[test]
    fn arbitrary_fields_never_panic(
        kind in 0u8..24,
        rest in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let mut bytes = vec![FRAME_VERSION, kind];
        bytes.extend_from_slice(&rest);
        let _ = decode_body(&bytes, Direction::ToHelper);
        let _ = decode_body(&bytes, Direction::ToInterface);
    }

    // A whole stream of arbitrary bytes ends: a message, an error or the end.
    #[test]
    fn arbitrary_streams_end(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let mut reader = FrameReader::new(Cursor::new(bytes), Direction::ToHelper, 1024);
        for _ in 0..4096 {
            match reader.read() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }

    // A real frame with bytes changed is still handled without a panic, and
    // if it still decodes it is a valid message that encodes again.
    #[test]
    fn damaged_frames_are_handled(
        which in 0usize..11,
        edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..6),
        cut in proptest::option::of(any::<usize>()),
    ) {
        let messages = samples();
        let message = &messages[which % messages.len()];
        let mut b = body(message);
        for (at, value) in edits {
            let len = b.len();
            b[at % len] = value;
        }
        if let Some(cut) = cut {
            let len = b.len();
            b.truncate(cut % (len + 1));
        }
        for direction in [Direction::ToHelper, Direction::ToInterface] {
            if let Ok(decoded) = decode_body(&b, direction) {
                let mut again = Vec::new();
                prop_assert!(encode_body(&decoded, &mut again).is_ok());
            }
        }
    }

    // What is encoded is decoded to the same message.
    #[test]
    fn delete_requests_round_trip(
        permanent in any::<bool>(),
        items in proptest::collection::vec(
            (proptest::collection::vec(any::<u8>(), 0..64), any::<bool>(),
             proptest::option::of((any::<u64>(), any::<u128>()))),
            0..20,
        ),
    ) {
        let message = Message::Delete(DeleteRequest {
            permanent,
            items: items.into_iter().map(|(path, confirmed_protected, seen)| DeleteItem {
                path,
                confirmed_protected,
                seen: seen.map(|(volume, file)| Seen { volume, file }),
            }).collect(),
        });
        prop_assert_eq!(decode_body(&body(&message), Direction::ToHelper).unwrap(), message);
    }
}

// ---------------------------------------------------------------- Client

/// A fake helper on the other end of two pipes.
struct Peer {
    reader: FrameReader<std::io::PipeReader>,
    writer: std::io::PipeWriter,
}

fn hello_back(peer: &mut Peer, nonce: [u8; NONCE_LEN], version: u16) {
    match peer.reader.read().unwrap() {
        Some(Message::Hello(h)) => assert_eq!(h.nonce, NONCE),
        other => panic!("{other:?}"),
    }
    write_message(&mut peer.writer, &Message::Hello(Hello { version, nonce })).unwrap();
}

type TestClient = Client<std::io::PipeReader, std::io::PipeWriter>;

/// Runs `helper` against a client that has already said hello and been
/// answered.
fn with_helper<T>(
    helper: impl FnOnce(Peer) + Send + 'static,
    client: impl FnOnce(TestClient) -> T,
) -> T {
    let (helper_in, interface_out) = std::io::pipe().unwrap();
    let (interface_in, helper_out) = std::io::pipe().unwrap();
    let thread = std::thread::spawn(move || {
        let mut peer = Peer {
            reader: FrameReader::new(helper_in, Direction::ToHelper, MAX_REQUEST_FRAME_LEN),
            writer: helper_out,
        };
        hello_back(&mut peer, NONCE, PROTOCOL_VERSION);
        helper(peer);
    });
    let connected = Client::connect(interface_in, interface_out, NONCE).unwrap();
    let out = client(connected);
    thread.join().unwrap();
    out
}

#[test]
fn the_client_refuses_a_helper_that_answers_with_another_nonce_or_version() {
    for (nonce, version) in [
        ([9u8; NONCE_LEN], PROTOCOL_VERSION),
        (NONCE, PROTOCOL_VERSION + 1),
    ] {
        let (helper_in, interface_out) = std::io::pipe().unwrap();
        let (interface_in, helper_out) = std::io::pipe().unwrap();
        let thread = std::thread::spawn(move || {
            let mut peer = Peer {
                reader: FrameReader::new(helper_in, Direction::ToHelper, 1024),
                writer: helper_out,
            };
            hello_back(&mut peer, nonce, version);
        });
        let result = Client::connect(interface_in, interface_out, NONCE);
        thread.join().unwrap();
        match result {
            Err(ClientError::WrongNonce) => assert_eq!(nonce, [9u8; NONCE_LEN]),
            Err(ClientError::Version(v)) => assert_eq!(v, PROTOCOL_VERSION + 1),
            Ok(_) => panic!("connected to a helper that failed the check"),
            Err(other) => panic!("{other}"),
        }
    }
}

#[test]
fn the_client_refuses_a_helper_that_closes_or_speaks_out_of_turn_at_the_start() {
    // Closes at once.
    let (helper_in, interface_out) = std::io::pipe().unwrap();
    let (interface_in, helper_out) = std::io::pipe().unwrap();
    drop(helper_out);
    let _keep = helper_in;
    assert!(matches!(
        Client::connect(interface_in, interface_out, NONCE),
        Err(ClientError::Disconnected)
    ));
    // Answers with progress instead of hello.
    let (helper_in, interface_out) = std::io::pipe().unwrap();
    let (interface_in, mut helper_out) = std::io::pipe().unwrap();
    write_message(&mut helper_out, &Message::Progress(progress())).unwrap();
    let _keep = helper_in;
    assert!(matches!(
        Client::connect(interface_in, interface_out, NONCE),
        Err(ClientError::Unexpected(_))
    ));
}

#[test]
fn a_scan_streams_batches_progress_and_the_end() {
    let done = ScanDone {
        backend: Backend::Walker,
        end: ScanEnd::Completed,
        fallback: Fallback::None,
        progress: progress(),
    };
    let events = with_helper(
        move |mut peer| {
            assert!(matches!(
                peer.reader.read().unwrap(),
                Some(Message::Scan(_))
            ));
            write_message(&mut peer.writer, &Message::Batch(batch())).unwrap();
            write_message(&mut peer.writer, &Message::Progress(progress())).unwrap();
            write_message(&mut peer.writer, &Message::ScanDone(done)).unwrap();
        },
        |mut client| {
            let mut stream = client.scan(scan_request()).unwrap();
            let mut seen = Vec::new();
            while let Some(event) = stream.next_event().unwrap() {
                seen.push(event);
            }
            // After the end nothing more is returned.
            assert!(stream.next_event().unwrap().is_none());
            seen
        },
    );
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], ScanEvent::Batch(_)));
    assert!(matches!(events[1], ScanEvent::Progress(_)));
    assert!(matches!(events[2], ScanEvent::Done(d) if d == done));
}

#[test]
fn a_scan_the_helper_cannot_do_is_an_error_and_a_cut_connection_is_too() {
    let result = with_helper(
        |mut peer| {
            let _ = peer.reader.read();
            let report = ErrorReport {
                code: ErrorCode::ScanFailed,
                message: "no".into(),
            };
            write_message(&mut peer.writer, &Message::Error(report)).unwrap();
        },
        |mut client| client.scan(scan_request()).unwrap().next_event(),
    );
    assert!(matches!(result, Err(ClientError::Helper(_))));
    let result = with_helper(
        |mut peer| {
            let _ = peer.reader.read();
            // Drops the connection without a word.
        },
        |mut client| client.scan(scan_request()).unwrap().next_event(),
    );
    assert!(matches!(result, Err(ClientError::Disconnected)));
}

fn delete_request(n: usize) -> DeleteRequest {
    DeleteRequest {
        permanent: true,
        items: (0..n)
            .map(|i| DeleteItem {
                path: format!("/fixture/{i}").into_bytes(),
                confirmed_protected: false,
                seen: None,
            })
            .collect(),
    }
}

fn deleted(bytes: u64) -> ItemOutcome {
    ItemOutcome::Deleted {
        bytes,
        entries: 1,
        shared_files: 0,
    }
}

#[test]
fn a_delete_returns_one_outcome_per_item_in_order() {
    let outcomes = with_helper(
        |mut peer| {
            assert!(matches!(
                peer.reader.read().unwrap(),
                Some(Message::Delete(_))
            ));
            // Out of order is fine: they carry their index.
            for index in [1u32, 0, 2] {
                let outcome = deleted(u64::from(index));
                write_message(&mut peer.writer, &Message::DeleteItem { index, outcome }).unwrap();
            }
            write_message(&mut peer.writer, &Message::DeleteDone { items: 3 }).unwrap();
        },
        |mut client| client.delete(delete_request(3)).unwrap(),
    );
    assert_eq!(outcomes, vec![deleted(0), deleted(1), deleted(2)]);
}

#[test]
fn a_delete_cut_short_is_never_reported_as_complete() {
    let failure = with_helper(
        |mut peer| {
            let _ = peer.reader.read();
            let outcome = deleted(5);
            write_message(&mut peer.writer, &Message::DeleteItem { index: 0, outcome }).unwrap();
            // The helper dies before the second item and before the end.
        },
        |mut client| client.delete(delete_request(2)).unwrap_err(),
    );
    assert!(matches!(failure.error, ClientError::Disconnected));
    assert_eq!(failure.completed, vec![(0, deleted(5))]);
}

#[test]
fn a_delete_end_before_every_item_has_an_outcome_is_refused() {
    let failure = with_helper(
        |mut peer| {
            let _ = peer.reader.read();
            let outcome = deleted(5);
            write_message(&mut peer.writer, &Message::DeleteItem { index: 0, outcome }).unwrap();
            write_message(&mut peer.writer, &Message::DeleteDone { items: 2 }).unwrap();
        },
        |mut client| client.delete(delete_request(2)).unwrap_err(),
    );
    assert!(matches!(failure.error, ClientError::Unexpected(_)));
}

#[test]
fn a_delete_outcome_for_an_unknown_or_repeated_item_is_refused() {
    for indexes in [vec![5u32], vec![0, 0]] {
        let failure = with_helper(
            move |mut peer| {
                let _ = peer.reader.read();
                for index in indexes {
                    let outcome = deleted(1);
                    let _ =
                        write_message(&mut peer.writer, &Message::DeleteItem { index, outcome });
                }
            },
            |mut client| client.delete(delete_request(2)).unwrap_err(),
        );
        assert!(matches!(failure.error, ClientError::Unexpected(_)));
    }
}

#[test]
fn a_delete_request_must_be_permanent() {
    let failure = with_helper(
        |_peer| {},
        |mut client| {
            let mut request = delete_request(1);
            request.permanent = false;
            client.delete(request).unwrap_err()
        },
    );
    assert!(matches!(failure.error, ClientError::BadRequest(_)));
}

#[test]
fn the_client_says_goodbye() {
    with_helper(
        |mut peer| {
            assert!(matches!(peer.reader.read().unwrap(), Some(Message::Bye)));
        },
        |client| client.finish(),
    );
}

#[test]
fn writing_to_a_closed_peer_is_an_error_not_a_panic() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(matches!(
        write_message(&mut Closed, &Message::Bye),
        Err(WriteError::Io(_))
    ));
}

#[test]
fn every_fallback_and_error_code_round_trips_and_an_unknown_one_is_refused() {
    // A peer built before a code existed reads it as an unknown value and
    // refuses the frame (a decode error), which ends the conversation; it
    // never takes it for another code.
    for fallback in [
        Fallback::None,
        Fallback::Unavailable,
        Fallback::TooLarge,
        Fallback::PartialMount,
        Fallback::NotSupported,
        Fallback::MountNotPlaced,
    ] {
        let done = Message::ScanDone(ScanDone {
            backend: Backend::Btrfs,
            end: ScanEnd::Completed,
            fallback,
            progress: progress(),
        });
        let bytes = frame(&done);
        assert_eq!(
            decode_body(&bytes[4..], Direction::ToInterface).unwrap(),
            done
        );
        // Frame body: version, kind, backend, end, then the fallback code.
        let mut damaged = bytes[4..].to_vec();
        damaged[4] = 99;
        assert!(matches!(
            decode_body(&damaged, Direction::ToInterface),
            Err(DecodeError::BadValue)
        ));
    }
    for code in [
        ErrorCode::BadRequest,
        ErrorCode::ScanFailed,
        ErrorCode::Unsupported,
        ErrorCode::Refused,
        ErrorCode::Internal,
        ErrorCode::NoAccount,
    ] {
        let error = Message::Error(ErrorReport {
            code,
            message: "x".into(),
        });
        let bytes = frame(&error);
        assert_eq!(
            decode_body(&bytes[4..], Direction::ToInterface).unwrap(),
            error
        );
        let mut damaged = bytes[4..].to_vec();
        damaged[2] = 7;
        assert!(matches!(
            decode_body(&damaged, Direction::ToInterface),
            Err(DecodeError::BadValue)
        ));
    }
}
