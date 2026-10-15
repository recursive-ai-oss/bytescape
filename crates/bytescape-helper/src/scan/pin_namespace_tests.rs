//! A scan runs on the folder that was judged, not on whatever its name leads
//! to later. These need the right to mount, so they run in a private user and
//! mount namespace (`unshare --user --map-root-user --mount`) or as the
//! superuser on a machine that may be changed. Where they cannot mount they
//! print `NOT RUN` and pass, unless `BYTESCAPE_REQUIRE_NAMESPACES` is set,
//! which turns that into a failure.
//!
//! The mounts are tmpfs inside a fixture folder, detached again when the test
//! ends. A tmpfs stands in for a network mount: the mount table handed to the
//! helper calls it `nfs4`, as a real NFS mount would be listed.

#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use super::{Host, prepare};
use crate::plan::Chosen;
use bs_scan::{ScanContext, ScanError, Scanner};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn c(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

/// A mount that is detached when it goes out of scope.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        // SAFETY: `c` is NUL-terminated; detaching a mount this test made.
        unsafe { libc::umount2(c(&self.0).as_ptr(), libc::MNT_DETACH) };
    }
}

fn bind_mount(source: &Path, target: &Path) -> Option<Mounted> {
    // SAFETY: all strings are NUL-terminated and outlive the call; no data.
    let status = unsafe {
        libc::mount(
            c(source).as_ptr(),
            c(target).as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    (status == 0).then(|| Mounted(target.to_path_buf()))
}

fn mount_tmpfs(target: &Path) -> Option<Mounted> {
    // SAFETY: all strings are NUL-terminated and outlive the call; no data.
    let status = unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            c(target).as_ptr(),
            c"tmpfs".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    (status == 0).then(|| Mounted(target.to_path_buf()))
}

/// A folder made for one test, removed (after its mounts are detached) when
/// it is dropped. Nothing outside it is touched.
struct Fixture(PathBuf);

impl Fixture {
    /// `None`, with a note, where a mount cannot be made here.
    fn new(name: &str) -> Option<Fixture> {
        let base = std::env::temp_dir().join(format!("bs-pin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("probe")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let fixture = Fixture(base);
        match mount_tmpfs(&fixture.0.join("probe")) {
            Some(_probe) => Some(fixture),
            None => {
                assert!(
                    std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none(),
                    "BYTESCAPE_REQUIRE_NAMESPACES is set but a user and mount namespace cannot be made"
                );
                eprintln!("NOT RUN: needs the right to mount (a user and mount namespace)");
                None
            }
        }
    }

    fn p(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a scan counted, if it ran to the end; the error otherwise.
fn files_counted(plan: crate::plan::Plan, options: bs_scan::ScanOptions, root: &Path) -> Counted {
    let (ctx, batches) = ScanContext::channel(options, 1 << 16);
    let chosen = Chosen::new(plan);
    let result = chosen.scan(root, &ctx);
    let mut sent = 0u64;
    let mut aggregator = bs_tree::Aggregator::new();
    while let Ok(batch) = batches.try_recv() {
        sent += 1;
        aggregator.apply(&batch);
    }
    aggregator.finish();
    match result {
        Ok(_) => Counted::Walked(
            aggregator
                .publish()
                .root()
                .map_or(0, |r| u64::from(r.file_count())),
        ),
        Err(error @ ScanError::RootMoved { .. }) => Counted::Moved { error, sent },
        Err(other) => panic!("the scan failed in another way: {other}"),
    }
}

enum Counted {
    Walked(u64),
    Moved { error: ScanError, sent: u64 },
}

/// Folder `base/a/x` with one local file, a tmpfs at `base/remote` with a
/// folder `x` of three files that the mount table calls NFS, and a link
/// `base/swap` to that mount. Returns the host whose table says so.
fn race_fixture(fx: &Fixture) -> (Host, Mounted) {
    std::fs::create_dir_all(fx.p("a/x")).unwrap();
    std::fs::write(fx.p("a/x/local"), "1").unwrap();
    std::fs::create_dir_all(fx.p("remote")).unwrap();
    let mount = mount_tmpfs(&fx.p("remote")).unwrap();
    std::fs::create_dir_all(fx.p("remote/x")).unwrap();
    for name in ["r1", "r2", "r3"] {
        std::fs::write(fx.p("remote/x").join(name), "1").unwrap();
    }
    std::os::unix::fs::symlink(fx.p("remote"), fx.p("swap")).unwrap();
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let want = fx.p("remote");
    let want = want.to_str().unwrap();
    let mut faked = 0;
    let rewritten: String = table
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            if fields.get(4) == Some(&want) {
                faked += 1;
                let (head, _) = line.split_once(" - ").unwrap();
                format!("{head} - nfs4 srv:/export rw\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    assert_eq!(faked, 1, "the mount table lists the fixture mount once");
    std::fs::write(fx.p("mountinfo"), rewritten).unwrap();
    (
        Host {
            mountinfo: fx.p("mountinfo"),
        },
        mount,
    )
}

/// What the helper does with a scan root up to the walk, then the walk.
fn serve_like(root: &Path, host: &Host) -> Option<Counted> {
    let (plan, options) = prepare(root, bs_ipc::Backend::Walker, &[], &[], host).ok()?;
    Some(files_counted(plan, options, root))
}

#[test]
fn a_folder_swapped_for_a_network_mount_after_the_check_is_never_walked() {
    let Some(fx) = Fixture::new("race") else {
        return;
    };
    let (host, _remote) = race_fixture(&fx);
    // Controls: the network place is refused outright, the local one scans.
    assert!(serve_like(&fx.p("remote/x"), &host).is_none());
    assert!(matches!(
        serve_like(&fx.p("a/x"), &host),
        Some(Counted::Walked(1))
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let racer = {
        let (a, swap, stop) = (c(&fx.p("a")), c(&fx.p("swap")), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                // SAFETY: NUL-terminated paths that outlive the call.
                unsafe {
                    libc::renameat2(
                        libc::AT_FDCWD,
                        a.as_ptr(),
                        libc::AT_FDCWD,
                        swap.as_ptr(),
                        libc::RENAME_EXCHANGE,
                    )
                };
            }
        })
    };
    let want = fx.p("a/x");
    let started = std::time::Instant::now();
    let (mut checked, mut local, mut remote_walked, mut refused_after) = (0u64, 0u64, 0u64, 0u64);
    let mut unexpected = Vec::new();
    while started.elapsed() < std::time::Duration::from_secs(4) && remote_walked == 0 {
        // What the scan request does first: resolve, then require a folder.
        let Ok(real) = std::fs::canonicalize(&want) else {
            continue;
        };
        if real != want || !real.is_dir() {
            continue;
        }
        checked += 1;
        match serve_like(&real, &host) {
            Some(Counted::Walked(1)) => local += 1,
            Some(Counted::Walked(3)) => remote_walked += 1,
            Some(Counted::Walked(n)) => unexpected.push(n),
            Some(Counted::Moved { sent, .. }) => {
                assert_eq!(sent, 0, "a refused scan had sent something");
                refused_after += 1;
            }
            None => {}
        }
    }
    stop.store(true, Ordering::SeqCst);
    racer.join().unwrap();
    eprintln!(
        "checked={checked} local_walks={local} refused_after_the_check={refused_after} \
         remote_walks_after_local_verdict={remote_walked}"
    );
    // A walk that counted neither the local folder (1 file) nor the network
    // one (3) read something else.
    assert!(
        unexpected.is_empty(),
        "walks of unexpected size: {unexpected:?}"
    );
    assert!(checked > 0, "the race produced no checked folder");
    assert_eq!(
        remote_walked, 0,
        "a folder on the network mount was walked after a Local verdict"
    );
}

#[test]
fn a_remount_at_the_root_after_the_check_is_refused_with_its_own_error() {
    let Some(fx) = Fixture::new("remount") else {
        return;
    };
    std::fs::create_dir_all(fx.p("root")).unwrap();
    let first = mount_tmpfs(&fx.p("root")).unwrap();
    std::fs::write(fx.p("root/one"), "1").unwrap();
    let host = Host::system();
    let (plan, options) = prepare(&fx.p("root"), bs_ipc::Backend::Auto, &[], &[], &host)
        .expect("a tmpfs of this computer is judged fit");
    // Control: the judged folder scans.
    {
        let (plan, options) = prepare(&fx.p("root"), bs_ipc::Backend::Auto, &[], &[], &host)
            .ok()
            .unwrap();
        assert!(matches!(
            files_counted(plan, options, &fx.p("root")),
            Counted::Walked(1)
        ));
    }
    // After the check, the judged mount goes away and another is mounted at
    // the same place with other contents.
    drop(first);
    let _second = mount_tmpfs(&fx.p("root")).unwrap();
    for name in ["x", "y", "z"] {
        std::fs::write(fx.p("root").join(name), "1").unwrap();
    }
    match files_counted(plan, options, &fx.p("root")) {
        Counted::Moved { error, sent } => {
            assert_eq!(sent, 0, "something was sent from the other mount");
            assert!(matches!(error, ScanError::RootMoved { .. }), "{error}");
        }
        Counted::Walked(n) => panic!("walked the other mount: {n} files"),
    }
}

/// The pin is made from the descriptor the verdict was taken on and travels
/// in the options of every plan, so a scan whose root has become another
/// mount is refused whichever backend the request named. (On a tmpfs the
/// raw readers decline and the walker runs; each raw reader's own check is
/// tested in its crate.)
#[test]
fn the_pin_travels_with_every_plan() {
    let Some(fx) = Fixture::new("readers") else {
        return;
    };
    std::fs::create_dir_all(fx.p("root")).unwrap();
    let first = mount_tmpfs(&fx.p("root")).unwrap();
    let host = Host::system();
    let mut prepared = Vec::new();
    for backend in [
        bs_ipc::Backend::Auto,
        bs_ipc::Backend::Walker,
        bs_ipc::Backend::Btrfs,
        bs_ipc::Backend::Xfs,
        bs_ipc::Backend::Ext4,
    ] {
        let (plan, options) = prepare(&fx.p("root"), backend, &[], &[], &host).expect("judged");
        prepared.push((backend, plan, options));
    }
    drop(first);
    let _second = mount_tmpfs(&fx.p("root")).unwrap();
    std::fs::write(fx.p("root/other"), "1").unwrap();
    for (backend, plan, options) in prepared {
        match files_counted(plan, options, &fx.p("root")) {
            Counted::Moved { sent, .. } => assert_eq!(sent, 0, "{backend:?}"),
            Counted::Walked(n) => panic!("{backend:?} walked the other mount: {n} files"),
        }
    }
}

/// The plan places the scan by the mount the judged descriptor is on, not by
/// opening the root again: a mount of the same file system below the root,
/// named by the request, is entered.
#[test]
fn the_plan_is_placed_by_the_judged_mount() {
    use crate::mounts::{at_exactly, parse};
    let Some(fx) = Fixture::new("placed") else {
        return;
    };
    std::fs::create_dir_all(fx.p("root")).unwrap();
    let _root = mount_tmpfs(&fx.p("root")).unwrap();
    std::fs::create_dir(fx.p("root/sub")).unwrap();
    let Some(_sub) = bind_mount(&fx.p("root/sub"), &fx.p("root/sub")) else {
        eprintln!("NOT RUN: cannot make a bind mount here");
        assert!(std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none());
        return;
    };
    let table = parse(&std::fs::read("/proc/self/mountinfo").unwrap());
    let sub_id = at_exactly(&table, fx.p("root/sub").as_os_str().as_bytes())
        .unwrap()
        .id;
    let (plan, _options) = prepare(
        &fx.p("root"),
        bs_ipc::Backend::Walker,
        &[sub_id],
        &[],
        &Host::system(),
    )
    .expect("judged");
    assert_eq!(plan.mount_ids, vec![sub_id]);
}

/// The figure for the volume's use is asked of the folder that was judged, not
/// of its name: the name is made to lead to another file system while the
/// judgement is under way (the mount table is a named pipe, so the judgement
/// waits for it after the folder is open), and the figure must still be the
/// judged file system's.
#[test]
fn the_figure_for_the_volumes_use_is_asked_of_the_judged_folder() {
    let Some(fx) = Fixture::new("used") else {
        return;
    };
    std::fs::create_dir_all(fx.p("root")).unwrap();
    let first = mount_tmpfs(&fx.p("root")).unwrap();
    // A file the judged file system holds and the next one will not.
    std::fs::write(fx.p("root/big"), vec![7u8; 1 << 20]).unwrap();
    let table = std::fs::read("/proc/self/mountinfo").unwrap();
    let pipe = fx.p("mountinfo");
    // SAFETY: a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c(&pipe).as_ptr(), 0o600) }, 0);
    let judge = {
        let (root, pipe) = (fx.p("root"), pipe.clone());
        std::thread::spawn(move || {
            let host = Host { mountinfo: pipe };
            crate::volume::judge_scan_root(&root, &host)
                .ok()
                .and_then(|judged| judged.os_used)
        })
    };
    // Wait until the folder is open: the judgement is now waiting for the table.
    let root = fx.p("root");
    let is_open = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|to| to == root))
    };
    let started = std::time::Instant::now();
    while !is_open() && started.elapsed() < std::time::Duration::from_secs(3) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let was_open = is_open();
    // The name now leads to an empty file system.
    // SAFETY: a NUL-terminated path that outlives the call.
    unsafe { libc::umount2(c(&fx.p("root")).as_ptr(), libc::MNT_DETACH) };
    let second = mount_tmpfs(&fx.p("root")).unwrap();
    std::fs::write(&pipe, table).unwrap();
    let used = judge.join().unwrap();
    drop(second);
    drop(first);
    assert!(was_open, "the folder was not open while the table was read");
    assert!(
        used.is_some_and(|bytes| bytes >= 1 << 20),
        "the figure is not the judged file system's: {used:?}"
    );
}

/// A subvolume mounted on its own folder, below the scan's start, is entered
/// and its files are counted: the layout of a btrfs top level with `home` on
/// `/home` and a subvolume of it on `/home/alice`, made with bind mounts of one
/// tmpfs (the same shape in the kernel's table: one device, roots that are
/// folders of the file system, mount points on those very folders).
#[test]
fn a_mount_on_its_own_folder_is_entered_on_a_real_mount_table() {
    use crate::mounts::{at_exactly, parse};
    let Some(fx) = Fixture::new("own-folder") else {
        return;
    };
    std::fs::create_dir_all(fx.p("top")).unwrap();
    let _top = mount_tmpfs(&fx.p("top")).unwrap();
    std::fs::create_dir_all(fx.p("top/home/alice")).unwrap();
    for file in ["top/file", "top/home/file", "top/home/alice/file"] {
        std::fs::write(fx.p(file), "1").unwrap();
    }
    let Some(_home) = bind_mount(&fx.p("top/home"), &fx.p("top/home")) else {
        eprintln!("NOT RUN: cannot make a bind mount here");
        assert!(std::env::var_os("BYTESCAPE_REQUIRE_NAMESPACES").is_none());
        return;
    };
    let _alice = bind_mount(&fx.p("top/home/alice"), &fx.p("top/home/alice")).unwrap();
    let table = parse(&std::fs::read("/proc/self/mountinfo").unwrap());
    let id = |relative: &str| {
        at_exactly(&table, fx.p(relative).as_os_str().as_bytes())
            .unwrap()
            .id
    };
    let (home_id, alice_id) = (id("top/home"), id("top/home/alice"));
    let host = Host::system();
    let (plan, options) = prepare(
        &fx.p("top"),
        bs_ipc::Backend::Walker,
        &[home_id, alice_id],
        &[],
        &host,
    )
    .expect("judged");
    let mut entered = plan.mount_ids.clone();
    entered.sort_unstable();
    let mut want = vec![home_id, alice_id];
    want.sort_unstable();
    assert_eq!(entered, want, "a mount on its own folder was left out");
    // All three files are counted, and none is marked unreadable.
    match files_counted(plan, options, &fx.p("top")) {
        Counted::Walked(n) => assert_eq!(n, 3),
        Counted::Moved { error, .. } => panic!("{error}"),
    }
    // Control: without the mounts named, the walk stops at them.
    let (plan, options) =
        prepare(&fx.p("top"), bs_ipc::Backend::Walker, &[], &[], &host).expect("judged");
    match files_counted(plan, options, &fx.p("top")) {
        Counted::Walked(n) => assert_eq!(n, 1),
        Counted::Moved { error, .. } => panic!("{error}"),
    }
}
