//! Which installed program or package a big folder belongs to, as the hover
//! card and the menus need to say it.
//!
//! The platform's matcher does the looking, in the background and only for
//! folders of 100 MB or more, after a scan has finished. This module keeps the
//! interface's side of that: a small trait the card is built from (so tests
//! can stand in for the system), the plain description of an answer, and the
//! choice of which folders of a finished scan are worth asking about.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bs_platform::software::{
    Candidate, Lookup, MIN_MATCH_BYTES, Matcher, Owner, PackageManager, Relation,
};
use bs_tree::{NodeKind, Snapshot};

/// Folders asked about at most, from one scan.
const MOST_CANDIDATES: usize = 3_000;
/// How far below the scanned folder candidates are looked for. Programs live
/// near the top of a drive; a deeper search would only find their parts.
const MOST_DEPTH: u32 = 6;

/// An installed Windows program that owns a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramInfo {
    pub name: String,
    pub publisher: Option<String>,
    /// The folder is the program's whole install location. When false it is a
    /// part of it, and uninstalling removes more than this folder.
    pub whole_folder: bool,
    /// The program registered a way to remove it.
    pub can_uninstall: bool,
}

/// An installed Linux package that owns a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    /// The package tool, as a person would say it.
    pub manager: &'static str,
    /// Other packages that also put files in the folder.
    pub other_packages: u32,
    /// A command to copy that removes the package; never run by Bytescape.
    pub removal_command: Option<String>,
    /// The package has an entry the software center can open.
    pub software_entry: bool,
}

/// What is known about the owner of a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// Not asked: too small, the scan is not finished, or no lookup here.
    Unknown,
    /// Being worked out in the background.
    Checking,
    /// Asked, and nothing owns the folder (or several packages share it).
    Nobody,
    Program(ProgramInfo),
    Package(PackageInfo),
}

/// Where answers come from. The real one is the platform's [`Matcher`].
pub trait Owners: Send + Sync {
    /// Starts matching these folders (path and size on disk) in the
    /// background and returns at once.
    fn request(&self, folders: &[(PathBuf, u64)]);
    /// What is known about a folder now.
    fn lookup(&self, path: &Path) -> Ownership;
    /// Whether nothing is being matched.
    fn is_idle(&self) -> bool;
}

/// Text from a registry or package database, made fit to draw: control
/// characters become spaces.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

impl Owners for Matcher {
    fn request(&self, folders: &[(PathBuf, u64)]) {
        let candidates: Vec<Candidate> = folders
            .iter()
            .map(|(path, bytes)| Candidate {
                path: path.clone(),
                bytes: *bytes,
            })
            .collect();
        Matcher::request(self, &candidates);
    }

    fn lookup(&self, path: &Path) -> Ownership {
        match Matcher::lookup(self, path) {
            Lookup::NotAsked => Ownership::Unknown,
            Lookup::Pending => Ownership::Checking,
            Lookup::NoOwner => Ownership::Nobody,
            Lookup::Found(Owner::Program(p)) => Ownership::Program(ProgramInfo {
                name: plain(&p.name),
                publisher: p.publisher.as_deref().map(plain),
                whole_folder: p.relation == Relation::Exact,
                can_uninstall: p.uninstall.is_some(),
            }),
            Lookup::Found(Owner::Package(p)) => Ownership::Package(PackageInfo {
                name: plain(&p.name),
                manager: match p.manager {
                    PackageManager::Dpkg => "dpkg",
                    PackageManager::Rpm => "rpm",
                    _ => "the package manager",
                },
                other_packages: p.other_packages,
                removal_command: p.removal_command.clone(),
                software_entry: p.software_center_url.is_some(),
            }),
            _ => Ownership::Nobody,
        }
    }

    fn is_idle(&self) -> bool {
        Matcher::is_idle(self)
    }
}

/// The folders of a finished scan worth asking the matcher about: those of
/// at least [`MIN_MATCH_BYTES`] near the top of the tree.
pub fn candidates(snapshot: &Snapshot) -> Vec<(PathBuf, u64)> {
    let mut found: Vec<(PathBuf, u64)> = Vec::new();
    let Some(root) = snapshot.root() else {
        return found;
    };
    let separator = if cfg!(windows) { b'\\' } else { b'/' };
    let mut level = vec![root.id()];
    let mut depth = 0;
    while !level.is_empty() && depth <= MOST_DEPTH && found.len() < MOST_CANDIDATES {
        let mut next = Vec::new();
        for id in level {
            let Some(node) = snapshot.node(id) else {
                continue;
            };
            if node.kind() != NodeKind::Dir
                || node.allocated() < MIN_MATCH_BYTES
                || node.is_under_synthetic()
            {
                continue;
            }
            if let Some(raw) = snapshot.path(id, separator)
                && let Some(path) = crate::tree::path_from_bytes(raw)
            {
                found.push((path, node.allocated()));
            }
            next.extend(node.children().map(|c| c.id()));
            if found.len() >= MOST_CANDIDATES {
                break;
            }
        }
        level = next;
        depth += 1;
    }
    found
}

/// Asks the matcher about the big folders of a finished scan, all on a worker
/// thread: neither walking the tree for candidates nor the matching can delay
/// the window. The receiver gets a message once the matcher has been handed
/// the folders (from then on [`Owners::is_idle`] tells when it is done), or is
/// disconnected if there was nothing to hand over.
pub fn request_in_background(
    owners: Arc<dyn Owners>,
    snapshot: Arc<Snapshot>,
) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("owner-candidates".into())
        .spawn(move || {
            let folders = candidates(&snapshot);
            if !folders.is_empty() {
                owners.request(&folders);
                let _ = tx.send(());
            }
        });
    rx
}
