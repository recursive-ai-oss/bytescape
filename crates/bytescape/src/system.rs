//! What the window asks of the operating system: deleting, opening, copying
//! and the folder dialog. Everything that can wait on the disk or on another
//! program runs on a worker thread, and failures come back as short messages
//! for the info bar.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};

use bs_platform::delete::{
    Decision, Disposal, EmptyOutcome, Item, OpError, Plan, PlanEntry, ProtectedConfirmation,
    Reason, RefuseReason, RestoreOutcome,
};
use bs_platform::launch;
use bs_ui::action::Action;
use bs_ui::shell::{ActOutcome, Disposer, Token};

/// The platform's delete service, built the first time it is needed (it lists
/// the drives, which takes a moment). Starting it early with
/// [`SystemDisposer::warm`] keeps the first Delete from waiting.
pub struct SystemDisposer {
    inner: OnceLock<Option<Disposal>>,
}

impl SystemDisposer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: OnceLock::new(),
        })
    }

    /// Builds the service on another thread.
    pub fn warm(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("disposal".into())
            .spawn(move || {
                let _ = me.get();
            });
    }

    fn get(&self) -> Option<&Disposal> {
        self.inner.get_or_init(|| Disposal::system().ok()).as_ref()
    }
}

fn unavailable() -> OpError {
    OpError {
        step: "list the drives",
        kind: std::io::ErrorKind::Unsupported,
        code: None,
        removed_bytes: 0,
        removed_entries: 0,
    }
}

impl Disposer for SystemDisposer {
    fn precheck(
        &self,
        items: &[Item],
        permanent: bool,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Plan {
        match self.get() {
            Some(d) => Disposer::precheck(d, items, permanent, confirmed),
            // Without the drive list nothing can be judged, so nothing is done.
            None => Plan {
                entries: items
                    .iter()
                    .map(|_| PlanEntry {
                        level: bs_knowledge::Level::Normal,
                        reason: Reason::None,
                        note: None,
                        decision: Decision::Refused(RefuseReason::CannotResolve),
                    })
                    .collect(),
                strictest: bs_knowledge::Level::Normal,
                drivers: Vec::new(),
                selection_refused: true,
            },
        }
    }

    fn trash(&self, items: &[Item], confirmed: Option<&ProtectedConfirmation>) -> Vec<ActOutcome> {
        match self.get() {
            Some(d) => Disposer::trash(d, items, confirmed),
            None => items
                .iter()
                .map(|_| ActOutcome::Failed(unavailable()))
                .collect(),
        }
    }

    fn delete_permanently(
        &self,
        items: &[Item],
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome> {
        match self.get() {
            Some(d) => Disposer::delete_permanently(d, items, confirmed),
            None => items
                .iter()
                .map(|_| ActOutcome::Failed(unavailable()))
                .collect(),
        }
    }

    fn restore(&self, token: &Token) -> RestoreOutcome {
        match self.get() {
            Some(d) => Disposer::restore(d, token),
            None => RestoreOutcome::Failed {
                error: unavailable(),
            },
        }
    }

    fn empty_bin(&self, mount: &Path) -> EmptyOutcome {
        match self.get() {
            Some(d) => Disposer::empty_bin(d, mount),
            None => EmptyOutcome::Failed {
                error: unavailable(),
            },
        }
    }
}

/// Asks the file manager whether it has a properties window, off the window's
/// thread (the first answer can start the file manager).
pub fn probe_properties(tx: Sender<bool>) {
    let _ = std::thread::Builder::new()
        .name("capabilities".into())
        .spawn(move || {
            let _ = tx.send(launch::properties_supported());
        });
}

/// Does `action` to `path` on a worker thread. A failure becomes a message.
pub fn run_action(action: Action, path: PathBuf, is_folder: bool, notices: Sender<String>) {
    let _ = std::thread::Builder::new()
        .name("action".into())
        .spawn(move || {
            let label = action.label();
            let outcome = match action {
                Action::Open => launch::open(&path),
                Action::ShowInFileManager => launch::reveal(&path),
                Action::Properties => launch::properties(&path),
                Action::OpenTerminal => {
                    let dir = if is_folder {
                        Some(path.as_path())
                    } else {
                        path.parent()
                    };
                    match dir {
                        Some(dir) => launch::open_terminal(dir),
                        None => Err(launch::LaunchError::BadInput("no folder")),
                    }
                }
                Action::Handoff(bs_knowledge::Handoff::DiskCleanup) => {
                    let letter = path
                        .to_string_lossy()
                        .chars()
                        .next()
                        .filter(|c| c.is_ascii_alphabetic() && cfg!(windows));
                    launch::open_disk_cleanup(letter)
                }
                _ => Err(launch::LaunchError::Unsupported),
            };
            if let Err(e) = outcome {
                let _ = notices.send(format!("{label}: {e}"));
            }
        });
}

/// Puts `text` on the clipboard on a worker thread.
pub fn copy_text(text: String, notices: Sender<String>) {
    let _ = std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            if let Err(e) = bs_platform::clipboard::set_text(&text) {
                let _ = notices.send(format!("Couldn't copy: {e}"));
            }
        });
}

/// Opens a web address in the browser on a worker thread.
pub fn open_url(url: &'static str, notices: Sender<String>) {
    let _ = std::thread::Builder::new()
        .name("browser".into())
        .spawn(move || {
            if let Err(e) = launch::open_url(url) {
                let _ = notices.send(format!("Couldn't open the browser: {e}"));
            }
        });
}

/// What the folder dialog answered.
#[derive(Debug)]
pub enum Choice {
    Folder(PathBuf),
    Cancelled,
    Unavailable(String),
}

/// Shows the system's folder dialog; call it on a worker thread.
pub fn choose_folder(start: &Path) -> Choice {
    match bs_platform::dialog::pick_folder(start) {
        Ok(Some(path)) => Choice::Folder(path),
        Ok(None) => Choice::Cancelled,
        Err(e) => Choice::Unavailable(format!("Couldn't show the folder dialog: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_platform_judges_nothing_and_refuses() {
        let d = SystemDisposer {
            inner: OnceLock::from(None),
        };
        let items = [Item::new("/a"), Item::new("/b")];
        let plan = Disposer::precheck(&d, &items, false, None);
        assert!(plan.selection_refused);
        assert_eq!(plan.entries.len(), 2);
        assert!(matches!(
            plan.entries[0].decision,
            Decision::Refused(RefuseReason::CannotResolve)
        ));
        assert!(
            Disposer::trash(&d, &items, None)
                .iter()
                .all(|o| matches!(o, ActOutcome::Failed(_)))
        );
    }

    #[test]
    fn other_actions_say_so_instead_of_pretending() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_action(Action::CopyPath, PathBuf::from("/a"), false, tx);
        let msg = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(msg.starts_with("Copy path:"), "{msg}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_real_service_never_offers_the_root_of_the_system() {
        let d = SystemDisposer::new();
        let plan = Disposer::precheck(&*d, &[Item::new("/")], false, None);
        assert!(plan.selection_refused);
        // Blocked when the platform could judge it; refused all the same when
        // it could not list the drives.
        assert!(plan.strictest == bs_knowledge::Level::Blocked || d.get().is_none());
        let outcomes = Disposer::trash(&*d, &[Item::new("/")], None);
        assert!(matches!(
            outcomes[0],
            ActOutcome::Refused(_) | ActOutcome::Failed(_)
        ));
    }
}
