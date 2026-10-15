//! Administrator scans and deletes as the interface presents them.
//!
//! The interface never runs with administrator rights. A full scan and a
//! delete that needs them are done by a short-lived helper that the platform
//! starts after the system's prompt. This module holds the parts of that which
//! are the interface's own and need no helper to test:
//!
//! - what is offered, and what is said where the offer would be ([`Offer`]);
//! - the race between the ordinary scan and the full one ([`Race`]);
//! - keeping the user's place when the full result replaces the map
//!   ([`Place`]);
//! - the words for what the helper did ([`ElevatedEnd`]).

use bs_platform::elevate::Unavailable;
use bs_tree::{DeleteState, NodeId};

use crate::tree::TreeSource;
use crate::{App, Effect};

/// The label on the button in the navigation bar.
pub const FULL_SCAN_LABEL: &str = "Full scan";

/// What the Unreadable block says about the gap, which depends on whether it
/// can still be read and how.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnreadableAs {
    /// Clicking it reads the drive with administrator rights.
    #[default]
    Grant,
    /// A full scan is running.
    Reading,
    /// Nothing can be granted (or it isn't known yet): the gap is just named.
    Plain,
    /// The full scan has run: what is left is the filesystem's own
    /// bookkeeping (journal, metadata), not something that could not be read.
    Overhead,
    /// This edition cannot ask for administrator rights.
    Edition,
    /// This computer cannot start the helper (the reason is in the info bar
    /// when the block is pressed).
    Unavailable,
}

/// Whether and how administrator rights are offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Not offered, and nothing to say about it. The helper isn't known to be
    /// usable.
    Hidden,
    /// The helper can be started, but this map is a folder, so there is no
    /// whole drive for a full scan to read. There is no button; a delete that
    /// needs administrator rights can still go through the helper.
    DeletesOnly,
    /// The "Full scan" button works.
    Available,
    /// A full scan is running.
    Running,
    /// The full scan has been done for this map.
    Done,
    /// Not possible in this edition; the text says why, where the offer would
    /// have been.
    Explained(String),
    /// Not possible on this computer; the text is the platform's own
    /// explanation, shown where the offer would have been.
    Unavailable(String),
}

impl Offer {
    /// The offer for what the platform said about elevation: the platform's
    /// explanation is shown where the offer would be. The two reasons about
    /// the edition the program came in point at the full version. A process
    /// that is itself elevated never gets here (the program refuses to start).
    pub fn for_unavailable(why: Unavailable) -> Offer {
        match why {
            Unavailable::AppImage | Unavailable::SnapConfined => {
                Offer::Explained(why.explanation().to_string())
            }
            Unavailable::AlreadyElevated => Offer::Hidden,
            other => Offer::Unavailable(other.explanation().to_string()),
        }
    }

    /// The offer for an edition that never asks for administrator rights.
    pub fn walk_only_edition() -> Offer {
        Offer::Explained(EDITION_TEXT.to_string())
    }

    /// Whether the helper can be started (for a scan or a delete).
    pub fn can_elevate(&self) -> bool {
        matches!(
            self,
            Offer::Available | Offer::DeletesOnly | Offer::Running | Offer::Done
        )
    }

    /// Whether the button shows in the navigation bar.
    pub fn shows_button(&self) -> bool {
        matches!(self, Offer::Available | Offer::Running)
    }

    /// Whether pressing the button starts a scan now.
    pub fn can_start(&self) -> bool {
        matches!(self, Offer::Available)
    }

    /// How the Unreadable block words itself.
    pub fn unreadable_as(&self) -> UnreadableAs {
        match self {
            Offer::Available => UnreadableAs::Grant,
            Offer::Hidden | Offer::DeletesOnly => UnreadableAs::Plain,
            Offer::Running => UnreadableAs::Reading,
            Offer::Done => UnreadableAs::Overhead,
            Offer::Explained(_) => UnreadableAs::Edition,
            Offer::Unavailable(_) => UnreadableAs::Unavailable,
        }
    }
}

/// What the block says in an edition that cannot ask for administrator
/// rights.
pub const EDITION_TEXT: &str = "System folders can't be scanned in this edition — get the full version from the releases page.";

// ---- the race --------------------------------------------------------------

/// How the full scan ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FastEnd {
    /// Every item was read. `note` says when a different reader ran than the
    /// one asked for.
    Completed { note: Option<String> },
    /// The user declined the prompt or stopped the scan. Not an error.
    Cancelled,
    /// The helper could not do it.
    Failed(String),
}

/// What the window does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing changes.
    Nothing,
    /// The full result is shown. If the ordinary scan still runs, it is
    /// stopped and dropped.
    UseFast { note: Option<String> },
    /// The ordinary scan finished first and is shown. The full scan, if it
    /// still runs, is stopped.
    UseWalk { message: Option<String> },
    /// The full scan ended without a result. The ordinary scan stays (it is
    /// still running or already shown), with the message when there is one.
    DropFast { message: Option<String> },
}

/// The ordinary scan and the full scan, side by side. Whichever finishes
/// first is shown; the other is stopped.
#[derive(Clone, Debug)]
pub struct Race {
    walk_running: bool,
    fast_running: bool,
    decided: bool,
}

impl Race {
    /// A full scan starts while the ordinary one `walk_running` (or is
    /// already shown).
    pub fn start(walk_running: bool) -> Race {
        Race {
            walk_running,
            fast_running: true,
            decided: false,
        }
    }

    /// Whether the full scan still counts for anything.
    pub fn fast_running(&self) -> bool {
        self.fast_running && !self.decided
    }

    /// The ordinary scan ended (finished or stopped).
    pub fn walk_finished(&mut self) -> Step {
        if !self.walk_running {
            return Step::Nothing;
        }
        self.walk_running = false;
        if self.decided || !self.fast_running {
            return Step::Nothing;
        }
        self.decided = true;
        Step::UseWalk {
            message: Some(
                "The normal scan finished first, so its result is shown. Choose Full scan again to read the rest."
                    .to_string(),
            ),
        }
    }

    /// The full scan ended.
    pub fn fast_finished(&mut self, end: FastEnd) -> Step {
        if !self.fast_running {
            return Step::Nothing;
        }
        self.fast_running = false;
        if self.decided {
            return Step::Nothing;
        }
        match end {
            FastEnd::Completed { note } => {
                self.decided = true;
                self.walk_running = false;
                Step::UseFast { note }
            }
            FastEnd::Cancelled => {
                self.decided = true;
                Step::DropFast { message: None }
            }
            FastEnd::Failed(why) => {
                self.decided = true;
                Step::DropFast {
                    message: Some(format!(
                        "The full scan didn't finish: {why} The normal scan's result stays."
                    )),
                }
            }
        }
    }
}

/// The sentence for a reader that was not the one asked for.
pub fn fallback_note(fallback: bs_platform::elevate::Fallback) -> Option<String> {
    use bs_platform::elevate::Fallback;
    Some(
        match fallback {
            Fallback::None => return None,
            Fallback::Unavailable => {
                "The helper has no fast reader for this drive, so it read it folder by folder with administrator rights."
            }
            Fallback::TooLarge => {
                "This drive's index is too big for the fast reader, so the helper read it folder by folder with administrator rights."
            }
            Fallback::PartialMount => {
                "This mount shows only part of its filesystem, so the helper read it folder by folder with administrator rights."
            }
            Fallback::NotSupported => {
                "The fast reader can't read this filesystem, so the helper read it folder by folder with administrator rights."
            }
            Fallback::MountNotPlaced => {
                "A mount of this drive couldn't be placed for the fast reader, so the helper read it folder by folder with administrator rights."
            }
            _ => {
                "The helper used a different reader than the one asked for, and read this drive with administrator rights."
            }
        }
        .to_string(),
    )
}

// ---- keeping the place -----------------------------------------------------

/// Where the user is in a map, by names rather than by node numbers, so it can
/// be found again in another tree of the same place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    /// The folder on screen: the names from the scanned root down to it.
    folder: Vec<Vec<u8>>,
    /// The selected boxes, each as names from the scanned root.
    selected: Vec<Vec<Vec<u8>>>,
}

impl Place {
    /// How deep the folder on screen is below the scanned root.
    pub fn depth(&self) -> usize {
        self.folder.len()
    }

    /// How many selected boxes could be named.
    pub fn selected(&self) -> usize {
        self.selected.len()
    }
}

/// Where the user is now.
pub fn capture(app: &App<TreeSource>) -> Place {
    let source = app.source();
    let view = app.current_view();
    let folder = source
        .view_folder(view)
        .map(|n| source.names_below_root(n))
        .unwrap_or_default();
    let selected = app
        .selection()
        .iter()
        .filter_map(|&id| crate::ViewSource::target(source, view, id)?.node)
        .map(|n| source.names_below_root(n))
        .collect();
    Place { folder, selected }
}

/// Goes back to `place` in `app`, a map of a new tree of the same place. What
/// is not there any more is skipped: the view falls back to the nearest folder
/// that is. Returns what the host applies to the renderer.
pub fn restore(app: &mut App<TreeSource>, place: &Place) -> Vec<Effect> {
    let mut fx = Vec::new();
    // The deepest folder of the old path that exists in the new tree.
    let mut folder: Option<NodeId> = None;
    for depth in (0..=place.folder.len()).rev() {
        if let Some(n) = app.source().folder_by_names(&place.folder[..depth]) {
            folder = Some(n);
            break;
        }
    }
    let Some(folder) = folder else {
        return fx;
    };
    if let Some(view) = app.source_mut().open_folder(folder)
        && view != app.current_view()
    {
        fx.extend(app.show_view(view));
    }
    let view = app.current_view();
    let ids: Vec<u32> = place
        .selected
        .iter()
        .filter_map(|names| app.source().folder_or_file_by_names(names))
        .filter_map(|n| app.source().item_for_node(view, n))
        .collect();
    if !ids.is_empty() {
        fx.extend(app.select(ids));
    }
    fx
}

// ---- what was deleted so far -------------------------------------------------

/// The delete marks of the open map, by names rather than by node numbers, so
/// they can be put on another tree of the same place: when the full result
/// replaces the map, what was moved to the bin or deleted before it must still
/// show as gone, and must not be counted again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteMarks {
    marks: Vec<(Vec<Vec<u8>>, DeleteState)>,
}

impl DeleteMarks {
    /// Notes that the item at `names` (from the scanned root) now has state
    /// `to`. The state `None` takes the note away (an undo).
    pub fn note(&mut self, names: Vec<Vec<u8>>, to: DeleteState) {
        self.marks.retain(|(n, _)| *n != names);
        if to != DeleteState::None {
            self.marks.push((names, to));
        }
    }

    /// Forgets everything (another map is opened).
    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// How many items are marked.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// Whether nothing is marked.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// The marks as changes to the tree `snapshot`. An item that the tree does
    /// not have is skipped: there is nothing there to mark.
    pub fn changes_in(&self, snapshot: &bs_tree::Snapshot) -> Vec<crate::shell::TreeChange> {
        self.marks
            .iter()
            .filter_map(|(names, to)| {
                let mut at = snapshot.root()?;
                for name in names {
                    at = at.children().find(|c| c.name() == name.as_slice())?;
                }
                Some(crate::shell::TreeChange {
                    node: at.id(),
                    to: *to,
                })
            })
            .collect()
    }
}

// ---- what the helper did to a delete ---------------------------------------

/// What happened to one item of an elevated delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElevatedItem {
    /// Gone for good. `bytes` is what the drive got back.
    Deleted { bytes: u64, shared_files: u64 },
    /// Not touched; the text says why.
    Refused(String),
    /// Tried and did not complete; the text says what is known.
    Failed(String),
}

/// How an elevated delete ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElevatedEnd {
    /// Every item has an outcome, in the order asked.
    Done(Vec<ElevatedItem>),
    /// The user said no to the prompt. Not an error; nothing was touched.
    Declined,
    /// Not possible here; the text says why.
    Unavailable(String),
    /// The helper could not tell which folders are the user's own, so it
    /// deleted nothing.
    NoAccount,
    /// The conversation broke off. `completed` are the items that have an
    /// outcome, by position; any other may or may not be deleted.
    Failed {
        message: String,
        completed: Vec<(usize, ElevatedItem)>,
    },
}

/// What to tell when the folder to scan was not the folder the helper had
/// checked. Nothing was read; asking again judges the folder afresh, which
/// the Full scan button (or the Unreadable block) offers again at once.
pub const ROOT_MOVED: &str =
    "The folder changed while it was being checked, so nothing was scanned. Try again.";

/// What to tell when the administrator helper refuses a volume: a network
/// share or remote file system, or a volume it could not identify. Used for
/// a refused scan and a refused delete alike.
pub const REMOTE_VOLUME: &str = "This is a network volume, or one the administrator helper couldn't identify. Administrator scans and deletes work only on this computer's own disks.";

/// The sentence for a helper error that has words of its own here, or `None`
/// for any other. Ends with a full stop.
pub fn elevate_error_words(error: &bs_platform::elevate::ElevateError) -> Option<String> {
    use bs_platform::elevate::ElevateError;
    match error {
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report))
            if report.code == bs_ipc::ErrorCode::RemoteVolume =>
        {
            Some(REMOTE_VOLUME.to_string())
        }
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report))
            if report.code == bs_ipc::ErrorCode::RootMoved =>
        {
            Some(ROOT_MOVED.to_string())
        }
        // These two carry a plain, path-free sentence from the helper that
        // says why nothing was scanned; it is shown as a sentence, without the
        // technical lead-in.
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report))
            if matches!(
                report.code,
                bs_ipc::ErrorCode::Unsupported | bs_ipc::ErrorCode::Refused
            ) =>
        {
            sentence(&report.message)
        }
        _ => None,
    }
}

/// A helper's short message as a sentence: first letter up, a full stop at
/// the end. `None` when there is nothing to say.
fn sentence(message: &str) -> Option<String> {
    let text = message.trim();
    let mut chars = text.chars();
    let first = chars.next()?;
    let mut out: String = first.to_uppercase().collect();
    out.push_str(chars.as_str());
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    Some(out)
}

/// What a refused item says.
pub fn refusal_words(refusal: bs_platform::elevate::Refusal) -> &'static str {
    use bs_platform::elevate::Refusal;
    match refusal {
        Refusal::Blocked => "part of the operating system",
        Refusal::ProtectedNotConfirmed => "it needs the hold-to-confirm",
        Refusal::SelectionRefused { .. } => "another selected item is refused",
        Refusal::BadPath | Refusal::NotRealPath | Refusal::CannotResolve => {
            "its path isn't a plain, direct path"
        }
        Refusal::NotFound => "it is already gone",
        Refusal::ChangedSinceScan => "it changed since the scan; rescan the folder first",
        Refusal::MountPoint => "it is the start of another drive",
        Refusal::Overlaps { .. } => "one selected item is inside another",
        Refusal::NotPermanent => "the request was not a permanent delete",
        Refusal::RemoteVolume => {
            "it is on a network volume, or one the administrator helper couldn't identify; administrator deletes work only on this computer's own disks"
        }
        _ => "the helper would not touch it",
    }
}

/// Words for an item the helper started on and did not finish.
pub fn failed_words(os_code: i32, removed_entries: u64) -> String {
    let base = if os_code != 0 {
        format!("the system reported an error (code {os_code})")
    } else {
        "the system reported an error".to_string()
    };
    if removed_entries > 0 {
        format!(
            "{base}; {removed_entries} entries were already removed, rescan the folder to see what is left"
        )
    } else {
        base
    }
}

/// Words for an administrator delete that was refused because the account has
/// no entry in the computer's account list.
pub const NO_ACCOUNT_TEXT: &str = "Administrator deletes are off on this computer: your account has no entry in its account list, so Bytescape can't tell which folders are yours. Nothing was deleted.";

/// Words for a prompt the user declined.
pub const DECLINED_TEXT: &str = "Administrator rights weren't granted, so nothing was deleted.";

/// Deletes `items` for good through the administrator helper: one prompt for
/// the whole call.
pub fn delete_with_helper(
    items: &[bs_platform::delete::Item],
    held: &[std::path::PathBuf],
) -> ElevatedEnd {
    // Only the paths whose hold-to-confirm was completed are marked
    // confirmed, one by one; the helper checks each path against its own
    // table again.
    let targets: Vec<bs_platform::elevate::DeleteTarget> = items
        .iter()
        .map(|i| bs_platform::elevate::DeleteTarget {
            path: i.path.clone(),
            confirmed_protected: held.contains(&i.path),
            seen: i.seen.map(|s| (s.volume, s.file)),
        })
        .collect();
    match bs_platform::elevate::elevate_and_delete(&targets) {
        Ok(outcomes) => ElevatedEnd::Done(outcomes.iter().map(item_from).collect()),
        Err(failure) => end_from_error(failure.error, failure.completed),
    }
}

/// What the helper reported for one item, in the interface's words.
pub fn item_from(outcome: &bs_ipc::ItemOutcome) -> ElevatedItem {
    use bs_ipc::ItemOutcome;
    match outcome {
        ItemOutcome::Deleted {
            bytes,
            shared_files,
            ..
        } => ElevatedItem::Deleted {
            bytes: *bytes,
            shared_files: *shared_files,
        },
        ItemOutcome::Refused(why) => ElevatedItem::Refused(refusal_words(*why).to_string()),
        ItemOutcome::Failed {
            os_code,
            removed_entries,
            ..
        } => ElevatedItem::Failed(failed_words(*os_code, *removed_entries)),
        _ => ElevatedItem::Failed("the helper reported something unexpected".to_string()),
    }
}

/// How an elevated delete that broke off ended: a declined prompt, an edition
/// that cannot ask, a refusal for want of an account entry, or a conversation
/// that stopped part way.
pub fn end_from_error(
    error: bs_platform::elevate::ElevateError,
    completed: Vec<(u32, bs_ipc::ItemOutcome)>,
) -> ElevatedEnd {
    use bs_platform::elevate::ElevateError;
    let done: Vec<(usize, ElevatedItem)> = completed
        .iter()
        .map(|(i, o)| (*i as usize, item_from(o)))
        .collect();
    match error {
        ElevateError::Declined => ElevatedEnd::Declined,
        ElevateError::Unavailable(why) => ElevatedEnd::Unavailable(why.explanation().to_string()),
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report))
            if report.code == bs_ipc::ErrorCode::NoAccount =>
        {
            ElevatedEnd::NoAccount
        }
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report))
            if report.code == bs_ipc::ErrorCode::RemoteVolume =>
        {
            ElevatedEnd::Failed {
                message: REMOTE_VOLUME.to_string(),
                completed: done,
            }
        }
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report))
            if report.code == bs_ipc::ErrorCode::RootMoved =>
        {
            ElevatedEnd::Failed {
                message: ROOT_MOVED.to_string(),
                completed: done,
            }
        }
        ElevateError::Protocol(bs_ipc::ClientError::Helper(report)) => ElevatedEnd::Failed {
            message: report.message,
            completed: done,
        },
        other => ElevatedEnd::Failed {
            message: other.to_string(),
            completed: done,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_reasons_hide_the_offer_and_two_explain_it() {
        assert_eq!(
            Offer::for_unavailable(Unavailable::NoElevationService),
            Offer::Unavailable(Unavailable::NoElevationService.explanation().to_string())
        );
        assert_eq!(
            Offer::for_unavailable(Unavailable::AlreadyElevated),
            Offer::Hidden
        );
        for why in [Unavailable::AppImage, Unavailable::SnapConfined] {
            let Offer::Explained(text) = Offer::for_unavailable(why) else {
                panic!("{why:?} should explain itself");
            };
            assert_eq!(text, why.explanation());
        }
    }

    #[test]
    fn only_an_available_offer_starts_a_scan_and_a_running_one_keeps_its_button() {
        assert!(Offer::Available.can_start());
        assert!(!Offer::Running.can_start());
        assert!(Offer::Running.shows_button());
        assert!(!Offer::Done.shows_button());
        assert!(Offer::Done.can_elevate());
        assert!(!Offer::Hidden.can_elevate());
        assert!(Offer::DeletesOnly.can_elevate());
        assert!(!Offer::DeletesOnly.shows_button());
        assert!(!Offer::DeletesOnly.can_start());
        assert_eq!(Offer::Hidden.unreadable_as(), UnreadableAs::Plain);
        assert_eq!(Offer::Available.unreadable_as(), UnreadableAs::Grant);
        assert!(!Offer::walk_only_edition().can_elevate());
    }

    #[test]
    fn the_walk_finishing_first_wins_and_stops_the_full_scan() {
        let mut race = Race::start(true);
        let step = race.walk_finished();
        assert!(matches!(step, Step::UseWalk { message: Some(_) }));
        // The full scan ends later (it was stopped): nothing more happens.
        assert_eq!(
            race.fast_finished(FastEnd::Cancelled),
            Step::Nothing,
            "a loser's end changes nothing"
        );
    }

    #[test]
    fn the_full_scan_finishing_first_wins_and_drops_the_walk() {
        let mut race = Race::start(true);
        let step = race.fast_finished(FastEnd::Completed { note: None });
        assert_eq!(step, Step::UseFast { note: None });
        assert_eq!(race.walk_finished(), Step::Nothing);
    }

    #[test]
    fn a_full_scan_started_after_the_walk_finished_just_replaces_it() {
        let mut race = Race::start(false);
        assert_eq!(race.walk_finished(), Step::Nothing);
        assert_eq!(
            race.fast_finished(FastEnd::Completed {
                note: Some("x".into())
            }),
            Step::UseFast {
                note: Some("x".into())
            }
        );
    }

    #[test]
    fn declining_the_prompt_is_not_an_error() {
        let mut race = Race::start(true);
        assert_eq!(
            race.fast_finished(FastEnd::Cancelled),
            Step::DropFast { message: None }
        );
        // The walk then ends on its own without any verdict.
        assert_eq!(race.walk_finished(), Step::Nothing);
    }

    #[test]
    fn a_failed_full_scan_keeps_the_walk_and_says_so() {
        let mut race = Race::start(true);
        let Step::DropFast { message: Some(m) } =
            race.fast_finished(FastEnd::Failed("the helper stopped.".into()))
        else {
            panic!("expected a message");
        };
        assert!(m.contains("helper stopped") && m.contains("normal scan"));
        assert!(!race.fast_running());
    }

    #[test]
    fn a_fallback_is_named_and_no_fallback_is_silent() {
        use bs_platform::elevate::Fallback;
        assert_eq!(fallback_note(Fallback::None), None);
        let text = fallback_note(Fallback::MountNotPlaced).expect("note");
        assert!(text.contains("couldn't be placed"));
        for f in [
            Fallback::Unavailable,
            Fallback::TooLarge,
            Fallback::PartialMount,
            Fallback::NotSupported,
        ] {
            assert!(fallback_note(f).is_some());
        }
    }

    #[test]
    fn the_words_never_use_the_banned_vocabulary() {
        let all = [
            EDITION_TEXT,
            NO_ACCOUNT_TEXT,
            DECLINED_TEXT,
            refusal_words(bs_platform::elevate::Refusal::Blocked),
            refusal_words(bs_platform::elevate::Refusal::RemoteVolume),
            REMOTE_VOLUME,
            ROOT_MOVED,
        ];
        for text in all {
            let lower = text.to_lowercase();
            for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
                assert!(!lower.contains(banned), "{text}");
            }
        }
    }

    fn helper_error(code: bs_ipc::ErrorCode) -> bs_platform::elevate::ElevateError {
        bs_platform::elevate::ElevateError::Protocol(bs_ipc::ClientError::Helper(
            bs_ipc::ErrorReport {
                code,
                message: "technical text from the helper".into(),
            },
        ))
    }

    #[test]
    fn a_moved_root_says_so_and_invites_another_try() {
        let words = elevate_error_words(&helper_error(bs_ipc::ErrorCode::RootMoved))
            .expect("words of its own");
        assert_eq!(words, ROOT_MOVED);
        assert!(words.ends_with("Try again."));
        // The full scan's failure keeps the normal result and names the cause.
        let mut race = Race::start(true);
        let Step::DropFast { message: Some(m) } = race.fast_finished(FastEnd::Failed(words)) else {
            panic!("expected a message");
        };
        assert!(
            m.contains("nothing was scanned") && m.contains("Try again."),
            "{m}"
        );
        // The words do not depend on the helper's own text.
        let end = end_from_error(helper_error(bs_ipc::ErrorCode::RootMoved), Vec::new());
        assert!(matches!(end, ElevatedEnd::Failed { ref message, .. } if message == ROOT_MOVED));
    }

    #[test]
    fn unsupported_and_refused_scans_show_the_helpers_sentence() {
        for code in [bs_ipc::ErrorCode::Unsupported, bs_ipc::ErrorCode::Refused] {
            let error = bs_platform::elevate::ElevateError::Protocol(bs_ipc::ClientError::Helper(
                bs_ipc::ErrorReport {
                    code,
                    message:
                        "this system cannot hold the scan to the folder, so nothing was scanned"
                            .into(),
                },
            ));
            let words = elevate_error_words(&error).expect("words");
            assert_eq!(
                words,
                "This system cannot hold the scan to the folder, so nothing was scanned."
            );
            assert!(!words.contains("could not do it"));
        }
        // An empty message gives no words, so the generic line is used.
        let empty = bs_platform::elevate::ElevateError::Protocol(bs_ipc::ClientError::Helper(
            bs_ipc::ErrorReport {
                code: bs_ipc::ErrorCode::Refused,
                message: "  ".into(),
            },
        ));
        assert_eq!(elevate_error_words(&empty), None);
    }

    #[test]
    fn a_remote_volume_is_explained_for_a_scan_and_for_a_delete() {
        // The refused scan.
        let scan = elevate_error_words(&helper_error(bs_ipc::ErrorCode::RemoteVolume))
            .expect("words of its own");
        assert_eq!(scan, REMOTE_VOLUME);
        assert!(scan.contains("network volume") && scan.contains("couldn't identify"));
        assert!(scan.contains("this computer's own disks"));
        // Other errors keep the helper's or the platform's own text.
        assert_eq!(
            elevate_error_words(&helper_error(bs_ipc::ErrorCode::ScanFailed)),
            None
        );
        assert_eq!(
            elevate_error_words(&bs_platform::elevate::ElevateError::Declined),
            None
        );
        // The refused delete of a whole request.
        let end = end_from_error(helper_error(bs_ipc::ErrorCode::RemoteVolume), Vec::new());
        assert_eq!(
            end,
            ElevatedEnd::Failed {
                message: REMOTE_VOLUME.to_string(),
                completed: Vec::new()
            }
        );
        // A refused item of a selection.
        let item = item_from(&bs_ipc::ItemOutcome::Refused(
            bs_platform::elevate::Refusal::RemoteVolume,
        ));
        let ElevatedItem::Refused(words) = item else {
            panic!("{item:?}");
        };
        assert!(words.contains("network volume"), "{words}");
        assert!(words.contains("couldn't identify"), "{words}");
        assert_ne!(words, refusal_words(bs_platform::elevate::Refusal::Blocked));
    }
}
