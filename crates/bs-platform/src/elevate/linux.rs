//! Starting the helper with `pkexec`, over standard input and output.

use super::{ElevateError, Elevation, Launched, hex};
use bs_ipc::{ClientError, NONCE_LEN, PROTOCOL_VERSION, ReadError, WriteError};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Where the packages install the helper, and where the system's policy says
/// it is.
pub(super) const INSTALLED_HELPER: &str = "/usr/libexec/bytescape/bytescape-helper";

/// `pkexec` exits with 126 when the user dismissed the prompt or is not
/// allowed, and 127 when authentication failed.
const PKEXEC_NOT_AUTHORIZED: [i32; 2] = [126, 127];

/// The started `pkexec` process. When it is dropped the process is left to
/// finish by itself (the helper exits when its connection closes) and is
/// collected in the background so it does not linger as a zombie.
pub(crate) struct Guard {
    child: Option<Child>,
}

impl Guard {
    /// Says why the conversation did not start: the user's refusal if that is
    /// what `pkexec` reported, the error otherwise.
    pub(crate) fn explain(mut self, error: ClientError) -> ElevateError {
        let closed = matches!(
            error,
            ClientError::Disconnected
                | ClientError::Read(ReadError::Io(_))
                | ClientError::Write(WriteError::Io(_))
        );
        if closed && let Some(mut child) = self.child.take() {
            // The connection ended before any hello: `pkexec` or the helper has
            // already gone, so waiting is short.
            if let Ok(status) = child.wait()
                && let Some(code) = status.code()
            {
                if PKEXEC_NOT_AUTHORIZED.contains(&code) {
                    return ElevateError::Declined;
                }
                if code == 3 {
                    return ElevateError::PeerNotVerified("the helper refused to start");
                }
            }
        }
        ElevateError::Protocol(error)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = std::thread::Builder::new()
                .name("collect-helper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

/// A program found on the user's `PATH`: only absolute entries count, and the
/// result must be an executable regular file.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Whether this process has the superuser's rights.
fn is_superuser() -> bool {
    std::fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0)
        .unwrap_or(false)
}

pub(super) fn launch(
    elevation: &Elevation,
    mode: &'static str,
    nonce: [u8; NONCE_LEN],
) -> Result<Launched, ElevateError> {
    if is_superuser() {
        return Err(ElevateError::Unavailable(
            "this program is running with administrator rights",
        ));
    }
    let pkexec = elevation
        .pkexec
        .clone()
        .or_else(|| find_on_path("pkexec"))
        .ok_or(ElevateError::Unavailable("pkexec is not installed"))?;
    if !elevation.helper.is_absolute() || !elevation.helper.is_file() {
        return Err(ElevateError::Unavailable("the helper is not installed"));
    }
    let mut child = Command::new(pkexec)
        .arg(&elevation.helper)
        .arg(mode)
        .arg("--protocol")
        .arg(PROTOCOL_VERSION.to_string())
        .arg("--nonce")
        .arg(hex(&nonce))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ElevateError::Launch)?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        return Err(ElevateError::Launch(io::Error::other("no pipes")));
    };
    Ok(Launched {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        guard: Guard { child: Some(child) },
    })
}

/// Random bytes from the kernel.
pub(super) fn random_bytes(buf: &mut [u8]) -> Result<(), ElevateError> {
    use std::io::Read;
    // Read from the random device: it never blocks once the system has started.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buf))
        .map_err(ElevateError::Launch)
}
