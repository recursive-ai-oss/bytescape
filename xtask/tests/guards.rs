//! Negative and positive tests for the guards. Every check must fail on
//! a planted violation and pass on clean input.
//!
//! The planted violations are assembled at run time from fragments, so this
//! file itself stays clean for `cargo xtask public-text`.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use xtask::public_text::Extra;
use xtask::{helper_deps, net_ban, public_text, unsafe_allow, wording};

#[allow(dead_code)]
mod build_metadata {
    include!("../../packaging/build-metadata.rs");
}

fn join(parts: &[&str]) -> String {
    parts.concat()
}

/// A scratch directory below the system temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let number = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("xtask-test-{}-{number}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch directory");
        Self(dir)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, text).expect("write file");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A dependency edge: the dependency's name and its kind (`None` is normal).
type Edge<'a> = (&'a str, Option<&'a str>);

/// A package: its name, resolved features and dependency edges.
type Package<'a> = (&'a str, &'a [&'a str], &'a [Edge<'a>]);

/// A `cargo metadata` document with the given packages.
fn metadata(packages: &[Package<'_>]) -> Value {
    let id = |name: &str| format!("registry+test#{name}@1.0.0");
    let package_list: Vec<Value> = packages
        .iter()
        .map(|(name, _, _)| json!({ "name": name, "version": "1.0.0", "id": id(name), "source": "registry+https://github.com/rust-lang/crates.io-index" }))
        .collect();
    let nodes: Vec<Value> = packages
        .iter()
        .map(|(name, features, deps)| {
            let deps: Vec<Value> = deps
                .iter()
                .map(|(dep, kind)| {
                    json!({ "name": dep, "pkg": id(dep), "dep_kinds": [{ "kind": kind, "target": null }] })
                })
                .collect();
            json!({ "id": id(name), "features": features, "deps": deps })
        })
        .collect();
    json!({ "packages": package_list, "resolve": { "nodes": nodes } })
}

/// Like [`metadata`], with the workspace members named (so a package that
/// nothing depends on can still be a non-member).
fn metadata_with_members(packages: &[Package<'_>], members: &[&str]) -> Value {
    let mut doc = metadata(packages);
    let ids: Vec<String> = members
        .iter()
        .map(|name| format!("registry+test#{name}@1.0.0"))
        .collect();
    doc["workspace_members"] = json!(ids);
    doc
}

fn no_allow() -> net_ban::Allow {
    net_ban::Allow::default()
}

fn flagged(doc: &Value) -> usize {
    net_ban::check_metadata(doc, "t", &no_allow()).len()
}

// ---- net-ban: crates ----

#[test]
fn net_ban_flags_every_banned_crate() {
    for name in net_ban::BANNED_CRATES {
        let doc = metadata(&[("bytescape", &[], &[(name, None)]), (name, &[], &[])]);
        assert_eq!(flagged(&doc), 1, "{name} should be flagged");
    }
}

#[test]
fn net_ban_flags_each_added_family() {
    let families: [(&str, &[&str]); 6] = [
        (
            "DNS stacks",
            &[
                "hickory-resolver",
                "hickory-proto",
                "hickory-client",
                "hickory-anything",
                "trust-dns-resolver",
                "trust-dns-server",
                "trust-dns-whatever",
            ],
        ),
        (
            "websockets",
            &["tungstenite", "tokio-tungstenite", "async-tungstenite"],
        ),
        ("async networking", &["async-net", "async-io", "smol"]),
        (
            "servers",
            &["axum", "warp", "actix-web", "tiny_http", "tiny-http"],
        ),
        (
            "http clients",
            &["reqwest", "ureq", "isahc", "surf", "attohttpc", "minreq"],
        ),
        (
            "tls and sockets",
            &["rustls", "native-tls", "openssl", "socket2", "quinn", "h2"],
        ),
    ];
    for (family, names) in families {
        for name in names {
            let doc = metadata(&[("bytescape", &[], &[(name, None)]), (name, &[], &[])]);
            assert_eq!(flagged(&doc), 1, "{family}: {name} should be flagged");
        }
    }
}

#[test]
fn net_ban_flags_the_reqwest_crate_by_name() {
    let doc = metadata(&[("reqwest", &[], &[])]);
    let findings = net_ban::check_metadata(&doc, "t", &no_allow());
    assert_eq!(findings[0].subject, "reqwest");
}

#[test]
fn net_ban_treats_underscores_like_dashes() {
    let doc = metadata(&[("native_tls", &[], &[])]);
    assert_eq!(flagged(&doc), 1);
}

#[test]
fn net_ban_passes_a_clean_graph() {
    let clean = metadata(&[("bytescape", &[], &[("serde", None)]), ("serde", &[], &[])]);
    assert_eq!(flagged(&clean), 0);
}

// ---- net-ban: features ----

#[test]
fn net_ban_flags_socket_features_only_when_enabled() {
    for (name, feature) in net_ban::BANNED_FEATURES {
        let on = metadata(&[(name, &["std", feature], &[])]);
        assert_eq!(flagged(&on), 1, "{name}/{feature}");
        let off = metadata(&[(name, &["std"], &[])]);
        assert_eq!(flagged(&off), 0, "{name} without {feature}");
    }
}

#[test]
fn net_ban_flags_the_windows_networking_features() {
    let features = [
        "Win32_Networking",
        "Win32_Networking_WinSock",
        "Win32_Networking_WinHttp",
        "Win32_Networking_WinInet",
        "Win32_Networking_Ldap",
        "Win32_System_Com_Urlmon",
    ];
    for crate_name in ["windows", "windows-sys", "windows_sys"] {
        for feature in features {
            let doc = metadata(&[(crate_name, &["Win32_Foundation", feature], &[])]);
            assert_eq!(flagged(&doc), 1, "{crate_name}/{feature}");
        }
        let fine = metadata(&[(
            crate_name,
            &[
                "Win32_Foundation",
                "Win32_Storage_FileSystem",
                "Win32_System_Pipes",
            ],
            &[],
        )]);
        assert_eq!(
            flagged(&fine),
            0,
            "{crate_name} file and pipe features are fine"
        );
    }
}

#[test]
fn refine_features_uses_the_exact_features_for_the_target() {
    // The metadata says the Windows API crate has a networking feature (some
    // other platform's dependency turns it on); the tree for this target says
    // it does not.
    let mut doc = metadata(&[
        ("bytescape", &[], &[("windows-sys", None), ("rustix", None)]),
        (
            "windows-sys",
            &["Win32_Foundation", "Win32_Networking_WinSock"],
            &[],
        ),
        ("rustix", &["net", "std"], &[]),
        ("unbuilt", &[], &[]),
    ]);
    assert_eq!(flagged(&doc), 2);
    let tree = tree_listing();
    net_ban::refine_features(&mut doc, &tree).expect("refines");
    // The windows-sys networking feature is gone; the tree keeps rustix/net.
    let findings = net_ban::check_metadata(&doc, "t", &no_allow());
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].subject, "rustix/net");
}

fn tree_listing() -> String {
    [
        "bytescape v1.0.0 (/some/path)|",
        "windows-sys v1.0.0|Win32_Foundation",
        "rustix v1.0.0|net,std",
        "rustix v1.0.0|std",
        "unbuilt v1.0.0|",
    ]
    .join("\n")
}

fn refinable() -> Value {
    metadata_with_members(
        &[
            ("bytescape", &[], &[("windows-sys", None), ("rustix", None)]),
            (
                "windows-sys",
                &["Win32_Foundation", "Win32_Networking_WinSock"],
                &[],
            ),
            ("rustix", &["net", "std"], &[]),
            ("plain", &[], &[]),
        ],
        &["bytescape"],
    )
}

#[test]
fn refine_features_fails_closed_when_the_tree_output_is_empty_or_unparsable() {
    for tree in ["", "\n\n", "no separator here\n", "|||\n"] {
        let mut doc = refinable();
        let error = net_ban::refine_features(&mut doc, tree).expect_err("must fail");
        assert!(error.contains("cannot be trusted"), "{error}");
    }
}

#[test]
fn refine_features_fails_when_a_workspace_member_is_missing() {
    let tree = "windows-sys v1.0.0|Win32_Foundation\nrustix v1.0.0|std\nplain v1.0.0|\n";
    let mut doc = refinable();
    let error = net_ban::refine_features(&mut doc, tree).expect_err("must fail");
    assert!(error.contains("bytescape"), "{error}");
}

#[test]
fn refine_features_fails_when_a_package_the_feature_bans_cover_is_missing() {
    let tree = "bytescape v1.0.0|\nrustix v1.0.0|std\nplain v1.0.0|\n";
    let mut doc = refinable();
    let error = net_ban::refine_features(&mut doc, tree).expect_err("must fail");
    assert!(error.contains("windows-sys"), "{error}");
}

#[test]
fn refine_features_accepts_packages_the_tree_leaves_out_when_no_ban_looks_at_them() {
    // The metadata also lists dependencies the target does not build; their
    // features matter to no ban.
    let mut doc = metadata_with_members(
        &[
            ("bytescape", &[], &[("windows-sys", None), ("errno", None)]),
            ("windows-sys", &["Win32_Foundation"], &[]),
            ("errno", &["std"], &[]),
        ],
        &["bytescape"],
    );
    let tree = "bytescape v1.0.0|\nwindows-sys v1.0.0|Win32_Foundation\n";
    net_ban::refine_features(&mut doc, tree).expect("errno is not covered by any feature ban");
}

/// A tree listing with the right packages and a damaged feature column.
fn listing_with_column(column: &str) -> String {
    [
        "bytescape v1.0.0 (/p)",
        "windows-sys v1.0.0",
        "rustix v1.0.0",
        "plain v1.0.0",
    ]
    .iter()
    .map(|package| format!("{package}|{column}"))
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn refine_features_fails_closed_when_the_feature_column_is_dropped() {
    // Every package is listed, but nothing follows the separator.
    let mut doc = refinable();
    let error =
        net_ban::refine_features(&mut doc, &listing_with_column("")).expect_err("must fail");
    assert!(error.contains("column is missing"), "{error}");
}

#[test]
fn refine_features_fails_closed_when_the_feature_column_is_a_literal_placeholder() {
    let mut doc = refinable();
    let error =
        net_ban::refine_features(&mut doc, &listing_with_column("{f}")).expect_err("must fail");
    assert!(error.contains("garbled"), "{error}");
}

#[test]
fn refine_features_fails_closed_when_the_tree_has_features_the_metadata_lacks() {
    for column in ["???", "Win32_Foundation,Win32_Made_Up", "std;net"] {
        let mut doc = refinable();
        let error = net_ban::refine_features(&mut doc, &listing_with_column(column))
            .expect_err("must fail");
        assert!(error.contains("garbled"), "{column}: {error}");
    }
}

#[test]
fn refine_features_accepts_features_that_are_a_subset_of_the_metadata() {
    let mut doc = refinable();
    let tree = "bytescape v1.0.0|\nwindows-sys v1.0.0|Win32_Foundation\nrustix v1.0.0|std\nplain v1.0.0|\n";
    net_ban::refine_features(&mut doc, tree).expect("subset");
}

#[test]
fn refine_features_accepts_a_workspace_with_no_features_at_all() {
    let mut doc = metadata_with_members(
        &[("bytescape", &[], &[]), ("plain", &[], &[])],
        &["bytescape"],
    );
    net_ban::refine_features(&mut doc, "bytescape v1.0.0|\nplain v1.0.0|\n")
        .expect("nothing to check");
}

#[test]
fn refine_features_accepts_a_featureless_package_the_tree_leaves_out() {
    let tree = "bytescape v1.0.0|\nwindows-sys v1.0.0|Win32_Foundation\nrustix v1.0.0|std\n";
    let mut doc = refinable();
    net_ban::refine_features(&mut doc, tree).expect("plain has no features, so it may be absent");
}

// ---- net-ban: the allowlist ----

fn windowing_graph(extra_edge: &[Edge<'_>]) -> Value {
    metadata_with_members(
        &[
            ("bytescape", &[], &[("winit", None), ("bs-ui", None)]),
            ("bs-ui", &[], extra_edge),
            ("winit", &[], &[("rustix", None)]),
            ("rustix", &["std", "net"], &[]),
        ],
        &["bytescape", "bs-ui"],
    )
}

#[test]
fn allow_entries_need_a_via() {
    let allow = net_ban::Allow::parse(
        "feature rustix/net\n\
         feature rustix via winit\n\
         crate x11rb via\n\
         source some/file.rs\n\
         feature rustix/net via winit,softbuffer  # ok\n",
    );
    assert_eq!(allow.errors.len(), 3, "{:?}", allow.errors);
    assert_eq!(allow.entries.len(), 1);
    assert_eq!(allow.entries[0].via, vec!["winit", "softbuffer"]);
    assert!(allow.sources.contains("some/file.rs"));
}

#[test]
fn no_allowlist_entry_can_admit_a_network_stack() {
    // Every banned crate except the two the screen-reader adapter admits.
    let mut names: Vec<String> = net_ban::BANNED_CRATES
        .iter()
        .map(ToString::to_string)
        .collect();
    names.extend([
        "hickory-newthing".to_string(),
        "trust-dns-newthing".to_string(),
    ]);
    let mut refused = 0;
    for name in &names {
        let allow = net_ban::Allow::parse(&format!("crate {name} via accesskit_unix\n"));
        if net_ban::MAY_BE_ALLOWED.contains(&name.as_str()) {
            assert!(allow.errors.is_empty(), "{name}: {:?}", allow.errors);
            assert_eq!(allow.entries.len(), 1, "{name}");
        } else {
            refused += 1;
            assert_eq!(allow.errors.len(), 1, "{name} must be refused");
            assert!(allow.entries.is_empty(), "{name}");
        }
    }
    for must in [
        "reqwest",
        "hyper",
        "ureq",
        "curl",
        "rustls",
        "native-tls",
        "openssl",
        "socket2",
    ] {
        assert!(net_ban::never_allowed(must), "{must}");
    }
    assert!(!net_ban::never_allowed("zbus") && !net_ban::never_allowed("async-io"));
    assert!(refused >= 30, "{refused}");
}

#[test]
fn an_entry_for_a_network_stack_is_refused_even_when_it_reaches_the_checker() {
    // The checker repeats the rule, in case an entry is built another way.
    let mut allow = net_ban::Allow::default();
    allow.entries.push(net_ban::Entry {
        kind: net_ban::Kind::Crate,
        key: "reqwest".to_string(),
        via: vec!["accesskit_unix".to_string()],
        line: 1,
    });
    let doc = metadata_with_members(
        &[
            ("bytescape", &[], &[("accesskit_unix", None)]),
            ("accesskit_unix", &[], &[("reqwest", None)]),
            ("reqwest", &[], &[]),
        ],
        &["bytescape"],
    );
    let findings = net_ban::check_metadata(&doc, "t", &allow);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].message.contains("cannot be allowed"),
        "{}",
        findings[0]
    );
}

#[test]
fn a_via_entry_admits_an_item_reached_only_through_that_crate() {
    let allow = net_ban::Allow::parse("feature rustix/net via winit  # display server socket\n");
    assert_eq!(flagged(&windowing_graph(&[])), 1, "banned without an entry");
    assert!(net_ban::check_metadata(&windowing_graph(&[]), "t", &allow).is_empty());
}

#[test]
fn a_via_entry_does_not_admit_another_path_to_the_item() {
    let allow = net_ban::Allow::parse("feature rustix/net via winit\n");
    let other_path = windowing_graph(&[("rustix", None)]);
    let findings = net_ban::check_metadata(&other_path, "t", &allow);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].message.contains("another way"),
        "{}",
        findings[0]
    );
    // A second windowing crate listed in the entry admits its path too.
    let two = metadata_with_members(
        &[
            ("bytescape", &[], &[("winit", None), ("softbuffer", None)]),
            ("winit", &[], &[("rustix", None)]),
            ("softbuffer", &[], &[("rustix", None)]),
            ("rustix", &["net"], &[]),
        ],
        &["bytescape"],
    );
    let both = net_ban::Allow::parse("feature rustix/net via winit,softbuffer\n");
    assert!(net_ban::check_metadata(&two, "t", &both).is_empty());
    assert_eq!(net_ban::check_metadata(&two, "t", &allow).len(), 1);
}

#[test]
fn an_entry_never_applies_inside_the_helper_closure() {
    let allow = net_ban::Allow::parse("feature rustix/net via winit\n");
    let doc = metadata_with_members(
        &[
            ("bytescape", &[], &[("winit", None)]),
            ("bytescape-helper", &[], &[("winit", None)]),
            ("winit", &[], &[("rustix", None)]),
            ("rustix", &["net"], &[]),
        ],
        &["bytescape", "bytescape-helper"],
    );
    let findings = net_ban::check_metadata(&doc, "t", &allow);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].message.contains("bytescape-helper"),
        "{}",
        findings[0]
    );
}

// ---- net-ban: the Linux screen-reader adapter ----

const ADAPTER_ALLOW: &str = "crate zbus via accesskit_unix\n\
                             crate async-io via accesskit_unix\n\
                             feature rustix/net via winit,softbuffer,accesskit_unix\n";

/// The interface reaches the bus crates only through `accesskit_unix`;
/// `extra` adds edges so a test can add another path.
fn adapter_graph(extra_ui: &[Edge<'_>], extra_helper: &[Edge<'_>]) -> Value {
    let mut ui: Vec<Edge<'_>> = vec![("accesskit_unix", None)];
    ui.extend_from_slice(extra_ui);
    let mut helper: Vec<Edge<'_>> = vec![];
    helper.extend_from_slice(extra_helper);
    metadata_with_members(
        &[
            ("bytescape", &[], &ui),
            ("bytescape-helper", &[], &helper),
            ("accesskit_unix", &[], &[("zbus", None), ("async-io", None)]),
            ("zbus", &[], &[("async-io", None), ("rustix", None)]),
            ("async-io", &[], &[("rustix", None)]),
            ("rustix", &["net"], &[]),
        ],
        &["bytescape", "bytescape-helper"],
    )
}

#[test]
fn the_adapter_entries_admit_the_bus_crates_only_through_accesskit_unix() {
    let allow = net_ban::Allow::parse(ADAPTER_ALLOW);
    assert!(allow.errors.is_empty(), "{:?}", allow.errors);
    let doc = adapter_graph(&[], &[]);
    assert_eq!(flagged(&doc), 3, "banned without the entries");
    assert!(net_ban::check_metadata(&doc, "t", &allow).is_empty());
}

#[test]
fn zbus_reached_by_another_path_is_still_flagged() {
    let allow = net_ban::Allow::parse(ADAPTER_ALLOW);
    // A second direct route from the interface to the bus crate.
    let doc = adapter_graph(&[("zbus", None)], &[]);
    let findings = net_ban::check_metadata(&doc, "t", &allow);
    assert!(
        findings
            .iter()
            .any(|f| f.subject == "zbus" && f.message.contains("another way")),
        "{findings:?}"
    );
    // So is a crate that is not accesskit_unix pulling in async-io.
    let doc = adapter_graph(&[("async-io", None)], &[]);
    let findings = net_ban::check_metadata(&doc, "t", &allow);
    assert!(
        findings
            .iter()
            .any(|f| f.subject == "async-io" && f.message.contains("another way")),
        "{findings:?}"
    );
}

#[test]
fn the_bus_crates_are_flagged_inside_the_helper_closure() {
    let allow = net_ban::Allow::parse(ADAPTER_ALLOW);
    for crate_name in ["zbus", "async-io", "accesskit_unix"] {
        let doc = adapter_graph(&[], &[(crate_name, None)]);
        let findings = net_ban::check_metadata(&doc, "t", &allow);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("bytescape-helper")),
            "{crate_name}: {findings:?}"
        );
    }
}

#[test]
fn the_adapter_entries_do_not_admit_other_banned_crates() {
    let allow = net_ban::Allow::parse(ADAPTER_ALLOW);
    for banned in ["reqwest", "socket2", "dbus", "smol"] {
        let doc = metadata_with_members(
            &[
                ("bytescape", &[], &[("accesskit_unix", None)]),
                ("accesskit_unix", &[], &[(banned, None)]),
                (banned, &[], &[]),
            ],
            &["bytescape"],
        );
        assert_eq!(
            net_ban::check_metadata(&doc, "t", &allow).len(),
            1,
            "{banned} must stay banned"
        );
    }
}

#[test]
fn the_shipped_allow_file_holds_exactly_the_reviewed_entries() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/net-ban.allow"))
        .expect("net-ban.allow");
    let allow = net_ban::Allow::parse(&text);
    assert!(allow.errors.is_empty(), "{:?}", allow.errors);
    let mut shipped: Vec<(String, Vec<String>)> = allow
        .entries
        .iter()
        .map(|e| (e.key.clone(), e.via.clone()))
        .collect();
    shipped.sort();
    let want = |key: &str, via: &[&str]| {
        (
            key.to_string(),
            via.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    };
    assert_eq!(
        shipped,
        vec![
            want("async-io", &["accesskit-unix"]),
            want("rustix/net", &["winit", "softbuffer", "accesskit-unix"]),
            want("zbus", &["accesskit-unix"]),
        ]
    );
    assert!(allow.sources.is_empty());
}

#[test]
fn a_via_that_scopes_nothing_does_not_admit_the_item() {
    // `via` naming the item's own crate, or a workspace crate that holds the
    // dependency, removes every path to the item; that must not admit it.
    let direct = metadata_with_members(
        &[
            ("bs-ui", &[], &[("rustix", None), ("zbus", None)]),
            ("rustix", &["net"], &[]),
            ("zbus", &[], &[]),
        ],
        &["bs-ui"],
    );
    for line in [
        "feature rustix/net via rustix",
        "feature rustix/net via bs-ui",
        "feature rustix/net via winit,bs-ui",
        "crate zbus via zbus",
        "crate zbus via bs-ui",
    ] {
        let allow = net_ban::Allow::parse(&format!("{line}\n"));
        let findings = net_ban::check_metadata(&direct, "t", &allow);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("scopes nothing")),
            "{line}: {findings:?}"
        );
    }
}

/// The windowing graph with the `winit` package's source replaced.
fn windowing_graph_with_winit_source(source: Value) -> Value {
    let mut doc = windowing_graph(&[]);
    for package in doc["packages"].as_array_mut().expect("packages") {
        if package["name"] == "winit" {
            package["source"] = source.clone();
        }
    }
    doc
}

#[test]
fn a_via_naming_a_crate_that_does_not_come_from_crates_io_does_not_admit_the_item() {
    // Anyone can write a path crate, a git repository or a private registry
    // crate with the name of a windowing library; only crates.io crates scope
    // an exception.
    let allow = net_ban::Allow::parse("feature rustix/net via winit\n");
    for (case, source) in [
        ("path crate", Value::Null),
        (
            "git checkout",
            json!("git+https://example.com/winit?branch=main"),
        ),
        ("git over a file url", json!("git+file:///tmp/gitwinit")),
        (
            "another registry",
            json!("registry+https://example.com/index"),
        ),
        (
            "another sparse registry",
            json!("sparse+https://example.com/index/"),
        ),
    ] {
        let doc = windowing_graph_with_winit_source(source);
        let findings = net_ban::check_metadata(&doc, "t", &allow);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("does not come from crates.io")),
            "{case}: {findings:?}"
        );
    }
    // Both spellings of crates.io scope the exception.
    for source in [
        "registry+https://github.com/rust-lang/crates.io-index",
        "sparse+https://index.crates.io/",
    ] {
        let doc = windowing_graph_with_winit_source(json!(source));
        assert!(
            net_ban::check_metadata(&doc, "t", &allow).is_empty(),
            "{source}"
        );
    }
}

#[test]
fn crate_entries_work_the_same_way() {
    let doc = metadata_with_members(
        &[
            ("bytescape", &[], &[("winit", None)]),
            ("winit", &[], &[("zbus", None)]),
            ("zbus", &[], &[]),
        ],
        &["bytescape"],
    );
    assert_eq!(flagged(&doc), 1);
    let allow = net_ban::Allow::parse("crate zbus via winit\n");
    assert!(net_ban::check_metadata(&doc, "t", &allow).is_empty());
}

// ---- net-ban: sources ----

fn write_and_scan(source: &str) -> usize {
    let scratch = Scratch::new();
    scratch.write("crates/bs-scan/src/lib.rs", source);
    net_ban::scan_sources(&scratch.0, &no_allow()).len()
}

#[test]
fn net_ban_finds_the_standard_network_module_in_sources() {
    let direct = join(&["use std", "::", "net::TcpStream;\n"]);
    let group = join(&["use std::{fmt, ", "net::UdpSocket};\n"]);
    let nested = join(&[
        "use std::{\n    fmt,\n    ",
        "net::{self, TcpListener},\n};\n",
    ]);
    let core_path = join(&["let _ = core", "::", "net::IpAddr::from([0u8; 4]);\n"]);
    for (case, source) in [
        ("direct", direct),
        ("group", group),
        ("nested", nested),
        ("core", core_path),
    ] {
        assert_eq!(write_and_scan(&source), 1, "{case}: {source}");
    }
}

#[test]
fn net_ban_finds_each_networking_system_api() {
    let samples = [
        ("inet constant", join(&["let f = AF_", "INET;\n"])),
        ("inet6 constant", join(&["let f = AF_", "INET6;\n"])),
        (
            "libc socket path",
            join(&["let s = libc", "::", "socket(2, 1, 0);\n"]),
        ),
        (
            "libc socket group",
            join(&["use libc::{c_int, ", "socket};\n"]),
        ),
        ("name lookup", join(&["extern { fn get", "addrinfo(); }\n"])),
        ("old name lookup", join(&["let _ = gethost", "byname;\n"])),
        ("winsock start", join(&["WSA", "Startup(0, p);\n"])),
        (
            "winsock socket",
            join(&["WSA", "SocketW(2, 1, 6, p, 0, 0);\n"]),
        ),
        ("winhttp", join(&["Win", "HttpOpen(p, 0, p, p, 0);\n"])),
        (
            "wininet open",
            join(&["Internet", "OpenW(p, 0, p, p, 0);\n"]),
        ),
        (
            "wininet connect",
            join(&["Internet", "ConnectA(h, p, 80);\n"]),
        ),
        (
            "url download",
            join(&["URLDownload", "ToFileW(p, u, f, 0, p);\n"]),
        ),
        ("url stream", join(&["URL", "OpenStreamW(p, u, 0, p);\n"])),
        (
            "windows networking path",
            join(&[
                "use windows_sys::Win32",
                "::",
                "Networking::WinSock::SOCKET;\n",
            ]),
        ),
        (
            "windows networking group",
            join(&[
                "use windows_sys::Win32::{Foundation, ",
                "Networking::WinSock};\n",
            ]),
        ),
        (
            "urlmon module",
            join(&["use windows::Win32::System::Com::Url", "mon::Foo;\n"]),
        ),
        ("address info", join(&["Get", "AddrInfoW(a, b, c, d);\n"])),
        ("address info ex", join(&["Get", "AddrInfoExW(a, b, c);\n"])),
    ];
    for (case, source) in &samples {
        assert!(write_and_scan(source) >= 1, "{case}: {source}");
    }
}

#[test]
fn net_ban_ignores_lookalikes_and_allowed_sources() {
    let scratch = Scratch::new();
    scratch.write(
        "crates/bs-platform/src/lib.rs",
        "use std::os::unix::net::UnixStream;\nuse std::{fmt, netlike};\nlet internet = 1;\n\
         let AF_INETISH = 2;\nlet socket = 3;\nuse libc::{c_int, close};\nuse windows_sys::Win32::Storage::FileSystem::CreateFileW;\n\
         let networking = 5;\n",
    );
    assert!(net_ban::scan_sources(&scratch.0, &no_allow()).is_empty());

    let planted = join(&["use std", "::", "net::TcpStream;\n"]);
    scratch.write("crates/bs-platform/src/other.rs", &planted);
    let allow = net_ban::Allow::parse("source crates/bs-platform/src/other.rs # reviewed\n");
    assert!(net_ban::scan_sources(&scratch.0, &allow).is_empty());
}

// ---- net-ban: the two layers agree ----

fn deny_toml() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../deny.toml"))
        .expect("deny.toml")
}

#[test]
fn deny_toml_repeats_the_crate_list() {
    let deny = deny_toml();
    for name in net_ban::BANNED_CRATES {
        let spelled = [
            format!("crate = \"{name}\""),
            format!("crate = \"{}\"", name.replace('-', "_")),
        ];
        assert!(
            spelled.iter().any(|s| deny.contains(s.as_str())),
            "deny.toml lacks {name}"
        );
    }
    for prefix in net_ban::BANNED_CRATE_PREFIXES {
        assert!(
            deny.contains(&format!("crate = \"{prefix}")),
            "deny.toml lacks the {prefix} family"
        );
    }
}

#[test]
fn deny_toml_leaves_feature_bans_to_net_ban() {
    // cargo-deny cannot judge features per target, so a feature ban there would
    // reject graphs the rules allow; net-ban holds all of them.
    let deny = deny_toml();
    assert!(
        !deny.contains("[[bans.features]]"),
        "feature bans belong in net-ban"
    );
    assert!(
        deny.contains("net-ban"),
        "deny.toml should say where the feature bans live"
    );
    assert!(
        net_ban::BANNED_FEATURE_PREFIXES
            .iter()
            .any(|(name, _)| *name == "windows-sys")
    );
}

#[test]
fn the_rust_version_matches_the_pinned_toolchain() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
    let toolchain =
        std::fs::read_to_string(format!("{root}/rust-toolchain.toml")).expect("toolchain file");
    let manifest = std::fs::read_to_string(format!("{root}/Cargo.toml")).expect("manifest");
    let value = |text: &str, key: &str| {
        text.lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix(key)?
                    .trim()
                    .strip_prefix('=')
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
            .unwrap_or_default()
    };
    let channel = value(&toolchain, "channel");
    let rust_version = value(&manifest, "rust-version");
    assert!(!rust_version.is_empty());
    assert!(
        channel == rust_version || channel.starts_with(&format!("{rust_version}.")),
        "rust-version {rust_version} does not match the pinned toolchain {channel}"
    );
}

// ---- helper-deps ----

fn allow(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn helper_deps_flags_a_dependency_that_is_not_listed() {
    let doc = metadata(&[
        ("bytescape-helper", &[], &[("bs-ipc", None), ("libc", None)]),
        ("bs-ipc", &[], &[]),
        ("libc", &[], &[]),
    ]);
    let findings = helper_deps::check_metadata(&doc, "t", &allow(&["bs-ipc"]));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].subject, "libc");
}

#[test]
fn helper_deps_follows_indirect_dependencies() {
    let doc = metadata(&[
        ("bytescape-helper", &[], &[("bs-ipc", None)]),
        ("bs-ipc", &[], &[("itoa", None)]),
        ("itoa", &[], &[]),
    ]);
    let findings = helper_deps::check_metadata(&doc, "t", &allow(&["bs-ipc"]));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].subject, "itoa");
}

#[test]
fn helper_deps_ignores_build_and_dev_dependencies_and_other_packages() {
    let doc = metadata(&[
        (
            "bytescape-helper",
            &[],
            &[
                ("bs-ipc", None),
                ("cc", Some("build")),
                ("tester", Some("dev")),
            ],
        ),
        ("bs-ipc", &[], &[]),
        ("cc", &[], &[]),
        ("tester", &[], &[]),
        ("bytescape", &[], &[("winit", None)]),
        ("winit", &[], &[]),
    ]);
    assert!(helper_deps::check_metadata(&doc, "t", &allow(&["bs-ipc"])).is_empty());
}

#[test]
fn helper_deps_fails_when_the_helper_is_missing() {
    let doc = metadata(&[("bytescape", &[], &[])]);
    assert_eq!(helper_deps::check_metadata(&doc, "t", &allow(&[])).len(), 1);
}

#[test]
fn helper_build_closure_flags_a_direct_build_dependency_that_is_not_listed() {
    let doc = metadata(&[
        (
            "bytescape-helper",
            &[],
            &[
                ("bs-ipc", None),
                ("embed-resource", Some("build")),
                ("cc", Some("build")),
                ("tester", Some("dev")),
            ],
        ),
        ("bs-ipc", &[], &[]),
        ("embed-resource", &[], &[]),
        ("cc", &[], &[]),
        ("tester", &[], &[]),
    ]);
    let findings = helper_deps::check_build_dependencies(&doc, "t", &allow(&["embed-resource"]));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].subject, "cc");
    let both = allow(&["embed-resource", "cc"]);
    assert!(helper_deps::check_build_dependencies(&doc, "t", &both).is_empty());
}

#[test]
fn helper_build_closure_follows_normal_and_build_edges_of_build_dependencies() {
    let doc = metadata(&[
        (
            "bytescape-helper",
            &[],
            &[("embed-resource", Some("build"))],
        ),
        (
            "embed-resource",
            &[],
            &[
                ("toml", None),
                ("vswhom", Some("build")),
                ("tester", Some("dev")),
            ],
        ),
        ("toml", &[], &[("serde-spanned", None)]),
        ("serde-spanned", &[], &[]),
        ("vswhom", &[], &[]),
        ("tester", &[], &[]),
    ]);
    let found = helper_deps::helper_build_closure(&doc).expect("helper present");
    assert_eq!(
        found,
        allow(&["embed-resource", "toml", "serde-spanned", "vswhom"])
    );
    // A new crate deep in the closure fails even though no direct dependency changed.
    let findings = helper_deps::check_build_dependencies(
        &doc,
        "t",
        &allow(&["embed-resource", "toml", "vswhom"]),
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].subject, "serde-spanned");
}

#[test]
fn helper_build_closure_covers_build_dependencies_of_the_linked_crates() {
    // A build dependency planted on a crate the helper links in (here bs-ipc,
    // two steps down) runs while the helper is built, so it is listed too.
    let doc = metadata(&[
        ("bytescape-helper", &[], &[("bs-ipc", None)]),
        ("bs-ipc", &[], &[("bs-tree", None), ("itoa", Some("build"))]),
        (
            "bs-tree",
            &[],
            &[("cc", Some("build")), ("tester", Some("dev"))],
        ),
        ("itoa", &[], &[("memchr", None)]),
        ("cc", &[], &[]),
        ("memchr", &[], &[]),
        ("tester", &[], &[]),
        ("unrelated", &[], &[("serde", Some("build"))]),
        ("serde", &[], &[]),
    ]);
    let found = helper_deps::helper_build_closure(&doc).expect("helper present");
    assert_eq!(found, allow(&["itoa", "memchr", "cc"]));
    let findings = helper_deps::check_build_dependencies(&doc, "t", &allow(&["cc"]));
    let subjects: Vec<&str> = findings.iter().map(|f| f.subject.as_str()).collect();
    assert_eq!(subjects, ["itoa", "memchr"]);
    assert!(
        helper_deps::check_build_dependencies(&doc, "t", &allow(&["cc", "itoa", "memchr"]))
            .is_empty()
    );
}

#[test]
fn helper_build_closure_ignores_the_normal_closure_and_dev_dependencies() {
    let doc = metadata(&[
        (
            "bytescape-helper",
            &[],
            &[("bs-ipc", None), ("tester", Some("dev"))],
        ),
        ("bs-ipc", &[], &[("itoa", None)]),
        ("itoa", &[], &[]),
        ("tester", &[], &[]),
    ]);
    let found = helper_deps::helper_build_closure(&doc).expect("helper present");
    assert!(found.is_empty());
}

#[test]
fn helper_build_closure_fails_when_the_helper_is_missing() {
    let doc = metadata(&[("bytescape", &[], &[])]);
    assert_eq!(
        helper_deps::check_build_dependencies(&doc, "t", &allow(&[])).len(),
        1
    );
}

#[test]
fn helper_deps_fails_when_the_helper_gets_the_iouring_feature() {
    // What `cargo tree -p bytescape-helper -e features --prefix none` prints
    // for a helper whose bs-scan dependency enables the benchmark feature.
    let planted = "bytescape-helper v0.1.0 (/w)\nbs-scan v0.1.0 (/w)\nbs-scan feature \"default\"\nbs-scan feature \"iouring-bench\"\n";
    let findings = helper_deps::check_iouring(planted, "x86_64-unknown-linux-gnu");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].subject, "bs-scan/iouring-bench");
    // The same line inside a drawn tree, with branch characters in front.
    let drawn =
        "bytescape-helper v0.1.0\n\u{2514}\u{2500}\u{2500} bs-scan feature \"iouring-bench\"\n";
    assert_eq!(helper_deps::check_iouring(drawn, "t").len(), 1);
}

#[test]
fn helper_deps_passes_a_helper_without_the_iouring_feature() {
    let clean = "bytescape-helper v0.1.0 (/w)\nbs-scan v0.1.0 (/w)\nbs-scan feature \"default\"\nsome-crate feature \"iouring-bench\"\nbs-scan feature \"iouring-bench-extra\"\n";
    assert!(helper_deps::check_iouring(clean, "t").is_empty());
}

#[test]
fn helper_deps_allow_file_ignores_comments() {
    let parsed = helper_deps::parse_allow("# note\nbs_tree  # why\n\nbs-ipc\n");
    assert_eq!(parsed, allow(&["bs-tree", "bs-ipc"]));
}

// ---- wording ----

#[test]
fn wording_flags_each_banned_word() {
    for text in [
        "This is safe to delete.",
        "Remove the JUNK first.",
        "Space was freed.",
        "Reclaim space now.",
        "Reclaimable: 4 GB",
        "reclaimed",
        "Time to clean up the disk.",
        "Time to clean\nup the disk.",
        "Time to Clean   Up.",
    ] {
        assert!(!wording::scan_text(text).is_empty(), "should flag: {text}");
    }
}

#[test]
fn wording_accepts_lookalikes() {
    for text in [
        "unsafe code is denied; safety comments are required",
        "A cleanup advisor is exactly what this is not.",
        "Free space and a clean install are fine.",
        "Moved to Recycle Bin, and the bin can be emptied.",
        "Look up the path, then clean the lens.",
        "safeguard, junkyard, reclamation",
    ] {
        assert!(wording::scan_text(text).is_empty(), "should pass: {text}");
    }
}

#[test]
fn wording_reports_line_numbers() {
    assert_eq!(wording::scan_text("fine\nfine\nsafe\n"), vec![(3, "safe")]);
}

#[test]
fn wording_passes_clean_files_in_every_covered_place() {
    let scratch = Scratch::new();
    scratch.write("README.md", "# Bytescape\n\nA visualizer.\n");
    scratch.write("CHANGELOG.md", "## Unreleased\n\n- Added things.\n");
    scratch.write("docs/building.md", "Build it.\n");
    scratch.write(".github/ISSUE_TEMPLATE/bug_report.yml", "name: Bug\n");
    scratch.write(
        "packaging/linux/app.metainfo.xml",
        "<description>ok</description>\n",
    );
    assert!(wording::run(&scratch.0, &[]).expect("runs").is_empty());
}

#[test]
fn wording_scans_every_covered_place() {
    let places = [
        "README.md",
        "CHANGELOG.md",
        "docs/building.md",
        "docs/deep/nested/page.md",
        ".github/ISSUE_TEMPLATE/bug_report.yml",
        ".github/ISSUE_TEMPLATE/extra.yml",
        "packaging/linux/app.metainfo.xml",
        "packaging/linux/bytescape.desktop",
        "extra/store.txt",
    ];
    for file in places {
        let scratch = Scratch::new();
        scratch.write("README.md", "# Bytescape\n");
        scratch.write(file, "12.3 GB freed. Safe to delete junk.\n");
        let extra = if file.starts_with("extra/") {
            vec![scratch.0.join("extra")]
        } else {
            Vec::new()
        };
        let findings = wording::run(&scratch.0, &extra).expect("runs");
        assert_eq!(findings.len(), 3, "{file}: {findings:?}");
    }
}

#[test]
fn wording_leaves_the_files_that_explain_the_rule_alone() {
    let scratch = Scratch::new();
    scratch.write("README.md", "# Bytescape\n");
    for file in [
        "CONTRIBUTING.md",
        "SECURITY.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
    ] {
        scratch.write(file, "Never say safe, junk or freed.\n");
    }
    assert!(wording::run(&scratch.0, &[]).expect("runs").is_empty());
}

#[test]
fn wording_fails_on_text_files_it_cannot_read_as_utf8() {
    // UTF-16 with a byte order mark, as some editors save it.
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("safe junk".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    for file in [
        "docs/page.md",
        "CHANGELOG.md",
        ".github/ISSUE_TEMPLATE/form.yml",
        "packaging/a.xml",
    ] {
        let scratch = Scratch::new();
        scratch.write("README.md", "# Bytescape\n");
        let path = scratch.0.join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, &utf16).expect("write");
        let findings = wording::run(&scratch.0, &[]).expect("runs");
        assert_eq!(findings.len(), 1, "{file}: {findings:?}");
        assert!(findings[0].message.contains("UTF-8"), "{}", findings[0]);
    }
}

#[test]
fn wording_skips_images_in_the_prose_folders() {
    let scratch = Scratch::new();
    scratch.write("README.md", "# Bytescape\n");
    let path = scratch.0.join("docs/screenshot.png");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, [0x89, b'P', b'N', b'G', 0xFF, 0xFE, 0x00]).expect("write");
    assert!(wording::run(&scratch.0, &[]).expect("runs").is_empty());
}

#[test]
fn wording_refuses_to_pass_without_a_readme() {
    let scratch = Scratch::new();
    assert!(wording::run(&scratch.0, &[]).is_err());
}

// ---- public-text ----

fn scan(text: &str) -> Vec<(usize, String)> {
    public_text::scan_text(text, &Extra::default())
}

#[test]
fn public_text_flags_internal_ids_and_names() {
    let planted = [
        join(&["see ", "Q", "15"]),
        join(&["rule ", "NN", "-", "3"]),
        join(&["rule ", "NN", "\u{2011}", "3"]),
        join(&["rule ", "NN", "\u{2013}", "3"]),
        join(&["gate ", "G", "2"]),
        join(&["phase ", "P", "0"]),
        join(&["unit ", "p2", "-example"]),
        join(&["unit ", "p2", "\u{2011}example"]),
        join(&["unit ", "12"]),
        join(&["read ", "PL", "AN.md"]),
        join(&["read ", "DES", "IGN.md"]),
        join(&["read ", "RU", "LES.md"]),
        join(&["read ", "REQUIRE", "MENTS.md"]),
        join(&["an ", "orches", "trator"]),
        join(&["a ", "special", "ist"]),
        join(&["an ", "ag", "ent"]),
        join(&["our ", "ag", "ents"]),
        join(&["the ", "review", "er"]),
        join(&["the ", "archi", "tect"]),
        join(&["per the ", "rul", "ing"]),
        join(&["two ", "rul", "ings"]),
        join(&["we ", "wai", "ve it"]),
        join(&["it was ", "wai", "ved"]),
        join(&["see \u{a7}", "2.10"]),
        join(&["commit ", "3d3c42", "e5aac5", "ba8"]),
    ];
    for text in &planted {
        assert!(!scan(text).is_empty(), "should flag: {text}");
    }
}

#[test]
fn public_text_accepts_ordinary_text() {
    for text in [
        "Q is a letter and so is P.",
        "A polkit authentication agent shows the prompt.",
        "The user agent string is never sent.",
        "Pinned: uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
        "AppId={{A1B2C3D4-0000-4000-8000-0123456789AB}",
        "Colors #F7F7F8 and #1B1B1E; 1700000000; version 1.99.0",
        "Windows 11, x64 and arm64; G is for gigabytes",
        "plan the release; design notes; the rules of the road",
        "The CPU architecture and the architectural style.",
        "A rule of thumb; ruled out; a ruler.",
        "Maintainers review the change.",
    ] {
        assert!(scan(text).is_empty(), "should pass: {text}");
    }
}

#[test]
fn public_text_keeps_names_specific_to_the_private_material_out_of_the_built_in_patterns() {
    let private_repo = join(&["bytescape-", "internal"]);
    let host_path = join(&["/x/", "al", "era/projects"]);
    let team_word = join(&["the ", "sw", "arm"]);
    let release_path = join(&["see release/", "human-", "steps/x"]);
    for text in [&private_repo, &host_path, &team_word, &release_path] {
        assert!(
            scan(text).is_empty(),
            "built in patterns should not name: {text}"
        );
    }
}

#[test]
fn public_text_extra_patterns_catch_the_private_names() {
    let extra = Extra::parse(&format!(
        "# comment\nword {}\nword {}\nsubstring {}\nsubstring {}\nsubstring {}\n",
        join(&["al", "era"]),
        join(&["sw", "arm"]),
        join(&["bytescape-", "internal"]),
        join(&["human-", "steps"]),
        join(&["release/", "store"]),
    ))
    .expect("parses");
    for text in [
        join(&["the ", "Al", "era server"]),
        join(&["the ", "sw", "arm folder"]),
        join(&["repo ", "bytescape-", "internal"]),
        join(&["/x/", "al", "era/foo"]),
        join(&["release/", "human-", "steps/x.md"]),
        join(&["release/", "store/listing"]),
    ] {
        assert!(
            !public_text::scan_text(&text, &extra).is_empty(),
            "should flag: {text}"
        );
    }
    assert!(public_text::scan_text("ordinary text", &extra).is_empty());
}

#[test]
fn public_text_extra_pattern_files_must_be_well_formed() {
    assert!(Extra::parse("word one\nsubstring two words\n").is_ok());
    assert!(Extra::parse("nonsense line\n").is_err());
    assert!(Extra::parse("word\n").is_err());
}

#[test]
fn public_text_scans_every_file_but_the_pattern_list() {
    let scratch = Scratch::new();
    scratch.write("README.md", "clean\n");
    scratch.write(".github/workflows/ci.yml", "# a comment\n");
    scratch.write("crates/bs-tree/src/lib.rs", "//! docs\n");
    assert!(
        public_text::run(&scratch.0, &Extra::default())
            .expect("runs")
            .is_empty()
    );

    let planted = join(&["// ", "Q", "7 says so\n"]);
    for file in [
        ".github/workflows/ci.yml",
        "crates/bs-tree/src/lib.rs",
        "docs/a.md",
        "Cargo.toml",
    ] {
        let scratch = Scratch::new();
        scratch.write("README.md", "clean\n");
        scratch.write(file, &planted);
        assert_eq!(
            public_text::run(&scratch.0, &Extra::default())
                .expect("runs")
                .len(),
            1,
            "{file}"
        );
    }
}

// ---- unsafe-allow ----

fn allow_attr(lint: &str) -> String {
    join(&["#![al", "low(", lint, ")]\n"])
}

#[test]
fn unsafe_allow_flags_the_lint_outside_the_listed_crates() {
    let unsafe_lint = join(&["unsafe", "_code"]);
    let attr = allow_attr(&unsafe_lint);
    for rel in [
        "crates/bs-ui/src/lib.rs",
        "crates/bs-tree/src/lib.rs",
        "crates/bs-layout/src/module/deep.rs",
        "crates/bs-ipc/src/lib.rs",
        "crates/bytescape/src/main.rs",
        "xtask/src/lib.rs",
        "packaging/build-metadata.rs",
    ] {
        assert_eq!(unsafe_allow::check_source(rel, &attr).len(), 1, "{rel}");
    }
    let in_list = join(&["#[cfg_attr(test, al", "low(unsafe", "_code, dead_code))]\n"]);
    assert_eq!(
        unsafe_allow::check_source("crates/bs-ui/src/lib.rs", &in_list).len(),
        1
    );
    let expect_form = join(&["#![ex", "pect(unsafe", "_code)]\n"]);
    assert_eq!(
        unsafe_allow::check_source("crates/bs-ui/src/lib.rs", &expect_form).len(),
        1
    );
}

#[test]
fn unsafe_allow_admits_the_listed_crates() {
    let attr = allow_attr(&join(&["unsafe", "_code"]));
    for (name, _) in unsafe_allow::MAY_ALLOW_UNSAFE {
        let rel = format!("crates/{name}/src/platform/linux.rs");
        assert!(unsafe_allow::check_source(&rel, &attr).is_empty(), "{name}");
    }
    assert_eq!(unsafe_allow::MAY_ALLOW_UNSAFE.len(), 9);
}

#[test]
fn unsafe_allow_never_admits_switching_off_the_safety_comment_lints() {
    for lint in [
        join(&["clippy::undocumented", "_unsafe_blocks"]),
        join(&["unsafe_op_in", "_unsafe_fn"]),
    ] {
        for rel in ["crates/bs-platform/src/lib.rs", "crates/bs-ui/src/lib.rs"] {
            assert_eq!(
                unsafe_allow::check_source(rel, &allow_attr(&lint)).len(),
                1,
                "{rel} {lint}"
            );
        }
    }
}

#[test]
fn unsafe_allow_ignores_other_lints_and_plain_words() {
    let other = allow_attr("dead_code");
    assert!(unsafe_allow::check_source("crates/bs-ui/src/lib.rs", &other).is_empty());
    let text = "// the crate does not use unsafe code\nfn may_allow(x: u8) -> u8 { x }\n";
    assert!(unsafe_allow::check_source("crates/bs-ui/src/lib.rs", text).is_empty());
}

const INHERITING: &str = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n";

#[test]
fn unsafe_allow_checks_manifests() {
    let setting = join(&["unsafe", "_code = \"allow\"\n"]);
    let manifest = format!("{INHERITING}\n[lints.rust]\n{setting}");
    assert_eq!(
        unsafe_allow::check_manifest("crates/bs-ui/Cargo.toml", &manifest).len(),
        1
    );
    assert!(unsafe_allow::check_manifest("crates/bs-platform/Cargo.toml", &manifest).is_empty());
    let commented = format!("{INHERITING}\n# {setting}");
    assert!(unsafe_allow::check_manifest("crates/bs-ui/Cargo.toml", &commented).is_empty());

    let denying = join(&["[workspace.lints.rust]\nunsafe", "_code = \"deny\"\n"]);
    assert!(unsafe_allow::check_manifest("Cargo.toml", &denying).is_empty());
    let relaxed = join(&["[workspace.lints.rust]\nunsafe", "_code = \"warn\"\n"]);
    assert_eq!(
        unsafe_allow::check_manifest("Cargo.toml", &relaxed).len(),
        1
    );
}

#[test]
fn unsafe_allow_requires_every_member_to_inherit_the_workspace_lints() {
    let without = "[package]\nname = \"x\"\n";
    let other_table = "[package]\nname = \"x\"\n\n[dependencies]\nworkspace = true\n";
    let not_true = "[lints]\nworkspace = false\n";
    for rel in [
        "crates/bs-ui/Cargo.toml",
        "crates/bs-platform/Cargo.toml",
        "xtask/Cargo.toml",
    ] {
        for text in [without, other_table, not_true] {
            let findings = unsafe_allow::check_manifest(rel, text);
            assert_eq!(findings.len(), 1, "{rel}: {text}");
            assert!(findings[0].message.contains("workspace = true"));
        }
        assert!(
            unsafe_allow::check_manifest(rel, INHERITING).is_empty(),
            "{rel}"
        );
        let spaced = "[ lints ]\nworkspace   =   true  # inherit\n";
        assert!(
            unsafe_allow::check_manifest(rel, spaced).is_empty(),
            "{rel} spaced"
        );
    }
}

#[test]
fn unsafe_allow_accepts_every_cargo_spelling_of_lint_inheritance() {
    let package = "[package]\nname = \"x\"\n";
    let spellings = [
        format!("{package}\n[lints]\nworkspace = true\n"),
        format!("{package}\n[lints]\nworkspace\t=\ttrue\n"),
        format!("{package}\n[\tlints\t]\n\tworkspace = true # inherit\n"),
        format!("{package}\n[lints]\r\nworkspace = true\r\n"),
        format!("lints.workspace = true\n\n{package}"),
        format!("lints\t.\tworkspace\t=\ttrue\n{package}"),
        format!("lints = {{ workspace = true }}\n\n{package}"),
        format!("lints={{workspace=true}}\n{package}"),
        format!("lints\t=\t{{\tworkspace\t=\ttrue\t}}\n{package}"),
    ];
    for text in &spellings {
        assert!(
            unsafe_allow::check_manifest("crates/bs-ui/Cargo.toml", text).is_empty(),
            "should accept: {text:?}"
        );
    }
}

#[test]
fn unsafe_allow_rejects_lookalikes_of_lint_inheritance() {
    let package = "[package]\nname = \"x\"\n";
    let rejected = [
        // Dotted and inline forms only count before the first table header.
        format!("{package}lints.workspace = true\n"),
        format!("{package}lints = {{ workspace = true }}\n"),
        // Wrong value, wrong key, wrong table.
        "lints.workspace = false\n".to_string(),
        "lints = { workspace = false }\n".to_string(),
        "lints = { other = true }\n".to_string(),
        format!("{package}\n[lints.rust]\nunused = \"warn\"\n"),
        format!("{package}\n[workspace]\nworkspace = true\n"),
        format!("{package}\n# lints.workspace = true\n"),
    ];
    for text in &rejected {
        assert_eq!(
            unsafe_allow::check_manifest("crates/bs-ui/Cargo.toml", text).len(),
            1,
            "should reject: {text:?}"
        );
    }
}

#[test]
fn unsafe_allow_checks_only_workspace_members_for_inheritance() {
    let scratch = Scratch::new();
    scratch.write(
        "Cargo.toml",
        "[workspace.lints.rust]\nunsafe_code = \"deny\"\n",
    );
    scratch.write("crates/bs-ui/Cargo.toml", INHERITING);
    scratch.write(
        "crates/bs-tree/Cargo.toml",
        "[package]\nname = \"bs-tree\"\n",
    );
    // A separate workspace (a fuzz setup, a test fixture) cannot inherit.
    scratch.write(
        "fuzz/Cargo.toml",
        "[package]\nname = \"fuzz\"\n[workspace]\n",
    );
    let members = vec![
        "crates/bs-ui/Cargo.toml".to_string(),
        "crates/bs-tree/Cargo.toml".to_string(),
    ];
    let findings = unsafe_allow::run_with_members(&scratch.0, &members).expect("runs");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].subject, "crates/bs-tree/Cargo.toml");
}

#[test]
fn unsafe_allow_still_checks_the_unsafe_setting_in_manifests_outside_the_workspace() {
    let scratch = Scratch::new();
    scratch.write(
        "Cargo.toml",
        "[workspace.lints.rust]\nunsafe_code = \"deny\"\n",
    );
    let relaxed = format!("{INHERITING}\n[lints.rust]\nunsafe_code = \"allow\"\n");
    scratch.write("crates/bs-ui/Cargo.toml", &relaxed);
    let findings = unsafe_allow::run_with_members(&scratch.0, &[]).expect("runs");
    assert_eq!(findings.len(), 1, "{findings:?}");
}

/// A real, path-only Cargo workspace in a scratch folder (no network needed).
fn real_workspace(members: &[&str], extra: &[(&str, &str)]) -> Scratch {
    let scratch = Scratch::new();
    let member_list = members
        .iter()
        .map(|m| format!("\"crates/{m}\""))
        .collect::<Vec<_>>()
        .join(", ");
    scratch.write(
        "Cargo.toml",
        &format!("[workspace]\nresolver = \"3\"\nmembers = [{member_list}]\nexclude = [\"vendor\", \"fuzz\"]\n\n[workspace.lints.rust]\nunsafe_code = \"deny\"\n"),
    );
    for member in members {
        scratch.write(
            &format!("crates/{member}/Cargo.toml"),
            &format!("[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lints]\nworkspace = true\n"),
        );
        scratch.write(&format!("crates/{member}/src/lib.rs"), "");
    }
    for (path, text) in extra {
        scratch.write(path, text);
    }
    let status =
        std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(&scratch.0)
            .args(["generate-lockfile", "--offline"])
            .output()
            .expect("cargo runs");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    scratch
}

const EVIL_LIB: &str = "pub fn peek(p: *const u8) -> u8 { unsafe { *p } }\n";

#[test]
fn unsafe_allow_flags_a_non_member_path_crate_compiled_into_the_product() {
    // A known bypass shape: a path crate with its own [workspace], excluded
    // from the workspace, that a member depends on.
    let scratch = real_workspace(&["bs-knowledge"], &[]);
    scratch.write(
        "vendor/evil/Cargo.toml",
        "[package]\nname = \"evil\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    );
    scratch.write("vendor/evil/src/lib.rs", EVIL_LIB);
    scratch.write(
        "crates/bs-knowledge/Cargo.toml",
        "[package]\nname = \"bs-knowledge\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nevil = { path = \"../../vendor/evil\" }\n\n[lints]\nworkspace = true\n",
    );
    let relock = std::process::Command::new("cargo")
        .current_dir(&scratch.0)
        .args(["generate-lockfile", "--offline"])
        .output()
        .expect("cargo");
    assert!(relock.status.success());
    let findings = unsafe_allow::run(&scratch.0).expect("runs");
    assert!(
        findings
            .iter()
            .any(|f| f.subject == "vendor/evil/Cargo.toml"
                && f.message.contains("not a workspace member")),
        "{findings:?}"
    );
}

#[test]
fn unsafe_allow_flags_a_path_crate_that_only_an_optional_feature_pulls_in() {
    // The crate is reachable only with a feature on, which a default
    // `cargo metadata` leaves out of the graph.
    let scratch = real_workspace(&["bs-knowledge"], &[]);
    scratch.write(
        "vendor/evil/Cargo.toml",
        "[package]\nname = \"evil\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    );
    scratch.write("vendor/evil/src/lib.rs", EVIL_LIB);
    scratch.write(
        "crates/bs-knowledge/Cargo.toml",
        "[package]\nname = \"bs-knowledge\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nevil = { path = \"../../vendor/evil\", optional = true }\n\n[features]\nextra = [\"dep:evil\"]\n\n[lints]\nworkspace = true\n",
    );
    let relock = std::process::Command::new("cargo")
        .current_dir(&scratch.0)
        .args(["generate-lockfile", "--offline"])
        .output()
        .expect("cargo");
    assert!(relock.status.success());
    let findings = unsafe_allow::run(&scratch.0).expect("runs");
    assert!(
        findings
            .iter()
            .any(|f| f.subject == "vendor/evil/Cargo.toml"
                && f.message.contains("not a workspace member")),
        "{findings:?}"
    );
}

#[test]
fn unsafe_allow_flags_a_non_member_that_brings_its_own_workspace_lints() {
    // The same shape, with the crate defining its own [workspace.lints] that
    // allows what the real ones deny: it still cannot inherit ours.
    let scratch = real_workspace(&["bs-knowledge"], &[]);
    scratch.write(
        "vendor/evil/Cargo.toml",
        "[package]\nname = \"evil\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lints]\nworkspace = true\n\n[workspace]\n\n[workspace.lints.rust]\nunsafe_code = \"allow\"\n",
    );
    scratch.write("vendor/evil/src/lib.rs", EVIL_LIB);
    scratch.write(
        "crates/bs-knowledge/Cargo.toml",
        "[package]\nname = \"bs-knowledge\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nevil = { path = \"../../vendor/evil\" }\n\n[lints]\nworkspace = true\n",
    );
    let relock = std::process::Command::new("cargo")
        .current_dir(&scratch.0)
        .args(["generate-lockfile", "--offline"])
        .output()
        .expect("cargo");
    assert!(relock.status.success());
    let findings = unsafe_allow::run(&scratch.0).expect("runs");
    assert!(
        findings
            .iter()
            .any(|f| f.subject == "vendor/evil/Cargo.toml"),
        "{findings:?}"
    );
}

#[test]
fn unsafe_allow_leaves_a_separate_workspace_outside_the_product_graph_alone() {
    let scratch = real_workspace(&["bs-knowledge"], &[]);
    scratch.write(
        "fuzz/Cargo.toml",
        "[package]\nname = \"fuzz\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
    );
    scratch.write("fuzz/src/lib.rs", EVIL_LIB);
    let findings = unsafe_allow::run(&scratch.0).expect("runs");
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn unsafe_allow_classifies_members_and_path_crates_from_metadata() {
    let metadata = json!({
        "workspace_members": ["m1"],
        "packages": [
            { "id": "m1", "manifest_path": "/r/crates/a/Cargo.toml", "source": null },
            { "id": "p1", "manifest_path": "/r/vendor/evil/Cargo.toml", "source": null },
            { "id": "g1", "manifest_path": "/home/.cargo/git/x/Cargo.toml", "source": "git+https://example.com/x#abc" },
            { "id": "c1", "manifest_path": "/home/.cargo/registry/y/Cargo.toml", "source": "registry+https://github.com/rust-lang/crates.io-index" }
        ]
    });
    let (members, outside) =
        unsafe_allow::classify_packages(&metadata, std::path::Path::new("/r")).expect("classifies");
    assert_eq!(members, vec!["crates/a/Cargo.toml"]);
    assert_eq!(outside, vec!["vendor/evil/Cargo.toml"]);
    let empty = json!({ "workspace_members": [], "packages": [] });
    assert!(unsafe_allow::classify_packages(&empty, std::path::Path::new("/r")).is_err());
}

#[test]
fn the_workspace_member_list_comes_from_cargo() {
    let members = unsafe_allow::member_manifests(&xtask::util::default_root()).expect("metadata");
    assert!(
        members.contains(&"xtask/Cargo.toml".to_string()),
        "{members:?}"
    );
    assert!(
        members.contains(&"crates/bytescape/Cargo.toml".to_string()),
        "{members:?}"
    );
    assert!(!members.contains(&"Cargo.toml".to_string()));
}

#[test]
fn unsafe_allow_runs_over_a_tree() {
    let scratch = Scratch::new();
    let denying = join(&["unsafe", "_code = \"deny\"\n"]);
    scratch.write("Cargo.toml", &denying);
    scratch.write(
        "crates/bs-platform/src/lib.rs",
        &allow_attr(&join(&["unsafe", "_code"])),
    );
    assert!(
        unsafe_allow::run_with_members(&scratch.0, &[])
            .expect("runs")
            .is_empty()
    );
    scratch.write(
        "crates/bs-ui/src/lib.rs",
        &allow_attr(&join(&["unsafe", "_code"])),
    );
    assert_eq!(
        unsafe_allow::run_with_members(&scratch.0, &[])
            .expect("runs")
            .len(),
        1
    );
}

// ---- build metadata ----

#[test]
fn build_dates_are_utc_calendar_dates() {
    use build_metadata::{build_date, utc_date_from_epoch};
    assert_eq!(utc_date_from_epoch(0), "1970-01-01");
    assert_eq!(utc_date_from_epoch(951_782_400), "2000-02-29");
    assert_eq!(utc_date_from_epoch(1_700_000_000), "2023-11-14");
    assert_eq!(utc_date_from_epoch(1_699_920_000), "2023-11-14");
    assert_eq!(utc_date_from_epoch(1_699_920_000 - 1), "2023-11-13");
    assert_eq!(utc_date_from_epoch(1_699_920_000 + 86_399), "2023-11-14");
    assert_eq!(build_date(Some(" 1700000000\n")), "2023-11-14");
    assert_eq!(build_date(Some("not a number")), "unknown");
    assert_eq!(build_date(None), "unknown");
}

// ---- line numbers ----

#[test]
fn line_counter_counts_incrementally() {
    let text = "a\nb\n\nc";
    let bytes = text.as_bytes();
    let mut lines = xtask::util::LineCounter::default();
    assert_eq!(lines.line_at(bytes, 0), 1);
    assert_eq!(lines.line_at(bytes, 2), 2);
    assert_eq!(lines.line_at(bytes, 4), 3);
    assert_eq!(lines.line_at(bytes, 5), 4);
    assert_eq!(lines.line_at(bytes, 5), 4);
    // Going backwards restarts, and still answers correctly.
    assert_eq!(lines.line_at(bytes, 1), 1);
    assert_eq!(lines.line_at(bytes, 100), 4);
}

#[test]
fn scanners_report_the_right_lines_and_handle_large_files() {
    let planted = join(&["let f = AF_", "INET;"]);
    let mut source = String::new();
    for _ in 0..200_000 {
        source.push_str("let a_perfectly_ordinary_word = 1;\n");
    }
    source.push_str(&planted);
    source.push('\n');
    let hits = net_ban::source_hits(&source);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, 200_001);

    let attr = allow_attr(&join(&["unsafe", "_code"]));
    let mut file = "fn f() {}\n".repeat(100_000);
    file.push_str(&attr);
    let findings = unsafe_allow::check_source("crates/bs-ui/src/lib.rs", &file);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].line, Some(100_001));
}
