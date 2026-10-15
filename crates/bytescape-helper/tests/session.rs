//! The session in the test process: time limits and a stuck peer, which the
//! program-level tests would take too long to show.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![cfg(target_os = "linux")]

use bs_ipc::{
    DeleteItem, DeleteRequest, Direction, ErrorCode, FrameReader, Hello, MAX_FRAME_LEN, Message,
    NONCE_LEN, PROTOCOL_VERSION, path_to_wire, write_message,
};
use bs_knowledge::{Environment, Knowledge};
use bytescape_helper::args::Mode;
use bytescape_helper::delete::Judge;
use bytescape_helper::scan::Host;
use bytescape_helper::session::{Config, Exit, run};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

mod common;

const NONCE: [u8; NONCE_LEN] = [3; NONCE_LEN];

fn config(
    request_timeout: Duration,
    write_stall: Duration,
    on_stall: Box<dyn Fn() + Send + Sync>,
) -> Config {
    Config {
        nonce: NONCE,
        mode: Mode::Delete,
        request_timeout,
        write_stall,
        judge: Judge::new(Knowledge::new(&Environment::new())),
        host: Host {
            mountinfo: "/nonexistent".into(),
        },
        on_stall,
    }
}

fn hello() -> Message {
    Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        nonce: NONCE,
    })
}

#[test]
fn a_peer_that_never_says_hello_is_given_up_on() {
    let (reader, _writer_kept_open) = std::io::pipe().unwrap();
    let started = Instant::now();
    let exit = run(
        reader,
        std::io::sink(),
        config(
            Duration::from_millis(300),
            Duration::from_secs(60),
            Box::new(|| {}),
        ),
    );
    assert_eq!(exit, Exit::Idle);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_peer_that_says_hello_and_nothing_else_is_given_up_on() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    let mut answer = Vec::new();
    let exit = run(
        reader,
        &mut answer,
        config(
            Duration::from_millis(300),
            Duration::from_secs(60),
            Box::new(|| {}),
        ),
    );
    assert_eq!(exit, Exit::Idle);
    // It was greeted before the silence.
    assert!(!answer.is_empty());
    drop(writer);
}

/// A writer that blocks until it is released, like a peer that stopped
/// reading with a full pipe.
struct Stuck {
    release: mpsc::Receiver<()>,
}

impl Write for Stuck {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.release.recv();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_write_that_stays_blocked_trips_the_watchdog() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    let (release_tx, release_rx) = mpsc::channel();
    let tripped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&tripped);
    let on_stall: Box<dyn Fn() + Send + Sync> = Box::new(move || {
        flag.store(true, Ordering::SeqCst);
        // Stands in for the process ending: lets the test thread go on.
        let _ = release_tx.send(());
    });
    let started = Instant::now();
    let exit = run(
        reader,
        Stuck {
            release: release_rx,
        },
        config(
            Duration::from_millis(1500),
            Duration::from_millis(200),
            on_stall,
        ),
    );
    assert!(tripped.load(Ordering::SeqCst), "the watchdog did not fire");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(exit, Exit::Idle);
    drop(writer);
}

#[test]
fn a_fast_writer_never_trips_the_watchdog() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    let tripped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&tripped);
    let exit = run(
        reader,
        std::io::sink(),
        config(
            Duration::from_millis(600),
            Duration::from_millis(100),
            Box::new(move || flag.store(true, Ordering::SeqCst)),
        ),
    );
    assert_eq!(exit, Exit::Idle);
    assert!(!tripped.load(Ordering::SeqCst));
    drop(writer);
}

#[test]
fn a_wrong_hello_gets_no_answer_and_no_work() {
    for (version, nonce) in [
        (PROTOCOL_VERSION, [4u8; NONCE_LEN]),
        (PROTOCOL_VERSION + 1, NONCE),
    ] {
        let (reader, mut writer) = std::io::pipe().unwrap();
        write_message(&mut writer, &Message::Hello(Hello { version, nonce })).unwrap();
        let mut answer = Vec::new();
        let exit = run(
            reader,
            &mut answer,
            config(
                Duration::from_secs(5),
                Duration::from_secs(60),
                Box::new(|| {}),
            ),
        );
        assert_eq!(exit, Exit::WrongHello);
        assert!(answer.is_empty());
        drop(writer);
    }
}

#[test]
fn the_exit_statuses_are_distinct_where_it_matters() {
    assert_eq!(Exit::Done.code(), 0);
    assert_eq!(Exit::PipeClosed.code(), 0);
    let codes = [
        Exit::Failed(String::new()).code(),
        Exit::WrongHello.code(),
        Exit::Protocol("x").code(),
        Exit::Idle.code(),
    ];
    for (i, a) in codes.iter().enumerate() {
        assert_ne!(*a, 0);
        for b in &codes[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert_eq!(Exit::WrongMode.code(), Exit::Protocol("x").code());
}

#[test]
fn a_judge_that_cannot_be_trusted_refuses_the_delete_and_touches_nothing() {
    let fixture = common::Fixture::new("unavailable");
    fixture.write("keep/file.txt", "still here");
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    write_message(
        &mut writer,
        &Message::Delete(DeleteRequest {
            permanent: true,
            items: vec![DeleteItem {
                path: path_to_wire(&fixture.p("keep/file.txt")),
                confirmed_protected: false,
                seen: None,
            }],
        }),
    )
    .unwrap();
    let mut cfg = config(
        Duration::from_secs(5),
        Duration::from_secs(60),
        Box::new(|| {}),
    );
    cfg.judge = Judge::new(Knowledge::new(&Environment::new()))
        .unavailable(ErrorCode::NoAccount, "no account");
    let mut answer = Vec::new();
    let exit = run(reader, &mut answer, cfg);
    assert_eq!(exit, Exit::Failed(String::from("no account")));
    assert!(fixture.exists("keep/file.txt"));

    // The interface hears why, and never a per-item outcome or a completed
    // delete.
    let mut frames = FrameReader::new(&answer[..], Direction::ToInterface, MAX_FRAME_LEN);
    assert!(matches!(frames.read().unwrap(), Some(Message::Hello(_))));
    match frames.read().unwrap() {
        Some(Message::Error(report)) => {
            assert_eq!(report.code, ErrorCode::NoAccount);
            assert_eq!(report.message, "no account");
        }
        other => panic!("expected an error report, got {other:?}"),
    }
    assert!(frames.read().unwrap().is_none());
    drop(writer);
}

// ------------------------------------------------------- Network volumes

use bs_ipc::{Backend, ScanRequest};
use std::path::Path;

/// The mount ID the kernel gives a folder.
fn mount_id_of(path: &Path) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let folder = std::fs::File::open(path).unwrap();
    bs_fsops::remove::fstat(folder.as_raw_fd()).unwrap().mnt
}

/// Runs a scan of `folder` against a mount table in which `folder`'s mount is
/// of type `fs_type`, and gives what the helper answered.
fn scan_with_table(fixture: &common::Fixture, folder: &Path, fs_type: &str) -> Vec<Message> {
    let Some(id) = mount_id_of(folder) else {
        println!("NOT RUN: this kernel does not report mount IDs");
        return Vec::new();
    };
    std::fs::write(
        fixture.p("table"),
        format!("{id} 1 0:99 / / rw - {fs_type} somewhere rw\n"),
    )
    .unwrap();
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    write_message(
        &mut writer,
        &Message::Scan(ScanRequest {
            volume: path_to_wire(folder),
            backend: Backend::Walker,
            extra_mount_ids: vec![],
            places: vec![],
        }),
    )
    .unwrap();
    let mut cfg = config(
        Duration::from_secs(5),
        Duration::from_secs(60),
        Box::new(|| {}),
    );
    cfg.mode = Mode::Scan;
    cfg.host = Host {
        mountinfo: fixture.p("table"),
    };
    let mut answer = Vec::new();
    run(reader, &mut answer, cfg);
    drop(writer);
    let mut frames = FrameReader::new(&answer[..], Direction::ToInterface, MAX_FRAME_LEN);
    let mut messages = Vec::new();
    while let Some(message) = frames.read().unwrap() {
        messages.push(message);
    }
    messages
}

#[test]
fn a_scan_of_a_network_volume_is_refused_with_its_own_code_and_reads_nothing() {
    let fixture = common::Fixture::new("scan-remote");
    fixture.write("tree/a", "1");
    for fs_type in ["nfs4", "cifs", "fuse.sshfs", "9p", "smb3"] {
        let messages = scan_with_table(&fixture, &fixture.p("tree"), fs_type);
        if messages.is_empty() {
            return;
        }
        assert!(matches!(messages[0], Message::Hello(_)), "{fs_type}");
        match &messages[1] {
            Message::Error(report) => {
                assert_eq!(report.code, ErrorCode::RemoteVolume, "{fs_type}");
                // Plain words, and no path.
                assert!(report.message.contains("network"), "{}", report.message);
                assert!(
                    !report.message.contains("scan-remote"),
                    "{}",
                    report.message
                );
            }
            other => panic!("{fs_type}: expected an error report, got {other:?}"),
        }
        // Nothing was read: no batch, no progress, no completion.
        assert_eq!(messages.len(), 2, "{fs_type}: {messages:?}");
    }
}

#[test]
fn a_scan_of_a_local_volume_goes_ahead_and_one_the_helper_cannot_place_does_not() {
    let fixture = common::Fixture::new("scan-local");
    fixture.write("tree/a", "1");
    let messages = scan_with_table(&fixture, &fixture.p("tree"), "ext4");
    if messages.is_empty() {
        return;
    }
    assert!(
        messages.iter().any(|m| matches!(m, Message::ScanDone(_))),
        "{messages:?}"
    );
    // A mount table that cannot be read cannot say what the folder is on, and
    // that is not taken for a local disk.
    let other = common::Fixture::new("scan-local-other");
    other.write("tree/a", "1");
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    write_message(
        &mut writer,
        &Message::Scan(ScanRequest {
            volume: path_to_wire(&other.p("tree")),
            backend: Backend::Walker,
            extra_mount_ids: vec![],
            places: vec![],
        }),
    )
    .unwrap();
    let mut cfg = config(
        Duration::from_secs(5),
        Duration::from_secs(60),
        Box::new(|| {}),
    );
    cfg.mode = Mode::Scan;
    cfg.host = Host {
        mountinfo: other.p("no-such-table"),
    };
    let mut answer = Vec::new();
    run(reader, &mut answer, cfg);
    drop(writer);
    let mut frames = FrameReader::new(&answer[..], Direction::ToInterface, MAX_FRAME_LEN);
    assert!(matches!(frames.read().unwrap(), Some(Message::Hello(_))));
    match frames.read().unwrap() {
        Some(Message::Error(report)) => assert_eq!(report.code, ErrorCode::Refused),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(frames.read().unwrap().is_none());
}

#[test]
fn a_delete_on_a_network_volume_is_refused_item_by_item_and_touches_nothing() {
    use bs_ipc::{ItemOutcome, Refusal};
    let fixture = common::Fixture::new("delete-remote");
    fixture.write("keep/file.txt", "still here");
    let Some(id) = mount_id_of(&fixture.p("keep")) else {
        return println!("NOT RUN: this kernel does not report mount IDs");
    };
    std::fs::write(
        fixture.p("table"),
        format!("{id} 1 0:99 / / rw - cifs //server/share rw\n"),
    )
    .unwrap();
    let (reader, mut writer) = std::io::pipe().unwrap();
    write_message(&mut writer, &hello()).unwrap();
    write_message(
        &mut writer,
        &Message::Delete(DeleteRequest {
            permanent: true,
            items: vec![DeleteItem {
                path: path_to_wire(&fixture.p("keep/file.txt")),
                confirmed_protected: false,
                seen: None,
            }],
        }),
    )
    .unwrap();
    let mut cfg = config(
        Duration::from_secs(5),
        Duration::from_secs(60),
        Box::new(|| {}),
    );
    cfg.judge = Judge::new(Knowledge::new(&Environment::new())).with_volume_check(Box::new(
        bytescape_helper::volume::MountTable::new(fixture.p("table")),
    ));
    let mut answer = Vec::new();
    run(reader, &mut answer, cfg);
    drop(writer);
    assert!(fixture.exists("keep/file.txt"));
    let mut frames = FrameReader::new(&answer[..], Direction::ToInterface, MAX_FRAME_LEN);
    assert!(matches!(frames.read().unwrap(), Some(Message::Hello(_))));
    match frames.read().unwrap() {
        Some(Message::DeleteItem { index: 0, outcome }) => {
            assert_eq!(outcome, ItemOutcome::Refused(Refusal::RemoteVolume));
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(matches!(
        frames.read().unwrap(),
        Some(Message::DeleteDone { items: 1 })
    ));
}
