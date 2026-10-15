//! Hiding paths and the user's name in a crash report.
//!
//! The report is shown, and sent to GitHub, only after this has run, unless
//! the user asks to see paths. The rules, in the order they apply to each line:
//!
//! 1. **Absolute paths** are replaced whole, with `<path-1>`, `<path-2>`, and
//!    so on. The same path always gets the same number within one report, so
//!    a reader can still tell that two lines mention the same file. A path is:
//!    - Unix: `/` at the start of a word, after a space, a quote, a bracket,
//!      `=`, `,` or `;`, or after `file://` (home folders, `/root`, `/tmp/...`
//!      and every other absolute path);
//!    - Windows: a drive letter and `:\` or `:/`, a UNC path `\\server\share`
//!      and device paths such as `\\?\C:\...`, in plain or backslash-doubled
//!      (`C:\\Users\\...`) spelling;
//!    - anything that begins with this computer's home folder, wherever it is.
//!
//!    A path runs to the end of the line, except that it stops at a closing
//!    quote when it began in quotes, and (when it did not) at a quote,
//!    `<`, `>`, `|`, `*`, `?`, a colon followed by a space or a digit (as in
//!    `file.rs:12:5`), a comma or semicolon followed by a space, or a bracket
//!    that closes one it did not open. Spaces do not end a path, because folder
//!    names have spaces. When words follow a path with no mark between, they
//!    are hidden with it: hiding too much is the safe way to be wrong.
//! 2. **Source files in a backtrace** (`at /dir/file.rs:12:5`) and in the
//!    `Location:` line keep the file name and the line, and lose the folder:
//!    `<source>/file.rs:12:5`. These are program source files, and the
//!    file name tells whoever reads the report where the crash was.
//! 3. **The user name**, as a whole word in any case, becomes `<user>`. Names of
//!    one character and `root` are left alone (they are too common to be a
//!    name, and `/root` is a path, which rule 1 hides).
//!
//! Relative paths, URLs and everything else are left as they are.

use std::collections::HashMap;

/// What this computer says about who is running the program.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Context {
    /// The home folder, as the environment gives it.
    pub home: Option<String>,
    /// The user name.
    pub user: Option<String>,
}

impl Context {
    /// The user name to hide: not one character, which would blot out ordinary
    /// words, and not `root`, which says nothing about who this is.
    fn hidden_user(&self) -> Option<&str> {
        self.user
            .as_deref()
            .map(str::trim)
            .filter(|user| user.chars().count() >= 2 && !user.eq_ignore_ascii_case("root"))
    }

    pub fn from_env() -> Self {
        let first = |names: &[&str]| {
            names
                .iter()
                .find_map(std::env::var_os)
                .map(|value| value.to_string_lossy().into_owned())
        };
        Self {
            home: first(&["HOME", "USERPROFILE"]).filter(|home| home.len() >= 2 && home != "/"),
            user: first(&["USER", "USERNAME", "LOGNAME"]),
        }
    }
}

/// Replaces paths and the user name in `text` as described above.
pub(crate) fn redact(text: &str, context: &Context) -> String {
    let mut numbers: HashMap<String, usize> = HashMap::new();
    let mut out = String::with_capacity(text.len());
    let mut in_backtrace = false;
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if line == "Backtrace:" {
            in_backtrace = true;
        } else if in_backtrace && !line.is_empty() && !line.starts_with(' ') {
            in_backtrace = false;
        }
        let source_line = (in_backtrace && line.trim_start().starts_with("at "))
            || line.starts_with("Location: ");
        redact_line(line, context, source_line, &mut numbers, &mut out);
    }
    out
}

fn redact_line(
    line: &str,
    context: &Context,
    source_line: bool,
    numbers: &mut HashMap<String, usize>,
    out: &mut String,
) {
    let bytes = line.as_bytes();
    let mut copied = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(start) = path_start(line, i, context) else {
            i += 1;
            continue;
        };
        let end = path_end(bytes, i, start);
        // Trailing spaces belong to the text after the path.
        let mut end = end;
        while end > i && bytes.get(end - 1) == Some(&b' ') {
            end -= 1;
        }
        let (Some(before), Some(path)) = (line.get(copied..i), line.get(i..end)) else {
            i += 1;
            continue;
        };
        push_text(out, before, context);
        push_placeholder(out, path, source_line, numbers);
        copied = end;
        i = end.max(i + 1);
    }
    if let Some(rest) = line.get(copied..) {
        push_text(out, rest, context);
    }
}

fn push_placeholder(
    out: &mut String,
    path: &str,
    source_line: bool,
    numbers: &mut HashMap<String, usize>,
) {
    if source_line {
        let file = path.rsplit(['/', '\\']).next().unwrap_or("");
        if file.len() > 3 && file.ends_with(".rs") && !file.contains(' ') {
            out.push_str("<source>/");
            out.push_str(file);
            return;
        }
    }
    let next = numbers.len() + 1;
    let number = *numbers.entry(path.to_owned()).or_insert(next);
    out.push_str("<path-");
    out.push_str(&number.to_string());
    out.push('>');
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// Whether `byte` can begin the text of a path (it is not a space or a mark
/// that ends one).
fn can_follow_root(byte: Option<&u8>) -> bool {
    byte.is_some_and(|b| !is_space(*b) && *b >= 0x20 && !b"\"`<>|*?".contains(b))
}

/// Whether a path may begin after the character `previous`: after a space or
/// a mark that introduces a value, or after punctuation outside ASCII (such as
/// curly quotes), but not in the middle of a word.
fn may_follow(previous: Option<char>) -> bool {
    match previous {
        None => true,
        Some(c) if c.is_ascii() => c.is_ascii_whitespace() || "\"'`([{<=,;|".contains(c),
        Some(c) => !c.is_alphanumeric(),
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// If a path begins at byte `i` of `line`, the index where reading its text
/// starts (after any prefix that was already recognised).
fn path_start(line: &str, i: usize, context: &Context) -> Option<usize> {
    let b = line.as_bytes();
    let current = *b.get(i)?;
    if !line.is_char_boundary(i) {
        return None;
    }
    let previous = line.get(..i).and_then(|before| before.chars().next_back());

    // This computer's own home folder, wherever it is and whatever is before it.
    if let Some(home) = context.home.as_deref() {
        let after = line
            .get(i + home.len()..)
            .and_then(|rest| rest.chars().next());
        if line.get(i..).is_some_and(|rest| rest.starts_with(home))
            && !after.is_some_and(is_word_char)
            && !previous.is_some_and(is_word_char)
        {
            return Some(i + home.len());
        }
    }

    match current {
        b'/' => {
            let after_file_scheme = i >= 7
                && b.get(i - 7..i)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case(b"file://"));
            if !(may_follow(previous) || after_file_scheme) {
                return None;
            }
            // `//server/share` is a path; `// comment` is not.
            let mut k = i + 1;
            while b.get(k) == Some(&b'/') {
                k += 1;
            }
            can_follow_root(b.get(k)).then_some(i + 1)
        }
        b'\\' => {
            if previous == Some('\\') || !may_follow(previous) {
                return None;
            }
            let mut k = i;
            while b.get(k) == Some(&b'\\') {
                k += 1;
            }
            if k - i < 2 {
                return None;
            }
            // `\\?\` and `\\.\`, plain or backslash-doubled.
            if matches!(b.get(k), Some(b'?' | b'.')) && b.get(k + 1) == Some(&b'\\') {
                k += 2;
                while b.get(k) == Some(&b'\\') {
                    k += 1;
                }
            }
            can_follow_root(b.get(k)).then_some(k)
        }
        letter if letter.is_ascii_alphabetic() => {
            let is_drive = b.get(i + 1) == Some(&b':')
                && matches!(b.get(i + 2), Some(b'\\' | b'/'))
                && !previous.is_some_and(is_word_char);
            is_drive.then_some(i + 3)
        }
        _ => None,
    }
}

/// Where the path that begins at `begin`, with its text starting at `from`,
/// ends.
fn path_end(b: &[u8], begin: usize, from: usize) -> usize {
    let opener = begin
        .checked_sub(1)
        .and_then(|p| b.get(p))
        .copied()
        .filter(|c| matches!(c, b'"' | b'\'' | b'`'));
    let mut depth = 0u32;
    let mut j = from;
    while let Some(&c) = b.get(j) {
        if c < 0x20 || c == 0x7f {
            break;
        }
        if let Some(opener) = opener {
            // In quotes only the closing quote ends the path.
            if c == opener || (opener == b'`' && c == b'\'') {
                break;
            }
            j += 1;
            continue;
        }
        let next = b.get(j + 1).copied();
        match c {
            b'"' | b'`' | b'<' | b'>' | b'|' | b'*' | b'?' => break,
            b':' if next.is_none_or(|n| is_space(n) || n.is_ascii_digit()) => break,
            b',' | b';' if next.is_none_or(is_space) => break,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        j += 1;
    }
    j
}

/// Adds text that is not a path, with the user name hidden.
fn push_text(out: &mut String, text: &str, context: &Context) {
    let Some(name) = context.hidden_user() else {
        out.push_str(text);
        return;
    };
    let name = name.as_bytes();
    let bytes = text.as_bytes();
    let mut copied = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let end = i + name.len();
        let found = bytes
            .get(i..end)
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            && text.is_char_boundary(i)
            && text.is_char_boundary(end)
            && !text
                .get(..i)
                .and_then(|b| b.chars().next_back())
                .is_some_and(is_word_char)
            && !text
                .get(end..)
                .and_then(|a| a.chars().next())
                .is_some_and(is_word_char);
        if found && let Some(before) = text.get(copied..i) {
            out.push_str(before);
            out.push_str("<user>");
            copied = end;
            i = end;
        } else {
            i += 1;
        }
    }
    if let Some(rest) = text.get(copied..) {
        out.push_str(rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            home: Some("/home/alice".to_owned()),
            user: Some("alice".to_owned()),
        }
    }

    fn windows_context() -> Context {
        Context {
            home: Some(r"C:\Users\Alice".to_owned()),
            user: Some("Alice".to_owned()),
        }
    }

    fn hide(text: &str) -> String {
        redact(text, &context())
    }

    #[test]
    fn unix_paths_are_hidden_whole() {
        assert_eq!(
            hide("failed to open /home/alice/Documents/tax return.pdf: No such file"),
            "failed to open <path-1>: No such file"
        );
        assert_eq!(hide("cwd=/root/.config/x"), "cwd=<path-1>");
        assert_eq!(hide("/tmp/.tmpAb12/session.sock"), "<path-1>");
        assert_eq!(
            hide("see '/var/log/app.log' for more"),
            "see '<path-1>' for more"
        );
        assert_eq!(hide("(/usr/lib/x.so)"), "(<path-1>)");
        assert_eq!(hide("a,/b/c"), "a,<path-1>");
        assert_eq!(
            hide("opened file:///home/bob/x.txt ok"),
            "opened file://<path-1>"
        );
    }

    #[test]
    fn a_path_in_quotes_ends_at_the_closing_quote() {
        assert_eq!(
            hide("could not read \"/mnt/My Disk/a: b/c.txt\" because it moved"),
            "could not read \"<path-1>\" because it moved"
        );
        assert_eq!(
            hide("`/opt/a b` and '/opt/c d' too"),
            "`<path-1>` and '<path-2>' too"
        );
    }

    #[test]
    fn the_same_path_keeps_its_number() {
        assert_eq!(hide("/a/b then /c/d then /a/b"), "<path-1>");
        assert_eq!(
            hide("x=\"/a/b\" y=\"/c/d\" z=\"/a/b\""),
            "x=\"<path-1>\" y=\"<path-2>\" z=\"<path-1>\""
        );
    }

    #[test]
    fn windows_paths_are_hidden_whole() {
        let hide = |t: &str| redact(t, &windows_context());
        assert_eq!(
            hide(r"cannot open C:\Users\Alice\AppData\Local\Bytescape\x.db: access denied"),
            "cannot open <path-1>: access denied"
        );
        assert_eq!(hide(r"D:\Data\Photos 2024\a.jpg"), "<path-1>");
        assert_eq!(hide("e:/tmp/x"), "<path-1>");
        assert_eq!(hide(r"\\server\share\folder\file.txt"), "<path-1>");
        assert_eq!(hide(r"\\?\C:\Users\bob\x"), "<path-1>");
        assert_eq!(hide(r"\\.\pipe\bytescape-0123"), "<path-1>");
        assert_eq!(
            hide(r#"Os { path: "C:\\Users\\bob\\x y", kind: NotFound }"#),
            r#"Os { path: "<path-1>", kind: NotFound }"#
        );
        assert_eq!(hide(r#"Err("\\\\server\\share\\f")"#), r#"Err("<path-1>")"#);
        assert_eq!(hide(r#"{ "\\\\?\\C:\\Users\\bob" }"#), r#"{ "<path-1>" }"#);
        assert_eq!(
            hide(r"in C:\Program Files (x86)\Foo\foo.exe (exit 1)"),
            "in <path-1>"
        );
    }

    #[test]
    fn the_home_folder_is_hidden_wherever_it_is() {
        assert_eq!(hide("HOME is /home/alice"), "HOME is <path-1>");
        assert_eq!(hide("in:/home/alice/y"), "in:<path-1>");
        // A relative path that happens to contain the name is not the home
        // folder; only the name is hidden.
        assert_eq!(hide("x/home/alice/y"), "x/home/<user>/y");
        // Longer names that merely start with the home folder are another path.
        assert_eq!(hide("/home/alicia/x"), "<path-1>");
        let odd = Context {
            home: Some("/data/people/al".to_owned()),
            user: None,
        };
        assert_eq!(redact("[/data/people/al/notes]", &odd), "[<path-1>]");
        assert_eq!(redact("in:/data/people/al/notes", &odd), "in:<path-1>");
    }

    #[test]
    fn the_user_name_is_hidden_as_a_whole_word() {
        assert_eq!(
            hide("user alice and ALICE and Alice_2 and malice and alice."),
            "user <user> and <user> and Alice_2 and malice and <user>."
        );
        assert_eq!(hide(r"DESKTOP-1\alice"), r"DESKTOP-1\<user>");
        let nobody = Context::default();
        assert_eq!(redact("user alice", &nobody), "user alice");
        // Too short or too common to count as a name.
        for user in ["a", "root", "ROOT", "", " "] {
            let context = Context {
                home: None,
                user: Some(user.to_owned()),
            };
            assert_eq!(redact("a root b", &context), "a root b", "{user:?}");
        }
    }

    #[test]
    fn things_that_are_not_absolute_paths_stay() {
        for same in [
            "src/main.rs:10:5",
            "crates/bs-platform/src/lib.rs",
            "https://github.com/recursive-ai-oss/bytescape/issues",
            "and/or w/o 1/2 50%/s a / b",
            "C:",
            "Note:/x",
            "<alloc::vec::Vec<T> as core::ops::Index>::index",
            "// comment",
            "/",
            "/ ",
            "",
        ] {
            assert_eq!(hide(same), same, "{same:?}");
        }
    }

    #[test]
    fn a_path_in_a_panic_message_is_hidden() {
        let text = "Panic:\n    called `Result::unwrap()` on an `Err` value: Custom { error: \"cannot read /home/alice/Pictures/holiday photos/a.png\" }\n    second line /tmp/x";
        let got = hide(text);
        assert!(!got.contains("alice") && !got.contains("holiday") && !got.contains("/tmp"));
        assert!(got.contains("cannot read <path-1>"), "{got}");
        assert!(got.contains("second line <path-2>"), "{got}");
    }

    #[test]
    fn backtrace_sources_keep_their_file_name_and_line() {
        let text = "Location: /home/alice/src/bytescape/crates/bs-ui/src/app.rs:120:9\n\nBacktrace:\n   0: bs_ui::app::run\n             at /home/alice/src/bytescape/crates/bs-ui/src/app.rs:120:9\n   1: std::rt::lang_start\n             at /rustc/abc123/library/std/src/rt.rs:199:18\n   2: libfoo\n             at /usr/lib/x86_64-linux-gnu/libfoo.so.1\n   3: tool\n             at C:\\Users\\Alice\\.cargo\\registry\\src\\x-1.0\\src\\lib.rs:7:1\n\nRecent actions:\n  +12 ms  at /home/alice/secret/readme.rs";
        let got = redact(text, &context());
        let expected = "Location: <source>/app.rs:120:9\n\nBacktrace:\n   0: bs_ui::app::run\n             at <source>/app.rs:120:9\n   1: std::rt::lang_start\n             at <source>/rt.rs:199:18\n   2: libfoo\n             at <path-1>\n   3: tool\n             at <source>/lib.rs:7:1\n\nRecent actions:\n  +12 ms  at <path-2>";
        assert_eq!(got, expected);
    }

    #[test]
    fn text_that_is_not_utf8_was_already_made_valid_and_still_works() {
        // A report read from disk goes through a lossy conversion first.
        let raw = b"opened /home/al\xFFice/x\xC3\nand /tmp/\xE2\x82 ok\nalice";
        let text = String::from_utf8_lossy(raw);
        let got = hide(&text);
        assert_eq!(got, "opened <path-1>\nand <path-2>\n<user>");
        // Non-ASCII around the marks.
        assert_eq!(hide("é/home/alice/x"), "é/home/<user>/x");
        // Curly quotes and other punctuation outside ASCII may precede a path.
        assert_eq!(hide("“/home/alice/ü ö” ok"), "“<path-1>");
        assert_eq!(hide("Zoë alice"), "Zoë <user>");
    }

    #[test]
    fn line_endings_and_blank_lines_survive() {
        assert_eq!(hide("a /x/y\r\nb /z\n\nc"), "a <path-1>\r\nb <path-2>\n\nc");
        assert_eq!(hide("tab\t/x/y\tafter"), "tab\t<path-1>\tafter");
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn nothing_panics_and_no_path_survives(
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("/".to_owned()),
                    Just("\\".to_owned()),
                    Just("C:".to_owned()),
                    Just(" ".to_owned()),
                    Just("\"".to_owned()),
                    Just("(".to_owned()),
                    Just(":".to_owned()),
                    Just("\n".to_owned()),
                    Just("alice".to_owned()),
                    Just("é".to_owned()),
                    "[a-z0-9._ -]{0,6}",
                    any::<char>().prop_map(|c| c.to_string()),
                ],
                0..40,
            )
        ) {
            let text: String = parts.concat();
            let got = redact(&text, &context());
            // A placeholder is longer than a two-byte path, but not unboundedly.
            prop_assert!(got.len() <= text.len() * 6 + 64);
        }

        #[test]
        fn text_without_slashes_colons_or_names_is_unchanged(text in "[a-zA-Z0-9 .,()\\[\\]\n-]{0,80}") {
            let context = Context { home: Some("/home/alice".to_owned()), user: Some("zzzzqq".to_owned()) };
            prop_assert_eq!(redact(&text, &context), text);
        }
    }
}
