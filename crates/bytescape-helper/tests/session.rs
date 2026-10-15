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
            isolate_environment: false,
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
    cfg.judge = Judge::new(Knowledge::new(&Environment::new())).unavailable("no account");
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
            assert_eq!(report.code, ErrorCode::Refused);
            assert_eq!(report.message, "no account");
        }
        other => panic!("expected an error report, got {other:?}"),
    }
    assert!(frames.read().unwrap().is_none());
    drop(writer);
}
