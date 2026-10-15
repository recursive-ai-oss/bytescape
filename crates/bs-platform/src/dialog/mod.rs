//! The system's folder dialog, and a plain message box for the few things
//! that have to be said before the interface can show anything.
//!
//! - **Windows:** the shell's file dialog in folder-picking mode
//!   (`IFileOpenDialog` with `FOS_PICKFOLDERS`).
//! - **Linux:** `zenity` or `kdialog`, chosen by the desktop. The desktop
//!   portal would be the nicer route, but its answer comes as a D-Bus signal
//!   that only a D-Bus client can wait for, and a client library would bring a
//!   socket-capable dependency into the program. Both tools run as the normal
//!   user, with an argument list and no shell; see [`crate::launch`] for how
//!   programs are started.

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Why no folder came back (as opposed to the user closing the dialog).
#[derive(Debug)]
#[non_exhaustive]
pub enum DialogError {
    /// This system has no dialog to show: neither `zenity` nor `kdialog` is
    /// installed, or the platform call failed.
    Unavailable,
    /// The session bus is not a local socket, so no dialog program is started.
    SessionBusNotLocal,
    /// The dialog answered with something that is not an existing folder.
    BadAnswer,
}

impl std::fmt::Display for DialogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "no folder dialog is available",
            Self::SessionBusNotLocal => "the session bus is not a local socket",
            Self::BadAnswer => "the dialog did not answer with a folder",
        })
    }
}

impl std::error::Error for DialogError {}

/// The most characters of a title or text that [`message`] shows. A longer
/// text is cut, with an ellipsis: the box is for a sentence or two.
pub const MESSAGE_LIMIT: usize = 1000;

/// What [`message`] shows for `text`: control characters other than the line
/// break and the tab are dropped (a NUL would end the text early), a long text
/// is cut at `limit` characters, and the result never ends mid-character.
fn tidy(text: &str, limit: usize) -> String {
    let clean = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'));
    let mut out: String = clean.clone().take(limit).collect();
    if clean.count() > limit {
        out.pop();
        out.push('\u{2026}');
    }
    out
}

/// Tells the user something and waits until it is dismissed.
///
/// For a program that has to refuse to start before it has a window, such as
/// the interface being started with administrator rights: a program started
/// from the desktop has no terminal, so a line on standard error is never
/// seen.
///
/// - **Windows:** a message box with an OK button and the error icon, task
///   modal, so it appears whether or not the program has a window.
/// - **Linux:** one line on standard error. Programs started from a desktop
///   launcher have no terminal there either, but a launcher keeps the message
///   in the session log, and no dialog program is started for it.
///
/// The text is shown as it is, not as a format string. It may be cut (see
/// [`MESSAGE_LIMIT`]).
pub fn message(title: &str, text: &str) {
    let title = tidy(title, 200);
    let text = tidy(text, MESSAGE_LIMIT);
    #[cfg(windows)]
    windows::message(&title, &text);
    #[cfg(not(windows))]
    {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{title}: {text}");
    }
}

/// Shows the folder dialog, starting in `start` when that is an existing
/// folder, and waits for it. `Ok(None)` means the user closed it without
/// choosing. The folder returned exists and is an absolute path.
///
/// Call it off the interface thread: it returns when the user is done.
pub fn pick_folder(start: &Path) -> Result<Option<PathBuf>, DialogError> {
    #[cfg(target_os = "linux")]
    return linux::pick_folder(&crate::launch::linux::Env::current(), start);
    #[cfg(windows)]
    return windows::pick_folder(start);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = start;
        Err(DialogError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_shown_as_it_is() {
        assert_eq!(tidy("Hello, world.", 100), "Hello, world.");
        assert_eq!(tidy("two\nlines\tand a tab", 100), "two\nlines\tand a tab");
        assert_eq!(tidy("100% of %s and {0}", 100), "100% of %s and {0}");
        assert_eq!(tidy("", 10), "");
    }

    #[test]
    fn control_characters_that_would_cut_or_confuse_the_text_are_dropped() {
        assert_eq!(tidy("a\0b", 10), "ab");
        assert_eq!(tidy("a\u{7}b\u{1b}[31mc\r", 10), "ab[31mc");
    }

    #[test]
    fn a_long_message_is_cut_on_a_character_with_an_ellipsis() {
        let long = "x".repeat(MESSAGE_LIMIT + 50);
        let shown = tidy(&long, MESSAGE_LIMIT);
        assert_eq!(shown.chars().count(), MESSAGE_LIMIT);
        assert!(shown.ends_with('\u{2026}'));
        // Exactly at the limit nothing is cut; characters are not split.
        let exact = "y".repeat(MESSAGE_LIMIT);
        assert_eq!(tidy(&exact, MESSAGE_LIMIT), exact);
        let wide = "\u{1F600}".repeat(MESSAGE_LIMIT + 1);
        let shown = tidy(&wide, MESSAGE_LIMIT);
        assert_eq!(shown.chars().count(), MESSAGE_LIMIT);
        assert!(shown.is_char_boundary(shown.len()));
    }

    #[cfg(not(windows))]
    #[test]
    fn on_this_system_a_message_is_a_line_on_standard_error_and_returns_at_once() {
        // It must not wait for anyone or start anything.
        message(
            "Bytescape",
            "The interface does not run with administrator rights.",
        );
        message("", "");
        message("title", &"z".repeat(5000));
    }
}
