//! `cargo xtask net-ban`: Bytescape never connects to the internet. This
//! check rejects the ways code usually does that, in two parts:
//!
//! 1. the resolved dependency graph of every release target must contain no
//!    known networking crate and no known networking crate feature (including
//!    the Windows networking API families), and
//! 2. no Rust source file may use the standard library's network module or a
//!    known networking system call.
//!
//! It is a backstop for review, not a proof: a networking library it does not
//! know about is not caught. `deny.toml` repeats the crate and feature lists
//! as a second, independent layer.
//!
//! Exceptions live in `xtask/net-ban.allow`. A crate or feature exception
//! must name the crate or crates that pull it in (`via`): it is honoured only
//! while every path to it in the graph goes through one of those crates, and
//! never when it is in the elevated helper's dependency closure.

use crate::util::{
    Finding, TARGETS, cargo_metadata, cargo_tree_features, normalize_crate_name, package_names,
    parse_allow_lines, read_text, relative, walk_files,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Crates that can open network connections, serve them, resolve names, or
/// speak a network protocol, plus a few the design rules out for other
/// reasons (a D-Bus crate brings sockets; the native-menu crate needs GTK).
pub const BANNED_CRATES: [&str; 40] = [
    "reqwest",
    "hyper",
    "hyper-util",
    "hyper-tls",
    "hyper-rustls",
    "ureq",
    "curl",
    "curl-sys",
    "isahc",
    "surf",
    "native-tls",
    "rustls",
    "tokio-rustls",
    "tokio-native-tls",
    "openssl",
    "openssl-sys",
    "socket2",
    "async-std",
    "async-net",
    "async-io",
    "smol",
    "attohttpc",
    "minreq",
    "h2",
    "quinn",
    "tungstenite",
    "tokio-tungstenite",
    "async-tungstenite",
    "axum",
    "warp",
    "actix-web",
    "tiny-http",
    "zbus",
    "dbus",
    "muda",
    "hickory-resolver",
    "hickory-proto",
    "trust-dns-resolver",
    "trust-dns-proto",
    "trust-dns-client",
];

/// Whole families banned by name prefix (DNS stacks under both of their
/// names). `deny.toml` lists the known members one by one.
pub const BANNED_CRATE_PREFIXES: [&str; 2] = ["hickory-", "trust-dns-"];

/// Crates that no allowlist entry can admit.
pub const NEVER_ALLOWED: [&str; 1] = ["async-io"];

/// Crate features that open sockets: `(crate, feature)`.
pub const BANNED_FEATURES: [(&str, &str); 13] = [
    ("tokio", "net"),
    ("mio", "net"),
    ("rustix", "net"),
    ("nix", "net"),
    ("nix", "socket"),
    ("winapi", "winsock2"),
    ("winapi", "ws2def"),
    ("winapi", "ws2ipdef"),
    ("winapi", "ws2tcpip"),
    ("winapi", "winhttp"),
    ("winapi", "wininet"),
    ("winapi", "urlmon"),
    ("winapi", "mswsock"),
];

/// Feature families banned by name prefix: `(crate, feature prefix)`. The
/// Windows API crates gate the networking APIs behind these features.
pub const BANNED_FEATURE_PREFIXES: [(&str, &str); 4] = [
    ("windows", "Win32_Networking"),
    ("windows-sys", "Win32_Networking"),
    ("windows", "Win32_System_Com_Urlmon"),
    ("windows-sys", "Win32_System_Com_Urlmon"),
];

/// The package whose closure may never contain an excepted item.
const HELPER: &str = "bytescape-helper";

/// What an allowlist entry admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Crate,
    Feature,
}

/// One reviewed exception for a crate or a crate feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// `name` for a crate, `name/feature` for a feature.
    pub key: String,
    /// The crates every path to the item must go through.
    pub via: Vec<String>,
    pub line: usize,
}

/// Reviewed exceptions, read from `xtask/net-ban.allow`.
#[derive(Debug, Default, Clone)]
pub struct Allow {
    pub entries: Vec<Entry>,
    pub sources: BTreeSet<String>,
    /// Lines that could not be used, as `(line, message)`.
    pub errors: Vec<(usize, String)>,
}

impl Allow {
    /// Parses the allow file. Formats (text after `#` is a comment):
    ///
    /// ```text
    /// crate NAME via CRATE[,CRATE...]
    /// feature CRATE/FEATURE via CRATE[,CRATE...]
    /// source PATH
    /// ```
    pub fn parse(text: &str) -> Self {
        let mut allow = Self::default();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let entry = parse_allow_lines(raw);
            let Some(entry) = entry.first() else {
                continue;
            };
            let parts: Vec<&str> = entry.split_whitespace().collect();
            match parts.as_slice() {
                ["source", path] => {
                    allow.sources.insert((*path).to_string());
                }
                [kind @ ("crate" | "feature"), key, "via", via] => {
                    let kind = if *kind == "crate" {
                        Kind::Crate
                    } else {
                        Kind::Feature
                    };
                    let key = normalize_key(key);
                    let name = key.split('/').next().unwrap_or("").to_string();
                    let via: Vec<String> = via
                        .split(',')
                        .filter(|name| !name.is_empty())
                        .map(normalize_crate_name)
                        .collect();
                    if kind == Kind::Feature && !key.contains('/') {
                        allow
                            .errors
                            .push((line, String::from("a feature entry reads `feature CRATE/FEATURE via ...`")));
                    } else if via.is_empty() {
                        allow
                            .errors
                            .push((line, String::from("`via` needs at least one crate")));
                    } else if NEVER_ALLOWED.contains(&name.as_str()) {
                        allow.errors.push((
                            line,
                            format!("`{name}` is banned for the application and cannot be allowed"),
                        ));
                    } else {
                        allow.entries.push(Entry {
                            kind,
                            key,
                            via,
                            line,
                        });
                    }
                }
                _ => allow.errors.push((
                    line,
                    String::from(
                        "expected `crate NAME via CRATE`, `feature CRATE/FEATURE via CRATE` or `source PATH`; \
                         crate and feature exceptions must name the crate that pulls them in",
                    ),
                )),
            }
        }
        allow
    }
}

fn normalize_key(key: &str) -> String {
    match key.split_once('/') {
        Some((name, feature)) => format!("{}/{feature}", normalize_crate_name(name)),
        None => normalize_crate_name(key),
    }
}

/// The resolved graph of one `cargo metadata` document.
struct Graph {
    names: BTreeMap<String, String>,
    /// For each package id: its dependencies as `(id, only a dev dependency)`.
    deps: BTreeMap<String, Vec<(String, bool)>>,
    features: BTreeMap<String, Vec<String>>,
    roots: Vec<String>,
}

impl Graph {
    fn parse(metadata: &Value) -> Option<Self> {
        let names = package_names(metadata);
        let mut deps = BTreeMap::new();
        let mut features = BTreeMap::new();
        let mut has_incoming = BTreeSet::new();
        for node in metadata["resolve"]["nodes"].as_array()? {
            let id = node["id"].as_str()?.to_string();
            let mut list = Vec::new();
            for dep in node["deps"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let Some(pkg) = dep["pkg"].as_str() else {
                    continue;
                };
                let kinds: Vec<&Value> = dep["dep_kinds"]
                    .as_array()
                    .map(|kinds| kinds.iter().collect())
                    .unwrap_or_default();
                let dev_only = !kinds.is_empty() && kinds.iter().all(|k| k["kind"] == "dev");
                has_incoming.insert(pkg.to_string());
                list.push((pkg.to_string(), dev_only));
            }
            let enabled = node["features"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            features.insert(id.clone(), enabled);
            deps.insert(id, list);
        }
        let roots = match metadata["workspace_members"].as_array() {
            Some(members) => members
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            None => deps
                .keys()
                .filter(|id| !has_incoming.contains(*id))
                .cloned()
                .collect(),
        };
        Some(Self {
            names,
            deps,
            features,
            roots,
        })
    }

    fn name(&self, id: &str) -> String {
        self.names
            .get(id)
            .map(|name| normalize_crate_name(name))
            .unwrap_or_default()
    }

    /// Ids reachable from `starts`, never entering a package whose name is in
    /// `skip`. With `skip_dev`, development-only edges are not followed.
    fn reachable(&self, starts: &[String], skip: &[String], skip_dev: bool) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<String> = starts
            .iter()
            .filter(|id| !skip.contains(&self.name(id)))
            .cloned()
            .collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            for (dep, dev_only) in self.deps.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
                if (skip_dev && *dev_only) || skip.contains(&self.name(dep)) {
                    continue;
                }
                stack.push(dep.clone());
            }
        }
        seen
    }
}

/// The banned items found on one package: `(kind, key)`.
fn banned_items(name: &str, features: &[String]) -> Vec<(Kind, String)> {
    let mut items = Vec::new();
    let banned_crate = BANNED_CRATES.contains(&name)
        || BANNED_CRATE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix));
    if banned_crate {
        items.push((Kind::Crate, name.to_string()));
    }
    for feature in features {
        let exact = BANNED_FEATURES
            .iter()
            .any(|(crate_name, banned)| *crate_name == name && banned == feature);
        let family = BANNED_FEATURE_PREFIXES
            .iter()
            .any(|(crate_name, prefix)| *crate_name == name && feature.starts_with(prefix));
        if exact || family {
            items.push((Kind::Feature, format!("{name}/{feature}")));
        }
    }
    items
}

/// Replaces the features in a `cargo metadata` document with the exact ones
/// for the target, taken from `cargo tree` output (see
/// [`cargo_tree_features`]). The metadata lists features that another
/// platform's dependencies turn on, which would flag the Windows API crates on
/// Windows because of a Linux-only dependency. A package the tree does not
/// list is not built for the target and gets no features.
pub fn refine_features(metadata: &mut Value, tree: &str) {
    let mut exact: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for line in tree.lines() {
        let Some((package, features)) = line.rsplit_once('|') else {
            continue;
        };
        let mut words = package.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else {
            continue;
        };
        let entry = exact
            .entry((
                normalize_crate_name(name),
                version.trim_start_matches('v').to_string(),
            ))
            .or_default();
        entry.extend(
            features
                .split(',')
                .filter(|f| !f.is_empty())
                .map(String::from),
        );
    }
    let identity: BTreeMap<String, (String, String)> = metadata["packages"]
        .as_array()
        .map(|packages| {
            packages
                .iter()
                .filter_map(|package| {
                    Some((
                        package["id"].as_str()?.to_string(),
                        (
                            normalize_crate_name(package["name"].as_str()?),
                            package["version"].as_str()?.to_string(),
                        ),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(nodes) = metadata["resolve"]["nodes"].as_array_mut() {
        for node in nodes {
            let features: Vec<String> = node["id"]
                .as_str()
                .and_then(|id| identity.get(id))
                .and_then(|key| exact.get(key))
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();
            node["features"] = serde_json::json!(features);
        }
    }
}

/// Checks one `cargo metadata` document.
pub fn check_metadata(metadata: &Value, target: &str, allow: &Allow) -> Vec<Finding> {
    let Some(graph) = Graph::parse(metadata) else {
        return vec![Finding::new(
            target,
            None,
            "cargo metadata has no resolved dependency graph",
        )];
    };
    let helper_closure = graph
        .deps
        .keys()
        .find(|id| graph.name(id) == HELPER)
        .map(|id| graph.reachable(std::slice::from_ref(id), &[], true))
        .unwrap_or_default();
    let mut findings = Vec::new();
    for id in graph.deps.keys() {
        let name = graph.name(id);
        let enabled = graph.features.get(id).map(Vec::as_slice).unwrap_or(&[]);
        for (kind, key) in banned_items(&name, enabled) {
            let entry = allow
                .entries
                .iter()
                .find(|entry| entry.kind == kind && entry.key == key);
            let what = match kind {
                Kind::Crate => "socket-capable or ruled-out crate in the dependency graph",
                Kind::Feature => "socket-capable feature enabled",
            };
            let problem = match entry {
                None => Some(format!("{what} ({target})")),
                Some(_) if NEVER_ALLOWED.contains(&name.as_str()) => Some(format!(
                    "banned for the application and cannot be allowed ({target})"
                )),
                Some(_) if helper_closure.contains(id) => Some(format!(
                    "is in the dependency closure of {HELPER}, where no exception applies ({target})"
                )),
                Some(entry) => {
                    let outside = graph.reachable(&graph.roots, &entry.via, false);
                    outside.contains(id).then(|| {
                        format!(
                            "is allowed only through {}, but is also reachable another way ({target})",
                            entry.via.join(", ")
                        )
                    })
                }
            };
            if let Some(message) = problem {
                findings.push(Finding::new(key, None, message));
            }
        }
    }
    findings
}

/// Descriptions of what a source pattern means, by pattern.
const SOURCE_MESSAGE_STD: &str = "uses the standard library's network module";
const SOURCE_MESSAGE_LIBC: &str = "calls the C socket function";
const SOURCE_MESSAGE_WINDOWS: &str = "uses a Windows networking API";
const SOURCE_MESSAGE_NAMES: &str = "uses a name-resolution or socket API";

/// Identifiers that are networking APIs by themselves. They are written in
/// pieces so this file does not trip its own scan.
const EXACT_IDENTIFIERS: [&str; 7] = [
    concat!("AF_", "INET"),
    concat!("AF_", "INET6"),
    concat!("get", "addrinfo"),
    concat!("gethost", "byname"),
    concat!("WSA", "Startup"),
    concat!("Url", "mon"),
    concat!("Get", "AddrInfoW"),
];

/// Identifier prefixes of networking API families.
const IDENTIFIER_PREFIXES: [&str; 7] = [
    concat!("Win", "Http"),
    concat!("Internet", "Open"),
    concat!("Internet", "Connect"),
    concat!("URLDownload", "ToFile"),
    concat!("URL", "OpenStream"),
    concat!("WSA", "Socket"),
    concat!("Get", "AddrInfoEx"),
];

/// Source lines (1-based) that use a networking API, with a description.
pub fn source_hits(text: &str) -> Vec<(usize, &'static str)> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !is_word_start(bytes, index) {
            index += 1;
            continue;
        }
        let end = word_end(bytes, index);
        let word = &text[index..end];
        let line = line_of(bytes, index);
        if (word == "std" || word == "core") && names_segment_after(bytes, end, "net") {
            hits.push((line, SOURCE_MESSAGE_STD));
        } else if word == "libc" && names_segment_after(bytes, end, "socket") {
            hits.push((line, SOURCE_MESSAGE_LIBC));
        } else if word == "Win32" && names_segment_after(bytes, end, "Networking") {
            hits.push((line, SOURCE_MESSAGE_WINDOWS));
        } else if EXACT_IDENTIFIERS.contains(&word)
            || IDENTIFIER_PREFIXES
                .iter()
                .any(|prefix| word.starts_with(prefix))
        {
            hits.push((line, SOURCE_MESSAGE_NAMES));
        }
        index = end;
    }
    hits
}

/// Line numbers (1-based) where the text names the standard library's
/// network module, written as a path or inside a `use` group.
pub fn std_net_lines(text: &str) -> Vec<usize> {
    source_hits(text)
        .into_iter()
        .filter(|(_, message)| *message == SOURCE_MESSAGE_STD)
        .map(|(line, _)| line)
        .collect()
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_word_start(bytes: &[u8], index: usize) -> bool {
    is_ident_byte(bytes[index]) && (index == 0 || !is_ident_byte(bytes[index - 1]))
}

fn word_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && is_ident_byte(bytes[end]) {
        end += 1;
    }
    end
}

fn skip_space(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn line_of(bytes: &[u8], index: usize) -> usize {
    1 + bytes[..index].iter().filter(|&&byte| byte == b'\n').count()
}

fn is_segment(bytes: &[u8], index: usize, segment: &str) -> bool {
    let end = index + segment.len();
    bytes[index..].starts_with(segment.as_bytes())
        && (end >= bytes.len() || !is_ident_byte(bytes[end]))
}

/// After a path segment: is the next segment `segment`, directly or as an
/// item of a `{ ... }` group?
fn names_segment_after(bytes: &[u8], after_word: usize, segment: &str) -> bool {
    let mut index = skip_space(bytes, after_word);
    if !bytes[index.min(bytes.len())..].starts_with(b"::") {
        return false;
    }
    index = skip_space(bytes, index + 2);
    if index >= bytes.len() {
        return false;
    }
    if bytes[index] != b'{' {
        return is_segment(bytes, index, segment);
    }
    // A group: look for the segment as a top-level item.
    let mut depth = 0usize;
    let mut expect_item = false;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => {
                depth += 1;
                expect_item = true;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return false;
                }
                expect_item = false;
            }
            b',' => expect_item = depth == 1,
            byte if byte.is_ascii_whitespace() => {}
            _ => {
                if expect_item && depth == 1 && is_segment(bytes, index, segment) {
                    return true;
                }
                expect_item = false;
            }
        }
        index += 1;
    }
    false
}

/// Scans the Rust sources below `root`.
pub fn scan_sources(root: &Path, allow: &Allow) -> Vec<Finding> {
    let mut findings = Vec::new();
    for path in walk_files(root) {
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let rel = relative(root, &path);
        if allow.sources.contains(&rel) {
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let mut seen = BTreeSet::new();
        for (line, message) in source_hits(&text) {
            if seen.insert((line, message)) {
                findings.push(Finding::new(rel.clone(), Some(line), message));
            }
        }
    }
    findings
}

/// Runs the whole check against the repository at `root`.
pub fn run(root: &Path) -> Result<Vec<Finding>, String> {
    let allow_text = read_text(&root.join("xtask/net-ban.allow")).unwrap_or_default();
    let allow = Allow::parse(&allow_text);
    let mut findings: Vec<Finding> = allow
        .errors
        .iter()
        .map(|(line, message)| Finding::new("xtask/net-ban.allow", Some(*line), message.clone()))
        .collect();
    for target in TARGETS {
        let mut metadata = cargo_metadata(root, target)?;
        refine_features(&mut metadata, &cargo_tree_features(root, target)?);
        findings.extend(check_metadata(&metadata, target, &allow));
    }
    findings.extend(scan_sources(root, &allow));
    findings
        .sort_by(|a, b| (&a.subject, a.line, &a.message).cmp(&(&b.subject, b.line, &b.message)));
    findings.dedup();
    Ok(findings)
}
