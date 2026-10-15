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
    /// Show the package that installed the folder in the software center.
    OpenInSoftware,
    /// Copy the command that removes the package that installed the folder.
    CopyRemovalCommand,
    /// Move to the Recycle Bin or Trash.
    Delete,
    /// Delete for good, after a confirmation.
    DeletePermanently,
    /// Put the last deleted item back.
    Undo,
    /// Open the context menu on the selection.
    ContextMenu,
    /// The About box. Not about any box of the map.
    About,
    /// Open the releases page in the browser. Not about any box of the map.
    CheckForUpdates,
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
            Action::Handoff(_) => "Open the tool that owns this",
            Action::OpenInSoftware => "Open in Software",
            Action::CopyRemovalCommand => "Copy removal command",
            Action::Delete => "Delete",
            Action::DeletePermanently => "Delete permanently",
            Action::Undo => "Undo",
            Action::ContextMenu => "Menu",
            Action::About => "About Bytescape",
            Action::CheckForUpdates => "Check for updates",
        }
    }

    /// What the action does, as a short sentence for a screen reader. A
    /// disabled action says why it can't be done instead (see [`reason`]).
    pub fn describe(self) -> &'static str {
        match self {
            Action::Open => "Opens it with the program the system chooses",
            Action::ShowInFileManager => {
                if cfg!(windows) {
                    "Shows it in Explorer"
                } else {
                    "Shows it in the file manager"
                }
            }
            Action::OpenTerminal => "Opens a terminal in this folder",
            Action::CopyPath => "Copies the full path",
            Action::Properties => "Shows the system's properties window",
            Action::ZoomInto => "Goes into this folder",
            Action::Rescan => "Reads this folder from the disk again",
            Action::Handoff(Handoff::Uninstall) => "Starts the program's own uninstaller",
            Action::Handoff(Handoff::DiskCleanup) => {
                if cfg!(windows) {
                    "Opens Disk Cleanup"
                } else {
                    "Opens the system's storage settings"
                }
            }
            Action::Handoff(_) => "Opens the tool that manages it",
            Action::OpenInSoftware => "Shows the package in the software center",
            Action::CopyRemovalCommand => "Copies the command that removes the package",
            Action::Delete => {
                if cfg!(windows) {
                    "Moves it to the Recycle Bin, with an Undo"
                } else {
                    "Moves it to the Trash, with an Undo"
                }
            }
            Action::DeletePermanently => "Deletes it for good, after a confirmation",
            Action::Undo => "Puts the last deleted item back",
            Action::ContextMenu => "Opens the menu of actions",
            Action::About => "Shows the version and build date",
            Action::CheckForUpdates => "Opens the releases page in your browser",
        }
    }

    /// Whether the interface carries this action out itself. The rest are
    /// handled by the layers that own them (files, the system, deletion) and
    /// are only announced here.
    pub fn is_handled_here(self) -> bool {
        matches!(
            self,
            Action::CopyPath
                | Action::ZoomInto
                | Action::CopyRemovalCommand
                | Action::About
                | Action::CheckForUpdates
        )
    }
}

/// Why an action is not available right now, in a few words.
pub mod reason {
    /// The box is still being counted.
    pub const SCANNING: &str = "Still scanning";
    /// A full scan with administrator rights is running, and what is on disk
    /// must not change until its result is in.
    pub const FULL_SCAN_RUNNING: &str = "A full scan is running";
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
    /// Under a folder the scan made up to hold files no folder lists.
    pub const NOT_A_REAL_FOLDER: &str = "Not a real folder (files no folder lists)";
    /// The program is installed but registered no way to remove it.
    pub const NO_UNINSTALLER: &str = "This program registered no uninstaller";
    /// Which program owns the folder is still being worked out.
    pub const LOOKING_UP_PROGRAM: &str = "Still looking up the program";
    /// No installed program lists the folder.
    pub const NO_PROGRAM: &str = "No installed program lists this folder";
    /// Programs and packages are matched only for big folders, after a scan.
    pub const NOT_MATCHED: &str = "Programs are matched for folders of 100 MB or more";
    /// The package has no entry in a software center.
    pub const NO_SOFTWARE_ENTRY: &str = "The package has no entry in Software";
    /// The desktop has no program that opens such entries.
    pub const NO_SOFTWARE_CENTER: &str = "No software center found";
    /// The package gave no command plain enough to paste into a terminal.
    pub const NO_REMOVAL_COMMAND: &str = "No removal command for this package";
}
