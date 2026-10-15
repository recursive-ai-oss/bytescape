//! Program lookup on Windows: the registry's Uninstall keys.

use super::programs::ProgramIndex;
use super::{Backend, Owner, registry};
use bs_knowledge::{Environment, Knowledge};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct System {
    /// Read the first time a folder is asked about, then kept: the list does
    /// not change while the program is looking at a scan.
    index: OnceLock<ProgramIndex>,
}

impl System {
    pub fn new() -> Self {
        Self {
            index: OnceLock::new(),
        }
    }

    fn index(&self) -> &ProgramIndex {
        self.index.get_or_init(|| {
            let environment = Environment::from_lookup(|name| std::env::var_os(name));
            ProgramIndex::new(registry::read_all(), &Knowledge::new(&environment))
        })
    }
}

impl Backend for System {
    fn find(&self, folders: &[PathBuf], cancel: &AtomicBool) -> Vec<Option<Owner>> {
        let index = self.index();
        folders
            .iter()
            .map(|folder| {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                folder
                    .to_str()
                    .and_then(|text| index.find(text))
                    .map(Owner::Program)
            })
            .collect()
    }
}
