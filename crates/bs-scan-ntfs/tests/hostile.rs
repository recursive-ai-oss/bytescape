//! The same corruption run over volumes made by ntfs-3g, whose records and
//! attribute lists were laid out by an implementation other than this
//! crate's test builder. Needs `mkntfs`, `ntfs-3g` and `/dev/fuse`.
#![cfg(target_os = "linux")]

mod common;
#[path = "common/hostile.rs"]
mod hostile;

use bs_scan_ntfs::FileSource;
use common::linux::*;
use hostile::*;
use std::sync::Arc;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn from_image(name: &str, spec: &ImageSpec) -> (Fixture, Base) {
    let mut fx = build(name, spec);
    fx.unmount();
    let src = FileSource::open(&fx.img).expect("open image");
    let base = survey(name, Arc::new(src));
    (fx, base)
}

#[test]
fn corrupted_ntfs_3g_volumes_never_panic_hang_or_balloon() {
    let mixed = vec![
        dir("docs"),
        file("docs/a.txt", 300),
        file("docs/b.bin", 70_000),
        link("docs/b2.bin", "docs/b.bin"),
        sparse("sparse.bin", 5 << 20, &[(0, 4096), (409_600, 12_288)]),
        compressed("z/z.txt", 300_000),
        stream("docs/a.txt", "note", 19),
        symlink_to("sym", "docs"),
    ];
    let mut lists = vec![
        file("many-streams", 20_000),
        file("many-names", 9_000),
        dir("d"),
    ];
    for i in 0..60 {
        lists.push(stream("many-streams", &format!("s{i:02}"), 150 + i));
    }
    for i in 0..45 {
        lists.push(link(
            &format!("d/a-fairly-long-hard-link-name-{i:02}"),
            "many-names",
        ));
    }
    let (_a, mixed) = from_image("ntfs3g-mixed", &ImageSpec::new(mixed));
    let (_b, lists) = from_image("ntfs3g-lists", &ImageSpec::new(lists));
    let bases = vec![mixed, lists];
    for b in &bases {
        let out = run_variant(Patched::clean(b.src.clone()), 2, b.baseline_unreachable);
        assert!(
            !out.refused && !out.damaged && out.panicked.is_none(),
            "{}: {out:?}",
            b.name
        );
    }
    let summary = drive(&bases);
    summary.print();
    summary.assert_every_family_bites();
}

#[test]
fn a_clean_read_of_the_ntfs_3g_volumes_reports_no_damage() {
    let (_a, base) = from_image("clean", &ImageSpec::new(vec![file("x", 5000)]));
    let out = run_variant(
        Patched::clean(base.src.clone()),
        1,
        base.baseline_unreachable,
    );
    assert!(
        out.report.is_some() && out.panicked.is_none() && !out.refused,
        "{out:?}"
    );
}
