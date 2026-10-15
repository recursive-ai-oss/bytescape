//! The helper's command line. The schema is fixed: the interface passes
//! exactly these arguments, in this order, and anything else ends the helper
//! before it does anything.
//!
//! ```text
//! bytescape-helper <scan|delete> --protocol 1 --nonce <32 hex digits>
//! bytescape-helper <scan|delete> --protocol 1 --nonce <32 hex digits>
//!                  --pipe \\.\pipe\bytescape-<32 hex digits> --parent <pid>   (Windows)
//! ```
//!
//! The first argument names the one kind of request this run may serve. The
//! system's elevation prompt can be told apart by it (a scan may be allowed to
//! keep its authorization for the session, a delete never is), so the helper
//! refuses any request of the other kind.

use bs_ipc::{NONCE_LEN, PROTOCOL_VERSION};
use std::ffi::OsString;

/// The kind of request a run serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Scan,
    Delete,
}

/// What the command line says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub mode: Mode,
    pub nonce: [u8; NONCE_LEN],
    /// The name of the pipe to connect to (Windows only).
    pub pipe: Option<String>,
    /// The process id of the interface that launched this run (Windows only).
    pub parent: Option<u32>,
}

/// The command line does not match the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage;

const PIPE_PREFIX: &str = r"\\.\pipe\bytescape-";

/// Parses the arguments after the program name. `windows` selects the schema.
pub fn parse(args: &[OsString], windows: bool) -> Result<Args, Usage> {
    let text: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or(Usage))
        .collect::<Result<_, _>>()?;
    let expected_len = if windows { 9 } else { 5 };
    if text.len() != expected_len {
        return Err(Usage);
    }
    let mut it = text.into_iter();
    let mode = match it.next() {
        Some("scan") => Mode::Scan,
        Some("delete") => Mode::Delete,
        _ => return Err(Usage),
    };
    if it.next() != Some("--protocol") || it.next() != Some(&PROTOCOL_VERSION.to_string()) {
        return Err(Usage);
    }
    if it.next() != Some("--nonce") {
        return Err(Usage);
    }
    let nonce = parse_nonce(it.next().ok_or(Usage)?)?;
    if !windows {
        return Ok(Args {
            mode,
            nonce,
            pipe: None,
            parent: None,
        });
    }
    if it.next() != Some("--pipe") {
        return Err(Usage);
    }
    let pipe = it.next().ok_or(Usage)?;
    if !valid_pipe_name(pipe) {
        return Err(Usage);
    }
    if it.next() != Some("--parent") {
        return Err(Usage);
    }
    let parent = it.next().ok_or(Usage)?;
    if parent.is_empty() || !parent.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Usage);
    }
    let parent: u32 = parent.parse().map_err(|_| Usage)?;
    Ok(Args {
        mode,
        nonce,
        pipe: Some(pipe.to_owned()),
        parent: Some(parent),
    })
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Exactly `\\.\pipe\bytescape-` and 32 lower-case hex digits: no other pipe,
/// device or file can be named.
pub fn valid_pipe_name(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX)
        .is_some_and(|rest| rest.len() == NONCE_LEN.saturating_mul(2) && is_lower_hex(rest))
}

fn parse_nonce(text: &str) -> Result<[u8; NONCE_LEN], Usage> {
    if text.len() != NONCE_LEN.saturating_mul(2) || !is_lower_hex(text) {
        return Err(Usage);
    }
    let mut out = [0u8; NONCE_LEN];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let digit = |b: u8| -> Option<u8> {
            match b {
                b'0'..=b'9' => b.checked_sub(b'0'),
                b'a'..=b'f' => b.checked_sub(b'a')?.checked_add(10),
                _ => None,
            }
        };
        let (hi, lo) = match pair {
            [hi, lo] => (digit(*hi).ok_or(Usage)?, digit(*lo).ok_or(Usage)?),
            _ => return Err(Usage),
        };
        *slot = hi
            .checked_mul(16)
            .and_then(|h| h.checked_add(lo))
            .ok_or(Usage)?;
    }
    Ok(out)
}

/// The nonce as the 32 hex digits the command line carries.
pub fn nonce_hex(nonce: &[u8; NONCE_LEN]) -> String {
    nonce.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    const HEX: &str = "01234567890123450123456789012345";

    #[test]
    fn the_linux_schema_is_accepted() {
        let args = parse(&os(&["scan", "--protocol", "1", "--nonce", HEX]), false).unwrap();
        assert_eq!(args.mode, Mode::Scan);
        assert_eq!(args.nonce[0], 0x01);
        assert_eq!(args.nonce[1], 0x23);
        assert_eq!(args.nonce[15], 0x45);
        assert_eq!(nonce_hex(&args.nonce), HEX);
        assert!(args.pipe.is_none() && args.parent.is_none());
        let delete = parse(&os(&["delete", "--protocol", "1", "--nonce", HEX]), false).unwrap();
        assert_eq!(delete.mode, Mode::Delete);
    }

    #[test]
    fn the_windows_schema_is_accepted() {
        let pipe = format!(r"\\.\pipe\bytescape-{HEX}");
        let args = parse(
            &os(&[
                "delete",
                "--protocol",
                "1",
                "--nonce",
                HEX,
                "--pipe",
                &pipe,
                "--parent",
                "4242",
            ]),
            true,
        )
        .unwrap();
        assert_eq!(args.pipe.as_deref(), Some(pipe.as_str()));
        assert_eq!(args.parent, Some(4242));
    }

    #[test]
    fn anything_else_is_refused() {
        let upper = "ABCD".repeat(8);
        let ok = ["scan", "--protocol", "1", "--nonce", HEX];
        let cases: Vec<Vec<&str>> = vec![
            vec![],
            vec!["scan"],
            // Wrong mode, flags out of order, other versions, extra words.
            vec!["both", "--protocol", "1", "--nonce", HEX],
            vec!["scan", "--nonce", HEX, "--protocol", "1"],
            vec!["scan", "--protocol", "2", "--nonce", HEX],
            vec!["scan", "--protocol", "01", "--nonce", HEX],
            vec!["scan", "--protocol", "1", "--nonce", &HEX[1..]],
            vec![
                "scan",
                "--protocol",
                "1",
                "--nonce",
                "0123456789012345 0123456789012345",
            ],
            vec!["scan", "--protocol", "1", "--nonce", &upper],
            vec![
                "scan",
                "--protocol",
                "1",
                "--nonce",
                "g0112233445566778899aabbccddeeff",
            ],
            vec!["scan", "--protocol", "1", "--nonce", HEX, "--extra"],
            vec!["--protocol", "1", "--nonce", HEX, "scan"],
        ];
        for case in cases {
            assert_eq!(parse(&os(&case), false), Err(Usage), "{case:?}");
        }
        // The Windows schema is not the Linux one and the reverse.
        assert_eq!(parse(&os(&ok), true), Err(Usage));
        let pipe = format!(r"\\.\pipe\bytescape-{HEX}");
        let windows = [
            "scan",
            "--protocol",
            "1",
            "--nonce",
            HEX,
            "--pipe",
            &pipe,
            "--parent",
            "1",
        ];
        assert_eq!(parse(&os(&windows), false), Err(Usage));
        assert!(parse(&os(&windows), true).is_ok());
    }

    #[test]
    fn only_a_pipe_of_the_exact_shape_can_be_named() {
        let good = format!(r"\\.\pipe\bytescape-{HEX}");
        assert!(valid_pipe_name(&good));
        for bad in [
            r"\\.\pipe\bytescape-".to_owned(),
            format!(r"\\.\pipe\bytescape-{HEX}x"),
            format!(r"\\.\pipe\other-{HEX}"),
            format!(r"\\.\pipe\bytescape-{HEX}\..\evil"),
            format!(r"\\?\pipe\bytescape-{HEX}"),
            format!(r"\\server\pipe\bytescape-{HEX}"),
            format!(r"C:\bytescape-{HEX}"),
            format!(r"\\.\pipe\bytescape-{}", "ABCD".repeat(8)),
            String::new(),
        ] {
            assert!(!valid_pipe_name(&bad), "{bad}");
        }
    }

    #[test]
    fn a_bad_parent_id_is_refused() {
        let pipe = format!(r"\\.\pipe\bytescape-{HEX}");
        for parent in ["", "-1", "0x10", "99999999999", "1 ", "+5"] {
            let line = [
                "scan",
                "--protocol",
                "1",
                "--nonce",
                HEX,
                "--pipe",
                &pipe,
                "--parent",
                parent,
            ];
            assert_eq!(parse(&os(&line), true), Err(Usage), "{parent:?}");
        }
    }

    #[test]
    fn arguments_that_are_not_text_are_refused() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut args = os(&["scan", "--protocol", "1", "--nonce", HEX]);
            args[0] = OsString::from_vec(vec![0xff, 0xfe]);
            assert_eq!(parse(&args, false), Err(Usage));
        }
    }
}
