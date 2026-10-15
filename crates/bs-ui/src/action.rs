//! What the user can ask for on a box. The keyboard, the hover card and the
//! context menu all produce these, so a new way to ask never needs a new code
//! path behind it.

use bs_knowledge::Handoff;

/// Something to do with the selected boxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open with the default program.
    Open,
    /// Show in the file manager.
    ShowInFileManager,
    /// A terminal in the folder.
    OpenTerminal,
    /// Copy the path to the clipboard.
    CopyPath,
    /// The system's properties dialog.
    Properties,
    /// Go into the folder.
    ZoomInto,
    /// Scan the folder again.
    Rescan,
    /// The tool that owns a recognized folder.
    Handoff(Handoff),
    /// Move to the Recycle Bin or Trash.
    Delete,
    /// Delete for good, after a confirmation.
    DeletePermanently,
    /// Put the last deleted item back.
    Undo,
    /// Open the context menu on the selection.
    ContextMenu,
}

impl Action {
    /// The words on a button or menu entry.
    pub fn label(self) -> &'static str {
        match self {
            Action::Open => "Open",
            Action::ShowInFileManager => {
                if cfg!(windows) {
                    "Show in Explorer"
                } else {
                    "Show in file manager"
                }
            }
            Action::OpenTerminal => "Open terminal here",
            Action::CopyPath => "Copy path",
            Action::Properties => "Properties",
            Action::ZoomInto => "Zoom into",
            Action::Rescan => "Rescan this folder",
            Action::Handoff(Handoff::Uninstall) => "Uninstall…",
            Action::Handoff(Handoff::DiskCleanup) => {
                if cfg!(windows) {
                    "Open Disk Cleanup"
                } else {
                    "Open storage settings"
                }
            }
            Action::Handoff(_) => "Installed by a package",
            Action::Delete => "Delete",
            Action::DeletePermanently => "Delete permanently",
            Action::Undo => "Undo",
            Action::ContextMenu => "Menu",
        }
    }

    /// Whether the interface carries this action out itself. The rest are
    /// handled by the layers that own them (files, the system, deletion) and
    /// are only announced here.
    pub fn is_handled_here(self) -> bool {
        matches!(self, Action::CopyPath | Action::ZoomInto)
    }
}

/// Why an action is not available right now, in a few words.
pub mod reason {
    /// The box is still being counted.
    pub const SCANNING: &str = "Still scanning";
    /// The scan was stopped before the box was counted.
    pub const SCAN_STOPPED: &str = "Scan stopped";
    /// The box is part of the operating system.
    pub const OPERATING_SYSTEM: &str = "Part of the operating system";
    /// Nothing is selected.
    pub const NOTHING_SELECTED: &str = "Nothing selected";
    /// The box already went to the bin.
    pub const MOVED: &str = "Already moved";
    /// The box was already deleted.
    pub const DELETED: &str = "Already deleted";
    /// Not a folder.
    pub const NOT_A_FOLDER: &str = "Not a folder";
    /// A block that stands for free space or the like, not for a file or
    /// folder.
    pub const NOT_ON_DISK: &str = "Not a file or folder";
    /// A handoff that is not wired up in this version.
    pub const NOT_YET: &str = "Not available in this version";
}
