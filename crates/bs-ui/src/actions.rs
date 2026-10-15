//! Acting on boxes: deleting (to the bin or for good), Undo, emptying the
//! bin, the context menu, the confirmation dialogs and the toast.
//!
//! What may be deleted, and how firmly, is decided by the platform's
//! [`Disposer`]; this module only asks it and says what it answered. The work
//! that touches the disk runs as a [`Job`] on a thread the host provides, and
//! its result comes back through [`Shell::job_done`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bs_knowledge::Level;
use bs_platform::delete::{
    ByteBasis, Decision, Disposal, EmptyOutcome, Identity, Item as DItem, OpError, Outcome,
    PermanentReason, Plan, ProtectedConfirmation, RefuseReason, RestoreOutcome, RestoreToken,
};
use bs_render::{Card, CardRole, Popup, Rect};
use bs_tree::{DeleteState, NodeId};
use std::any::Any;

use super::{Shell, ShellEffect};
use crate::action::{Action, reason};
use crate::card::{self, Metrics};
use crate::elevation::{self, ElevatedEnd, ElevatedItem, Offer};
use crate::notice::{CrashNotice, RELEASES_URL};
use crate::popup::{self, Button, DialogSpec, DialogState, MenuRow, ToastSpec};
use crate::{Direction, InputEvent, Key, Target, ViewId, ViewSource};

/// How long a toast stays.
pub const TOAST_FOR: Duration = Duration::from_secs(8);
/// How long a toast takes to slide in or out.
pub const TOAST_MOTION: Duration = Duration::from_millis(150);
/// How long a protected delete must be held.
pub const HOLD_FOR: Duration = Duration::from_millis(1500);
/// At most this many selected boxes are looked up and remembered.
const MAX_SEEN: usize = 256;
/// Names listed in a dialog before "and N more".
const LISTED_NAMES: usize = 4;
/// Notes (what a place is and who manages it) listed before "and N more".
const LISTED_NOTES: usize = 2;

/// The reason an action needs exactly one box.
pub const SELECT_ONE: &str = "Select one item";
/// What a delete row of the menu says until the platform has answered.
pub const CHECKING: &str = "Checking…";

// ---- the platform service ----------------------------------------------

/// What is needed to put a deleted item back. Opaque: only the [`Disposer`]
/// that made it can use it.
#[derive(Clone)]
pub struct Token(Arc<dyn Any + Send + Sync>);

impl Token {
    pub fn new<T: Any + Send + Sync>(inner: T) -> Self {
        Token(Arc::new(inner))
    }

    pub fn get<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token")
    }
}

/// What happened to one item. The platform's answer, with the restore token
/// made opaque so the interface can be tested without a real bin.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ActOutcome {
    MovedToBin {
        token: Token,
        bytes: u64,
        basis: ByteBasis,
        shared_files: u64,
    },
    Deleted {
        bytes: u64,
        shared_files: u64,
    },
    WouldBePermanent(PermanentReason),
    Refused(RefuseReason),
    Failed(OpError),
}

impl From<Outcome> for ActOutcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::MovedToBin {
                token,
                bytes,
                basis,
                shared_files,
            } => ActOutcome::MovedToBin {
                token: Token::new(token),
                bytes,
                basis,
                shared_files,
            },
            Outcome::Deleted {
                bytes,
                shared_files,
            } => ActOutcome::Deleted {
                bytes,
                shared_files,
            },
            Outcome::WouldBePermanent { reason } => ActOutcome::WouldBePermanent(reason),
            Outcome::Refused { reason } => ActOutcome::Refused(reason),
            Outcome::Failed { error } => ActOutcome::Failed(error),
            _ => ActOutcome::Failed(OpError {
                step: "delete",
                kind: std::io::ErrorKind::Other,
                code: None,
                removed_bytes: 0,
                removed_entries: 0,
            }),
        }
    }
}

/// What the interface needs from the platform to act on files. The real one is
/// [`Disposal`]; tests use a fake.
pub trait Disposer: Send + Sync {
    /// Checks a selection for moving to the bin (`permanent` false) or for a
    /// permanent delete. Nothing is changed.
    fn precheck(
        &self,
        items: &[DItem],
        permanent: bool,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Plan;
    fn trash(&self, items: &[DItem], confirmed: Option<&ProtectedConfirmation>) -> Vec<ActOutcome>;
    fn delete_permanently(
        &self,
        items: &[DItem],
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome>;
    fn restore(&self, token: &Token) -> RestoreOutcome;
    /// Empties the bin of the drive mounted at `mount`.
    fn empty_bin(&self, mount: &Path) -> EmptyOutcome;
    /// Deletes `items` for good with administrator rights, through the
    /// helper: one prompt for the whole call. The default says it cannot be
    /// done.
    fn delete_elevated(&self, _items: &[DItem], _held: &[PathBuf]) -> ElevatedEnd {
        ElevatedEnd::Unavailable("administrator deletes aren't available here".to_string())
    }
}

impl Disposer for Disposal {
    fn precheck(
        &self,
        items: &[DItem],
        permanent: bool,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Plan {
        if permanent {
            self.precheck_permanent(items, confirmed)
        } else {
            Disposal::precheck(self, items, confirmed)
        }
    }

    fn trash(&self, items: &[DItem], confirmed: Option<&ProtectedConfirmation>) -> Vec<ActOutcome> {
        Disposal::trash(self, items, confirmed)
            .into_iter()
            .map(ActOutcome::from)
            .collect()
    }

    fn delete_permanently(
        &self,
        items: &[DItem],
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome> {
        Disposal::delete_permanently(self, items, confirmed)
            .into_iter()
            .map(ActOutcome::from)
            .collect()
    }

    fn restore(&self, token: &Token) -> RestoreOutcome {
        match token.get::<RestoreToken>() {
            Some(t) => Disposal::restore(self, t),
            None => RestoreOutcome::Refused {
                reason: RefuseReason::NotInBin,
            },
        }
    }

    fn empty_bin(&self, mount: &Path) -> EmptyOutcome {
        match self
            .drives()
            .iter()
            .find(|d| d.mount_point == mount)
            .cloned()
        {
            Some(drive) => Disposal::empty_bin(self, &drive),
            None => EmptyOutcome::Refused {
                reason: RefuseReason::NoBinOnDrive,
            },
        }
    }

    fn delete_elevated(&self, items: &[DItem], held: &[PathBuf]) -> ElevatedEnd {
        elevation::delete_with_helper(items, held)
    }
}

/// What the system can do, as far as the menus need to know.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// The file manager can show a properties dialog.
    pub properties: bool,
}

/// What the bin is called here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinStyle {
    /// "Recycle Bin".
    Recycle,
    /// "Trash".
    Trash,
}

impl BinStyle {
    pub fn system() -> Self {
        if cfg!(windows) {
            BinStyle::Recycle
        } else {
            BinStyle::Trash
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            BinStyle::Recycle => "Recycle Bin",
            BinStyle::Trash => "Trash",
        }
    }
}

/// The drive whose bin the capacity bar and the bin block stand for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinDrive {
    pub mount: PathBuf,
    /// "C:" on Windows; empty where drives have no letter.
    pub label: String,
}

/// A dialog or menu, for the accessibility tree.
pub(super) struct PopupSummary {
    pub menu: bool,
    /// What a screen reader calls the dialog.
    pub name: String,
    pub text: String,
    pub rect: Rect,
    /// Label, reason or instruction, whether it works, whether it needs a
    /// hold, and where it is.
    pub controls: Vec<(String, String, bool, bool, Rect)>,
}

// ---- jobs ---------------------------------------------------------------

/// Work that touches the disk. The host runs it off the UI thread with
/// [`Job::run`] and gives the result to [`Shell::job_done`].
#[derive(Clone)]
pub struct Job {
    id: u64,
    disposer: Arc<dyn Disposer>,
    work: Work,
}

#[derive(Clone, Debug)]
enum Work {
    Trash {
        items: Vec<DItem>,
        confirmed: Option<ProtectedConfirmation>,
    },
    Permanent {
        items: Vec<DItem>,
        confirmed: Option<ProtectedConfirmation>,
    },
    Restore(Vec<Token>),
    Empty(PathBuf),
    /// Deletes for good with administrator rights, through the helper.
    Elevated {
        items: Vec<DItem>,
        /// The protected paths whose hold-to-confirm was completed.
        held: Vec<PathBuf>,
    },
    /// Asks the platform what it thinks of deleting `items`. On Windows this
    /// can walk a whole folder, so it never runs on the UI thread.
    Check {
        items: Vec<DItem>,
        permanent: bool,
    },
    /// The same question for both rows of the context menu.
    CheckMenu {
        items: Vec<DItem>,
    },
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("work", &self.work)
            .finish()
    }
}

/// What a [`Job`] found out.
#[derive(Clone, Debug)]
pub struct JobResult {
    id: u64,
    done: Done,
}

#[derive(Clone, Debug)]
enum Done {
    Trash(Vec<ActOutcome>),
    Permanent(Vec<ActOutcome>),
    Restore(Vec<RestoreOutcome>),
    Empty(EmptyOutcome),
    Elevated(ElevatedEnd),
    Check(Box<Plan>),
    CheckMenu(Box<(Plan, Plan)>),
}

impl Job {
    /// Whether this only asks a question (nothing on disk is changed).
    pub fn is_check(&self) -> bool {
        matches!(self.work, Work::Check { .. } | Work::CheckMenu { .. })
    }

    /// Does the work. Blocks until the disk is done.
    pub fn run(self) -> JobResult {
        let d = &*self.disposer;
        let done = match &self.work {
            Work::Trash { items, confirmed } => Done::Trash(d.trash(items, confirmed.as_ref())),
            Work::Permanent { items, confirmed } => {
                Done::Permanent(d.delete_permanently(items, confirmed.as_ref()))
            }
            Work::Restore(tokens) => Done::Restore(tokens.iter().map(|t| d.restore(t)).collect()),
            Work::Empty(mount) => Done::Empty(d.empty_bin(mount)),
            Work::Elevated { items, held } => Done::Elevated(d.delete_elevated(items, held)),
            Work::Check { items, permanent } => {
                Done::Check(Box::new(d.precheck(items, *permanent, None)))
            }
            Work::CheckMenu { items } => Done::CheckMenu(Box::new((
                d.precheck(items, false, None),
                d.precheck(items, true, None),
            ))),
        };
        JobResult { id: self.id, done }
    }
}

/// A change to a box's delete mark in the size tree. `to` of
/// [`DeleteState::None`] takes the mark off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeChange {
    pub node: NodeId,
    pub to: DeleteState,
}

// ---- state --------------------------------------------------------------

struct UndoEntry {
    token: Token,
    node: Option<NodeId>,
    name: String,
}

/// Items that went to the bin together. The node ids belong to the map they
/// were made on (`scene`); on any other map they mean something else, so
/// they are never used there. The bin that holds them is the one of `mount`.
struct UndoGroup {
    scene: u64,
    mount: Option<PathBuf>,
    entries: Vec<UndoEntry>,
}

enum Pending {
    Trash(Vec<Target>),
    Permanent(Vec<Target>),
    Restore(Vec<(Option<NodeId>, String)>),
    Empty(PathBuf),
    /// The helper is deleting these with administrator rights.
    Elevated(Vec<Target>),
    /// The platform is looking at a Delete or Shift+Delete, or at items
    /// that need administrator rights.
    Delete {
        elevated: bool,
        permanent: bool,
        items: Vec<DItem>,
        targets: Vec<Target>,
    },
    /// The platform is looking at the Delete rows of the open menu.
    Menu(u64),
}

/// A job in flight and the map it was started on.
struct Slot {
    scene: u64,
    mount: Option<PathBuf>,
    what: Pending,
}

struct RowModel {
    row: MenuRow,
    action: Option<Action>,
}

struct MenuUi {
    seq: u64,
    rows: Vec<RowModel>,
    view: ViewId,
    ids: Vec<u32>,
    at: (f32, f32),
    hot: Option<usize>,
    built: popup::Built,
}

enum Confirm {
    Delete {
        permanent: bool,
        /// Deleted by the helper with administrator rights.
        elevated: bool,
        protected: Vec<PathBuf>,
        items: Vec<DItem>,
        targets: Vec<Target>,
    },
    Empty,
    /// The About box.
    About,
    /// The crash report viewer.
    Report(ReportView),
}

/// What the crash report viewer shows now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReportView {
    notice: CrashNotice,
    show_paths: bool,
    /// The first report line on screen.
    offset: usize,
    /// How many report lines fit.
    rows: usize,
}

/// The names an administrator delete lists, which scroll when they do not all
/// fit: the listed names are what the helper is asked to delete, so none is
/// left out.
struct NameList {
    /// The first name on screen.
    offset: usize,
    /// How many names fit.
    rows: usize,
    /// The notes of the protected items, repeated in the dialog.
    notes: Vec<&'static str>,
}

struct DialogUi {
    confirm: Confirm,
    list: Option<NameList>,
    spec: DialogSpec,
    focus: usize,
    hold_since: Option<Duration>,
    built: popup::Built,
}

enum Modal {
    Menu(MenuUi),
    Dialog(Box<DialogUi>),
}

struct ToastUi {
    spec: ToastSpec,
    shown: Duration,
    until: Duration,
    hovered: bool,
    hot_button: bool,
    closing: Option<Duration>,
}

/// Everything the action layer remembers.
#[derive(Default)]
pub(super) struct ActionsUi {
    disposer: Option<Arc<dyn Disposer>>,
    caps: Capabilities,
    style: Option<BinStyle>,
    bin_drive: Option<BinDrive>,
    modal: Option<Modal>,
    toast: Option<ToastUi>,
    undo: Vec<UndoGroup>,
    next_job: u64,
    pending: HashMap<u64, Slot>,
    seen: HashMap<PathBuf, Identity>,
    /// Changes whenever the map is replaced (another drive, a rescan).
    scene: u64,
    /// Counts the menus opened, so a late answer finds its own menu.
    menu_seq: u64,
    /// The toast's Undo button has keyboard focus.
    pub(super) toast_focus: bool,
    /// The desktop can open software-center entries.
    pub(super) software_center: bool,
    /// The menu check that waits for the one running to finish: the newest
    /// request replaces an older one that never started.
    menu_waiting: Option<(Vec<DItem>, u64)>,
}

fn size_text(bytes: u64, lower: bool, estimated: bool) -> String {
    let mark = if lower {
        "≥ "
    } else if estimated {
        "≈ "
    } else {
        ""
    };
    format!("{mark}{}", bs_layout::format_size(bytes))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

impl<S: ViewSource> Shell<S> {
    // ---- setup ------------------------------------------------------------

    /// Gives the shell the platform service that deletes, restores and empties.
    pub fn set_disposer(&mut self, disposer: Arc<dyn Disposer>) {
        self.acts.disposer = Some(disposer);
    }

    /// Tells the menus what the system can do.
    pub fn set_capabilities(&mut self, caps: Capabilities) {
        self.acts.caps = caps;
    }

    /// Tells the cards whether the desktop has a software center, so the
    /// button that opens one is offered or says why not.
    pub fn set_software_center(&mut self, available: bool) {
        self.acts.software_center = available;
    }

    /// Names the bin as the system does (tests pick either).
    pub fn set_bin_style(&mut self, style: BinStyle) {
        self.acts.style = Some(style);
    }

    /// Sets the drive whose bin "Empty" acts on.
    pub fn set_bin_drive(&mut self, drive: Option<BinDrive>) {
        self.acts.bin_drive = drive;
    }

    fn bin(&self) -> &'static str {
        self.acts.style.unwrap_or_else(BinStyle::system).name()
    }

    /// Whether a dialog or menu is open and takes all input.
    pub fn modal_open(&self) -> bool {
        self.acts.modal.is_some()
    }

    /// The words of the open dialog, as one text.
    pub fn dialog_text(&self) -> Option<String> {
        match self.acts.modal.as_ref()? {
            Modal::Dialog(d) => {
                let mut out = String::new();
                if let Some(lead) = &d.spec.lead {
                    out.push_str(lead);
                    out.push(' ');
                }
                out.push_str(&d.spec.title);
                // A list that scrolls is read whole, not just the part on
                // screen.
                let all: Option<&[Target]> = match (&d.confirm, &d.list) {
                    (Confirm::Delete { targets, .. }, Some(_)) => Some(targets),
                    _ => None,
                };
                for (line, _) in &d.spec.lines {
                    if all.is_some() && line.starts_with("\u{2022} ") {
                        continue;
                    }
                    out.push(' ');
                    out.push_str(line);
                }
                for t in all.into_iter().flatten() {
                    out.push_str(" \u{2022} ");
                    out.push_str(&t.name);
                }
                Some(out)
            }
            Modal::Menu(_) => None,
        }
    }

    /// The labels of the open dialog's buttons.
    pub fn dialog_buttons(&self) -> Vec<String> {
        match self.acts.modal.as_ref() {
            Some(Modal::Dialog(d)) => d.spec.buttons.iter().map(|b| b.label.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// The rows of the open menu: label, hint (shortcut or reason) and whether
    /// the row works. Separators are left out.
    pub fn menu_items(&self) -> Vec<(String, String, bool)> {
        match self.acts.modal.as_ref() {
            Some(Modal::Menu(m)) => m
                .rows
                .iter()
                .filter(|r| !r.row.separator)
                .map(|r| (r.row.label.clone(), r.row.hint.clone(), r.row.enabled))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The toast's words, with its button as the last part
    /// ("Moved to Recycle Bin · 12.3 GB · Undo").
    pub fn toast_text(&self) -> Option<String> {
        let t = self.acts.toast.as_ref()?;
        let mut out = t
            .spec
            .lines
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(b) = &t.spec.button {
            out.push_str(" · ");
            out.push_str(b);
        }
        Some(out)
    }

    /// The open dialog or menu as a screen reader needs it.
    pub(super) fn popup_summary(&self) -> Option<PopupSummary> {
        match self.acts.modal.as_ref()? {
            Modal::Dialog(d) => {
                let controls = d
                    .built
                    .popup
                    .card
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        bs_render::CardPart::Button { rect, label, .. } => {
                            Some((label.to_string(), String::new(), true, false, *rect))
                        }
                        bs_render::CardPart::Hold { rect, label, .. } => Some((
                            label.to_string(),
                            format!(
                                "Hold for {} seconds to confirm. Focus the button and hold Enter or Space.",
                                HOLD_FOR.as_secs_f32()
                            ),
                            true,
                            true,
                            *rect,
                        )),
                        _ => None,
                    })
                    .collect();
                let name = match d.confirm {
                    Confirm::About | Confirm::Report(_) => d.spec.title.clone(),
                    _ => "Confirm".to_string(),
                };
                Some(PopupSummary {
                    menu: false,
                    name,
                    text: self.dialog_text().unwrap_or_default(),
                    rect: d.built.popup.card.rect,
                    controls,
                })
            }
            Modal::Menu(m) => {
                let controls = m
                    .built
                    .popup
                    .card
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        bs_render::CardPart::Row {
                            rect,
                            label,
                            hint,
                            enabled,
                            ..
                        } => Some((
                            label.to_string(),
                            if *enabled {
                                String::new()
                            } else {
                                hint.to_string()
                            },
                            *enabled,
                            false,
                            *rect,
                        )),
                        _ => None,
                    })
                    .collect();
                Some(PopupSummary {
                    menu: true,
                    name: "Actions".to_string(),
                    text: String::new(),
                    rect: m.built.popup.card.rect,
                    controls,
                })
            }
        }
    }

    /// The toast as a screen reader needs it: its words, and the Undo button.
    pub(super) fn toast_summary(&self, now: Duration) -> Option<(String, Rect, Option<Rect>)> {
        let built = self.toast_built(now)?;
        let text = self.toast_text()?;
        Some((text, built.card.rect, built.button))
    }

    /// Whether a toast with a button is up and not on its way out.
    pub(super) fn toast_has_button(&self) -> bool {
        self.acts
            .toast
            .as_ref()
            .is_some_and(|t| t.closing.is_none() && t.spec.button.is_some())
    }

    /// Whether the toast's button has the keyboard.
    pub(super) fn toast_focused(&self) -> bool {
        self.acts.toast_focus && self.toast_has_button()
    }

    /// The control that has the keyboard in the open dialog or menu, by
    /// position among the popup's controls. A menu with no row chosen yet
    /// gives `None`.
    pub(super) fn popup_focus(&self) -> Option<usize> {
        match self.acts.modal.as_ref()? {
            Modal::Dialog(d) => Some(d.focus.min(d.spec.buttons.len().saturating_sub(1))),
            Modal::Menu(m) => {
                let hot = m.hot?;
                Some(m.rows.iter().take(hot).filter(|r| !r.row.separator).count())
            }
        }
    }

    /// How many deletes can still be undone.
    pub fn undo_depth(&self) -> usize {
        self.acts.undo.len()
    }

    /// The dialog or menu to draw, if one is open.
    pub fn popup(&self) -> Option<Popup> {
        match self.acts.modal.as_ref()? {
            Modal::Menu(m) => Some(m.built.popup.clone()),
            Modal::Dialog(d) => Some(d.built.popup.clone()),
        }
    }

    /// The toast to draw at `now`, if one is up.
    pub fn toast_card(&self, now: Duration) -> Option<Card> {
        Some(self.toast_built(now)?.card)
    }

    fn metrics(&self) -> Metrics<'_> {
        Metrics {
            em: self.font_px * self.scale,
            scale: self.scale,
            measure: self.measure.as_ref(),
        }
    }

    fn window_rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    // ---- the toast ----------------------------------------------------------

    fn toast_rise(&self, t: &ToastUi, now: Duration) -> Option<f32> {
        if self.reduced_motion {
            return t.closing.is_none().then_some(1.0);
        }
        let ease = |x: f32| 1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3);
        let span = TOAST_MOTION.as_secs_f32();
        match t.closing {
            Some(at) => {
                let p = now.saturating_sub(at).as_secs_f32() / span;
                (p < 1.0).then(|| 1.0 - ease(p))
            }
            None => Some(ease(now.saturating_sub(t.shown).as_secs_f32() / span)),
        }
    }

    fn toast_built(&self, now: Duration) -> Option<popup::BuiltToast> {
        let t = self.acts.toast.as_ref()?;
        let rise = self.toast_rise(t, now)?;
        Some(popup::toast(
            &t.spec,
            self.map_rect(),
            self.metrics(),
            t.hot_button || self.acts.toast_focus,
            rise,
        ))
    }

    fn show_toast(&mut self, lines: Vec<(String, CardRole)>, undo: bool, now: Duration) {
        // A new toast starts without the keyboard on its button.
        self.acts.toast_focus = false;
        self.acts.toast = Some(ToastUi {
            spec: ToastSpec {
                lines,
                button: undo.then(|| Action::Undo.label().to_string()),
            },
            shown: now,
            until: now + TOAST_FOR,
            hovered: false,
            hot_button: false,
            closing: None,
        });
    }

    fn close_toast(&mut self, now: Duration) {
        // A toast on its way out has no button for the keyboard to be on.
        self.acts.toast_focus = false;
        if self.reduced_motion {
            self.acts.toast = None;
        } else if let Some(t) = self.acts.toast.as_mut() {
            t.closing.get_or_insert(now);
        }
    }

    /// The pointer on the toast. Returns whether the toast took the event.
    pub(super) fn toast_input(
        &mut self,
        event: InputEvent,
        now: Duration,
    ) -> Option<Vec<ShellEffect>> {
        let (x, y) = match event {
            InputEvent::PointerMoved { x, y }
            | InputEvent::Click { x, y }
            | InputEvent::ClickWith { x, y, .. }
            | InputEvent::DoubleClick { x, y }
            | InputEvent::PointerDown { x, y }
            | InputEvent::PointerUp { x, y } => (x, y),
            _ => {
                return None;
            }
        };
        let built = self.toast_built(now);
        let over = built
            .as_ref()
            .is_some_and(|b| b.card.dirty_rect(self.scale).contains(x, y));
        let on_button = built
            .as_ref()
            .and_then(|b| b.button)
            .is_some_and(|r| r.contains(x, y));
        let t = self.acts.toast.as_mut()?;
        let was = (t.hovered, t.hot_button);
        t.hovered = over;
        t.hot_button = on_button;
        if !over {
            if was.0 {
                // Leaving starts the full time again.
                t.until = now + TOAST_FOR;
            }
            return None;
        }
        if matches!(event, InputEvent::Click { .. }) && on_button {
            return Some(self.undo_last(now));
        }
        Some(Vec::new())
    }

    // ---- selection bookkeeping ----------------------------------------------

    /// Remembers what the selected boxes are on disk right now, so a delete
    /// refuses an item that was replaced since.
    pub(super) fn note_selection(&mut self) {
        let Some(app) = self.app.as_ref() else {
            return;
        };
        let view = app.current_view();
        let mut paths = Vec::new();
        for &id in app.selection().iter().take(32) {
            if let Some(t) = app.source().target(view, id) {
                paths.push(t.path);
            }
        }
        if self.acts.seen.len() > MAX_SEEN {
            self.acts.seen.clear();
        }
        for p in paths {
            if !self.acts.seen.contains_key(&p)
                && let Ok(identity) = bs_platform::delete::identity_of(&p)
            {
                self.acts.seen.insert(p, identity);
            }
        }
    }

    fn targets(&self, view: ViewId, ids: &[u32]) -> Option<Vec<Target>> {
        let app = self.app.as_ref()?;
        ids.iter()
            .map(|&id| app.source().target(view, id))
            .collect()
    }

    fn items_for(&self, targets: &[Target]) -> Vec<DItem> {
        targets
            .iter()
            .map(|t| {
                let item = DItem::new(t.path.clone());
                match self.acts.seen.get(&t.path) {
                    Some(identity) => item.seen(*identity),
                    None => item,
                }
            })
            .collect()
    }

    // ---- delete ----------------------------------------------------------------

    /// Whether an action is still running. A question for the menu does not
    /// count: the menu may be closed before it is answered.
    pub(super) fn busy(&self) -> bool {
        self.acts
            .pending
            .values()
            .any(|s| !matches!(s.what, Pending::Menu(_)))
    }

    /// A new map is on screen (or none): what was remembered about the old
    /// one no longer applies.
    pub(super) fn new_scene(&mut self) {
        self.acts.scene += 1;
        self.acts.seen.clear();
        self.acts.modal = None;
        self.acts.toast_focus = false;
    }

    /// The window lost the keyboard focus: a button held down (Enter or
    /// Space whose release goes to another window) stops counting.
    pub(super) fn focus_lost(&mut self) {
        self.cancel_hold();
        self.acts.toast_focus = false;
    }

    /// Delete or Shift+Delete on boxes of `view`. What the platform thinks of
    /// it is asked on a worker thread; the answer decides what happens next.
    pub(super) fn request_delete(
        &mut self,
        permanent: bool,
        view: ViewId,
        ids: Vec<u32>,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let label = if permanent {
            Action::DeletePermanently.label()
        } else {
            Action::Delete.label()
        };
        if self.acts.disposer.is_none() {
            self.say(format!("{label}: not available here"), now);
            return Vec::new();
        }
        if self.busy() {
            self.say(format!("{label}: wait for the last action to finish"), now);
            return Vec::new();
        }
        let Some(targets) = self.targets(view, &ids) else {
            self.say(format!("{label}: {}", reason::NOT_ON_DISK), now);
            return Vec::new();
        };
        let items = self.items_for(&targets);
        self.dispatch(
            Work::Check {
                items: items.clone(),
                permanent,
            },
            Pending::Delete {
                elevated: false,
                permanent,
                items,
                targets,
            },
        )
    }

    /// Items the user's own rights could not delete, offered to the helper:
    /// the platform is asked what it thinks of deleting them for good (the
    /// strictest rule still applies), and the answer opens the dialog.
    fn request_admin_delete(&mut self, targets: Vec<Target>) -> Vec<ShellEffect> {
        let items = self.items_for(&targets);
        self.dispatch(
            Work::Check {
                items: items.clone(),
                permanent: true,
            },
            Pending::Delete {
                elevated: true,
                permanent: true,
                items,
                targets,
            },
        )
    }

    /// The platform answered a Delete or Shift+Delete: refuse, ask, or go.
    fn delete_checked(
        &mut self,
        plan: &Plan,
        permanent: bool,
        elevated: bool,
        items: Vec<DItem>,
        targets: Vec<Target>,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let label = if permanent {
            Action::DeletePermanently.label()
        } else {
            Action::Delete.label()
        };
        if let Some(why) = refusal_text(plan) {
            self.say(format!("{label}: {why}"), now);
            return Vec::new();
        }
        let protected: Vec<PathBuf> = plan
            .entries
            .iter()
            .zip(&items)
            .filter(|(e, _)| e.level == Level::Protected)
            .map(|(_, i)| i.path.clone())
            .collect();
        let overflow = if permanent {
            None
        } else {
            plan.entries.iter().find_map(|e| match &e.decision {
                Decision::WouldBePermanent(why) => Some(why.clone()),
                _ => None,
            })
        };
        if elevated {
            let notes: Vec<&'static str> = plan.entries.iter().filter_map(|e| e.note).collect();
            let mut list = NameList {
                offset: 0,
                rows: 1,
                notes,
            };
            let spec = self.fitted_admin_dialog(&targets, !protected.is_empty(), &mut list);
            self.open_dialog(
                Confirm::Delete {
                    permanent: true,
                    elevated: true,
                    protected,
                    items,
                    targets,
                },
                spec,
            );
            if let Some(Modal::Dialog(d)) = self.acts.modal.as_mut() {
                d.list = Some(list);
            }
            return Vec::new();
        }
        let deleting_for_good = permanent || overflow.is_some();
        if !deleting_for_good && protected.is_empty() {
            return self.start_trash(items, None, targets, now);
        }
        let notes: Vec<&'static str> = plan.entries.iter().filter_map(|e| e.note).collect();
        let spec = self.delete_dialog(
            &targets,
            deleting_for_good,
            !protected.is_empty(),
            overflow.as_ref(),
            &notes,
        );
        self.open_dialog(
            Confirm::Delete {
                permanent: deleting_for_good,
                elevated: false,
                protected,
                items,
                targets,
            },
            spec,
        );
        Vec::new()
    }

    fn delete_dialog(
        &self,
        targets: &[Target],
        permanent: bool,
        protected: bool,
        overflow: Option<&PermanentReason>,
        notes: &[&'static str],
    ) -> DialogSpec {
        let bin = self.bin();
        let bytes = targets.iter().fold(0u64, |a, t| a.saturating_add(t.bytes));
        let lower = targets.iter().any(|t| t.lower_bound);
        let est = targets.iter().any(|t| t.estimated);
        let size = size_text(bytes, lower, est);
        let count = plural(targets.len(), "item", "items");
        let mut lines: Vec<(String, CardRole)> = Vec::new();
        // What must stay readable however long the rest is.
        let mut pinned: Vec<usize> = Vec::new();
        let title = if permanent {
            pinned.push(lines.len());
            lines.push(("This can't be undone.".into(), CardRole::Secondary));
            format!("Permanently delete {count} ({size})?")
        } else {
            match targets {
                [one] => format!("Move “{}” to the {bin}? ({size})", one.name),
                _ => format!("Move {count} to the {bin}? ({size})"),
            }
        };
        if protected {
            // Each distinct note once; a long list says how many are left.
            let mut seen: Vec<&str> = Vec::new();
            for n in notes {
                if !seen.contains(n) {
                    seen.push(n);
                }
            }
            if seen.is_empty() {
                lines.push(("This is a protected location.".into(), CardRole::Normal));
            }
            for n in seen.iter().take(LISTED_NOTES) {
                lines.push(((*n).to_string(), CardRole::Normal));
            }
            if seen.len() > LISTED_NOTES {
                lines.push((
                    format!("and {} more", seen.len() - LISTED_NOTES),
                    CardRole::Secondary,
                ));
            }
            pinned.push(lines.len());
            lines.push((
                format!(
                    "Hold the button for {} seconds to confirm.",
                    HOLD_FOR.as_secs_f32()
                ),
                CardRole::Secondary,
            ));
        }
        for t in targets.iter().take(LISTED_NAMES) {
            lines.push((format!("• {}", t.name), CardRole::Secondary));
        }
        if targets.len() > LISTED_NAMES {
            lines.push((
                format!("and {} more", targets.len() - LISTED_NAMES),
                CardRole::Secondary,
            ));
        }
        let confirm = match (permanent, protected) {
            (true, false) => "Delete permanently".to_string(),
            (true, true) => "Hold to delete permanently".to_string(),
            (false, _) => format!("Hold to move to the {bin}"),
        };
        DialogSpec {
            lead: overflow.map(|why| self.overflow_line(why)),
            title,
            lines,
            pinned,
            rows: 0,
            buttons: vec![
                Button {
                    label: confirm,
                    hold: protected,
                },
                Button {
                    label: "Cancel".into(),
                    hold: false,
                },
            ],
        }
    }

    /// The dialog for items that need administrator rights: what will be
    /// deleted, that it is for good, and that the system will ask once.
    fn admin_delete_dialog(
        &self,
        targets: &[Target],
        protected: bool,
        list: &NameList,
    ) -> DialogSpec {
        let notes = &list.notes;
        let bytes = targets.iter().fold(0u64, |a, t| a.saturating_add(t.bytes));
        let lower = targets.iter().any(|t| t.lower_bound);
        let est = targets.iter().any(|t| t.estimated);
        let size = size_text(bytes, lower, est);
        let count = plural(targets.len(), "item", "items");
        let mut lines: Vec<(String, CardRole)> = Vec::new();
        let mut pinned: Vec<usize> = Vec::new();
        pinned.push(lines.len());
        lines.push((
            if self.acts.style.unwrap_or_else(BinStyle::system) == BinStyle::Trash {
                "Will be permanently deleted (system Trash isn't available for administrator deletes)."
                    .to_string()
            } else {
                "Will be permanently deleted. This can't be undone.".to_string()
            },
            CardRole::Normal,
        ));
        lines.push((
            "The system asks for administrator rights once, for this delete only.".into(),
            CardRole::Secondary,
        ));
        if protected {
            let mut seen: Vec<&str> = Vec::new();
            for n in notes {
                if !seen.contains(n) {
                    seen.push(n);
                }
            }
            if seen.is_empty() {
                lines.push(("This is a protected location.".into(), CardRole::Normal));
            }
            for n in seen.iter().take(LISTED_NOTES) {
                lines.push(((*n).to_string(), CardRole::Normal));
            }
            if seen.len() > LISTED_NOTES {
                lines.push((
                    format!("and {} more", seen.len() - LISTED_NOTES),
                    CardRole::Secondary,
                ));
            }
            lines.push((
                format!(
                    "Hold the button for {} seconds to confirm.",
                    HOLD_FOR.as_secs_f32()
                ),
                CardRole::Secondary,
            ));
        }
        // Everything above stays on screen; the names below scroll.
        pinned.extend(1..lines.len());
        let first = list.offset.min(targets.len());
        let shown: Vec<&Target> = targets.iter().skip(first).take(list.rows).collect();
        let room = self.dialog_inner_width() - self.text_width("\u{2022} ");
        for t in &shown {
            lines.push((
                format!("\u{2022} {}", self.fit_line(&t.name, room)),
                CardRole::Secondary,
            ));
        }
        if shown.len() < targets.len() {
            pinned.push(lines.len());
            lines.push((
                format!(
                    "Items {}–{} of {} · wheel or arrow keys to scroll",
                    first + 1,
                    first + shown.len(),
                    targets.len()
                ),
                CardRole::Secondary,
            ));
        }
        DialogSpec {
            lead: Some("These need administrator rights.".to_string()),
            title: format!("Delete {count} ({size}) as administrator?"),
            lines,
            pinned,
            // Room for every name that is shown and the lines around them.
            rows: 60,
            buttons: vec![
                Button {
                    label: if protected {
                        "Hold to delete as administrator".to_string()
                    } else {
                        "Delete as administrator".to_string()
                    },
                    hold: protected,
                },
                Button {
                    label: "Cancel".into(),
                    hold: false,
                },
            ],
        }
    }

    /// The width of `text` on one line, in device pixels.
    fn text_width(&self, text: &str) -> f32 {
        self.measure
            .width(text)
            .unwrap_or_else(|| text.chars().count() as f32 * self.font_px * self.scale * 0.56)
    }

    /// How wide a line of text in a dialog can be: the dialog's own width
    /// less its padding, worked out the way the dialog is built.
    fn dialog_inner_width(&self) -> f32 {
        let em = self.font_px * self.scale;
        let pad = em.round();
        let margin = (12.0 * self.scale).round();
        let window = self.window_rect();
        let width = (em * 34.0)
            .min((window.w - 2.0 * margin).max(em * 12.0))
            .round();
        (width - 2.0 * pad).max(em)
    }

    /// `text` on one line no wider than `width`: runs of white space become
    /// one space, and what doesn't fit is replaced in the middle by an
    /// ellipsis. Works on characters, never on byte positions.
    fn fit_line(&self, text: &str, width: f32) -> String {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if self.text_width(&text) <= width {
            return text;
        }
        let chars: Vec<char> = text.chars().collect();
        let make = |kept: usize| -> String {
            let head = kept.div_ceil(2);
            let tail = kept / 2;
            let mut s: String = chars[..head].iter().collect();
            s.push('\u{2026}');
            s.extend(chars[chars.len() - tail..].iter());
            s
        };
        // The most characters that still fit.
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.text_width(&make(mid)) <= width {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        make(lo)
    }

    /// The administrator dialog with as many names as fit in the window: the
    /// dialog stays inside it with its buttons, and the "Items a to b of n" line
    /// is the count that is drawn.
    fn fitted_admin_dialog(
        &self,
        targets: &[Target],
        protected: bool,
        list: &mut NameList,
    ) -> DialogSpec {
        let window = self.window_rect();
        let mut rows = self.name_rows().min(targets.len()).max(1);
        loop {
            list.rows = rows;
            list.offset = list.offset.min(targets.len().saturating_sub(rows));
            let spec = self.admin_delete_dialog(targets, protected, list);
            let built = popup::dialog(
                &spec,
                window,
                self.metrics(),
                DialogState {
                    focus: 0,
                    progress: 0.0,
                },
            );
            let card = built.popup.card.rect;
            let drawn = built
                .popup
                .card
                .parts
                .iter()
                .filter(|p| {
                    matches!(p, bs_render::CardPart::Line { text, .. }
                        if text.starts_with("\u{2022} "))
                })
                .count();
            let shown = targets.len().saturating_sub(list.offset).min(rows);
            if rows == 1 || (card.y >= 0.0 && card.bottom() <= window.h && drawn == shown) {
                return spec;
            }
            rows -= 1;
        }
    }

    /// The window changed size: the names that fit may be more or fewer.
    fn refit_names(&mut self) {
        if !matches!(&self.acts.modal, Some(Modal::Dialog(d)) if d.list.is_some()) {
            return;
        }
        let Some(Modal::Dialog(mut d)) = self.acts.modal.take() else {
            return;
        };
        {
            let d = &mut *d;
            if let (
                Confirm::Delete {
                    targets, protected, ..
                },
                Some(list),
            ) = (&d.confirm, d.list.as_mut())
            {
                d.spec = self.fitted_admin_dialog(targets, !protected.is_empty(), list);
            }
        }
        self.acts.modal = Some(Modal::Dialog(d));
    }

    /// The first line of the dialog that explains why the bin is not used.
    fn overflow_line(&self, why: &PermanentReason) -> String {
        let bin = self.bin();
        match why {
            PermanentReason::TooLargeForBin {
                limit_bytes,
                item_bytes,
            } => format!(
                "This is {} and the {bin} holds at most {}, so it can't be moved there.",
                bs_layout::format_size(*item_bytes),
                bs_layout::format_size(*limit_bytes)
            ),
            PermanentReason::BinDisabled => {
                format!("This drive is set to delete without using the {bin}.")
            }
            PermanentReason::NoTrashOnVolume => {
                format!("This drive has no {bin} that can be used, so it can't be moved there.")
            }
            PermanentReason::CrossesFilesystem => format!(
                "Moving this to the {bin} would mean copying it to another drive, so it can't be moved there."
            ),
            PermanentReason::BinUnknown => {
                format!("Couldn't tell whether this drive has a {bin}, so it can't be moved there.")
            }
            _ => format!("This can't be moved to the {bin}."),
        }
    }

    fn start_trash(
        &mut self,
        items: Vec<DItem>,
        confirmed: Option<ProtectedConfirmation>,
        targets: Vec<Target>,
        _now: Duration,
    ) -> Vec<ShellEffect> {
        let work = Work::Trash { items, confirmed };
        self.dispatch(work, Pending::Trash(targets))
    }

    fn dispatch(&mut self, work: Work, pending: Pending) -> Vec<ShellEffect> {
        self.dispatch_for(work, pending, self.acts.scene)
    }

    fn dispatch_for(&mut self, work: Work, pending: Pending, scene: u64) -> Vec<ShellEffect> {
        let Some(disposer) = self.acts.disposer.clone() else {
            return Vec::new();
        };
        let id = self.acts.next_job;
        self.acts.next_job += 1;
        let mount = self.acts.bin_drive.as_ref().map(|d| d.mount.clone());
        self.acts.pending.insert(
            id,
            Slot {
                scene,
                mount,
                what: pending,
            },
        );
        vec![ShellEffect::Run(Job { id, disposer, work })]
    }

    // ---- results ------------------------------------------------------------

    /// A [`Job`] finished. Returns the tree changes and the like the host
    /// carries out, and shows what happened.
    pub fn job_done(&mut self, result: JobResult, now: Duration) -> Vec<ShellEffect> {
        self.clock = self.clock.max(now);
        let Some(slot) = self.acts.pending.remove(&result.id) else {
            return Vec::new();
        };
        let Slot { scene, mount, what } = slot;
        match (what, result.done) {
            (Pending::Trash(targets), Done::Trash(outcomes)) => {
                self.trash_done(&targets, outcomes, false, scene, mount, now)
            }
            (Pending::Permanent(targets), Done::Permanent(outcomes)) => {
                self.trash_done(&targets, outcomes, true, scene, mount, now)
            }
            (Pending::Restore(entries), Done::Restore(outcomes)) => {
                self.restore_done(&entries, outcomes, scene, now)
            }
            (Pending::Empty(mount), Done::Empty(outcome)) => self.empty_done(outcome, &mount, now),
            (Pending::Elevated(targets), Done::Elevated(end)) => {
                self.elevated_done(&targets, end, scene, now)
            }
            (
                Pending::Delete {
                    elevated,
                    permanent,
                    items,
                    targets,
                },
                Done::Check(plan),
            ) => {
                if scene != self.acts.scene {
                    // The map was replaced while the question was open; the
                    // boxes it was about are no longer on screen.
                    let label = if permanent {
                        Action::DeletePermanently.label()
                    } else {
                        Action::Delete.label()
                    };
                    self.say(
                        format!("{label}: the view changed, so nothing was deleted"),
                        now,
                    );
                    return Vec::new();
                }
                self.delete_checked(&plan, permanent, elevated, items, targets, now)
            }
            (Pending::Menu(seq), Done::CheckMenu(plans)) => {
                self.menu_checked(seq, *plans);
                // Only one question about a menu runs at a time; the newest
                // one that waited goes next, if its menu is still open.
                match self.acts.menu_waiting.take() {
                    Some((items, waiting)) if self.menu_is_open(waiting) => {
                        self.start_menu_check(items, waiting)
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn trash_done(
        &mut self,
        targets: &[Target],
        outcomes: Vec<ActOutcome>,
        asked_permanent: bool,
        scene: u64,
        mount: Option<PathBuf>,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let bin = self.bin();
        let mut changes = Vec::new();
        // The boxes belong to the map the delete was made on. If another one
        // is showing now, the files are still gone, but no box may be marked.
        let same_map = scene == self.acts.scene;
        let mut group = UndoGroup {
            scene,
            mount,
            entries: Vec::new(),
        };
        let (mut moved, mut moved_lower, mut moved_est, mut logical) = (0u64, false, false, false);
        let (mut gone, mut gone_lower, mut gone_est) = (0u64, false, false);
        let (mut shared, mut moved_n, mut gone_n) = (0u64, 0usize, 0usize);
        let mut problems: Vec<String> = Vec::new();
        // Items the user's own rights could not touch, offered to the helper.
        let mut denied: Vec<Target> = Vec::new();
        for (target, outcome) in targets.iter().zip(outcomes) {
            match outcome {
                ActOutcome::MovedToBin {
                    token,
                    bytes,
                    basis,
                    shared_files,
                } => {
                    moved = moved.saturating_add(bytes);
                    moved_n += 1;
                    moved_lower |= target.lower_bound;
                    moved_est |= target.estimated;
                    logical |= basis == ByteBasis::Logical;
                    shared = shared.saturating_add(shared_files);
                    if let Some(node) = target.node.filter(|_| same_map) {
                        changes.push(TreeChange {
                            node,
                            to: DeleteState::InBin,
                        });
                    }
                    group.entries.push(UndoEntry {
                        token,
                        node: target.node.filter(|_| same_map),
                        name: target.name.clone(),
                    });
                }
                ActOutcome::Deleted {
                    bytes,
                    shared_files,
                } => {
                    gone = gone.saturating_add(bytes);
                    gone_n += 1;
                    gone_lower |= target.lower_bound;
                    gone_est |= target.estimated;
                    shared = shared.saturating_add(shared_files);
                    if let Some(node) = target.node.filter(|_| same_map) {
                        changes.push(TreeChange {
                            node,
                            to: DeleteState::Deleted,
                        });
                    }
                    if !asked_permanent {
                        problems.push(format!(
                            "“{}” couldn't be moved to the {bin}, so it was deleted instead.",
                            target.name
                        ));
                    }
                }
                ActOutcome::WouldBePermanent(_) => problems.push(format!(
                    "“{}” wasn't moved: the {bin} can't take it.",
                    target.name
                )),
                ActOutcome::Refused(reason) => problems.push(format!(
                    "“{}” wasn't touched: {}.",
                    target.name,
                    refuse_text(&reason)
                )),
                ActOutcome::Failed(error) => {
                    if error.kind == std::io::ErrorKind::PermissionDenied
                        && error.removed_entries == 0
                        && same_map
                    {
                        denied.push(target.clone());
                    }
                    let verb = if asked_permanent {
                        "delete".to_string()
                    } else {
                        format!("move to the {bin}")
                    };
                    problems.push(format!(
                        "Couldn't {verb} “{}”: {}.",
                        target.name,
                        error_text(&error)
                    ));
                }
            }
        }
        let mut lines: Vec<(String, CardRole)> = Vec::new();
        let mut undo = false;
        if moved_n > 0 {
            let basis = if logical { " of files" } else { "" };
            lines.push((
                format!(
                    "Moved to {bin} · {}{basis}",
                    size_text(moved, moved_lower, moved_est)
                ),
                CardRole::Normal,
            ));
            undo = true;
            self.acts.undo.push(group);
        }
        if gone_n > 0 {
            let text = format!("Deleted · {}", size_text(gone, gone_lower, gone_est));
            if lines.is_empty() {
                lines.push((text, CardRole::Normal));
            } else {
                lines.push((text, CardRole::Secondary));
            }
        }
        if shared > 0 {
            lines.push((
                format!(
                    "{} also {} another name, so {} data stays on the drive.",
                    plural(shared as usize, "file", "files"),
                    if shared == 1 { "has" } else { "have" },
                    if shared == 1 { "its" } else { "their" }
                ),
                CardRole::Secondary,
            ));
        }
        for p in problems {
            lines.push((p, CardRole::Secondary));
        }
        if lines.is_empty() {
            return Vec::new();
        }
        if lines
            .first()
            .is_some_and(|(_, r)| *r == CardRole::Secondary)
        {
            lines[0].1 = CardRole::Normal;
        }
        self.show_toast(lines, undo, now);
        let mut fx = Vec::new();
        if !changes.is_empty() {
            fx.push(ShellEffect::Tree(changes));
        }
        fx.push(ShellEffect::BinChanged);
        if !denied.is_empty() && self.offer.can_elevate() && !self.busy() {
            fx.extend(self.request_admin_delete(denied));
        }
        fx
    }

    /// The helper finished (or broke off) an administrator delete.
    fn elevated_done(
        &mut self,
        targets: &[Target],
        end: ElevatedEnd,
        scene: u64,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let same_map = scene == self.acts.scene;
        let mut lines: Vec<(String, CardRole)> = Vec::new();
        let outcomes: Vec<Option<ElevatedItem>>;
        let mut broke_off: Option<String> = None;
        match end {
            ElevatedEnd::Declined => {
                self.show_toast(
                    vec![(elevation::DECLINED_TEXT.into(), CardRole::Normal)],
                    false,
                    now,
                );
                return Vec::new();
            }
            ElevatedEnd::NoAccount => {
                self.show_toast(
                    vec![(elevation::NO_ACCOUNT_TEXT.into(), CardRole::Normal)],
                    false,
                    now,
                );
                return Vec::new();
            }
            ElevatedEnd::Unavailable(why) => {
                self.show_toast(
                    vec![(
                        format!("Couldn't delete as administrator: {why}. Nothing was deleted."),
                        CardRole::Normal,
                    )],
                    false,
                    now,
                );
                return Vec::new();
            }
            ElevatedEnd::Done(items) => {
                outcomes = (0..targets.len()).map(|i| items.get(i).cloned()).collect();
            }
            ElevatedEnd::Failed { message, completed } => {
                let mut slots: Vec<Option<ElevatedItem>> = vec![None; targets.len()];
                for (i, item) in completed {
                    if let Some(slot) = slots.get_mut(i) {
                        *slot = Some(item);
                    }
                }
                outcomes = slots;
                broke_off = Some(message);
            }
        }
        let mut changes = Vec::new();
        let (mut gone, mut gone_n, mut shared) = (0u64, 0usize, 0u64);
        let (mut lower, mut est) = (false, false);
        let mut problems: Vec<String> = Vec::new();
        let mut unknown = 0usize;
        for (target, outcome) in targets.iter().zip(outcomes) {
            match outcome {
                Some(ElevatedItem::Deleted {
                    bytes,
                    shared_files,
                }) => {
                    gone = gone.saturating_add(bytes);
                    gone_n += 1;
                    lower |= target.lower_bound;
                    est |= target.estimated;
                    shared = shared.saturating_add(shared_files);
                    if let Some(node) = target.node.filter(|_| same_map) {
                        changes.push(TreeChange {
                            node,
                            to: DeleteState::Deleted,
                        });
                    }
                }
                Some(ElevatedItem::Refused(why)) => {
                    problems.push(format!("“{}” wasn't touched: {why}.", target.name));
                }
                Some(ElevatedItem::Failed(why)) => {
                    problems.push(format!("Couldn't delete “{}”: {why}.", target.name));
                }
                None => unknown += 1,
            }
        }
        if gone_n > 0 {
            lines.push((
                format!("Deleted · {}", size_text(gone, lower, est)),
                CardRole::Normal,
            ));
        }
        if shared > 0 {
            lines.push((
                format!(
                    "{} also {} another name, so {} data stays on the drive.",
                    plural(shared as usize, "file", "files"),
                    if shared == 1 { "has" } else { "have" },
                    if shared == 1 { "its" } else { "their" }
                ),
                CardRole::Secondary,
            ));
        }
        for p in problems {
            lines.push((p, CardRole::Secondary));
        }
        if let Some(message) = broke_off {
            let text = if unknown > 0 {
                format!(
                    "The helper stopped before it finished ({message}). {} may or may not be deleted; rescan the folder to see what is left.",
                    plural(unknown, "item", "items")
                )
            } else {
                format!("The helper stopped after it had dealt with every item ({message}).")
            };
            lines.push((text, CardRole::Secondary));
        }
        if let Some(first) = lines.first_mut() {
            first.1 = CardRole::Normal;
            self.show_toast(lines, false, now);
        }
        let mut fx = Vec::new();
        if !changes.is_empty() {
            fx.push(ShellEffect::Tree(changes));
        }
        fx
    }

    // ---- undo -----------------------------------------------------------------

    /// The boxes the undo list can mark again, so the same items can be found
    /// in another tree of the same place when the full result replaces the map.
    pub fn undo_nodes(&self) -> Vec<NodeId> {
        self.acts
            .undo
            .iter()
            .flat_map(|g| g.entries.iter().filter_map(|e| e.node))
            .collect()
    }

    /// Points the undo list at the boxes of the map now on screen: `find`
    /// answers, for a box of the old map, the same box here. An item that
    /// isn't there keeps its entry (it can still be put back) but marks
    /// nothing.
    pub fn remap_undo(&mut self, find: &dyn Fn(NodeId) -> Option<NodeId>) {
        let scene = self.acts.scene;
        for group in &mut self.acts.undo {
            group.scene = scene;
            for entry in &mut group.entries {
                entry.node = entry.node.and_then(find);
            }
        }
    }

    /// Puts the last delete back, if it is still in the bin.
    pub(super) fn undo_last(&mut self, now: Duration) -> Vec<ShellEffect> {
        if self.offer == Offer::Running {
            self.say(format!("Undo: {}", reason::FULL_SCAN_RUNNING), now);
            return Vec::new();
        }
        if self.busy() {
            self.say("Undo: wait for the last action to finish", now);
            return Vec::new();
        }
        let Some(group) = self.acts.undo.pop() else {
            self.say("Nothing to undo", now);
            return Vec::new();
        };
        self.close_toast(now);
        let tokens: Vec<Token> = group.entries.iter().map(|e| e.token.clone()).collect();
        let entries = group
            .entries
            .into_iter()
            .map(|e| (e.node, e.name))
            .collect();
        // The boxes are only touched while the map they came from is up.
        self.dispatch_for(
            Work::Restore(tokens),
            Pending::Restore(entries),
            group.scene,
        )
    }

    fn restore_done(
        &mut self,
        entries: &[(Option<NodeId>, String)],
        outcomes: Vec<RestoreOutcome>,
        scene: u64,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let bin = self.bin();
        let same_map = scene == self.acts.scene;
        let mut changes = Vec::new();
        let mut back = Vec::new();
        let mut problems = Vec::new();
        for ((node, name), outcome) in entries.iter().zip(outcomes) {
            match outcome {
                RestoreOutcome::Restored { .. } => {
                    back.push(name.clone());
                    if let Some(node) = node.filter(|_| same_map) {
                        changes.push(TreeChange {
                            node,
                            to: DeleteState::None,
                        });
                    }
                }
                RestoreOutcome::Refused { reason } => problems.push(format!(
                    "Couldn't put “{name}” back: {}.",
                    match reason {
                        RefuseReason::NotInBin => format!("it is no longer in the {bin}"),
                        RefuseReason::OriginalExists => "something is already there".to_string(),
                        RefuseReason::OriginalFolderMissing => "its folder is gone".to_string(),
                        other => refuse_text(&other),
                    }
                )),
                RestoreOutcome::Failed { error } => problems.push(format!(
                    "Couldn't put “{name}” back: {}.",
                    error_text(&error)
                )),
                _ => {}
            }
        }
        let mut lines: Vec<(String, CardRole)> = Vec::new();
        match back.as_slice() {
            [] => {}
            [one] => lines.push((format!("Put “{one}” back"), CardRole::Normal)),
            many => lines.push((format!("Put {} items back", many.len()), CardRole::Normal)),
        }
        for p in problems {
            lines.push((p, CardRole::Secondary));
        }
        if let Some(first) = lines.first_mut() {
            first.1 = CardRole::Normal;
            self.show_toast(lines, false, now);
        }
        let mut fx = Vec::new();
        if !changes.is_empty() {
            fx.push(ShellEffect::Tree(changes));
        }
        fx.push(ShellEffect::BinChanged);
        fx
    }

    // ---- emptying the bin -------------------------------------------------------

    /// What the empty action is called and its dialog title.
    fn empty_words(&self, bytes: u64) -> (String, String) {
        let bin = self.bin();
        let place = match &self.acts.bin_drive {
            Some(d) if !d.label.is_empty() => d.label.clone(),
            _ => "this drive".to_string(),
        };
        (
            format!("Empty {bin} on {place}"),
            format!(
                "Empty {bin} on {place} ({})?",
                bs_layout::format_size(bytes)
            ),
        )
    }

    /// The bin block or the bar's bin segment was clicked.
    pub fn request_empty(&mut self, now: Duration) -> Vec<ShellEffect> {
        let bin = self.bin();
        if self.acts.disposer.is_none() || self.acts.bin_drive.is_none() {
            self.say(format!("Empty {bin}: not available here"), now);
            return Vec::new();
        }
        if self.offer == Offer::Running {
            self.say(format!("Empty {bin}: {}", reason::FULL_SCAN_RUNNING), now);
            return Vec::new();
        }
        if self.busy() {
            self.say(
                format!("Empty {bin}: wait for the last action to finish"),
                now,
            );
            return Vec::new();
        }
        let bytes = self.capacity.breakdown().map_or(0, |b| b.recycle_bin);
        if bytes == 0 {
            self.say(format!("The {bin} on this drive is empty"), now);
            return Vec::new();
        }
        let (action, title) = self.empty_words(bytes);
        let spec = DialogSpec {
            lead: None,
            title,
            lines: vec![(
                "Undo won't be available for these items.".into(),
                CardRole::Secondary,
            )],
            pinned: Vec::new(),
            rows: 0,
            buttons: vec![
                Button {
                    label: action,
                    hold: false,
                },
                Button {
                    label: "Cancel".into(),
                    hold: false,
                },
            ],
        };
        self.open_dialog(Confirm::Empty, spec);
        Vec::new()
    }

    fn empty_done(
        &mut self,
        outcome: EmptyOutcome,
        emptied: &Path,
        now: Duration,
    ) -> Vec<ShellEffect> {
        let bin = self.bin();
        let (words, _) = self.empty_words(0);
        let words = words.replacen("Empty", "Emptied", 1);
        let mut fx = Vec::new();
        let line = |s: String| vec![(s, CardRole::Normal)];
        let (lines, cleared) = match outcome {
            EmptyOutcome::Emptied { bytes, .. } => (
                line(if bytes > 0 {
                    format!(
                        "{words} · {} given back to the drive",
                        bs_layout::format_size(bytes)
                    )
                } else {
                    words
                }),
                true,
            ),
            EmptyOutcome::Partial { bytes, error, .. } => (
                vec![
                    (
                        format!(
                            "Emptied part of the {bin} · {} given back to the drive",
                            bs_layout::format_size(bytes)
                        ),
                        CardRole::Normal,
                    ),
                    (
                        format!("Couldn't empty the rest: {}.", error_text(&error)),
                        CardRole::Secondary,
                    ),
                ],
                true,
            ),
            EmptyOutcome::Refused { reason } => (
                line(match reason {
                    RefuseReason::NoBinOnDrive => format!("This drive has no {bin} to empty"),
                    other => format!("Couldn't empty the {bin}: {}", refuse_text(&other)),
                }),
                false,
            ),
            EmptyOutcome::Failed { error } => (
                line(format!("Couldn't empty the {bin}: {}.", error_text(&error))),
                false,
            ),
            _ => return fx,
        };
        if cleared {
            // What was in the bin is gone for good and can't be put back.
            // Only this drive's bin was emptied: what went to another
            // drive's bin can still be put back.
            let mut changes = Vec::new();
            let (gone, kept): (Vec<UndoGroup>, Vec<UndoGroup>) =
                std::mem::take(&mut self.acts.undo)
                    .into_iter()
                    .partition(|g| g.mount.as_deref() == Some(emptied));
            self.acts.undo = kept;
            for group in gone {
                for entry in group.entries {
                    if let Some(node) = entry.node.filter(|_| group.scene == self.acts.scene) {
                        changes.push(TreeChange {
                            node,
                            to: DeleteState::Deleted,
                        });
                    }
                }
            }
            if !changes.is_empty() {
                fx.push(ShellEffect::Tree(changes));
            }
        }
        self.show_toast(lines, false, now);
        fx.push(ShellEffect::BinChanged);
        fx
    }

    // ---- dialogs ----------------------------------------------------------------

    fn open_dialog(&mut self, confirm: Confirm, spec: DialogSpec) {
        self.open_dialog_at(confirm, spec, 1);
    }

    /// Opens a dialog with the keyboard on button `focus`.
    fn open_dialog_at(&mut self, confirm: Confirm, spec: DialogSpec, focus: usize) {
        self.hide_card();
        self.card.rest = None;
        let built = popup::dialog(
            &spec,
            self.window_rect(),
            self.metrics(),
            DialogState {
                focus,
                progress: 0.0,
            },
        );
        self.acts.modal = Some(Modal::Dialog(Box::new(DialogUi {
            confirm,
            list: None,
            spec,
            focus,
            hold_since: None,
            built,
        })));
    }

    /// Builds the open dialog or menu again: after a resize, a focus change or
    /// as a hold fills.
    pub(super) fn rebuild_modal(&mut self, now: Duration) {
        self.refit_names();
        let window = self.window_rect();
        let built = match self.acts.modal.as_ref() {
            Some(Modal::Dialog(d)) => {
                let progress = d.hold_since.map_or(0.0, |s| {
                    now.saturating_sub(s).as_secs_f32() / HOLD_FOR.as_secs_f32()
                });
                Some((
                    true,
                    popup::dialog(
                        &d.spec,
                        window,
                        self.metrics(),
                        DialogState {
                            focus: d.focus,
                            progress,
                        },
                    ),
                ))
            }
            Some(Modal::Menu(m)) => {
                let rows: Vec<MenuRow> = m.rows.iter().map(|r| r.row.clone()).collect();
                Some((
                    false,
                    popup::menu(&rows, m.at, self.map_rect(), self.metrics(), m.hot),
                ))
            }
            None => None,
        };
        match (built, self.acts.modal.as_mut()) {
            (Some((_, b)), Some(Modal::Dialog(d))) => d.built = b,
            (Some((_, b)), Some(Modal::Menu(m))) => m.built = b,
            _ => {}
        }
    }

    fn close_modal(&mut self) {
        self.acts.modal = None;
    }

    /// A menu does not survive a resize; a dialog is built again instead.
    pub(super) fn close_menu_if_open(&mut self) {
        if matches!(self.acts.modal, Some(Modal::Menu(_))) {
            self.acts.modal = None;
        }
    }

    fn confirm_dialog(&mut self, now: Duration) -> Vec<ShellEffect> {
        let Some(Modal::Dialog(d)) = self.acts.modal.take() else {
            return Vec::new();
        };
        match d.confirm {
            Confirm::About | Confirm::Report(_) => Vec::new(),
            Confirm::Empty => {
                let Some(drive) = self.acts.bin_drive.clone() else {
                    return Vec::new();
                };
                self.dispatch(
                    Work::Empty(drive.mount.clone()),
                    Pending::Empty(drive.mount),
                )
            }
            Confirm::Delete {
                permanent,
                elevated,
                protected,
                items,
                targets,
            } => {
                let held = protected.clone();
                let confirmed =
                    (!protected.is_empty()).then(|| ProtectedConfirmation::for_paths(protected));
                if elevated {
                    self.dispatch(Work::Elevated { items, held }, Pending::Elevated(targets))
                } else if permanent {
                    self.dispatch(
                        Work::Permanent { items, confirmed },
                        Pending::Permanent(targets),
                    )
                } else {
                    self.start_trash(items, confirmed, targets, now)
                }
            }
        }
    }

    /// Input while a dialog or menu is open: all of it belongs to the modal.
    pub(super) fn modal_input(&mut self, event: InputEvent, now: Duration) -> Vec<ShellEffect> {
        let fx = match event {
            InputEvent::Key(key) => self.modal_key(key, now),
            InputEvent::KeyReleased(Key::Enter | Key::Space) => {
                self.cancel_hold();
                Vec::new()
            }
            InputEvent::PointerMoved { x, y } => {
                self.modal_hover(x, y);
                Vec::new()
            }
            InputEvent::PointerDown { x, y } => self.modal_down(x, y, now),
            InputEvent::PointerUp { .. } => {
                self.cancel_hold();
                Vec::new()
            }
            InputEvent::Click { x, y } | InputEvent::ClickWith { x, y, .. } => {
                self.modal_click(x, y, now)
            }
            InputEvent::Wheel { steps, .. } | InputEvent::Touchpad { steps, .. } => {
                // The crash report and a long list of names scroll; everything
                // else ignores the wheel.
                self.scroll_report(-(steps * 3.0).round() as i64);
                self.scroll_names(-(steps * 3.0).round() as i64);
                Vec::new()
            }
            InputEvent::RightClick { .. } => {
                // A second menu replaces the first; a dialog ignores it.
                if matches!(self.acts.modal, Some(Modal::Menu(_))) {
                    self.close_modal();
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.rebuild_modal(now);
        fx
    }

    fn cancel_hold(&mut self) {
        if let Some(Modal::Dialog(d)) = self.acts.modal.as_mut() {
            d.hold_since = None;
        }
    }

    fn modal_hover(&mut self, x: f32, y: f32) {
        match self.acts.modal.as_mut() {
            Some(Modal::Menu(m)) => {
                let hit = m.built.hit(x, y);
                if hit.is_some() {
                    m.hot = hit;
                } else if !m.built.popup.card.rect.contains(x, y) {
                    m.hot = None;
                }
            }
            Some(Modal::Dialog(d)) => {
                if let Some(i) = d.built.hit(x, y)
                    && d.hold_since.is_none()
                {
                    d.focus = i;
                }
            }
            None => {}
        }
    }

    fn modal_down(&mut self, x: f32, y: f32, now: Duration) -> Vec<ShellEffect> {
        if let Some(Modal::Dialog(d)) = self.acts.modal.as_mut()
            && let Some(i) = d.built.hit(x, y)
        {
            d.focus = i;
            if d.spec.buttons.get(i).is_some_and(|b| b.hold) {
                d.hold_since = Some(now);
            }
        }
        Vec::new()
    }

    fn modal_click(&mut self, x: f32, y: f32, now: Duration) -> Vec<ShellEffect> {
        match self.acts.modal.as_ref() {
            Some(Modal::Menu(m)) => match m.built.hit(x, y) {
                Some(i) => self.menu_activate(i, now),
                None => {
                    // A click away from the menu closes it and does nothing
                    // else.
                    if !m.built.popup.card.rect.contains(x, y) {
                        self.close_modal();
                    }
                    Vec::new()
                }
            },
            Some(Modal::Dialog(d)) => match d.built.hit(x, y) {
                Some(i) => self.dialog_activate(i, now),
                None => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    fn dialog_activate(&mut self, button: usize, now: Duration) -> Vec<ShellEffect> {
        let Some(Modal::Dialog(d)) = self.acts.modal.as_ref() else {
            return Vec::new();
        };
        let Some(b) = d.spec.buttons.get(button) else {
            return Vec::new();
        };
        match d.confirm {
            Confirm::About => return self.about_press(button),
            Confirm::Report(_) => return self.report_press(button),
            _ => {}
        }
        if b.hold {
            // Only holding confirms; a click or a press does not.
            return Vec::new();
        }
        if button == 0 {
            self.confirm_dialog(now)
        } else {
            self.close_modal();
            Vec::new()
        }
    }

    fn modal_key(&mut self, key: Key, now: Duration) -> Vec<ShellEffect> {
        match self.acts.modal.as_mut() {
            Some(Modal::Dialog(d)) => {
                let n = d.spec.buttons.len().max(1);
                let long_list = match (&d.confirm, &d.list) {
                    (Confirm::Delete { targets, .. }, Some(l)) => targets.len() > l.rows,
                    _ => false,
                };
                if matches!(d.confirm, Confirm::Report(_)) || long_list {
                    // Up and Down move through the report or the names; Left,
                    // Right and Tab move between the buttons.
                    match key {
                        Key::Arrow(Direction::Up) => {
                            self.scroll_report(-1);
                            self.scroll_names(-1);
                            return Vec::new();
                        }
                        Key::Arrow(Direction::Down) => {
                            self.scroll_report(1);
                            self.scroll_names(1);
                            return Vec::new();
                        }
                        Key::Home => {
                            self.scroll_report(i64::MIN / 2);
                            self.scroll_names(i64::MIN / 2);
                            return Vec::new();
                        }
                        _ => {}
                    }
                }
                let Some(Modal::Dialog(d)) = self.acts.modal.as_mut() else {
                    return Vec::new();
                };
                match key {
                    Key::Escape => {
                        self.close_modal();
                    }
                    Key::Tab | Key::Arrow(Direction::Right | Direction::Down) => {
                        d.focus = (d.focus + 1) % n;
                        d.hold_since = None;
                    }
                    Key::BackTab | Key::Arrow(Direction::Left | Direction::Up) => {
                        d.focus = (d.focus + n - 1) % n;
                        d.hold_since = None;
                    }
                    Key::Enter | Key::Space => {
                        let focus = d.focus;
                        if d.spec.buttons.get(focus).is_some_and(|b| b.hold) {
                            // Key repeat must not restart the hold.
                            d.hold_since.get_or_insert(now);
                        } else {
                            return self.dialog_activate(focus, now);
                        }
                    }
                    _ => {}
                }
                Vec::new()
            }
            Some(Modal::Menu(m)) => {
                let enabled: Vec<usize> = m
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.row.enabled && !r.row.separator)
                    .map(|(i, _)| i)
                    .collect();
                let pos = m.hot.and_then(|h| enabled.iter().position(|&i| i == h));
                match key {
                    Key::Escape => self.close_modal(),
                    Key::Arrow(Direction::Down) | Key::Tab if !enabled.is_empty() => {
                        m.hot = Some(enabled[pos.map_or(0, |p| (p + 1) % enabled.len())]);
                    }
                    Key::Arrow(Direction::Up) | Key::BackTab if !enabled.is_empty() => {
                        m.hot = Some(
                            enabled[pos.map_or(enabled.len() - 1, |p| {
                                (p + enabled.len() - 1) % enabled.len()
                            })],
                        );
                    }
                    Key::Enter | Key::Space => {
                        if let Some(h) = m.hot.filter(|h| enabled.contains(h)) {
                            return self.menu_activate(h, now);
                        }
                    }
                    _ => {}
                }
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Time passes: a hold may complete, and the toast may run out.
    pub(super) fn actions_tick(&mut self, now: Duration) -> bool {
        let mut changed = false;
        let mut confirm = false;
        if let Some(Modal::Dialog(d)) = self.acts.modal.as_ref()
            && let Some(since) = d.hold_since
        {
            if now.saturating_sub(since) >= HOLD_FOR {
                confirm = true;
            } else {
                changed = true;
            }
        }
        if confirm {
            let fx = self.confirm_dialog(now);
            self.pending_fx.extend(fx);
            changed = true;
        }
        if self.acts.modal.is_some() {
            self.rebuild_modal(now);
        }
        let reduced = self.reduced_motion;
        let mut close = false;
        if let Some(t) = self.acts.toast.as_mut() {
            match t.closing {
                Some(at) => {
                    if reduced || now.saturating_sub(at) >= TOAST_MOTION {
                        close = true;
                    }
                    changed = true;
                }
                None => {
                    if t.hovered {
                        t.until = now + TOAST_FOR;
                    } else if now >= t.until {
                        t.closing = Some(now);
                        if reduced {
                            close = true;
                        }
                        changed = true;
                    }
                    if now.saturating_sub(t.shown) < TOAST_MOTION {
                        changed = true;
                    }
                }
            }
        }
        if close {
            self.acts.toast = None;
            self.acts.toast_focus = false;
        }
        changed
    }

    /// When the action layer next needs a tick.
    pub(super) fn actions_deadline(&self, now: Duration) -> Option<Duration> {
        let frame = Duration::from_millis(16);
        let mut soonest: Option<Duration> = None;
        let mut at = |d: Duration| soonest = Some(soonest.map_or(d, |s| s.min(d)));
        if let Some(Modal::Dialog(d)) = self.acts.modal.as_ref()
            && d.hold_since.is_some()
        {
            at(frame);
        }
        if let Some(t) = self.acts.toast.as_ref() {
            if t.closing.is_some() || now.saturating_sub(t.shown) < TOAST_MOTION {
                at(frame);
            } else if !t.hovered {
                at(t.until.saturating_sub(now));
            } else {
                at(Duration::from_millis(250));
            }
        }
        soonest
    }

    // ---- the context menu ----------------------------------------------------

    /// Why `action` can't be done to `ids` right now, apart from what the
    /// platform says about deleting (that arrives later, see
    /// [`Shell::menu_checked`]).
    fn why_not_now(&self, action: Action, view: ViewId, ids: &[u32]) -> Option<String> {
        if ids.len() > 1
            && matches!(
                action,
                Action::Open
                    | Action::ShowInFileManager
                    | Action::OpenTerminal
                    | Action::Properties
                    | Action::ZoomInto
                    | Action::Rescan
                    | Action::Handoff(_)
                    | Action::OpenInSoftware
                    | Action::CopyRemovalCommand
            )
        {
            return Some(SELECT_ONE.to_string());
        }
        if let Some(why) = self.refusal(action, view, ids) {
            return Some(why.to_string());
        }
        None
    }

    /// The rows of the menu. `checked` holds the platform's answers for
    /// moving to the bin and for deleting for good; until they arrive the two
    /// delete rows wait.
    fn menu_rows(
        &self,
        view: ViewId,
        ids: &[u32],
        checked: Option<&(Plan, Plan)>,
    ) -> Vec<RowModel> {
        let handoffs = (ids.len() == 1)
            .then(|| self.card_data(view, ids[0], false))
            .flatten()
            .map(|d| card::handoff_actions(&d))
            .unwrap_or_default();
        let mut spec: Vec<(Option<Action>, &str)> = vec![
            (Some(Action::Open), "Enter"),
            (Some(Action::ShowInFileManager), ""),
            (Some(Action::OpenTerminal), ""),
            (Some(Action::CopyPath), "Ctrl+C"),
        ];
        if self.acts.caps.properties {
            spec.push((Some(Action::Properties), ""));
        }
        spec.push((None, ""));
        spec.push((Some(Action::ZoomInto), "Enter"));
        spec.push((Some(Action::Rescan), "Shift+F5"));
        if !handoffs.is_empty() {
            spec.push((None, ""));
            for h in handoffs {
                spec.push((Some(h), ""));
            }
        }
        spec.push((None, ""));
        spec.push((Some(Action::Delete), "Del"));
        spec.push((Some(Action::DeletePermanently), "Shift+Del"));
        spec.into_iter()
            .map(|(action, shortcut)| match action {
                None => RowModel {
                    row: MenuRow {
                        label: String::new(),
                        hint: String::new(),
                        enabled: false,
                        separator: true,
                    },
                    action: None,
                },
                Some(a) => {
                    let mut why = self.why_not_now(a, view, ids);
                    if why.is_none() && matches!(a, Action::Delete | Action::DeletePermanently) {
                        why = match (self.acts.disposer.as_ref(), checked) {
                            (None, _) => None,
                            (Some(_), None) => Some(CHECKING.to_string()),
                            (Some(_), Some((trash, perm))) => {
                                refusal_text(if a == Action::Delete { trash } else { perm })
                            }
                        };
                    }
                    RowModel {
                        row: MenuRow {
                            label: a.label().to_string(),
                            hint: why.clone().unwrap_or_else(|| shortcut.to_string()),
                            enabled: why.is_none(),
                            separator: false,
                        },
                        action: Some(a),
                    }
                }
            })
            .collect()
    }

    /// Opens the menu for the selection with its corner at `at`. It opens at
    /// once; the delete rows read "Checking…" until the platform answers, and
    /// the returned effects start that question.
    pub(super) fn open_menu(&mut self, at: (f32, f32), now: Duration) -> Vec<ShellEffect> {
        let (view, ids) = self.selection();
        if ids.is_empty() {
            return Vec::new();
        }
        self.hide_card();
        self.card.rest = None;
        let rows = self.menu_rows(view, &ids, None);
        let first = rows.iter().position(|r| r.row.enabled);
        let plain: Vec<MenuRow> = rows.iter().map(|r| r.row.clone()).collect();
        let built = popup::menu(&plain, at, self.map_rect(), self.metrics(), first);
        self.acts.menu_seq += 1;
        let seq = self.acts.menu_seq;
        let waiting = rows.iter().any(|r| r.row.hint == CHECKING);
        self.acts.modal = Some(Modal::Menu(MenuUi {
            seq,
            rows,
            view,
            ids: ids.clone(),
            at,
            hot: first,
            built,
        }));
        let _ = now;
        if !waiting {
            return Vec::new();
        }
        let Some(targets) = self.targets(view, &ids) else {
            return Vec::new();
        };
        let items = self.items_for(&targets);
        self.start_menu_check(items, seq)
    }

    /// Whether the menu with this number is the one open now.
    fn menu_is_open(&self, seq: u64) -> bool {
        matches!(&self.acts.modal, Some(Modal::Menu(m)) if m.seq == seq)
    }

    /// Asks the platform about the delete rows of menu `seq`. At most one such
    /// question runs at a time, because on some systems each one walks the
    /// folder; a newer one replaces any older one that is still waiting.
    fn start_menu_check(&mut self, items: Vec<DItem>, seq: u64) -> Vec<ShellEffect> {
        let running = self
            .acts
            .pending
            .values()
            .any(|s| matches!(s.what, Pending::Menu(_)));
        if running {
            self.acts.menu_waiting = Some((items, seq));
            return Vec::new();
        }
        self.dispatch(Work::CheckMenu { items }, Pending::Menu(seq))
    }

    /// The platform answered for the open menu: the delete rows show what it
    /// said.
    fn menu_checked(&mut self, seq: u64, plans: (Plan, Plan)) {
        let (view, ids) = match self.acts.modal.as_ref() {
            Some(Modal::Menu(m)) if m.seq == seq => (m.view, m.ids.clone()),
            _ => return,
        };
        let rows = self.menu_rows(view, &ids, Some(&plans));
        if let Some(Modal::Menu(m)) = self.acts.modal.as_mut() {
            m.rows = rows;
        }
        self.rebuild_modal(self.clock);
    }

    /// Shift+F10 or the menu key: the menu opens beside the selected box.
    pub(super) fn open_menu_for_selection(&mut self, now: Duration) -> Vec<ShellEffect> {
        let at = self
            .app
            .as_ref()
            .and_then(|a| a.selected().and_then(|id| a.item(id)))
            .map(|i| {
                (
                    i.rect.x + 8.0 * self.scale,
                    i.rect.y + i.rect.h.min(24.0 * self.scale),
                )
            });
        match at {
            Some(at) => self.open_menu(at, now),
            None => {
                // Nothing is selected: the menu of the program itself.
                let map = self.map_rect();
                self.open_app_menu((map.x + 8.0 * self.scale, map.y + 8.0 * self.scale))
            }
        }
    }

    /// The menu of the program itself, for a click on nothing in particular:
    /// the About box and the releases page.
    pub(super) fn open_app_menu(&mut self, at: (f32, f32)) -> Vec<ShellEffect> {
        self.hide_card();
        self.card.rest = None;
        let entries: [(Action, &str); 2] = [(Action::About, "F1"), (Action::CheckForUpdates, "")];
        let rows: Vec<RowModel> = entries
            .iter()
            .map(|(a, hint)| RowModel {
                row: MenuRow {
                    label: a.label().to_string(),
                    hint: (*hint).to_string(),
                    enabled: true,
                    separator: false,
                },
                action: Some(*a),
            })
            .collect();
        let plain: Vec<MenuRow> = rows.iter().map(|r| r.row.clone()).collect();
        let built = popup::menu(&plain, at, self.map_rect(), self.metrics(), Some(0));
        self.acts.menu_seq += 1;
        let seq = self.acts.menu_seq;
        self.acts.modal = Some(Modal::Menu(MenuUi {
            seq,
            rows,
            view: 0,
            ids: Vec::new(),
            at,
            hot: Some(0),
            built,
        }));
        Vec::new()
    }

    /// A right click on the map or the chrome.
    pub(super) fn right_click(&mut self, x: f32, y: f32, now: Duration) -> Vec<ShellEffect> {
        let over_map = !self.geometry().in_chrome(x, y);
        if self.over_card(x, y) {
            return Vec::new();
        }
        let item = if over_map {
            self.app.as_ref().and_then(|a| a.item_at(x, y))
        } else {
            None
        };
        let Some(id) = item else {
            return self.open_app_menu((x, y));
        };
        let mut fx = Vec::new();
        let selected = self
            .app
            .as_ref()
            .is_some_and(|a| a.selection().contains(&id));
        if !selected {
            fx.extend(self.map_event(InputEvent::Click { x, y }, now));
        }
        fx.extend(self.open_menu((x, y), now));
        fx
    }

    fn menu_activate(&mut self, row: usize, now: Duration) -> Vec<ShellEffect> {
        let Some(Modal::Menu(m)) = self.acts.modal.as_ref() else {
            return Vec::new();
        };
        let Some(r) = m.rows.get(row) else {
            return Vec::new();
        };
        let (Some(action), true) = (r.action, r.row.enabled) else {
            return Vec::new();
        };
        let (view, ids) = (m.view, m.ids.clone());
        self.close_modal();
        self.run_action(action, view, ids, now)
    }

    // ---- the bin block and the bar -------------------------------------------

    /// A click on the capacity bar: the bin segment empties the bin.
    pub(super) fn capacity_click(&mut self, x: f32, now: Duration) -> Vec<ShellEffect> {
        let parts = self.capacity.parts_at(now).0;
        let rect = self.geometry().capacity;
        let start = rect.x + parts[0] * rect.w;
        let end = start + parts[1] * rect.w;
        if parts[1] > 0.0 && x >= start && x < end {
            self.request_empty(now)
        } else {
            Vec::new()
        }
    }
}

// ---- About and the crash report ------------------------------------------------

impl<S: ViewSource> Shell<S> {
    /// Tells the About box what this build is.
    pub fn set_about(&mut self, about: crate::notice::AboutInfo) {
        self.about = about;
    }

    /// Replaces the clock the About box reads the date from (Unix seconds).
    pub fn set_unix_clock(&mut self, clock: Box<dyn Fn() -> i64>) {
        self.unix_clock = clock;
    }

    /// Opens the About box: the version and build date, how old the build is
    /// when it is old, and the two links.
    pub(super) fn open_about(&mut self) -> Vec<ShellEffect> {
        let now = (self.unix_clock)();
        let about = self.about.clone();
        let mut lines: Vec<(String, CardRole)> = vec![(about.version_line(), CardRole::Normal)];
        if !about.target.is_empty() {
            lines.push((about.target.clone(), CardRole::Secondary));
        }
        if let Some(age) = about.age_line(now) {
            lines.push((age, CardRole::Normal));
        }
        lines.push((
            "Bytescape never connects to the internet. Links open in your browser.".into(),
            CardRole::Secondary,
        ));
        lines.push((
            "© Recursive AI, LLC · MIT OR Apache-2.0".into(),
            CardRole::Secondary,
        ));
        let spec = DialogSpec {
            lead: None,
            title: "About Bytescape".into(),
            lines,
            pinned: Vec::new(),
            rows: 0,
            buttons: vec![
                Button {
                    label: crate::chrome::LINK_TEXT.into(),
                    hold: false,
                },
                Button {
                    label: Action::CheckForUpdates.label().into(),
                    hold: false,
                },
                Button {
                    label: "Close".into(),
                    hold: false,
                },
            ],
        };
        self.open_dialog_at(Confirm::About, spec, 2);
        Vec::new()
    }

    pub(super) fn about_press(&mut self, button: usize) -> Vec<ShellEffect> {
        match button {
            0 => vec![ShellEffect::OpenLink(crate::chrome::LINK_URL)],
            1 => vec![ShellEffect::OpenLink(RELEASES_URL)],
            _ => {
                self.close_modal();
                Vec::new()
            }
        }
    }

    /// Opens the crash report with paths hidden.
    pub(super) fn open_report(&mut self) {
        let Some(notice) = self.crash.as_ref().map(|c| c.notice.clone()) else {
            return;
        };
        let line_h = (self.font_px * self.scale * 1.5).round().max(1.0);
        let rows = ((self.height as f32 * 0.5) / line_h)
            .floor()
            .clamp(6.0, 40.0) as usize;
        let view = ReportView {
            notice,
            show_paths: false,
            offset: 0,
            rows,
        };
        let spec = report_spec(&view);
        self.open_dialog_at(Confirm::Report(view), spec, 2);
    }

    pub(super) fn report_press(&mut self, button: usize) -> Vec<ShellEffect> {
        let Some(Modal::Dialog(d)) = self.acts.modal.as_mut() else {
            return Vec::new();
        };
        let Confirm::Report(view) = &mut d.confirm else {
            return Vec::new();
        };
        match button {
            0 => {
                view.show_paths = !view.show_paths;
                view.offset = 0;
                d.spec = report_spec(view);
                Vec::new()
            }
            1 => vec![ShellEffect::OpenUrl(view.notice.issue_url().to_string())],
            _ => {
                self.close_modal();
                Vec::new()
            }
        }
    }

    /// Moves the crash report up or down by `by` lines.
    fn scroll_report(&mut self, by: i64) {
        let Some(Modal::Dialog(d)) = self.acts.modal.as_mut() else {
            return;
        };
        let Confirm::Report(view) = &mut d.confirm else {
            return;
        };
        let total = view.notice.lines(view.show_paths).len();
        let last = total.saturating_sub(view.rows) as i64;
        let to = (view.offset as i64).saturating_add(by).clamp(0, last);
        let to = to as usize;
        if to != view.offset {
            view.offset = to;
            d.spec = report_spec(view);
        }
    }

    /// How many names an administrator delete dialog lists at once: what fits
    /// in about half the window, after the lines that stay on screen.
    fn name_rows(&self) -> usize {
        let line_h = (self.font_px * self.scale * 1.5).round().max(1.0);
        let lines = ((self.height as f32 * 0.6) / line_h).floor() as usize;
        lines.saturating_sub(10).clamp(3, 40)
    }

    /// Moves the names of an administrator delete dialog up or down by `by`.
    fn scroll_names(&mut self, by: i64) {
        let Some(Modal::Dialog(mut d)) = self.acts.modal.take() else {
            return;
        };
        {
            let d = &mut *d;
            if let (
                Confirm::Delete {
                    targets, protected, ..
                },
                Some(list),
            ) = (&d.confirm, d.list.as_mut())
            {
                let last = targets.len().saturating_sub(list.rows) as i64;
                let to = (list.offset as i64).saturating_add(by).clamp(0, last) as usize;
                if to != list.offset {
                    list.offset = to;
                    d.spec = self.fitted_admin_dialog(targets, !protected.is_empty(), list);
                }
            }
        }
        self.acts.modal = Some(Modal::Dialog(d));
    }

    /// The words of the open crash report viewer, as one text (for tests).
    pub fn report_text(&self) -> Option<String> {
        match self.acts.modal.as_ref()? {
            Modal::Dialog(d) if matches!(d.confirm, Confirm::Report(_)) => self.dialog_text(),
            _ => None,
        }
    }

    /// What the About box and the app menu do that is not about a box.
    pub(super) fn app_action(&mut self, action: Action) -> Option<Vec<ShellEffect>> {
        match action {
            Action::About => Some(self.open_about()),
            Action::CheckForUpdates => Some(vec![ShellEffect::OpenLink(RELEASES_URL)]),
            _ => None,
        }
    }
}

/// The crash report viewer as a dialog: a few lines of the report, where they
/// are in it, and the buttons.
fn report_spec(view: &ReportView) -> DialogSpec {
    let all = view.notice.lines(view.show_paths);
    let first = view.offset.min(all.len());
    let shown: Vec<&String> = all.iter().skip(first).take(view.rows).collect();
    let mut lines: Vec<(String, CardRole)> = Vec::new();
    lines.push((
        if view.show_paths {
            "Paths are shown. Hide them before you share this report.".to_string()
        } else {
            "This report is kept on this computer. Paths are hidden. Nothing was sent anywhere."
                .to_string()
        },
        CardRole::Secondary,
    ));
    for l in &shown {
        lines.push(((*l).clone(), CardRole::Normal));
    }
    let to = first + shown.len();
    lines.push((
        if all.is_empty() {
            "The report is empty.".to_string()
        } else {
            format!(
                "Lines {}–{} of {} · scroll with the wheel or the arrow keys",
                first + 1,
                to,
                all.len()
            )
        },
        CardRole::Secondary,
    ));
    let pinned = vec![0, lines.len() - 1];
    DialogSpec {
        lead: None,
        title: "Crash report".into(),
        lines,
        pinned,
        rows: view.rows + 3,
        buttons: vec![
            Button {
                label: if view.show_paths {
                    "Hide paths".into()
                } else {
                    "Show paths".into()
                },
                hold: false,
            },
            Button {
                label: "Report on GitHub".into(),
                hold: false,
            },
            Button {
                label: "Close".into(),
                hold: false,
            },
        ],
    }
}

// ---- words -------------------------------------------------------------------

/// Why the platform won't touch the selection at all, or `None` when it may
/// go ahead (possibly after a confirmation).
fn refusal_text(plan: &Plan) -> Option<String> {
    if plan.strictest == Level::Blocked {
        return Some(reason::OPERATING_SYSTEM.to_string());
    }
    for e in &plan.entries {
        if let Decision::Refused(why) = &e.decision {
            match why {
                RefuseReason::Blocked { .. } => return Some(reason::OPERATING_SYSTEM.to_string()),
                RefuseReason::ProtectedNotConfirmed { .. }
                | RefuseReason::SelectionRefused { .. } => {}
                other => return Some(refuse_text(other)),
            }
        }
    }
    None
}

fn refuse_text(why: &RefuseReason) -> String {
    match why {
        RefuseReason::Blocked { .. } => reason::OPERATING_SYSTEM.to_string(),
        RefuseReason::ProtectedNotConfirmed { .. } => "needs the hold-to-confirm".to_string(),
        RefuseReason::SelectionRefused { .. } => "another selected item is refused".to_string(),
        RefuseReason::ChangedSinceScan => {
            "it changed since the scan; rescan the folder first".to_string()
        }
        RefuseReason::NotFound => "it is already gone".to_string(),
        RefuseReason::Overlaps { .. } => "one selected item is inside another".to_string(),
        RefuseReason::MountPoint => "it is the start of another drive".to_string(),
        RefuseReason::OutsideRestriction => "it is outside the allowed area".to_string(),
        RefuseReason::NoBinOnDrive => "this drive has no bin".to_string(),
        RefuseReason::NotInBin => "it is no longer in the bin".to_string(),
        RefuseReason::OriginalExists => "something is already there".to_string(),
        RefuseReason::OriginalFolderMissing => "its folder is gone".to_string(),
        _ => "it can't be checked".to_string(),
    }
}

fn error_text(e: &OpError) -> String {
    let base = match e.kind {
        std::io::ErrorKind::PermissionDenied => {
            "permission denied; it needs administrator rights".to_string()
        }
        std::io::ErrorKind::NotFound => "it is already gone".to_string(),
        _ => match e.code {
            Some(code) => format!("the system reported an error (code {code})"),
            None => "the system reported an error".to_string(),
        },
    };
    if e.removed_entries > 0 {
        format!(
            "{base}; {} entries were already removed, rescan the folder to see what is left",
            e.removed_entries
        )
    } else {
        base
    }
}
