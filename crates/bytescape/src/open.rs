//! Opening a link in the user's browser.
//!
//! Bytescape never connects to the network itself; a link is handed to the
//! operating system's opener, which runs as the normal user and decides what
//! to start.

use std::process::{Command, Stdio};

/// The program and arguments that open `url`. The address is one argument,
/// never part of a command line a shell would read.
pub fn opener_command(url: &str) -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        (
            "rundll32.exe",
            vec!["url.dll,FileProtocolHandler".into(), url.into()],
        )
    } else {
        ("xdg-open", vec![url.into()])
    }
}

/// Starts the opener and returns at once. A failure to start it is reported
/// to the caller, which has nothing better to do than ignore it.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let (program, args) = opener_command(url);
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the opener when it ends, without making the window wait.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_is_a_single_argument() {
        let url = "https://recursiveai.org/?a=b&c=d e";
        let (program, args) = opener_command(url);
        assert!(!program.is_empty());
        assert_eq!(args.last().map(String::as_str), Some(url));
        // Nothing is joined into a command line.
        assert!(args.iter().filter(|a| a.contains("recursiveai")).count() == 1);
    }

    #[test]
    fn the_link_target_is_the_project_site() {
        assert_eq!(bs_ui::chrome::LINK_URL, "https://recursiveai.org/");
    }
}
