//! Start-up failures that come from the computer rather than from a bug: a
//! system library the window needs is missing. They end the program with one
//! plain line saying what is missing, not with a panic, a backtrace or a
//! crash report for the next start.

use std::panic::{AssertUnwindSafe, PanicHookInfo};
use std::sync::{Arc, Mutex};

/// The one line to print for a panic message that means "a system library
/// could not be loaded", or `None` for any other panic (which is a bug and
/// stays a crash).
pub fn explain_missing_library(message: &str) -> Option<String> {
    // The windowing libraries panic with "Library <file> could not be loaded."
    let rest = message.strip_prefix("Library ")?;
    let file = rest.strip_suffix(" could not be loaded.").or_else(|| {
        rest.split_once(" could not be loaded")
            .map(|(file, _)| file)
    })?;
    let file = file.trim();
    if file.is_empty() || file.chars().any(char::is_whitespace) {
        return None;
    }
    let name = file.split(".so").next().unwrap_or(file);
    let (what, package) = match name {
        "libxkbcommon-x11" => (
            "the X11 keyboard library (libxkbcommon-x11)",
            " (Debian and Ubuntu: libxkbcommon-x11-0)",
        ),
        "libxkbcommon" => (
            "the keyboard library (libxkbcommon)",
            " (Debian and Ubuntu: libxkbcommon0)",
        ),
        "libX11" | "libX11-xcb" | "libxcb" => ("the X11 library", ""),
        _ => ("a system library", ""),
    };
    let named = if what.contains(name) {
        String::new()
    } else {
        format!(" ({file})")
    };
    Some(format!(
        "Bytescape needs {what}{named} and could not find it. Install it{package}, then start Bytescape again."
    ))
}

/// Runs `start`. If it panics because a system library is missing, returns
/// the line that says so; the panic prints nothing and leaves no crash
/// report. Any other panic goes through the previous panic hook (the crash
/// report) and continues unwinding.
pub fn guard_missing_libraries<T>(start: impl FnOnce() -> T) -> Result<T, String> {
    let previous = Arc::new(std::panic::take_hook());
    let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let previous = Arc::clone(&previous);
        let seen = Arc::clone(&seen);
        std::panic::set_hook(Box::new(move |info| {
            match explain_missing_library(&panic_text(info)) {
                Some(line) => {
                    if let Ok(mut slot) = seen.lock() {
                        slot.get_or_insert(line);
                    }
                }
                None => previous(info),
            }
        }));
    }
    let result = std::panic::catch_unwind(AssertUnwindSafe(start));
    let _ = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| previous(info)));
    match result {
        Ok(value) => Ok(value),
        Err(payload) => {
            let line = seen.lock().ok().and_then(|mut slot| slot.take());
            match line {
                Some(line) => Err(line),
                None => std::panic::resume_unwind(payload),
            }
        }
    }
}

fn panic_text(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// What to say when the computer has no fonts at all: without one there is
/// no text to draw.
pub const NO_FONTS: &str = "Bytescape found no fonts on this computer, so it has no text to draw. Install a font (Debian and Ubuntu: fonts-dejavu-core), then start Bytescape again.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_library_is_named_with_its_package() {
        let line =
            explain_missing_library("Library libxkbcommon-x11.so could not be loaded.").unwrap();
        assert!(line.contains("libxkbcommon-x11"), "{line}");
        assert!(line.contains("libxkbcommon-x11-0"), "{line}");
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains("panick"));
    }

    #[test]
    fn another_library_is_named_by_its_file() {
        let line = explain_missing_library("Library libfoo.so.1 could not be loaded.").unwrap();
        assert!(line.contains("libfoo.so.1"), "{line}");
    }

    #[test]
    fn other_panics_are_not_taken_for_a_missing_library() {
        for text in [
            "",
            "index out of bounds",
            "no default font found",
            "Library  could not be loaded.",
            "called `Option::unwrap()` on a `None` value",
            "Library with spaces in it could not be loaded.",
        ] {
            assert_eq!(explain_missing_library(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_wording_avoids_banned_words() {
        for line in [
            explain_missing_library("Library libxkbcommon.so could not be loaded.").unwrap(),
            NO_FONTS.to_string(),
        ] {
            let lower = line.to_lowercase();
            for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
                assert!(!lower.contains(banned), "{line}");
            }
        }
    }

    #[test]
    fn a_missing_library_panic_becomes_a_line_and_nothing_else() {
        let result: Result<(), String> = guard_missing_libraries(|| {
            panic!("Library libxkbcommon-x11.so could not be loaded.");
        });
        let line = result.unwrap_err();
        assert!(line.contains("libxkbcommon-x11"));
    }

    #[test]
    fn a_normal_return_passes_through() {
        assert_eq!(guard_missing_libraries(|| 7).unwrap(), 7);
    }
}
