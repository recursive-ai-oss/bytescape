//! Package lookup on Linux: dpkg's file lists, or `rpm -qf` where rpm is used.

use super::{Backend, Owner, Package, PackageManager, dpkg, is_package_name};
use crate::launch::linux::{BusUse, Env, tool};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where dpkg keeps its file lists.
const DPKG_INFO: &str = "/var/lib/dpkg/info";
/// Where an rpm database is, depending on the distribution's age.
const RPM_DATABASES: [&str; 2] = ["/var/lib/rpm", "/usr/lib/sysimage/rpm"];
/// How long one `rpm -qf` may take.
const RPM_LIMIT: Duration = Duration::from_secs(5);

pub(crate) struct System {
    dpkg_info: PathBuf,
    rpm_databases: Vec<PathBuf>,
    path: OsString,
    /// Reads a folder's contents to check dpkg's lists against.
    children: fn(&Path) -> Option<Vec<Vec<u8>>>,
}

impl System {
    pub fn new() -> Self {
        Self {
            dpkg_info: PathBuf::from(DPKG_INFO),
            rpm_databases: RPM_DATABASES.iter().map(PathBuf::from).collect(),
            path: std::env::var_os("PATH").unwrap_or_default(),
            children: dpkg::children_on_disk,
        }
    }

    /// A system whose folders are not looked at: the folders a test asks
    /// about are made up, and a real machine may well have one of them.
    #[cfg(test)]
    fn with(dpkg_info: PathBuf, rpm_databases: Vec<PathBuf>, path: OsString) -> Self {
        Self {
            dpkg_info,
            rpm_databases,
            path,
            children: |_| None,
        }
    }

    fn env(&self) -> Env {
        Env {
            path: self.path.clone(),
            bus_address: None,
            runtime_dir: None,
            desktop: String::new(),
            socket_exists: |_| false,
            wayland: false,
        }
    }

    fn uses_rpm(&self) -> bool {
        self.rpm_databases.iter().any(|db| db.is_dir())
    }

    /// The command that removes an rpm package, by the tool the system has.
    fn rpm_removal(&self, name: &str) -> String {
        let has = |tool: &str| crate::launch::spawn::find_on_path(tool, &self.path).is_some();
        if has("dnf") {
            format!("sudo dnf remove {name}")
        } else if has("zypper") {
            format!("sudo zypper remove {name}")
        } else {
            format!("sudo rpm -e {name}")
        }
    }

    fn dpkg_package(owner: dpkg::Owner) -> Owner {
        let software_center_url =
            dpkg::appstream_id(&owner.list).map(|id| format!("appstream://{id}"));
        Owner::Package(Package {
            manager: PackageManager::Dpkg,
            removal_command: is_package_name(&owner.name)
                .then(|| format!("sudo apt remove {}", owner.name)),
            name: owner.name,
            listed_entries: Some(owner.listed),
            other_packages: owner.others,
            software_center_url,
        })
    }

    /// The one package `rpm -qf` names for a folder; none when there are
    /// several or when rpm fails.
    fn rpm_owner(&self, folder: &Path) -> Option<Owner> {
        let output = tool(&self.env(), "rpm", BusUse::No)
            .ok()?
            .arg("-qf")
            .arg("--queryformat")
            .arg("%{NAME}\\n")
            .arg(folder.as_os_str())
            .run("ask rpm", Some(RPM_LIMIT))
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let mut names: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        names.sort_unstable();
        names.dedup();
        let [name] = names.as_slice() else {
            return None;
        };
        let name = *name;
        is_package_name(name).then(|| {
            Owner::Package(Package {
                manager: PackageManager::Rpm,
                name: name.to_owned(),
                listed_entries: None,
                other_packages: 0,
                removal_command: Some(self.rpm_removal(name)),
                software_center_url: None,
            })
        })
    }
}

impl Backend for System {
    fn find(&self, folders: &[PathBuf], cancel: &AtomicBool) -> Vec<Option<Owner>> {
        if self.dpkg_info.is_dir() {
            return dpkg::owners(&self.dpkg_info, folders, cancel, &self.children)
                .into_iter()
                .map(|owner| owner.map(Self::dpkg_package))
                .collect();
        }
        if self.uses_rpm() {
            return folders
                .iter()
                .map(|folder| {
                    if cancel.load(Ordering::Relaxed) {
                        None
                    } else {
                        self.rpm_owner(folder)
                    }
                })
                .collect();
        }
        vec![None; folders.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::spawn::testing::script;
    use crate::software::{Candidate, Lookup, MIN_MATCH_BYTES, Matcher};
    use crate::testutil::Fixture;
    use std::sync::Arc;
    use std::time::Instant;

    fn quiet() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn package(owner: Option<&Owner>) -> &Package {
        let Some(Owner::Package(p)) = owner else {
            panic!("expected a package, got {owner:?}");
        };
        p
    }

    #[test]
    fn dpkg_gives_the_package_a_removal_command_and_a_software_center_link() {
        let fx = Fixture::new("sw-dpkg");
        fx.write(
            "info/firefox.list",
            "/usr/lib/firefox\n/usr/lib/firefox/a\n/usr/lib/firefox/b\n\
             /usr/share/metainfo/org.mozilla.firefox.metainfo.xml\n",
        );
        let system = System::with(fx.path().join("info"), Vec::new(), OsString::new());
        let got = system.find(&[PathBuf::from("/usr/lib/firefox")], &quiet());
        let p = package(got[0].as_ref());
        assert_eq!(p.manager, PackageManager::Dpkg);
        assert_eq!(p.name, "firefox");
        assert_eq!(p.listed_entries, Some(2));
        assert_eq!(
            p.removal_command.as_deref(),
            Some("sudo apt remove firefox")
        );
        assert_eq!(
            p.software_center_url.as_deref(),
            Some("appstream://org.mozilla.firefox")
        );
    }

    #[test]
    fn the_system_checks_the_lists_against_the_folder_on_disk() {
        let fx = Fixture::new("sw-on-disk");
        let app = fx.path().join("app");
        let first = app.display().to_string();
        fx.write(
            "info/plugin.list",
            &format!("{first}\n{first}/f0\n{first}/f1\n{first}/f2\n"),
        );
        // Ten files are there; only three are the package's.
        for n in 0..10 {
            fx.write(&format!("app/f{n}"), "x");
        }
        let folders = [app.clone()];
        let looking = System {
            children: dpkg::children_on_disk,
            ..System::with(fx.path().join("info"), Vec::new(), OsString::new())
        };
        assert!(looking.find(&folders, &quiet())[0].is_none());
        // The same lists, with the folder not looked at.
        let blind = System::with(fx.path().join("info"), Vec::new(), OsString::new());
        assert_eq!(
            package(blind.find(&folders, &quiet())[0].as_ref()).name,
            "plugin"
        );
    }

    #[test]
    fn rpm_is_asked_only_where_it_is_used_and_names_one_package() {
        let fx = Fixture::new("sw-rpm");
        fx.mkdir("rpmdb");
        let bin = fx.path().join("bin");
        script(
            &bin,
            "rpm",
            r#"if [ "$1 $2" != "-qf --queryformat" ] || [ "$3" != '%{NAME}\n' ]; then exit 9; fi
case "$4" in
  /opt/one) echo one;;
  /opt/dup) echo dup; echo dup;;
  /opt/two) echo a; echo b;;
  /opt/bad) echo 'bad name; rm';;
  *) exit 1;;
esac"#,
        );
        script(&bin, "dnf", "exit 0");
        let rpm_db = vec![fx.path().join("rpmdb")];
        let system = System::with(
            fx.path().join("no-dpkg"),
            rpm_db.clone(),
            bin.clone().into_os_string(),
        );
        let folders: Vec<PathBuf> = ["/opt/one", "/opt/dup", "/opt/two", "/opt/bad", "/opt/none"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let got = system.find(&folders, &quiet());
        let one = package(got[0].as_ref());
        assert_eq!(one.manager, PackageManager::Rpm);
        assert_eq!(one.name, "one");
        assert_eq!(one.listed_entries, None);
        assert_eq!(one.removal_command.as_deref(), Some("sudo dnf remove one"));
        assert_eq!(package(got[1].as_ref()).name, "dup");
        assert!(got[2].is_none(), "two packages");
        assert!(got[3].is_none(), "a name that is not plain");
        assert!(got[4].is_none(), "rpm failed");

        // Without dnf, zypper, then plain rpm.
        std::fs::remove_file(bin.join("dnf")).unwrap();
        script(&bin, "zypper", "exit 0");
        let got = system.find(&folders[..1], &quiet());
        assert_eq!(
            package(got[0].as_ref()).removal_command.as_deref(),
            Some("sudo zypper remove one")
        );
        std::fs::remove_file(bin.join("zypper")).unwrap();
        let got = system.find(&folders[..1], &quiet());
        assert_eq!(
            package(got[0].as_ref()).removal_command.as_deref(),
            Some("sudo rpm -e one")
        );

        // No rpm database: rpm is never started.
        let ran = fx.path().join("ran");
        script(&bin, "rpm", &format!("touch '{}'; echo one", ran.display()));
        let system = System::with(
            fx.path().join("no-dpkg"),
            vec![fx.path().join("none")],
            bin.into_os_string(),
        );
        assert!(system.find(&folders[..1], &quiet())[0].is_none());
        assert!(!ran.exists());
    }

    #[test]
    fn rpm_is_not_asked_when_dpkg_is_present_or_the_matching_is_cancelled() {
        let fx = Fixture::new("sw-rpm-skip");
        fx.mkdir("rpmdb");
        fx.mkdir("info");
        let ran = fx.path().join("ran");
        let bin = fx.path().join("bin");
        script(&bin, "rpm", &format!("touch '{}'; echo one", ran.display()));
        let rpm_db = vec![fx.path().join("rpmdb")];
        let both = System::with(
            fx.path().join("info"),
            rpm_db.clone(),
            bin.clone().into_os_string(),
        );
        let _ = both.find(&[PathBuf::from("/opt/one")], &quiet());
        let rpm_only = System::with(fx.path().join("no-dpkg"), rpm_db, bin.into_os_string());
        let _ = rpm_only.find(&[PathBuf::from("/opt/one")], &AtomicBool::new(true));
        assert!(!ran.exists());
    }

    #[test]
    fn the_matcher_runs_the_dpkg_pass_in_the_background() {
        let fx = Fixture::new("sw-matcher");
        fx.write(
            "info/bigapp.list",
            "/opt/bigapp\n/opt/bigapp/x\n/opt/bigapp/y\n",
        );
        let system = System::with(fx.path().join("info"), Vec::new(), OsString::new());
        let matcher = Matcher::with_backend(Arc::new(system));
        matcher.request(&[
            Candidate {
                path: PathBuf::from("/opt/bigapp"),
                bytes: MIN_MATCH_BYTES,
            },
            Candidate {
                path: PathBuf::from("/opt/other"),
                bytes: MIN_MATCH_BYTES,
            },
        ]);
        let started = Instant::now();
        while !matcher.is_idle() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        match matcher.lookup(Path::new("/opt/bigapp")) {
            Lookup::Found(Owner::Package(p)) => assert_eq!(p.name, "bigapp"),
            other => panic!("{other:?}"),
        }
        assert_eq!(matcher.lookup(Path::new("/opt/other")), Lookup::NoOwner);
    }

    #[test]
    fn a_system_with_no_package_database_has_no_owners() {
        let fx = Fixture::new("sw-none");
        let system = System::with(fx.path().join("nothing"), Vec::new(), OsString::new());
        assert_eq!(
            system.find(&[PathBuf::from("/opt/x")], &quiet()),
            vec![None]
        );
    }
}
