//! Reading a program's registered uninstall command safely.
//!
//! Windows stores one command-line string per installed program. It is never
//! handed to a shell here. The string is split into the executable and its
//! arguments by the rules Windows itself uses, the executable is checked, and
//! only then can it be started: by its absolute path, with the arguments as a
//! list. `msiexec` strings are read separately and rebuilt from the product
//! code alone, so nothing else in them can reach the command.

/// A parsed uninstall command.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UninstallCommand {
    /// Start `exe` (an absolute drive path) with `args`.
    Program { exe: String, args: Vec<String> },
    /// Remove a product with Windows Installer: `msiexec.exe /X{code}`. The
    /// program is the system's `msiexec.exe`, whatever the registered string
    /// said.
    Msi {
        /// The product code in braces, in upper case.
        product_code: String,
    },
}

impl UninstallCommand {
    /// The executable, for a command that names one. `None` for a Windows
    /// Installer removal, which always runs the system's `msiexec.exe`.
    pub fn executable(&self) -> Option<&str> {
        match self {
            Self::Program { exe, .. } => Some(exe),
            Self::Msi { .. } => None,
        }
    }

    /// The arguments, one per item.
    pub fn arguments(&self) -> Vec<String> {
        match self {
            Self::Program { args, .. } => args.clone(),
            Self::Msi { product_code } => vec![format!("/X{product_code}")],
        }
    }
}

/// Why a registered string is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UninstallParseError {
    Empty,
    /// Too long, or has a NUL or other control character.
    Unusable,
    /// No executable could be told apart: an unclosed quote, or no `.exe`.
    NoExecutable,
    /// The executable is not an absolute drive path (a relative name would be
    /// searched for, a network path would load from the network).
    NotAbsolute,
    /// The executable is a command interpreter or script host, which would
    /// run whatever follows as a command.
    Interpreter,
    /// A Windows Installer string that is not a plain removal of one product.
    UnsupportedInstaller,
    TooManyArguments,
}

const MAX_CHARS: usize = 32 * 1024;
const MAX_ARGS: usize = 64;
const MAX_ARG_CHARS: usize = 4096;

/// Programs that treat what follows as a command or a script.
const INTERPRETERS: &[&str] = &[
    "cmd.exe",
    "command.com",
    "powershell.exe",
    "pwsh.exe",
    "powershell_ise.exe",
    "wscript.exe",
    "cscript.exe",
    "mshta.exe",
    "wsl.exe",
    "bash.exe",
    "sh.exe",
    "conhost.exe",
];

/// Parses an `UninstallString` value.
pub fn parse_uninstall_string(text: &str) -> Result<UninstallCommand, UninstallParseError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(UninstallParseError::Empty);
    }
    if text.chars().count() > MAX_CHARS || text.chars().any(char::is_control) {
        return Err(UninstallParseError::Unusable);
    }
    let (exe, rest) = split_executable(text)?;
    let args = split_arguments(rest)?;
    let name = exe_file_name(exe).to_ascii_lowercase();

    if name == "msiexec.exe" {
        // Whatever the string said about where `msiexec.exe` is, the system's
        // own is used; so only the argument matters.
        return parse_msi(&args);
    }
    let exe = absolute_drive_path(exe)?;
    let name = exe_file_name(&exe).to_ascii_lowercase();
    if INTERPRETERS.contains(&name.as_str()) {
        return Err(UninstallParseError::Interpreter);
    }
    Ok(UninstallCommand::Program { exe, args })
}

/// Splits the executable off the front. A quoted executable ends at the next
/// quote. An unquoted one ends at the first place where the text so far ends in
/// `.exe` and a space follows (or the text ends), as Windows searches.
fn split_executable(text: &str) -> Result<(&str, &str), UninstallParseError> {
    if let Some(quoted) = text.strip_prefix('"') {
        let end = quoted.find('"').ok_or(UninstallParseError::NoExecutable)?;
        let exe = &quoted[..end];
        if exe.is_empty() {
            return Err(UninstallParseError::NoExecutable);
        }
        return Ok((exe, &quoted[end + 1..]));
    }
    let mut ends: Vec<usize> = text
        .char_indices()
        .filter(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .collect();
    ends.push(text.len());
    for end in ends {
        let prefix = &text[..end];
        if prefix.len() >= 4 && prefix[prefix.len() - 4..].eq_ignore_ascii_case(".exe") {
            return Ok((prefix, &text[end..]));
        }
    }
    Err(UninstallParseError::NoExecutable)
}

fn exe_file_name(exe: &str) -> &str {
    exe.rsplit(['\\', '/']).next().unwrap_or(exe)
}

/// The executable as a normalised absolute drive path, or an error.
fn absolute_drive_path(exe: &str) -> Result<String, UninstallParseError> {
    let path = exe.replace('/', "\\");
    let bytes = path.as_bytes();
    let drive =
        bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    // `%` would be an environment variable the registry did not expand.
    let odd = path.contains(['%', '<', '>', '|', '*', '?', '"', '\0'])
        || path.get(2..).is_none_or(|rest| rest.contains(':'));
    let dotdot = path.split('\\').any(|part| part == "..");
    if !drive || odd || dotdot || path.contains("\\\\") {
        return Err(UninstallParseError::NotAbsolute);
    }
    if !path.to_ascii_lowercase().ends_with(".exe") {
        return Err(UninstallParseError::NoExecutable);
    }
    Ok(path)
}

/// Splits a command line into arguments the way `CommandLineToArgvW` does for
/// everything after the program name.
pub(crate) fn split_arguments(text: &str) -> Result<Vec<String>, UninstallParseError> {
    let mut args = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut arg = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() && !quoted {
                break;
            }
            chars.next();
            match c {
                '\\' => {
                    let mut slashes = 1;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        slashes += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        // Pairs of backslashes before a quote are one each;
                        // an odd one out makes the quote a plain character.
                        arg.extend(std::iter::repeat_n('\\', slashes / 2));
                        if slashes % 2 == 1 {
                            chars.next();
                            arg.push('"');
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', slashes));
                    }
                }
                '"' => {
                    if quoted && chars.peek() == Some(&'"') {
                        // A doubled quote inside quotes is one quote.
                        chars.next();
                        arg.push('"');
                    } else {
                        quoted = !quoted;
                    }
                }
                other => arg.push(other),
            }
        }
        if arg.chars().count() > MAX_ARG_CHARS {
            return Err(UninstallParseError::Unusable);
        }
        args.push(arg);
        if args.len() > MAX_ARGS {
            return Err(UninstallParseError::TooManyArguments);
        }
    }
    Ok(args)
}

/// `/X{code}`, `/I{code}`, `/X {code}`, `/uninstall {code}`: the removal of
/// one product. `/I` is how many strings register a repair-or-remove entry;
/// it is read as the removal this action stands for.
fn parse_msi(args: &[String]) -> Result<UninstallCommand, UninstallParseError> {
    let unsupported = UninstallParseError::UnsupportedInstaller;
    let code = match args {
        [one] => {
            let lower = one.to_ascii_lowercase();
            if lower.starts_with("/x") || lower.starts_with("/i") {
                &one[2..]
            } else {
                return Err(unsupported);
            }
        }
        [flag, code]
            if ["/x", "/i", "/uninstall"].contains(&flag.to_ascii_lowercase().as_str()) =>
        {
            code
        }
        _ => return Err(unsupported),
    };
    if is_product_code(code) {
        Ok(UninstallCommand::Msi {
            product_code: code.to_ascii_uppercase(),
        })
    } else {
        Err(unsupported)
    }
}

/// `{8-4-4-4-12}` hexadecimal digits in braces.
fn is_product_code(text: &str) -> bool {
    let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) else {
        return false;
    };
    let groups: Vec<&str> = inner.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Joins arguments into one command-line string that `CommandLineToArgvW`
/// splits back into the same arguments. Windows' shell takes its arguments as
/// one string; no shell reads it as a command line.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn quote_arguments(args: &[String]) -> String {
    let mut line = String::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        let plain = !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']);
        if plain {
            line.push_str(arg);
            continue;
        }
        line.push('"');
        let mut slashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                    line.push('"');
                    slashes = 0;
                }
                other => {
                    line.extend(std::iter::repeat_n('\\', slashes));
                    line.push(other);
                    slashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', slashes * 2));
        line.push('"');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(exe: &str, args: &[&str]) -> UninstallCommand {
        UninstallCommand::Program {
            exe: exe.into(),
            args: args.iter().map(|a| (*a).into()).collect(),
        }
    }

    fn msi(code: &str) -> UninstallCommand {
        UninstallCommand::Msi {
            product_code: code.into(),
        }
    }

    const CODE: &str = "{0A1B2C3D-0000-4000-8000-0123456789AB}";

    #[test]
    fn quoted_and_unquoted_executables_with_arguments() {
        assert_eq!(
            parse_uninstall_string(
                r#""C:\Program Files\Foo\unins000.exe" /SILENT /LOG="C:\a b\l.txt""#
            ),
            Ok(program(
                r"C:\Program Files\Foo\unins000.exe",
                &["/SILENT", r"/LOG=C:\a b\l.txt"]
            ))
        );
        assert_eq!(
            parse_uninstall_string(r"C:\Program Files\Foo\uninstall.exe /S"),
            Ok(program(r"C:\Program Files\Foo\uninstall.exe", &["/S"]))
        );
        assert_eq!(
            parse_uninstall_string(r"C:\Tools\un.exe"),
            Ok(program(r"C:\Tools\un.exe", &[]))
        );
        assert_eq!(
            parse_uninstall_string("  \"D:\\x\\u.EXE\"  "),
            Ok(program(r"D:\x\u.EXE", &[]))
        );
        // Forward slashes are read as backslashes.
        assert_eq!(
            parse_uninstall_string("\"C:/Foo/u.exe\" -u"),
            Ok(program(r"C:\Foo\u.exe", &["-u"]))
        );
    }

    #[test]
    fn windows_installer_strings_are_rebuilt_from_the_product_code() {
        for text in [
            format!("MsiExec.exe /X{CODE}"),
            format!("MsiExec.exe /I{CODE}"),
            format!("msiexec.exe /uninstall {CODE}"),
            format!(r"C:\Windows\System32\msiexec.exe /x{CODE}"),
            format!(
                r#""C:\anywhere\else\MSIEXEC.EXE" /X{}"#,
                CODE.to_lowercase()
            ),
        ] {
            assert_eq!(parse_uninstall_string(&text), Ok(msi(CODE)), "{text}");
        }
        // Without `.exe` there is no executable to find.
        assert!(parse_uninstall_string(&format!("msiexec /x {CODE}")).is_err());
        assert_eq!(msi(CODE).arguments(), [format!("/X{CODE}")]);
        assert_eq!(msi(CODE).executable(), None);
    }

    #[test]
    fn windows_installer_strings_with_anything_else_are_not_used() {
        for text in [
            "MsiExec.exe".to_string(),
            "MsiExec.exe /X".to_string(),
            "MsiExec.exe /X{not-a-guid}".to_string(),
            format!("MsiExec.exe /X{CODE} /qn"),
            format!("MsiExec.exe /X{CODE} && del x"),
            format!("MsiExec.exe /p{CODE}"),
            format!("MsiExec.exe /a {CODE}"),
            format!("MsiExec.exe /X{{{}}}", "0".repeat(36)),
            "MsiExec.exe /X{0A1B2C3D-0000-4000-8000-0123456789AB}x".to_string(),
            "MsiExec.exe /X{0A1B2C3D-0000-4000-8000-0123456789A}".to_string(),
        ] {
            assert_eq!(
                parse_uninstall_string(&text),
                Err(UninstallParseError::UnsupportedInstaller),
                "{text}"
            );
        }
    }

    #[test]
    fn what_cannot_be_started_safely_is_refused() {
        use UninstallParseError::*;
        for (text, why) in [
            ("", Empty),
            ("   ", Empty),
            (r#""C:\x\u.exe /S"#, NoExecutable),
            (r#""" /S"#, NoExecutable),
            ("just words", NoExecutable),
            (r"cmd.exe /c rd /s /q C:\x", NotAbsolute),
            (r"C:\x\setup.msi /x", NoExecutable),
            (r"uninstall.exe /S", NotAbsolute),
            (r"..\u.exe", NotAbsolute),
            (r"\Windows\u.exe", NotAbsolute),
            (r"C:u.exe", NotAbsolute),
            (r"\\server\share\u.exe /S", NotAbsolute),
            (r"\\?\C:\x\u.exe", NotAbsolute),
            (r"C:\x\..\y\u.exe", NotAbsolute),
            (r"%ProgramFiles%\Foo\u.exe", NotAbsolute),
            (r"C:\x\u.exe:stream.exe", NotAbsolute),
            (r#""C:\Windows\System32\cmd.exe" /c del x"#, Interpreter),
            (
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -File x.ps1",
                Interpreter,
            ),
            (r"C:\w\WSCRIPT.EXE x.vbs", Interpreter),
            ("C:\\x\\u.exe\n/S", Unusable),
            ("C:\\x\\u.exe\0", Unusable),
            ("C:\\x\\u.exe \u{7}", Unusable),
        ] {
            assert_eq!(parse_uninstall_string(text), Err(why), "{text:?}");
        }
        assert!(parse_uninstall_string(&format!("C:\\x\\u.exe {}", "a ".repeat(100))).is_err());
        assert!(parse_uninstall_string(&format!("C:\\x\\u.exe {}", "a".repeat(5000))).is_err());
        assert!(parse_uninstall_string(&"x".repeat(40_000)).is_err());
    }

    #[test]
    fn nothing_in_the_string_becomes_a_second_command() {
        // Shell syntax after the executable is just text in an argument.
        let parsed =
            parse_uninstall_string(r#""C:\Foo\u.exe" /S & calc.exe | more ; del *"#).unwrap();
        assert_eq!(
            parsed.arguments(),
            ["/S", "&", "calc.exe", "|", "more", ";", "del", "*"]
        );
        // Quoting rebuilds one command line with no loose operator.
        let UninstallCommand::Program { args, .. } = &parsed else {
            panic!("expected a program");
        };
        assert_eq!(quote_arguments(args), "/S & calc.exe | more ; del *");
        let one = vec!["a & b".to_string()];
        assert_eq!(quote_arguments(&one), "\"a & b\"");
    }

    #[test]
    fn arguments_split_like_the_windows_rules() {
        let split = |t: &str| split_arguments(t).unwrap();
        assert_eq!(split(""), Vec::<String>::new());
        assert_eq!(split("a b  c"), ["a", "b", "c"]);
        assert_eq!(split(r#"a "b c" d"#), ["a", "b c", "d"]);
        assert_eq!(split(r#"a"b c"d"#), ["ab cd"]);
        assert_eq!(split(r#""" x"#), ["", "x"]);
        assert_eq!(split(r#"a\"b"#), ["a\"b"]);
        assert_eq!(split(r#"a\\"b c""#), ["a\\b c"]);
        assert_eq!(split(r#"a\\\"b"#), ["a\\\"b"]);
        assert_eq!(split(r"a\b\\c"), [r"a\b\\c"]);
        assert_eq!(split(r#""a ""quoted"" b""#), ["a \"quoted\" b"]);
        assert_eq!(split("\t a\t"), ["a"]);
    }

    #[test]
    fn quoting_and_splitting_round_trip() {
        for args in [
            vec!["plain"],
            vec!["with space", "x"],
            vec![r#"quote"inside"#],
            vec![r"trailing\"],
            vec![r"two\\", "x"],
            vec![r#"back\"slash"#],
            vec![""],
            vec!["a", "", "b"],
            vec!["tab\there"],
            vec![r"C:\Program Files\x\"],
            vec!["/LOG=C:\\a b\\l.txt"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let line = quote_arguments(&args);
            assert_eq!(split_arguments(&line).unwrap(), args, "{line}");
        }
    }
}
