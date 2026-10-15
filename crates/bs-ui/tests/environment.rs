//! The interface judges which folders are the operating system's from the
//! platform's trusted environment, not from the process environment a user
//! (or a program that started this one) can set. A child process carries the
//! planted variables, because setting them here would need unsafe code.

use std::process::Command;

use bs_layout::Metrics;
use bs_render::Rect;
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind};
use bs_ui::ViewSource;
use bs_ui::tree::TreeSource;

const CHILD: &str = "BS_UI_ENVIRONMENT_CHILD";

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        mtime: 1_709_596_800,
        ..Meta::default()
    }
}

/// Whether the card of folder `sub` under a scanned root named `root` says
/// the folder is part of the operating system.
fn blocked_under(root: &str) -> bool {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, root.as_bytes(), &meta(4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"sub", &meta(4096)).unwrap();
    for f in 0..20u32 {
        b.file(
            NodeKind::File,
            DirKey(1),
            format!("file{f}").as_bytes(),
            &meta(1_000_000 + u64::from(f)),
        )
        .unwrap();
    }
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snapshot = agg.publish();
    let mut source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).expect("root");
    let view = source.root();
    let layout = source.layout(view, Rect::new(0.0, 0.0, 1000.0, 700.0));
    let id = layout
        .items
        .iter()
        .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == "sub"))
        .expect("the folder is laid out")
        .id;
    source.card(view, id).expect("a card").blocked
}

/// Runs in a child process with the variables planted; does nothing when run
/// on its own.
#[test]
fn planted_environment_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    if cfg!(windows) {
        // SystemRoot was planted to point at this root. If the table believed
        // it, everything below would be the operating system.
        assert!(
            !blocked_under(r"Z:\planted\win"),
            "a planted SystemRoot made a folder part of the operating system"
        );
    } else {
        // Elsewhere the table comes from the process environment, as before:
        // the system folders stay blocked, an ordinary place does not.
        assert!(blocked_under("/usr/share"), "/usr/share is the system's");
        assert!(!blocked_under("/srv/planted/x"));
    }
}

#[test]
fn a_planted_variable_does_not_change_the_table() {
    let exe = std::env::current_exe().expect("the test program");
    let planted = if cfg!(windows) {
        r"Z:\planted\win"
    } else {
        "/srv/planted/x"
    };
    let output = Command::new(exe)
        .args(["planted_environment_child", "--exact", "--test-threads=1"])
        .env(CHILD, "1")
        .env("SystemRoot", planted)
        .env("windir", planted)
        .env("USERPROFILE", planted)
        .env("HOME", planted)
        .output()
        .expect("runs the child");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The child really ran the check (the filter matched one test).
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains("1 passed"), "{out}");
}
