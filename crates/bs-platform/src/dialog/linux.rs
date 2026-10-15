//! The folder dialog on Linux: `zenity`, or `kdialog` on KDE.

use super::DialogError;
use crate::launch::LaunchError;
use crate::launch::linux::{BusUse, Env, tool};
use crate::launch::spawn::Spec;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const TITLE: &str = "Choose a folder";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    Zenity,
    Kdialog,
}

impl Dialog {
    fn program(self) -> &'static str {
        match self {
            Self::Zenity => "zenity",
            Self::Kdialog => "kdialog",
        }
    }

    /// `start` is an absolute folder, so neither tool can read it as an option.
    fn args(self, start: &Path) -> Vec<OsString> {
        match self {
            Self::Zenity => {
                let mut start_option = OsString::from("--filename=");
                start_option.push(start);
                if !start.as_os_str().as_bytes().ends_with(b"/") {
                    start_option.push("/");
                }
                vec![
                    "--file-selection".into(),
                    "--directory".into(),
                    format!("--title={TITLE}").into(),
                    start_option,
                ]
            }
            Self::Kdialog => vec![
                "--title".into(),
                TITLE.into(),
                "--getexistingdirectory".into(),
                start.as_os_str().to_os_string(),
            ],
        }
    }
}

pub(crate) fn pick_folder(env: &Env, start: &Path) -> Result<Option<PathBuf>, DialogError> {
    let order = if env.desktop.to_ascii_lowercase().contains("kde") {
        [Dialog::Kdialog, Dialog::Zenity]
    } else {
        [Dialog::Zenity, Dialog::Kdialog]
    };
    let start = if start.is_absolute() && start.is_dir() {
        start
    } else {
        Path::new("/")
    };
    for dialog in order {
        let spec: Spec = match tool(env, dialog.program(), BusUse::Maybe) {
            Ok(spec) => spec,
            Err(LaunchError::SessionBusNotLocal) => return Err(DialogError::SessionBusNotLocal),
            Err(_) => continue,
        };
        // The user may take as long as they like.
        let Ok(output) = spec
            .args(dialog.args(start))
            .run("show the folder dialog", None)
        else {
            continue;
        };
        match output.status.code() {
            Some(0) => return parse_answer(&output.stdout).map(Some),
            // The tool ran and the user closed the dialog.
            Some(1) => return Ok(None),
            // The tool is broken or could not show a window: try the other.
            _ => {}
        }
    }
    Err(DialogError::Unavailable)
}

/// A tool's answer: one absolute path and a newline, naming a folder.
fn parse_answer(output: &[u8]) -> Result<PathBuf, DialogError> {
    let text = output.strip_suffix(b"\n").unwrap_or(output);
    if text.is_empty() || text.contains(&0) || text.contains(&b'\n') || text[0] != b'/' {
        return Err(DialogError::BadAnswer);
    }
    let path = PathBuf::from(OsString::from_vec(text.to_vec()));
    if path.is_dir() {
        Ok(path)
    } else {
        Err(DialogError::BadAnswer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::spawn::testing::{recorder, script};
    use crate::testutil::Fixture;

    fn env(fx: &Fixture, desktop: &str) -> Env {
        Env {
            path: fx.path().join("bin").into_os_string(),
            bus_address: Some(OsString::from("unix:path=/run/user/1000/bus")),
            runtime_dir: None,
            desktop: desktop.into(),
            socket_exists: |_| true,
            wayland: false,
        }
    }

    fn fixture(label: &str) -> Fixture {
        let fx = Fixture::new(label);
        fx.mkdir("bin");
        fx.mkdir("start dir");
        fx.mkdir("chosen dir");
        fx
    }

    #[test]
    fn zenity_is_asked_for_a_folder_starting_where_told() {
        let fx = fixture("dialog-zenity");
        let out = fx.path().join("args.txt");
        let chosen = fx.path().join("chosen dir");
        script(
            &fx.path().join("bin"),
            "zenity",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'; printf '%s\\n' '{}'",
                out.display(),
                chosen.display()
            ),
        );
        let start = fx.path().join("start dir");
        let got = pick_folder(&env(&fx, "GNOME"), &start).unwrap();
        assert_eq!(got, Some(chosen));
        let args = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            args,
            format!(
                "--file-selection\n--directory\n--title=Choose a folder\n--filename={}/\n",
                start.display()
            )
        );
    }

    #[test]
    fn kde_prefers_kdialog_and_passes_the_folder_as_one_argument() {
        let fx = fixture("dialog-kdialog");
        let out = fx.path().join("args.txt");
        let chosen = fx.path().join("chosen dir");
        recorder(
            &fx.path().join("bin"),
            "zenity",
            &fx.path().join("zenity.txt"),
            0,
        );
        script(
            &fx.path().join("bin"),
            "kdialog",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'; printf '%s\\n' '{}'",
                out.display(),
                chosen.display()
            ),
        );
        let start = fx.path().join("start dir");
        assert_eq!(pick_folder(&env(&fx, "KDE"), &start).unwrap(), Some(chosen));
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            format!(
                "--title\nChoose a folder\n--getexistingdirectory\n{}\n",
                start.display()
            )
        );
        assert!(!fx.path().join("zenity.txt").exists());
    }

    #[test]
    fn a_start_that_is_not_a_folder_becomes_the_root() {
        let fx = fixture("dialog-start");
        let out = fx.path().join("args.txt");
        recorder(&fx.path().join("bin"), "kdialog", &out, 1);
        for start in ["--help", "relative", "/definitely/not/here"] {
            let got = pick_folder(&env(&fx, "KDE"), Path::new(start)).unwrap();
            assert_eq!(got, None);
            let args = std::fs::read_to_string(&out).unwrap();
            assert!(
                args.contains("--getexistingdirectory\n/\n"),
                "{start}: {args}"
            );
        }
    }

    #[test]
    fn closing_the_dialog_is_not_an_error() {
        let fx = fixture("dialog-cancel");
        script(&fx.path().join("bin"), "zenity", "exit 1");
        script(
            &fx.path().join("bin"),
            "kdialog",
            "echo should not run; exit 0",
        );
        assert_eq!(pick_folder(&env(&fx, ""), Path::new("/")).unwrap(), None);
    }

    #[test]
    fn a_tool_that_breaks_hands_over_to_the_other_one() {
        let fx = fixture("dialog-broken");
        let chosen = fx.path().join("chosen dir");
        script(&fx.path().join("bin"), "zenity", "exit 127");
        script(
            &fx.path().join("bin"),
            "kdialog",
            &format!("printf '%s\\n' '{}'", chosen.display()),
        );
        assert_eq!(
            pick_folder(&env(&fx, "GNOME"), Path::new("/")).unwrap(),
            Some(chosen)
        );
        script(&fx.path().join("bin"), "kdialog", "exit 2");
        assert!(matches!(
            pick_folder(&env(&fx, "GNOME"), Path::new("/")),
            Err(DialogError::Unavailable)
        ));
    }

    #[test]
    fn no_tool_is_unavailable() {
        let fx = fixture("dialog-none");
        assert!(matches!(
            pick_folder(&env(&fx, "GNOME"), Path::new("/")),
            Err(DialogError::Unavailable)
        ));
    }

    #[test]
    fn an_answer_must_be_an_existing_absolute_folder() {
        let fx = fixture("dialog-answer");
        let file = fx.path().join("bin/zenity");
        for answer in [
            "relative/dir",
            "/definitely/not/here",
            "",
            "-option",
            &format!("{}\\n{}", fx.path().display(), fx.path().display()),
        ] {
            script(
                &fx.path().join("bin"),
                "zenity",
                &format!("printf '%b' '{}'", answer.replace('\'', "")),
            );
            assert!(file.exists());
            let got = pick_folder(&env(&fx, "GNOME"), Path::new("/"));
            assert!(
                matches!(got, Err(DialogError::BadAnswer)),
                "{answer:?}: {got:?}"
            );
        }
        // A file is not a folder.
        script(
            &fx.path().join("bin"),
            "zenity",
            &format!("printf '%s\\n' '{}'", file.display()),
        );
        assert!(matches!(
            pick_folder(&env(&fx, "GNOME"), Path::new("/")),
            Err(DialogError::BadAnswer)
        ));
    }

    #[test]
    fn nothing_starts_when_the_session_bus_is_not_a_local_socket() {
        let fx = fixture("dialog-bus");
        let ran = fx.path().join("ran.txt");
        recorder(&fx.path().join("bin"), "zenity", &ran, 0);
        recorder(&fx.path().join("bin"), "kdialog", &ran, 0);
        let mut env = env(&fx, "GNOME");
        env.bus_address = Some(OsString::from("tcp:host=example.org,port=1"));
        assert!(matches!(
            pick_folder(&env, Path::new("/")),
            Err(DialogError::SessionBusNotLocal)
        ));
        assert!(!ran.exists());
    }
}
