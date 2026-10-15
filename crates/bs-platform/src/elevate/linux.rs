//! Starting the helper with `pkexec`, over standard input and output.

use super::{ElevateError, Elevation, Launched, Unavailable, hex};
use bs_ipc::{ClientError, NONCE_LEN, PROTOCOL_VERSION, ReadError, WriteError};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Where the packages install the helper, and where the system's policy says
/// it is.
pub(super) const INSTALLED_HELPER: &str = "/usr/libexec/bytescape/bytescape-helper";

/// Where the helper is inside a snap, relative to the snap's folder.
const SNAP_HELPER: &str = "usr/libexec/bytescape/bytescape-helper";

/// Who must own the helper and every folder on its way, and the top folder
/// the check climbs to. For the system, the administrator and `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Trust {
    owner: u32,
    top: PathBuf,
}

/// What the choice of helper depends on. Read from the process once; tests
/// give their own.
pub(super) struct Facts {
    /// `$SNAP`, when it is set to an absolute path.
    snap: Option<PathBuf>,
    /// Whether this program runs from an AppImage (`$APPIMAGE` is set).
    appimage: bool,
    trust: Trust,
}

impl Facts {
    fn from_process() -> Self {
        Self {
            snap: std::env::var_os("SNAP")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            appimage: std::env::var_os("APPIMAGE").is_some_and(|value| !value.is_empty()),
            trust: Trust {
                owner: 0,
                top: PathBuf::from("/"),
            },
        }
    }
}

/// The helper this system provides.
pub(super) fn system() -> Result<Elevation, ElevateError> {
    system_in(&Facts::from_process())
}

pub(super) fn system_in(facts: &Facts) -> Result<Elevation, ElevateError> {
    let unavailable = |why| Err(ElevateError::Unavailable(why));
    // An AppImage is a filesystem mounted for one account; the administrator
    // cannot enter it, so a helper inside it cannot be started.
    if facts.appimage {
        return unavailable(Unavailable::AppImage);
    }
    let helper = match &facts.snap {
        Some(snap) => {
            // Only a snap with classic confinement can start programs with
            // administrator rights, and then its own helper is the one to
            // start. The environment is the user's to set, so the helper is
            // checked like any other: it has to be the administrator's file
            // in the administrator's folders. What is kept is the checked
            // path itself, with every link resolved: a path that runs through
            // a folder the user can write to could be pointed somewhere else
            // after the check.
            if !snap_is_classic(snap) {
                return unavailable(Unavailable::SnapConfined);
            }
            check_trusted(&snap.join(SNAP_HELPER), &facts.trust)?
        }
        None => PathBuf::from(INSTALLED_HELPER),
    };
    Ok(Elevation {
        helper,
        pkexec: None,
        trusted: Some(facts.trust.clone()),
    })
}

/// Whether the snap whose folder is `snap` says it has classic confinement.
/// Anything else, including a missing or unreadable description, is treated as
/// confined.
fn snap_is_classic(snap: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(snap.join("meta/snap.yaml")) else {
        return false;
    };
    text.lines().any(|line| {
        line.strip_prefix("confinement:")
            .is_some_and(|value| value.trim().trim_matches(['"', '\'']) == "classic")
    })
}

/// Checks that `helper` is a program only the administrator can have put
/// there: it and every folder from `trust.top` down to it belong to
/// `trust.owner` and cannot be written by the group or others. The path is
/// resolved first, so links are judged by where they lead.
///
/// Returns the resolved path, which is the one that was judged. It is the one
/// to start: it has no links in it, so nothing on the way to it can be
/// changed by anyone but the owner, and what runs is what was checked.
pub(super) fn check_trusted(helper: &Path, trust: &Trust) -> Result<PathBuf, ElevateError> {
    let missing = || ElevateError::Unavailable(Unavailable::HelperNotInstalled);
    let untrusted = || ElevateError::Unavailable(Unavailable::HelperNotTrusted);
    if !helper.is_absolute() {
        return Err(missing());
    }
    let real = std::fs::canonicalize(helper).map_err(|_| missing())?;
    let file = std::fs::metadata(&real).map_err(|_| missing())?;
    if !file.is_file() || file.permissions().mode() & 0o111 == 0 {
        return Err(missing());
    }
    if !real.starts_with(&trust.top) {
        return Err(untrusted());
    }
    let mut current: &Path = &real;
    loop {
        let meta = std::fs::symlink_metadata(current).map_err(|_| untrusted())?;
        if meta.uid() != trust.owner || meta.mode() & 0o022 != 0 {
            return Err(untrusted());
        }
        if current == trust.top {
            return Ok(real);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return Ok(real),
        }
    }
}

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
        return Err(ElevateError::Unavailable(Unavailable::AlreadyElevated));
    }
    let pkexec = elevation
        .pkexec
        .clone()
        .or_else(|| find_on_path("pkexec"))
        .ok_or(ElevateError::Unavailable(Unavailable::NoElevationService))?;
    let helper = match &elevation.trusted {
        // The helper the system provides is checked again now, just before
        // it is started, and the path that was checked is the path that is
        // handed to `pkexec`.
        Some(trust) => check_trusted(&elevation.helper, trust)?,
        None => {
            if !elevation.helper.is_absolute() || !elevation.helper.is_file() {
                return Err(ElevateError::Unavailable(Unavailable::HelperNotInstalled));
            }
            elevation.helper.clone()
        }
    };
    let mut child = Command::new(pkexec)
        .arg(&helper)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A folder made for one test, removed when it is dropped. The files in
    /// it belong to whoever runs the tests, so the tests name that user as the
    /// one who must own the helper, and the fixture folder as the top.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(label: &str) -> Self {
            let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let root = base.join(format!(
                "bs-platform-elevate-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { root }
        }

        fn trust(&self) -> Trust {
            Trust {
                owner: std::fs::metadata(&self.root).unwrap().uid(),
                top: self.root.clone(),
            }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.root.join(relative)
        }

        /// A program at `relative` with mode 0755 in folders of mode 0755.
        fn program(&self, relative: &str) -> PathBuf {
            let path = self.path(relative);
            let mut folder = path.parent().unwrap().to_path_buf();
            let mut made = Vec::new();
            while folder != self.root && !folder.exists() {
                made.push(folder.clone());
                folder = folder.parent().unwrap().to_path_buf();
            }
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            for dir in made {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn snap(&self, confinement: Option<&str>) -> PathBuf {
            let snap = self.path("snap/bytescape/7");
            self.program(&format!("snap/bytescape/7/{SNAP_HELPER}"));
            if let Some(confinement) = confinement {
                let meta = snap.join("meta");
                std::fs::create_dir_all(&meta).unwrap();
                std::fs::write(
                    meta.join("snap.yaml"),
                    format!("name: bytescape\nversion: 1\nconfinement: {confinement}\n"),
                )
                .unwrap();
            }
            snap
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let named = self
                .root
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("bs-platform-elevate-"));
            if named {
                // Folders the tests made unwritable can be removed again.
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }

    fn why(result: Result<Elevation, ElevateError>) -> Unavailable {
        match result {
            Err(ElevateError::Unavailable(why)) => why,
            other => panic!("expected an unavailable elevation, got {other:?}"),
        }
    }

    fn facts(tree: &Tree, snap: Option<PathBuf>, appimage: bool) -> Facts {
        Facts {
            snap,
            appimage,
            trust: tree.trust(),
        }
    }

    // ----------------------------------------------------- Where it lives

    #[test]
    fn a_helper_in_the_administrators_folders_is_trusted() {
        let tree = Tree::new("trusted");
        let helper = tree.program("usr/libexec/bytescape/bytescape-helper");
        check_trusted(&helper, &tree.trust()).unwrap();
        // A link to it is judged by where it leads.
        symlink(&helper, tree.path("link")).unwrap();
        check_trusted(&tree.path("link"), &tree.trust()).unwrap();
    }

    #[test]
    fn a_helper_that_someone_else_can_change_is_not_started() {
        let tree = Tree::new("untrusted");
        let helper = tree.program("usr/libexec/bytescape/bytescape-helper");
        let untrusted = |tree: &Tree, path: &Path| {
            assert!(matches!(
                check_trusted(path, &tree.trust()),
                Err(ElevateError::Unavailable(Unavailable::HelperNotTrusted))
            ));
        };
        // The file can be written by the group, or by anyone.
        for mode in [0o775, 0o757, 0o777] {
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(mode)).unwrap();
            untrusted(&tree, &helper);
        }
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        check_trusted(&helper, &tree.trust()).unwrap();
        // A folder on the way can be written by the group, or by anyone: a
        // program could be renamed away and another put in its place.
        for folder in ["usr/libexec/bytescape", "usr/libexec", "usr"] {
            for mode in [0o775, 0o777, 0o1777] {
                let path = tree.path(folder);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
                untrusted(&tree, &helper);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        check_trusted(&helper, &tree.trust()).unwrap();
        // The top folder counts too.
        std::fs::set_permissions(&tree.root, std::fs::Permissions::from_mode(0o777)).unwrap();
        untrusted(&tree, &helper);
        std::fs::set_permissions(&tree.root, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Owned by someone other than the one required.
        let other = Trust {
            owner: tree.trust().owner.wrapping_add(1),
            top: tree.root.clone(),
        };
        assert!(matches!(
            check_trusted(&helper, &other),
            Err(ElevateError::Unavailable(Unavailable::HelperNotTrusted))
        ));
        // A link that leads out of the trusted folders is judged where it leads.
        let outside = Tree::new("outside");
        let elsewhere = outside.program("helper");
        symlink(&elsewhere, tree.path("escape")).unwrap();
        untrusted(&tree, &tree.path("escape"));
    }

    #[test]
    fn what_is_not_a_program_is_not_installed() {
        let tree = Tree::new("absent");
        let missing = tree.path("nothing");
        for path in [missing, tree.root.clone(), PathBuf::from("relative/helper")] {
            assert!(matches!(
                check_trusted(&path, &tree.trust()),
                Err(ElevateError::Unavailable(Unavailable::HelperNotInstalled))
            ));
        }
        // A file that cannot be started.
        let plain = tree.path("plain");
        std::fs::write(&plain, b"x").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            check_trusted(&plain, &tree.trust()),
            Err(ElevateError::Unavailable(Unavailable::HelperNotInstalled))
        ));
    }

    // ------------------------------------------------------- Which helper

    #[test]
    fn the_packaged_helper_is_the_default() {
        let tree = Tree::new("default");
        let elevation = system_in(&facts(&tree, None, false)).unwrap();
        assert_eq!(elevation.helper, Path::new(INSTALLED_HELPER));
        assert_eq!(elevation.trusted, Some(tree.trust()));
    }

    #[test]
    fn an_appimage_cannot_elevate_and_says_why() {
        let tree = Tree::new("appimage");
        let reason = why(system_in(&facts(&tree, None, true)));
        assert_eq!(reason, Unavailable::AppImage);
        assert!(reason.explanation().contains("AppImage"));
        // Not even a snap helper next to it changes that.
        let snap = tree.snap(Some("classic"));
        assert_eq!(
            why(system_in(&facts(&tree, Some(snap), true))),
            Unavailable::AppImage
        );
    }

    #[test]
    fn a_classic_snap_uses_the_helper_inside_it() {
        let tree = Tree::new("classic");
        let snap = tree.snap(Some("classic"));
        let elevation = system_in(&facts(&tree, Some(snap.clone()), false)).unwrap();
        assert_eq!(elevation.helper, snap.join(SNAP_HELPER));
        assert_eq!(elevation.trusted, Some(tree.trust()));
        // The yaml may quote the value.
        std::fs::write(
            snap.join("meta/snap.yaml"),
            "name: bytescape\nconfinement: \"classic\"\n",
        )
        .unwrap();
        assert!(system_in(&facts(&tree, Some(snap), false)).is_ok());
    }

    #[test]
    fn a_snap_that_is_not_classic_or_does_not_say_cannot_elevate() {
        for (label, confinement) in [
            ("strict", Some("strict")),
            ("devmode", Some("devmode")),
            ("classically", Some("classically")),
            ("silent", None),
        ] {
            let tree = Tree::new(label);
            let snap = tree.snap(confinement);
            assert_eq!(
                why(system_in(&facts(&tree, Some(snap), false))),
                Unavailable::SnapConfined,
                "{label}"
            );
        }
        // A line that only mentions the word is not the setting.
        let tree = Tree::new("comment");
        let snap = tree.snap(Some("strict"));
        std::fs::write(
            snap.join("meta/snap.yaml"),
            "description: a classic example\nconfinement: strict\n# confinement: classic\n",
        )
        .unwrap();
        assert_eq!(
            why(system_in(&facts(&tree, Some(snap), false))),
            Unavailable::SnapConfined
        );
    }

    #[test]
    fn the_snap_variable_is_the_users_to_set_so_the_helper_it_names_is_checked() {
        // A folder the user made, called a snap and claiming classic
        // confinement, owned by the wrong person: not started.
        let tree = Tree::new("forged");
        let snap = tree.snap(Some("classic"));
        let mut forged = facts(&tree, Some(snap.clone()), false);
        forged.trust.owner = forged.trust.owner.wrapping_add(1);
        assert_eq!(why(system_in(&forged)), Unavailable::HelperNotTrusted);
        // A helper that anyone could replace.
        let helper = snap.join(SNAP_HELPER);
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            why(system_in(&facts(&tree, Some(snap.clone()), false))),
            Unavailable::HelperNotTrusted
        );
        // A snap folder without a helper.
        std::fs::remove_file(&helper).unwrap();
        assert_eq!(
            why(system_in(&facts(&tree, Some(snap), false))),
            Unavailable::HelperNotInstalled
        );
    }

    /// A stand-in for `pkexec` that needs no stand-in program: the shell
    /// runs the file it is given, and that file writes down how it was named
    /// (`$0`) next to itself.
    const RECORD_NAME: &str = "#!/bin/sh\nprintf '%s' \"$0\" > \"$0.named\"\n";

    /// Starts through `/bin/sh` and returns the path the shell was given.
    fn path_handed_to_pkexec(elevation: &Elevation, recorded_beside: &Path) -> PathBuf {
        let elevation = elevation.clone().with_pkexec("/bin/sh");
        let mut launched = launch(&elevation, "scan", [0; NONCE_LEN]).unwrap();
        launched.guard.child.take().unwrap().wait().unwrap();
        let named = std::fs::read(format!("{}.named", recorded_beside.display())).unwrap();
        PathBuf::from(String::from_utf8(named).unwrap())
    }

    /// The tests that start a program do not run as the superuser, whose
    /// launches are refused before anything is checked.
    fn superuser_skips(name: &str) -> bool {
        let skip = is_superuser();
        if skip {
            println!("NOT RUN: {name} starts the helper, which is refused for the superuser");
        }
        skip
    }

    #[test]
    fn a_forged_snap_of_writable_folders_with_a_link_starts_only_the_file_that_was_checked() {
        // A folder anyone can write to, posing as a snap with classic
        // confinement. Its helper is a link to a file that passes the check.
        // The link could be pointed elsewhere after the check, so the path
        // that is kept and started must not run through the forged folders.
        let tree = Tree::new("forged-link");
        let trusted = tree.program("trusted/helper");
        let forged = tree.path("forged");
        let helper_dir = forged.join(SNAP_HELPER).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&helper_dir).unwrap();
        std::fs::create_dir_all(forged.join("meta")).unwrap();
        std::fs::write(forged.join("meta/snap.yaml"), "confinement: classic\n").unwrap();
        symlink(&trusted, forged.join(SNAP_HELPER)).unwrap();
        for folder in ["", "meta", "usr", "usr/libexec", "usr/libexec/bytescape"] {
            std::fs::set_permissions(forged.join(folder), std::fs::Permissions::from_mode(0o777))
                .unwrap();
        }
        let elevation = system_in(&facts(&tree, Some(forged.clone()), false)).unwrap();
        assert_eq!(elevation.helper, trusted, "the resolved, checked file");
        assert!(!elevation.helper.starts_with(&forged));
        assert_eq!(elevation.helper_path(), trusted);

        if superuser_skips("the forged snap") {
            return;
        }
        // What `pkexec` is given is that file too, even if the elevation was
        // built with the path through the forged folders.
        let through_the_link = Elevation {
            helper: forged.join(SNAP_HELPER),
            ..elevation.clone()
        };
        // The stand-in records its own name, so it is the checked file's body.
        std::fs::write(&trusted, RECORD_NAME).unwrap();
        std::fs::set_permissions(&trusted, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(path_handed_to_pkexec(&through_the_link, &trusted), trusted);
        assert_eq!(path_handed_to_pkexec(&elevation, &trusted), trusted);
    }

    #[test]
    fn the_helper_is_checked_again_when_it_is_launched() {
        if superuser_skips("the launch check") {
            return;
        }
        let tree = Tree::new("relaunch");
        let snap = tree.snap(Some("classic"));
        let elevation = system_in(&facts(&tree, Some(snap.clone()), false))
            .unwrap()
            .with_pkexec("/bin/true");
        // Changed after it was found and before it is started.
        let helper = snap.join(SNAP_HELPER);
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o777)).unwrap();
        match launch(&elevation, "scan", [0; NONCE_LEN]) {
            Err(ElevateError::Unavailable(Unavailable::HelperNotTrusted)) => {}
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("the helper was started"),
        }
    }

    #[test]
    fn every_reason_has_something_to_say() {
        for reason in [
            Unavailable::NoElevationService,
            Unavailable::HelperNotInstalled,
            Unavailable::HelperNotTrusted,
            Unavailable::AppImage,
            Unavailable::SnapConfined,
            Unavailable::AlreadyElevated,
            Unavailable::UnsupportedSystem,
            Unavailable::Other("a reason of its own"),
        ] {
            assert!(reason.explanation().len() > 10, "{reason:?}");
        }
        assert!(
            Unavailable::SnapConfined
                .explanation()
                .contains("System folders can't be scanned in this edition")
        );
        // The snap ships with strict confinement, which cannot run a full
        // scan, so the text that sends AppImage users elsewhere must not name
        // it.
        let text = Unavailable::AppImage.explanation();
        assert!(text.contains(".deb package"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("snap"), "{text}");
    }
}
