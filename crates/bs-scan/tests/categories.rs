//! Categories set by the Linux walker while it reads: by extension, by place,
//! and not at all when switched off.
#![cfg(target_os = "linux")]

mod support;

use bs_knowledge::{Category as Known, Environment, Knowledge, classify_name};
use bs_scan::ScanOptions;
use bs_tree::{Category, NodeKind};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::process::Command;
use support::*;

fn number(category: Known) -> Category {
    Category(category.number())
}

/// The category the tables give to everything in the fixture's root folder,
/// if its own location is a place of the tables. Fixtures live under the
/// build directory, which is normally in none; when it is (a build directory
/// below `/tmp`, say) every file in the scan takes that category, and the
/// tests check that instead.
fn place_of_root(fx: &Fixture) -> Option<Category> {
    let env = Environment::from_lookup(|name| std::env::var_os(name));
    let canonical = std::fs::canonicalize(&fx.root).unwrap();
    Knowledge::new(&env)
        .place_bytes(canonical.as_os_str().as_bytes())
        .map(number)
}

#[test]
fn files_are_colored_by_extension_and_folders_are_not_colored() {
    let fx = Fixture::new("cat-ext");
    fx.file("clip.mp4", 5000);
    fx.file("Photo.JPG", 5000);
    fx.file("song.flac", 5000);
    fx.file("report.pdf", 5000);
    fx.file("backup.tar", 5000);
    fx.file("tool.exe", 5000);
    fx.file("main.rs", 5000);
    fx.file("scratch.tmp", 5000);
    fx.file("README", 5000);
    fx.file(".bashrc", 5000);
    fx.file("unknown.zzzzz", 5000);
    fx.dir("folder.mp4");
    fx.file("folder.mp4/inside.png", 100);
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    let snap = &result.snapshot;
    let ambient = place_of_root(&fx);
    let expect = |rel: &str, by_extension: Known| {
        let got = node_at(snap, rel).category();
        let want = ambient.unwrap_or(number(by_extension));
        assert_eq!(got, want, "{rel}");
    };
    expect("clip.mp4", Known::Video);
    expect("Photo.JPG", Known::Images);
    expect("song.flac", Known::Audio);
    expect("report.pdf", Known::Documents);
    expect("backup.tar", Known::Archives);
    expect("tool.exe", Known::AppsSystem);
    expect("main.rs", Known::CodeDev);
    expect("scratch.tmp", Known::CacheTemp);
    expect("README", Known::Other);
    expect(".bashrc", Known::Other);
    expect("unknown.zzzzz", Known::Other);
    expect("folder.mp4/inside.png", Known::Images);

    // A folder keeps the unclassified category, whatever it is called, and so
    // does the root.
    assert_eq!(node_at(snap, "folder.mp4").category(), Category::OTHER);
    assert_eq!(snap.root().unwrap().category(), Category::OTHER);
}

#[test]
fn links_and_special_files_are_not_told_by_their_names() {
    let fx = Fixture::new("cat-links");
    fx.file("real.bin", 100);
    symlink("real.bin", fx.path("movie.mp4")).unwrap();
    symlink("nowhere", fx.path("dangling.mp3")).unwrap();
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    let snap = &result.snapshot;
    let want = place_of_root(&fx).unwrap_or(Category::OTHER);
    for rel in ["movie.mp4", "dangling.mp3"] {
        let node = node_at(snap, rel);
        assert_eq!(node.kind(), NodeKind::Symlink, "{rel}");
        assert_eq!(node.category(), want, "{rel}");
    }
}

#[test]
fn a_place_gives_its_category_to_everything_below_it() {
    let fx = Fixture::new("cat-place");
    fx.file("proj/.cache/big.mp4", 5000);
    fx.file("proj/.cache/deeper/still/x.rs", 5000);
    fx.file("proj/.cache/plain", 5000);
    fx.file("proj/node_modules/pkg/lib.mp3", 5000);
    fx.file("proj/node_modules/pkg/sub/n.pdf", 5000);
    fx.file("proj/src/main.rs", 5000);
    fx.file("proj/src/clip.mp4", 5000);
    fx.file("proj/top.mp4", 5000);
    fx.file("pycache/__pycache__/m.mp3", 5000);
    // Looks like a place only by its prefix.
    fx.file("proj/.cached/y.mp4", 5000);
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    let snap = &result.snapshot;
    let ambient = place_of_root(&fx);
    let check = |rel: &str, want: Known| {
        assert_eq!(node_at(snap, rel).category(), number(want), "{rel}");
    };
    match ambient {
        None => {
            check("proj/.cache/big.mp4", Known::CacheTemp);
            check("proj/.cache/deeper/still/x.rs", Known::CacheTemp);
            check("proj/.cache/plain", Known::CacheTemp);
            check("proj/node_modules/pkg/lib.mp3", Known::CodeDev);
            check("proj/node_modules/pkg/sub/n.pdf", Known::CodeDev);
            check("pycache/__pycache__/m.mp3", Known::CacheTemp);
            // Outside every place, the extension decides.
            check("proj/src/main.rs", Known::CodeDev);
            check("proj/src/clip.mp4", Known::Video);
            check("proj/top.mp4", Known::Video);
            check("proj/.cached/y.mp4", Known::Video);
        }
        Some(ambient) => {
            // The fixture root is itself in a place: nothing here is outside it.
            for rel in ["proj/top.mp4", "proj/src/clip.mp4", "proj/.cached/y.mp4"] {
                assert_eq!(node_at(snap, rel).category(), ambient, "{rel}");
            }
        }
    }
    // The place folders themselves are folders.
    assert_eq!(node_at(snap, "proj/.cache").category(), Category::OTHER);
    assert_eq!(
        node_at(snap, "proj/node_modules").category(),
        Category::OTHER
    );
}

#[test]
fn every_entry_gets_what_the_tables_say_for_its_own_path() {
    // A mixed tree with awkward names; every node is checked against the
    // tables asked directly with the node's path rebuilt from the tree.
    let fx = Fixture::new("cat-oracle");
    fx.file("a/b/c/d.mp4", 3000);
    fx.file("a/.cache/e.txt", 3000);
    fx.file("a/.cache/z/f.rs", 3000);
    fx.file("a/node_modules/g/h.mp4", 3000);
    fx.file("new\nline/clip.MKV", 3000);
    fx.file("spaces in name/a b.mp3", 3000);
    fx.file("tab\tname.pdf", 3000);
    // Not UTF-8.
    let odd = fx.dir("odd");
    std::fs::write(
        odd.join(OsStr::from_bytes(b"caf\xe9 \xff.mp4")),
        vec![b'x'; 3000],
    )
    .unwrap();
    std::fs::create_dir(odd.join(OsStr::from_bytes(b"\xfe\xfd"))).unwrap();
    std::fs::write(odd.join(OsStr::from_bytes(b"\xfe\xfd/q.png")), b"x").unwrap();
    fx.file("hl/one.zip", 9000);
    std::fs::hard_link(fx.path("hl/one.zip"), fx.path("a/two.zip")).unwrap();
    symlink("a", fx.path("link-to-a")).unwrap();
    let long = "n".repeat(200);
    fx.file(&format!("{long}/{long}.iso"), 3000);
    fx.dir("empty");
    fx.settle();

    let result = scan(&fx.root);
    assert_clean(&result);
    let snap = &result.snapshot;
    let env = Environment::from_lookup(|name| std::env::var_os(name));
    let knowledge = Knowledge::new(&env);
    let mut checked = 0;
    for node in all_nodes(snap) {
        let want = match node.kind() {
            NodeKind::Dir => Category::OTHER,
            kind => {
                let parent = node.parent().expect("only the root has no parent");
                let dir = snap.path(parent.id(), b'/').unwrap();
                match knowledge.place_bytes(&dir) {
                    Some(place) => number(place),
                    None if kind == NodeKind::File => number(classify_name(node.name())),
                    None => Category::OTHER,
                }
            }
        };
        let path = snap.path(node.id(), b'/').unwrap();
        assert_eq!(node.category(), want, "{}", String::from_utf8_lossy(&path));
        checked += 1;
    }
    assert!(checked >= 20, "the whole tree was checked ({checked})");
    // And the cases the oracle and the code could get wrong together.
    if place_of_root(&fx).is_none() {
        assert_eq!(
            node_at(snap, "a/b/c/d.mp4").category(),
            number(Known::Video)
        );
        assert_eq!(
            node_at(snap, "a/.cache/e.txt").category(),
            number(Known::CacheTemp)
        );
        assert_eq!(
            node_at(snap, "a/node_modules/g/h.mp4").category(),
            number(Known::CodeDev)
        );
        assert_eq!(
            node_at(snap, "new\nline/clip.MKV").category(),
            number(Known::Video)
        );
        assert_eq!(
            node_at(snap, "tab\tname.pdf").category(),
            number(Known::Documents)
        );
        assert_eq!(
            node_at(snap, "hl/one.zip").category(),
            number(Known::Archives)
        );
        assert_eq!(
            node_at(snap, "a/two.zip").category(),
            number(Known::Archives),
            "both names of a hard link are classified"
        );
    }
}

#[test]
fn a_very_deep_tree_is_classified_without_trouble() {
    // Deeper than the number of path parts the tables look at: the answer for
    // the part of the path that is seen still holds, and nothing breaks.
    let fx = Fixture::new("cat-deep");
    let mut rel = String::from("top/.cache");
    for i in 0..70 {
        rel.push_str(&format!("/d{i}"));
    }
    rel.push_str("/leaf.mp4");
    fx.file(&rel, 100);
    fx.file("top/other.mp4", 100);
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    let snap = &result.snapshot;
    if place_of_root(&fx).is_none() {
        assert_eq!(
            node_at(snap, &rel).category(),
            number(Known::CacheTemp),
            "the cache folder is near the top"
        );
        assert_eq!(
            node_at(snap, "top/other.mp4").category(),
            number(Known::Video)
        );
    }
}

/// A chain of nested directories as deep as asked, built through
/// `/proc/self/fd` so that no path is longer than the system allows. A file at
/// the bottom, and one every 1000 levels.
fn build_chain(base: &std::path::Path, depth: usize) {
    use std::os::fd::AsRawFd;
    let mut held = std::fs::File::open(base).unwrap();
    for level in 0..depth {
        let at = format!("/proc/self/fd/{}", held.as_raw_fd());
        std::fs::create_dir(format!("{at}/d")).unwrap();
        if level % 1000 == 0 || level + 1 == depth {
            std::fs::write(format!("{at}/d/clip{level}.mp4"), vec![b'f'; 100]).unwrap();
        }
        held = std::fs::File::open(format!("{at}/d")).unwrap();
    }
}

#[test]
fn a_tree_deeper_than_a_path_can_name_keeps_the_place_of_its_top() {
    // The path to the bottom is far longer than the walker keeps for the
    // lookup, yet files down there are still inside the cache folder at the
    // top, and the walk stays fast.
    const DEPTH: usize = 6000;
    let fx = Fixture::new("cat-chain");
    fx.dir("top/.cache");
    build_chain(&fx.path("top/.cache"), DEPTH);
    fx.dir("top/plain");
    build_chain(&fx.path("top/plain"), 1500);
    fx.settle();
    let started = std::time::Instant::now();
    let result = scan(&fx.root);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    assert_clean(&result);
    let (mut in_cache, mut in_plain) = (0, 0);
    let cache = number(Known::CacheTemp);
    let ambient = place_of_root(&fx);
    for node in all_nodes(&result.snapshot) {
        if node.kind() != NodeKind::File {
            continue;
        }
        // Which chain is it in? The third name up from the chain's first
        // directory is `.cache` or `plain`.
        let mut at = node.parent();
        let mut top = None;
        while let Some(dir) = at {
            if dir.name() == b".cache" || dir.name() == b"plain" {
                top = Some(dir.name() == b".cache");
                break;
            }
            at = dir.parent();
        }
        match top {
            Some(true) => {
                in_cache += 1;
                if ambient.is_none() {
                    assert_eq!(node.category(), cache);
                }
            }
            Some(false) => {
                in_plain += 1;
                // Outside every place the extension decides.
                assert_eq!(node.category(), ambient.unwrap_or(number(Known::Video)));
            }
            None => panic!("a file outside both chains"),
        }
    }
    assert_eq!(in_cache, DEPTH.div_ceil(1000) + 1);
    assert_eq!(in_plain, 1500usize.div_ceil(1000) + 1);
}

#[test]
fn categories_can_be_switched_off() {
    let fx = Fixture::new("cat-off");
    fx.file("a.mp4", 100);
    fx.file(".cache/b.mp3", 100);
    fx.settle();
    let result = scan_with(
        &fx.root,
        ScanOptions {
            categories: false,
            ..ScanOptions::default()
        },
    );
    assert_clean(&result);
    for node in all_nodes(&result.snapshot) {
        assert_eq!(node.category(), Category::OTHER);
    }
    // And the default is on.
    assert!(ScanOptions::default().categories);
}

#[test]
fn the_tree_and_the_tables_agree_on_the_numbers() {
    // The walker writes `Category::number()` into the tree; a file of each
    // kind comes back with the number of its category.
    let fx = Fixture::new("cat-numbers");
    let samples = [
        ("a.mp4", Known::Video),
        ("a.png", Known::Images),
        ("a.mp3", Known::Audio),
        ("a.pdf", Known::Documents),
        ("a.zip", Known::Archives),
        ("a.exe", Known::AppsSystem),
        ("a.rs", Known::CodeDev),
        ("a.tmp", Known::CacheTemp),
        ("a.zzzzz", Known::Other),
    ];
    for (name, _) in samples {
        fx.file(name, 100);
    }
    fx.settle();
    let result = scan(&fx.root);
    assert_clean(&result);
    if let Some(ambient) = place_of_root(&fx) {
        // The fixture is itself in a place: every file takes its category.
        for (name, _) in samples {
            assert_eq!(
                node_at(&result.snapshot, name).category(),
                ambient,
                "{name}"
            );
        }
        return;
    }
    let mut seen = std::collections::HashSet::new();
    for (name, category) in samples {
        let got = node_at(&result.snapshot, name).category();
        assert_eq!(got.0, category.number(), "{name}");
        assert_eq!(Known::from_number(got.0), Some(category), "{name}");
        seen.insert(got.0);
    }
    assert_eq!(seen.len(), Known::ALL.len(), "every number is used once");
}

/// Home-relative places (`~/.var/app` and so on) depend on the process
/// environment, which the walker reads once when the scan starts. A child
/// copy of this test binary runs with `HOME` set to the fixture.
#[test]
fn home_relative_places_follow_the_process_environment() {
    const CHILD: &str = "BS_SCAN_CATEGORIES_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let result = scan(std::path::Path::new(&root));
        assert_clean(&result);
        let got = node_at(&result.snapshot, ".var/app/org.example/clip.mp4").category();
        assert_eq!(got, number(Known::AppsSystem), "~/.var/app with HOME set");
        return;
    }
    let fx = Fixture::new("cat-home");
    fx.file(".var/app/org.example/clip.mp4", 5000);
    fx.settle();
    let root = std::fs::canonicalize(&fx.root).unwrap();

    // Without HOME pointing here, the folder is only a folder, so the
    // extension decides (unless the fixture is itself in a place).
    let result = scan(&fx.root);
    assert_clean(&result);
    if place_of_root(&fx).is_none() {
        assert_eq!(
            node_at(&result.snapshot, ".var/app/org.example/clip.mp4").category(),
            number(Known::Video)
        );
    }

    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .env("HOME", &root)
        .env(CHILD, &root)
        .args([
            "--exact",
            "home_relative_places_follow_the_process_environment",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child run failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
