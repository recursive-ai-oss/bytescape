//! The helper program, run unprivileged against fixtures, over pipes.
//!
//! Each test starts the real binary the way `pkexec` would (standard input and
//! output are pipes, `PKEXEC_UID` is set, the starting process is the user's
//! own) and speaks the protocol to it. Everything that is deleted lives in a
//! fixture folder. The elevation itself is the one thing these tests cannot
//! do; see docs/security/helper.md for what the privileged runs add.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![allow(clippy::cast_possible_truncation)]
#![cfg(target_os = "linux")]

mod common;

use bs_ipc::{
    Backend, ClientError, DeleteItem, DeleteRequest, Direction, ErrorCode, Fallback, FrameReader,
    Hello, ItemOutcome, MAX_FRAME_LEN, MAX_REQUEST_FRAME_LEN, Message, PROTOCOL_VERSION, Place,
    Refusal, ScanEnd, ScanEvent, ScanRequest, path_to_wire, write_message,
};
use bs_scan::{ScanOptions, Walker};
use bs_tree::{Aggregator, Snapshot};
use common::{Fixture, Launch, NONCE, current_uid};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

const SOON: Duration = Duration::from_secs(10);

fn scan_request(volume: &Path) -> ScanRequest {
    ScanRequest {
        volume: path_to_wire(volume),
        backend: Backend::Walker,
        extra_mount_ids: vec![],
        places: vec![],
    }
}

fn item(path: &Path) -> DeleteItem {
    DeleteItem {
        path: path_to_wire(path),
        confirmed_protected: false,
        seen: None,
    }
}

fn delete_request(items: Vec<DeleteItem>) -> DeleteRequest {
    DeleteRequest {
        permanent: true,
        items,
    }
}

// ------------------------------------------------------- Refusing to start

#[test]
fn a_wrong_command_line_ends_the_helper_with_no_work_and_no_output() {
    let bad: Vec<Vec<&str>> = vec![
        vec![],
        vec!["scan"],
        vec!["both", "--protocol", "1", "--nonce", common::NONCE_HEX],
        vec!["scan", "--protocol", "2", "--nonce", common::NONCE_HEX],
        vec!["scan", "--protocol", "1", "--nonce", "short"],
        vec![
            "scan",
            "--protocol",
            "1",
            "--nonce",
            common::NONCE_HEX,
            "--pipe",
            "x",
        ],
    ];
    for args in bad {
        let mut launch = Launch::new("scan");
        launch.args = args.iter().map(|s| (*s).to_owned()).collect();
        let mut helper = launch.spawn();
        assert_eq!(helper.exit_code(SOON), 2, "{args:?}");
        assert!(helper.all_output().is_empty());
    }
}

#[test]
fn without_a_valid_pkexec_uid_the_helper_does_not_start() {
    let uid = current_uid();
    let wrong = if uid == 4242 { 4243 } else { 4242 };
    for value in [
        None,
        Some(String::new()),
        Some("0".to_owned()),
        Some("abc".to_owned()),
        Some(format!("{uid}x")),
        Some(format!("+{uid}")),
        Some("99999999999999999999".to_owned()),
        // A real number that is not the user who started this process.
        Some(wrong.to_string()),
    ] {
        let mut launch = Launch::new("scan");
        launch.pkexec_uid = value.clone();
        let mut helper = launch.spawn();
        assert_eq!(helper.exit_code(SOON), 3, "{value:?}");
        assert!(helper.all_output().is_empty(), "{value:?}");
    }
}

#[test]
fn standard_input_and_output_must_be_pipes() {
    let mut launch = Launch::new("scan");
    launch.stdin = Stdio::null();
    let mut helper = launch.spawn();
    assert_eq!(helper.exit_code(SOON), 3);

    let fx = Fixture::new("not-a-pipe");
    let file = fs::File::create(fx.p("out")).unwrap();
    let mut launch = Launch::new("scan");
    launch.stdout = Stdio::from(file);
    let mut helper = launch.spawn();
    assert_eq!(helper.exit_code(SOON), 3);
    assert_eq!(fs::read(fx.p("out")).unwrap().len(), 0);
}

#[test]
fn other_environment_variables_change_nothing() {
    if common::refuses_the_superuser() {
        return;
    }
    // The helper trusts PKEXEC_UID and nothing else: settings that would
    // redirect a program do not reach its work.
    let fx = Fixture::new("env");
    fx.write("tree/a", "x");
    let mut launch = Launch::new("scan");
    for (k, v) in [
        ("LD_PRELOAD", "/nonexistent/evil.so"),
        ("HOME", "/nonexistent"),
        ("TMPDIR", "/nonexistent"),
        ("PATH", "/nonexistent"),
        ("PKEXEC_HOME", "/nonexistent"),
        ("RUST_BACKTRACE", "1"),
    ] {
        launch.env.push((k.into(), v.into()));
    }
    let mut helper = launch.spawn();
    let mut client = helper.client();
    let mut stream = client.scan(scan_request(&fx.p("tree"))).unwrap();
    let mut done = false;
    while let Some(event) = stream.next_event().unwrap() {
        done |= matches!(event, ScanEvent::Done(_));
    }
    assert!(done);
    drop(client);
    assert_eq!(helper.exit_code(SOON), 0);
}

// ------------------------------------------------------------- The hello

#[test]
fn a_wrong_launch_value_ends_the_helper_without_an_answer() {
    let mut helper = Launch::new("delete").spawn();
    let mut stdin = helper.stdin.take().unwrap();
    let mut wrong = NONCE;
    wrong[15] ^= 1;
    write_message(
        &mut stdin,
        &Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            nonce: wrong,
        }),
    )
    .unwrap();
    assert_eq!(helper.exit_code(SOON), 3);
    assert!(
        helper.all_output().is_empty(),
        "nothing is said to a stranger"
    );
}

#[test]
fn another_protocol_version_ends_the_helper_without_an_answer() {
    let mut helper = Launch::new("delete").spawn();
    let mut stdin = helper.stdin.take().unwrap();
    write_message(
        &mut stdin,
        &Message::Hello(Hello {
            version: PROTOCOL_VERSION + 1,
            nonce: NONCE,
        }),
    )
    .unwrap();
    assert_eq!(helper.exit_code(SOON), 3);
    assert!(helper.all_output().is_empty());
}

#[test]
fn a_request_before_the_hello_does_no_work() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("no-hello");
    fx.write("victim", "x");
    let mut helper = Launch::new("delete").spawn();
    let mut stdin = helper.stdin.take().unwrap();
    let request = delete_request(vec![item(&fx.p("victim"))]);
    write_message(&mut stdin, &Message::Delete(request)).unwrap();
    assert_eq!(helper.exit_code(SOON), 4);
    assert!(helper.all_output().is_empty());
    assert!(fx.exists("victim"));
}

#[test]
fn broken_frames_end_the_helper() {
    if common::refuses_the_superuser() {
        return;
    }
    let frames: Vec<(&str, Vec<u8>)> = vec![
        ("length zero", vec![0, 0, 0, 0]),
        ("length one", vec![1, 0, 0, 0, 1]),
        (
            "over the request limit",
            ((MAX_REQUEST_FRAME_LEN + 1) as u32).to_le_bytes().to_vec(),
        ),
        ("the largest length", u32::MAX.to_le_bytes().to_vec()),
        ("unknown version", vec![2, 0, 0, 0, 9, 4]),
        ("unknown kind", vec![2, 0, 0, 0, 1, 200]),
        ("a tree batch sent to the helper", vec![2, 0, 0, 0, 1, 16]),
        (
            "random bytes",
            vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0x05],
        ),
    ];
    for (name, bytes) in frames {
        let mut helper = Launch::new("scan").spawn();
        let mut stdin = helper.stdin.take().unwrap();
        stdin.write_all(&bytes).unwrap();
        stdin.flush().unwrap();
        let code = helper.exit_code(SOON);
        assert_eq!(code, 4, "{name}");
        assert!(helper.all_output().is_empty(), "{name}");
    }
}

#[test]
fn a_closed_connection_ends_the_helper_at_once() {
    if common::refuses_the_superuser() {
        return;
    }
    // Before anything is said.
    let mut helper = Launch::new("scan").spawn();
    drop(helper.stdin.take());
    assert_eq!(helper.exit_code(SOON), 0);
    // In the middle of a frame.
    let mut helper = Launch::new("scan").spawn();
    let mut stdin = helper.stdin.take().unwrap();
    stdin.write_all(&[50, 0, 0, 0, 1, 1, 7]).unwrap();
    drop(stdin);
    assert_eq!(helper.exit_code(SOON), 0);
    // After the hello, before the request.
    let mut helper = Launch::new("scan").spawn();
    let client = helper.client();
    drop(client);
    assert_eq!(helper.exit_code(SOON), 0);
}

#[test]
fn a_helper_that_hears_nothing_gives_up() {
    if common::refuses_the_superuser() {
        return;
    }
    // The idle limit is 20 seconds; this waits for the real thing.
    let mut helper = Launch::new("scan").spawn();
    let _keep_open = helper.stdin.take();
    let started = std::time::Instant::now();
    assert_eq!(helper.exit_code(Duration::from_secs(40)), 5);
    assert!(started.elapsed() >= Duration::from_secs(15));
    // And one that was greeted but never asked for anything.
    let mut helper = Launch::new("scan").spawn();
    let client = helper.client();
    assert_eq!(helper.exit_code(Duration::from_secs(40)), 5);
    drop(client);
}

// ------------------------------------------------- One kind of work per run

#[test]
fn a_run_serves_only_the_kind_of_request_it_was_started_for() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("modes");
    fx.write("victim", "x");
    // A delete asked of a run that the user authorized as a scan.
    let mut helper = Launch::new("scan").spawn();
    let mut client = helper.client();
    let failure = client
        .delete(delete_request(vec![item(&fx.p("victim"))]))
        .unwrap_err();
    assert!(matches!(
        failure.error,
        ClientError::Disconnected | ClientError::Read(_)
    ));
    assert!(failure.completed.is_empty());
    drop(client);
    assert_eq!(helper.exit_code(SOON), 4);
    assert!(fx.exists("victim"), "nothing was deleted");
    // A scan asked of a delete run.
    let mut helper = Launch::new("delete").spawn();
    let mut client = helper.client();
    let mut stream = client.scan(scan_request(fx.path())).unwrap();
    assert!(stream.next_event().is_err());
    drop(client);
    assert_eq!(helper.exit_code(SOON), 4);
}

#[test]
fn only_one_request_is_served() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("one-request");
    fx.write("a", "x");
    fx.write("b", "x");
    let mut helper = Launch::new("delete").spawn();
    let mut client = helper.client();
    let outcomes = client
        .delete(delete_request(vec![item(&fx.p("a"))]))
        .unwrap();
    assert!(matches!(outcomes[0], ItemOutcome::Deleted { .. }));
    // The helper has gone; a second request finds nobody.
    let second = client.delete(delete_request(vec![item(&fx.p("b"))]));
    assert!(second.is_err());
    assert!(fx.exists("b"));
    assert_eq!(helper.exit_code(SOON), 0);
}

#[test]
fn goodbye_as_the_request_ends_the_helper_quietly() {
    if common::refuses_the_superuser() {
        return;
    }
    let mut helper = Launch::new("delete").spawn();
    let client = helper.client();
    client.finish();
    assert_eq!(helper.exit_code(SOON), 0);
}

// ------------------------------------------------------------- Deleting

#[test]
fn a_delete_reports_each_item_and_partial_results_are_not_success() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("delete");
    fx.write("work/a.txt", "hello");
    fx.write("work/dir/inner/b", "world");
    fx.write("work/keep", "keep");
    let mut helper = Launch::new("delete").spawn();
    let mut client = helper.client();
    let outcomes = client
        .delete(delete_request(vec![
            item(&fx.p("work/a.txt")),
            item(&fx.p("work/dir")),
            item(&fx.p("work/missing")),
        ]))
        .unwrap();
    assert!(matches!(
        outcomes[0],
        ItemOutcome::Deleted { entries: 1, .. }
    ));
    assert!(matches!(
        outcomes[1],
        ItemOutcome::Deleted { entries: 3, .. }
    ));
    assert_eq!(outcomes[2], ItemOutcome::Refused(Refusal::NotFound));
    assert!(!fx.exists("work/a.txt") && !fx.exists("work/dir"));
    assert!(fx.exists("work/keep"));
    assert_eq!(helper.exit_code(SOON), 0);
}

#[test]
fn a_request_that_is_not_permanent_is_refused_item_by_item() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("not-permanent");
    fx.write("f", "x");
    let mut helper = Launch::new("delete").spawn();
    let stdout = helper.stdout.take().unwrap();
    let mut stdin = helper.stdin.take().unwrap();
    write_message(
        &mut stdin,
        &Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            nonce: NONCE,
        }),
    )
    .unwrap();
    let mut request = delete_request(vec![item(&fx.p("f"))]);
    request.permanent = false;
    write_message(&mut stdin, &Message::Delete(request)).unwrap();
    let mut reader = FrameReader::new(stdout, Direction::ToInterface, MAX_FRAME_LEN);
    assert!(matches!(reader.read().unwrap(), Some(Message::Hello(_))));
    match reader.read().unwrap() {
        Some(Message::DeleteItem { index: 0, outcome }) => {
            assert_eq!(outcome, ItemOutcome::Refused(Refusal::NotPermanent));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        reader.read().unwrap(),
        Some(Message::DeleteDone { items: 1 })
    ));
    assert!(fx.exists("f"));
}

/// Paths under the operating system's folders that do not exist, so that even
/// a helper that wrongly went ahead would find nothing to delete. The test
/// first checks that none of them exists.
#[test]
fn the_operating_systems_own_places_are_refused_whatever_the_interface_says() {
    if common::refuses_the_superuser() {
        return;
    }
    let candidates = [
        "/usr/bytescape-test-absent",
        "/etc/bytescape-test-absent",
        "/boot/bytescape-test-absent",
        "/bin/bytescape-test-absent",
        "/sbin/bytescape-test-absent",
        "/lib/bytescape-test-absent",
        "/proc/bytescape-test-absent",
        "/sys/bytescape-test-absent",
        "/dev/bytescape-test-absent",
        "/var/lib/dpkg/bytescape-test-absent",
        "//usr/bytescape-test-absent",
        "/usr//bytescape-test-absent",
    ];
    for path in candidates {
        assert!(
            fs::symlink_metadata(path).is_err(),
            "{path} must not exist for this test"
        );
    }
    let fx = Fixture::new("system");
    fx.write("work/f", "x");
    let mut helper = Launch::new("delete").spawn();
    let mut client = helper.client();
    let mut items: Vec<DeleteItem> = candidates
        .iter()
        .map(|p| {
            // The user even says they confirmed it.
            let mut it = item(Path::new(p));
            it.confirmed_protected = true;
            it
        })
        .collect();
    items.push(item(&fx.p("work/f")));
    let count = items.len();
    let outcomes = client.delete(delete_request(items)).unwrap();
    assert_eq!(outcomes.len(), count);
    for (path, outcome) in candidates.iter().zip(&outcomes) {
        assert_eq!(*outcome, ItemOutcome::Refused(Refusal::Blocked), "{path}");
    }
    // The ordinary item went down with the selection.
    assert!(matches!(
        outcomes[count - 1],
        ItemOutcome::Refused(Refusal::SelectionRefused { .. })
    ));
    assert!(fx.exists("work/f"));
}

#[test]
fn paths_with_dot_parts_or_no_root_are_refused_untouched() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("dots");
    fx.write("work/f", "x");
    let base = fx.path().display().to_string();
    let mut helper = Launch::new("delete").spawn();
    let mut client = helper.client();
    let paths = [
        format!("{base}/work/../work/f"),
        format!("{base}/./work/f"),
        "relative/path".to_owned(),
        String::new(),
        "/".to_owned(),
    ];
    let items: Vec<_> = paths.iter().map(|p| item(Path::new(p))).collect();
    let outcomes = client.delete(delete_request(items)).unwrap();
    for outcome in &outcomes {
        assert!(matches!(outcome, ItemOutcome::Refused(_)), "{outcome:?}");
    }
    assert_eq!(outcomes[0], ItemOutcome::Refused(Refusal::BadPath));
    assert!(fx.exists("work/f"));
}

#[test]
fn closing_the_connection_mid_delete_stops_the_delete() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("hangup");
    for i in 0..200 {
        fx.write(&format!("items/i{i:03}/f"), "x");
    }
    let mut helper = Launch::new("delete").spawn();
    let mut stdin = helper.stdin.take().unwrap();
    let stdout = helper.stdout.take().unwrap();
    let items: Vec<_> = (0..200)
        .map(|i| item(&fx.p(&format!("items/i{i:03}"))))
        .collect();
    for message in [
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            nonce: NONCE,
        }),
        Message::Delete(delete_request(items)),
    ] {
        write_message(&mut stdin, &message).unwrap();
    }
    // Hang up on both ends without reading anything.
    drop(stdin);
    drop(stdout);
    assert_eq!(helper.exit_code(SOON), 0);
    let left = (0..200)
        .filter(|i| fx.exists(&format!("items/i{i:03}")))
        .count();
    assert!(
        left >= 195,
        "the delete went on after the hang-up: {left} left"
    );
}

// --------------------------------------------------------------- Scanning

fn snapshot_of(events: impl IntoIterator<Item = ScanEvent>) -> (Arc<Snapshot>, bs_ipc::ScanDone) {
    let mut aggregator = Aggregator::new();
    let mut done = None;
    for event in events {
        match event {
            ScanEvent::Batch(batch) => {
                aggregator.apply(&batch);
            }
            ScanEvent::Done(d) => done = Some(d),
            _ => {}
        }
    }
    aggregator.finish();
    (
        aggregator.publish(),
        done.expect("the scan ended with a done"),
    )
}

fn run_scan(helper: &mut common::Helper, request: ScanRequest) -> Vec<ScanEvent> {
    let mut client = helper.client();
    let mut stream = client.scan(request).unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().unwrap() {
        events.push(event);
    }
    events
}

fn paths_and_categories(snapshot: &Snapshot) -> Vec<(Vec<u8>, bs_tree::Category)> {
    fn walk(
        node: bs_tree::NodeRef<'_>,
        prefix: &[u8],
        out: &mut Vec<(Vec<u8>, bs_tree::Category)>,
    ) {
        let mut path = prefix.to_vec();
        path.push(b'/');
        path.extend_from_slice(node.name());
        out.push((path.clone(), node.category()));
        for child in node.children() {
            walk(child, &path, out);
        }
    }
    let mut out = Vec::new();
    if let Some(root) = snapshot.root() {
        walk(root, b"", &mut out);
    }
    out
}

fn paths_and_sizes(snapshot: &Snapshot) -> Vec<(Vec<u8>, u64, u64)> {
    fn walk(node: bs_tree::NodeRef<'_>, prefix: &[u8], out: &mut Vec<(Vec<u8>, u64, u64)>) {
        let mut path = prefix.to_vec();
        path.push(b'/');
        path.extend_from_slice(node.name());
        out.push((path.clone(), node.allocated(), node.logical()));
        for child in node.children() {
            walk(child, &path, out);
        }
    }
    let mut out = Vec::new();
    if let Some(root) = snapshot.root() {
        walk(root, b"", &mut out);
    }
    out.sort();
    out
}

#[test]
fn a_scan_streams_the_same_tree_the_walker_builds() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("scan");
    for d in 0..12 {
        for f in 0..25 {
            fx.write(&format!("tree/d{d}/f{f}.txt"), &"x".repeat(f * 37));
        }
    }
    fx.mkdir("tree/empty");
    std::os::unix::fs::symlink(fx.p("tree/d0"), fx.p("tree/link")).unwrap();
    let root = fx.p("tree");

    let mut helper = Launch::new("scan").spawn();
    let events = run_scan(&mut helper, scan_request(&root));
    assert!(matches!(events.last(), Some(ScanEvent::Done(_))));
    let (snapshot, done) = snapshot_of(events);
    assert_eq!(done.end, ScanEnd::Completed);
    assert_eq!(done.backend, Backend::Walker);
    assert_eq!(done.fallback, Fallback::None);
    assert!(snapshot.scan_complete());
    assert_eq!(done.progress.files, 12 * 25 + 1, "files and the link");
    assert_eq!(helper.exit_code(SOON), 0);

    let reference = bs_scan::start(Arc::new(Walker), root, ScanOptions::default()).wait();
    assert_eq!(
        paths_and_sizes(&snapshot),
        paths_and_sizes(&reference.snapshot)
    );
}

#[test]
fn a_volume_that_is_not_a_plain_absolute_folder_is_a_bad_request() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("bad-volume");
    fx.write("file", "x");
    let base = fx.path().display().to_string();
    for volume in [
        "relative".to_owned(),
        // Exists relative to where the helper runs; must still not be read.
        "src".to_owned(),
        "..".to_owned(),
        String::new(),
        format!("{base}/file"),
        format!("{base}/nope"),
        format!(
            "{base}/../{}",
            fx.path().file_name().unwrap().to_string_lossy()
        ),
        format!("{base}/./"),
    ] {
        let mut helper = Launch::new("scan").spawn();
        let mut client = helper.client();
        let mut request = scan_request(Path::new("/"));
        request.volume = volume.clone().into_bytes();
        let mut stream = client.scan(request).unwrap();
        match stream.next_event() {
            Err(ClientError::Helper(report)) => {
                assert_eq!(report.code, ErrorCode::BadRequest, "{volume:?}");
                assert!(!report.message.contains(&base), "no path in the message");
            }
            other => panic!("{volume:?}: {other:?}"),
        }
        drop(client);
        assert_eq!(helper.exit_code(SOON), 0);
    }
}

#[test]
fn a_reader_that_cannot_serve_the_folder_falls_back_to_the_walker_and_says_so() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("fallback");
    fx.write("tree/a", "x");
    // The reader that exists only on Windows is not here.
    let mut helper = Launch::new("scan").spawn();
    let mut request = scan_request(&fx.p("tree"));
    request.backend = Backend::Ntfs;
    let (_, done) = snapshot_of(run_scan(&mut helper, request));
    assert_eq!(done.backend, Backend::Walker);
    assert_eq!(done.fallback, Fallback::Unavailable);
    // The raw readers are for a filesystem at its mount point, never a folder
    // in it; asked for one, the helper says it did not use it.
    for backend in [Backend::Ext4, Backend::Btrfs, Backend::Xfs] {
        let mut helper = Launch::new("scan").spawn();
        let mut request = scan_request(&fx.p("tree"));
        request.backend = backend;
        let (_, done) = snapshot_of(run_scan(&mut helper, request));
        assert_eq!(done.backend, Backend::Walker, "{backend:?}");
        assert_eq!(done.fallback, Fallback::NotSupported, "{backend:?}");
    }
    // Automatic choice on a folder is the walker, with nothing to explain.
    let mut helper = Launch::new("scan").spawn();
    let mut request = scan_request(&fx.p("tree"));
    request.backend = Backend::Auto;
    let (_, done) = snapshot_of(run_scan(&mut helper, request));
    assert_eq!(
        (done.backend, done.fallback),
        (Backend::Walker, Fallback::None)
    );
}

#[test]
fn the_users_places_come_from_the_request_and_never_from_the_helpers_environment() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("places");
    fx.write("home/.local/share/Steam/steamapps/game", "game");
    fx.write("home/Documents/doc", "doc");
    let root = fx.p("home");
    // A place that exists only relative to the home folder.
    let place_category = bs_tree::Category(bs_knowledge::Category::AppsSystem.number());
    let category_of_blob = |snapshot: &Snapshot| {
        paths_and_categories(snapshot)
            .into_iter()
            .find(|(path, _)| path.ends_with(b"/steamapps/game"))
            .unwrap()
            .1
    };

    // The request names the user's home; the helper's own HOME is elsewhere.
    let mut launch = Launch::new("scan");
    launch
        .env
        .push(("HOME".into(), "/nonexistent-root-home".into()));
    let mut helper = launch.spawn();
    let mut request = scan_request(&root);
    request.places = vec![Place {
        name: "HOME".into(),
        value: path_to_wire(&root),
    }];
    let (snapshot, _) = snapshot_of(run_scan(&mut helper, request));
    assert_eq!(category_of_blob(&snapshot), place_category);

    // The helper's HOME is the user's home, but the request names no place:
    // the environment is not consulted.
    let mut launch = Launch::new("scan");
    launch.env.push(("HOME".into(), root.display().to_string()));
    let mut helper = launch.spawn();
    let (snapshot, _) = snapshot_of(run_scan(&mut helper, scan_request(&root)));
    assert_ne!(category_of_blob(&snapshot), place_category);
}

#[test]
fn places_that_are_not_what_they_claim_are_a_bad_request() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("bad-places");
    fx.write("tree/a", "x");
    for place in [
        Place {
            name: "PATH".into(),
            value: b"/usr/bin".to_vec(),
        },
        Place {
            name: "LD_PRELOAD".into(),
            value: b"/evil.so".to_vec(),
        },
        Place {
            name: "HOME".into(),
            value: b"relative".to_vec(),
        },
        Place {
            name: "HOME".into(),
            value: vec![],
        },
    ] {
        let mut helper = Launch::new("scan").spawn();
        let mut client = helper.client();
        let mut request = scan_request(&fx.p("tree"));
        request.places = vec![place.clone()];
        let mut stream = client.scan(request).unwrap();
        match stream.next_event() {
            Err(ClientError::Helper(report)) => assert_eq!(report.code, ErrorCode::BadRequest),
            other => panic!("{place:?}: {other:?}"),
        }
    }
}

fn big_tree(fx: &Fixture) {
    for d in 0..400 {
        let dir = fx.p(&format!("big/d{d:03}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            fs::File::create(dir.join(format!("file-with-a-longer-name-{f:03}"))).unwrap();
        }
    }
}

#[test]
fn goodbye_stops_a_running_scan_and_it_ends_cancelled() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("cancel");
    big_tree(&fx);
    let mut helper = Launch::new("scan").spawn();
    let mut client = helper.client();
    let mut stream = client.scan(scan_request(&fx.p("big"))).unwrap();
    // Nothing is read yet, so the helper is blocked writing: it is mid-scan.
    std::thread::sleep(Duration::from_millis(300));
    stream.stop().unwrap();
    let mut last = None;
    while let Some(event) = stream.next_event().unwrap() {
        last = Some(event);
    }
    match last {
        Some(ScanEvent::Done(done)) => assert_eq!(done.end, ScanEnd::Cancelled),
        other => panic!("{other:?}"),
    }
    drop(client);
    assert_eq!(helper.exit_code(SOON), 0);
}

#[test]
fn closing_the_connection_mid_scan_ends_the_helper_at_once() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("hangup-scan");
    big_tree(&fx);
    let mut helper = Launch::new("scan").spawn();
    let mut client = helper.client();
    let _stream = client.scan(scan_request(&fx.p("big"))).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(client);
    assert_eq!(helper.exit_code(SOON), 0);
}

#[test]
fn a_message_during_a_scan_ends_it() {
    if common::refuses_the_superuser() {
        return;
    }
    let fx = Fixture::new("chatter");
    big_tree(&fx);
    let mut helper = Launch::new("scan").spawn();
    let stdout = helper.stdout.take().unwrap();
    let mut stdin = helper.stdin.take().unwrap();
    for message in [
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            nonce: NONCE,
        }),
        Message::Scan(scan_request(&fx.p("big"))),
    ] {
        write_message(&mut stdin, &message).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    // A second request in the middle of the first.
    write_message(&mut stdin, &Message::Scan(scan_request(&fx.p("big")))).unwrap();
    drop(stdout);
    assert!(matches!(helper.exit_code(SOON), 0 | 4));
}
