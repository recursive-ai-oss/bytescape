//! Finding the package that owns a folder from dpkg's file lists.
//!
//! dpkg keeps one `<package>.list` file per installed package in
//! `/var/lib/dpkg/info`, naming every file and folder the package installed.
//! The lists are read once for all the folders being asked about, and only as
//! text: nothing is run.
//!
//! A folder is named by many packages when they share it (`/opt`, `/usr/lib`),
//! and a folder such as `/usr/lib/firefox` is named by every package that adds
//! a file to it. So a folder has an owner only when one package
//!
//! - lists more than half of everything listed *inside* the folder, and
//! - lists something in every folder or file directly inside it, so no other
//!   package has a part of its own there.
//!
//! `/opt` fails the second test as soon as two packages install under it.
//!
//! The lists say what the package installed, not what is in the folder now.
//! So when the folder can be read, most of what is directly inside it must be
//! listed by the owner too: a folder that is mostly something else (an
//! application installed by hand next to a package's plugin) has no owner.
//! When no package passes all of these, the folder has no owner here.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Most list files read in one pass.
const MAX_LISTS: usize = 200_000;
/// Most bytes read from one list. A real list is far smaller.
const MAX_LIST_BYTES: u64 = 64 * 1024 * 1024;
/// A path longer than this is not a path dpkg wrote.
const MAX_LINE: usize = 4096;

/// A package found to own a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub name: String,
    /// The list file the package was read from.
    pub list: PathBuf,
    /// Entries inside the folder that this package lists.
    pub listed: u64,
    /// Other packages that list something inside the folder.
    pub others: u32,
}

/// Since the `/usr` merge, `/bin`, `/sbin` and `/lib*` are links into `/usr`,
/// yet dpkg lists the files under the old names. A folder under `/usr/lib`
/// is therefore also looked up as `/lib`.
fn alias(folder: &[u8]) -> Option<Vec<u8>> {
    let rest = folder.strip_prefix(b"/usr/")?;
    let first = rest.split(|b| *b == b'/').next()?;
    [&b"bin"[..], b"sbin", b"lib", b"lib32", b"lib64", b"libx32"]
        .contains(&first)
        .then(|| folder[4..].to_vec())
}

fn trimmed(folder: &Path) -> &[u8] {
    let bytes = folder.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    &bytes[..end]
}

/// The package name a list file stands for: `foo.list` and `foo:amd64.list`
/// are both `foo`.
fn package_of(file_name: &OsStr) -> Option<String> {
    let stem = file_name.to_str()?.strip_suffix(".list")?;
    let name = stem.split(':').next()?;
    super::is_package_name(name).then(|| name.to_owned())
}

/// Most packages remembered per direct child of a folder. A child that more
/// packages than this add to is shared by any measure.
const MAX_TOUCHING: usize = 64;

/// The packages that list something at or below one direct child of a folder.
#[derive(Default)]
struct Touch {
    ids: Vec<u32>,
    overflowed: bool,
}

impl Touch {
    fn add(&mut self, package: u32) {
        if !self.ids.contains(&package) {
            if self.ids.len() < MAX_TOUCHING {
                self.ids.push(package);
            } else {
                self.overflowed = true;
            }
        }
    }
}

#[derive(Default)]
struct Folder {
    /// Entries inside it, per package id.
    inside: HashMap<u32, u64>,
    /// Direct children, by name.
    children: HashMap<Vec<u8>, Touch>,
}

/// The names directly inside a folder, or `None` when it cannot be read.
pub(crate) type Children = dyn Fn(&Path) -> Option<Vec<Vec<u8>>>;

/// Most names read from a folder to compare with the lists.
const MAX_CHILDREN: usize = 10_000;

/// The names directly inside a folder on disk.
pub(crate) fn children_on_disk(folder: &Path) -> Option<Vec<Vec<u8>>> {
    let entries = std::fs::read_dir(folder).ok()?;
    Some(
        entries
            .flatten()
            .take(MAX_CHILDREN)
            .map(|entry| entry.file_name().as_bytes().to_vec())
            .collect(),
    )
}

/// How much of a folder may be unlisted by its owner: one name in ten.
fn mostly_listed(unlisted: usize, total: usize) -> bool {
    unlisted * 10 <= total
}

/// Looks up the owner of each folder in `folders`, in the same order. Stops
/// early, returning what it has, when `cancel` is set. `children` reads a
/// folder's contents to check the lists against.
pub(crate) fn owners(
    info_dir: &Path,
    folders: &[PathBuf],
    cancel: &AtomicBool,
    children: &Children,
) -> Vec<Option<Owner>> {
    // Every spelling to look for, mapped to the folder it stands for.
    let mut keys: HashMap<Vec<u8>, usize> = HashMap::new();
    for (index, folder) in folders.iter().enumerate() {
        let key = trimmed(folder);
        if key.first() != Some(&b'/') || key.len() < 2 {
            continue;
        }
        if let Some(old) = alias(key) {
            keys.entry(old).or_insert(index);
        }
        keys.entry(key.to_vec()).or_insert(index);
    }
    let mut state: Vec<Folder> = folders.iter().map(|_| Folder::default()).collect();
    if keys.is_empty() {
        return vec![None; folders.len()];
    }
    // Only prefixes of a length some key has need a lookup.
    let longest = keys.keys().map(Vec::len).max().unwrap_or(0).min(MAX_LINE);
    let mut lengths = vec![false; longest + 1];
    for key in keys.keys() {
        if key.len() <= longest {
            lengths[key.len()] = true;
        }
    }

    let mut packages: Vec<(String, PathBuf)> = Vec::new();
    let Ok(dir) = std::fs::read_dir(info_dir) else {
        return vec![None; folders.len()];
    };
    for entry in dir.flatten().take(MAX_LISTS) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(name) = package_of(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut id: Option<u32> = None;
        read_lines(file, |line| {
            hits(line, &keys, &lengths, |index, child| {
                let package = *id.get_or_insert_with(|| {
                    packages.push((name.clone(), path.clone()));
                    (packages.len() - 1) as u32
                });
                let folder = &mut state[index];
                *folder.inside.entry(package).or_insert(0) += 1;
                match folder.children.get_mut(child) {
                    Some(touch) => touch.add(package),
                    None => {
                        let mut touch = Touch::default();
                        touch.add(package);
                        folder.children.insert(child.to_vec(), touch);
                    }
                }
            });
        });
    }

    state
        .into_iter()
        .zip(folders)
        .map(|(folder, path)| {
            let total: u64 = folder.inside.values().sum();
            let (best, count) = folder
                .inside
                .iter()
                .max_by_key(|(id, count)| (**count, std::cmp::Reverse(**id)))
                .map(|(id, count)| (*id, *count))?;
            // The owner lists more than half of what is inside, and has a part
            // in every direct child.
            let in_every_child = folder
                .children
                .values()
                .all(|touch| !touch.overflowed && touch.ids.contains(&best));
            let on_disk = || match children(path) {
                Some(names) => {
                    let unlisted = names
                        .iter()
                        .filter(|name| {
                            !folder
                                .children
                                .get(name.as_slice())
                                .is_some_and(|touch| touch.ids.contains(&best))
                        })
                        .count();
                    mostly_listed(unlisted, names.len())
                }
                // A folder that cannot be read cannot be checked.
                None => true,
            };
            (count * 2 > total && in_every_child && on_disk()).then(|| {
                let (name, list) = &packages[best as usize];
                Owner {
                    name: name.clone(),
                    list: list.clone(),
                    listed: count,
                    others: (folder.inside.len() - 1) as u32,
                }
            })
        })
        .collect()
}

/// Calls `found(index, child)` for each folder the line lies inside, with the
/// name of the folder's direct child that the line is, or is below. A line that
/// is a folder itself is not inside it.
fn hits(
    line: &[u8],
    keys: &HashMap<Vec<u8>, usize>,
    lengths: &[bool],
    mut found: impl FnMut(usize, &[u8]),
) {
    if line.first() != Some(&b'/') {
        return;
    }
    let longest = lengths.len() - 1;
    for (position, byte) in line.iter().enumerate().skip(1) {
        if *byte != b'/' {
            continue;
        }
        if position > longest {
            break;
        }
        if lengths[position]
            && let Some(&index) = keys.get(&line[..position])
        {
            let rest = &line[position + 1..];
            let child = rest.split(|b| *b == b'/').next().unwrap_or(rest);
            if !child.is_empty() {
                found(index, child);
            }
        }
    }
}

/// Reads lines of at most [`MAX_LINE`] bytes, up to [`MAX_LIST_BYTES`] in all.
/// A longer line is skipped.
fn read_lines(file: File, mut each: impl FnMut(&[u8])) {
    let mut reader = BufReader::new(file.take(MAX_LIST_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        // Cap what a single endless line can make us hold.
        let mut limited = (&mut reader).take(MAX_LINE as u64 + 1);
        match limited.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() <= MAX_LINE {
                each(&line);
            }
        } else if line.len() > MAX_LINE {
            // Too long: skip the rest of it.
            let mut sink = Vec::new();
            loop {
                sink.clear();
                match (&mut reader).take(8192).read_until(b'\n', &mut sink) {
                    Ok(0) | Err(_) => return,
                    Ok(_) if sink.last() == Some(&b'\n') => break,
                    Ok(_) => {}
                }
            }
        } else if !line.is_empty() {
            // The last line, without a newline.
            each(&line);
        }
    }
}

/// The AppStream component id a package ships, when it ships exactly one.
/// The id is the name of its metainfo file, which the file list gives.
pub(crate) fn appstream_id(list: &Path) -> Option<String> {
    let file = File::open(list).ok()?;
    let mut ids = HashSet::new();
    read_lines(file, |line| {
        for directory in [&b"/usr/share/metainfo/"[..], b"/usr/share/appdata/"] {
            let Some(name) = line.strip_prefix(directory) else {
                continue;
            };
            let Ok(name) = std::str::from_utf8(name) else {
                continue;
            };
            let id = name
                .strip_suffix(".metainfo.xml")
                .or_else(|| name.strip_suffix(".appdata.xml"));
            if let Some(id) = id
                && crate::launch::is_component_id(id)
            {
                ids.insert(id.to_owned());
            }
        }
    });
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::time::Instant;

    fn list(fx: &Fixture, file: &str, lines: &[&str]) {
        let mut text = lines.join("\n");
        text.push('\n');
        fx.write(&format!("info/{file}"), &text);
    }

    /// Asks with no look at the disk: the folders need not exist.
    fn ask(fx: &Fixture, folders: &[&str]) -> Vec<Option<Owner>> {
        let folders: Vec<PathBuf> = folders.iter().map(PathBuf::from).collect();
        owners(
            &fx.path().join("info"),
            &folders,
            &AtomicBool::new(false),
            &|_| None,
        )
    }

    fn name(owner: &Option<Owner>) -> Option<&str> {
        owner.as_ref().map(|o| o.name.as_str())
    }

    #[test]
    fn the_package_listing_a_folder_and_its_contents_owns_it() {
        let fx = Fixture::new("dpkg-owner");
        list(
            &fx,
            "foo.list",
            &[
                "/.",
                "/opt",
                "/opt/foo",
                "/opt/foo/bin",
                "/opt/foo/bin/foo",
                "/opt/foo/lib.so",
            ],
        );
        list(&fx, "bar.list", &["/.", "/opt", "/opt/bar", "/opt/bar/x"]);
        let got = ask(&fx, &["/opt/foo", "/opt/bar", "/opt", "/opt/none"]);
        assert_eq!(name(&got[0]), Some("foo"));
        assert_eq!(got[0].as_ref().unwrap().listed, 3);
        assert_eq!(got[0].as_ref().unwrap().others, 0);
        assert_eq!(
            got[0].as_ref().unwrap().list,
            fx.path().join("info/foo.list")
        );
        assert_eq!(name(&got[1]), Some("bar"));
        // Two packages each have a part of their own in `/opt`.
        assert_eq!(got[2], None);
        assert_eq!(got[3], None);
    }

    #[test]
    fn a_shared_folder_belongs_to_the_package_with_most_of_it() {
        let fx = Fixture::new("dpkg-majority");
        list(
            &fx,
            "firefox.list",
            &[
                "/usr/lib/firefox",
                "/usr/lib/firefox/a",
                "/usr/lib/firefox/b",
                "/usr/lib/firefox/c",
            ],
        );
        list(
            &fx,
            "firefox-locale-de.list",
            &["/usr/lib/firefox", "/usr/lib/firefox/a/de.xpi"],
        );
        let got = ask(&fx, &["/usr/lib/firefox"]);
        let owner = got[0].as_ref().unwrap();
        assert_eq!(owner.name, "firefox");
        assert_eq!(owner.listed, 3);
        assert_eq!(owner.others, 1);

        // Packages that add more than the owner has itself: not more than half.
        list(
            &fx,
            "fr.list",
            &["/usr/lib/firefox/a/fr.xpi", "/usr/lib/firefox/a/fr2"],
        );
        list(
            &fx,
            "es.list",
            &["/usr/lib/firefox/a/es.xpi", "/usr/lib/firefox/a/es2"],
        );
        let got = ask(&fx, &["/usr/lib/firefox"]);
        assert_eq!(got[0], None, "3 of 8 is not more than half");
    }

    #[test]
    fn a_package_with_a_part_of_its_own_in_the_folder_breaks_the_ownership() {
        let fx = Fixture::new("dpkg-own-part");
        list(
            &fx,
            "big.list",
            &["/srv/app/a", "/srv/app/a/1", "/srv/app/a/2", "/srv/app/a/3"],
        );
        // Without the plugin, `big` owns the folder.
        assert_eq!(name(&ask(&fx, &["/srv/app"])[0]), Some("big"));
        list(&fx, "plugin.list", &["/srv/app/plugin"]);
        // `plugin` lists a direct child that `big` has no part in.
        assert_eq!(ask(&fx, &["/srv/app"])[0], None);
    }

    #[test]
    fn a_child_that_very_many_packages_add_to_is_shared() {
        let fx = Fixture::new("dpkg-overflow");
        list(
            &fx,
            "owner.list",
            &[
                "/srv/pool/a",
                "/srv/pool/a/1",
                "/srv/pool/a/2",
                "/srv/pool/a/3",
            ],
        );
        for n in 0..(MAX_TOUCHING + 5) {
            list(&fx, &format!("p{n}.list"), &[&format!("/srv/pool/a/x{n}")]);
        }
        assert_eq!(ask(&fx, &["/srv/pool"])[0], None);
    }

    #[test]
    fn architecture_suffixes_and_other_file_names() {
        let fx = Fixture::new("dpkg-names");
        list(
            &fx,
            "libfoo:amd64.list",
            &["/usr/lib/foo", "/usr/lib/foo/x"],
        );
        list(
            &fx,
            "Bad Name.list",
            &["/usr/lib/other", "/usr/lib/other/x"],
        );
        list(&fx, "foo.md5sums", &["/usr/lib/md5/x"]);
        list(&fx, "-rf.list", &["/usr/lib/dash/x"]);
        let got = ask(
            &fx,
            &[
                "/usr/lib/foo",
                "/usr/lib/other",
                "/usr/lib/md5",
                "/usr/lib/dash",
            ],
        );
        assert_eq!(name(&got[0]), Some("libfoo"));
        assert_eq!(got[1], None);
        assert_eq!(got[2], None);
        assert_eq!(got[3], None);
    }

    #[test]
    fn the_merged_usr_names_are_looked_up_too() {
        let fx = Fixture::new("dpkg-merged");
        list(
            &fx,
            "libbar.list",
            &[
                "/lib/x86_64-linux-gnu/bar",
                "/lib/x86_64-linux-gnu/bar/plugin.so",
            ],
        );
        list(
            &fx,
            "tool.list",
            &["/bin/tool", "/sbin/other", "/usr/share/tool/x"],
        );
        let got = ask(
            &fx,
            &[
                "/usr/lib/x86_64-linux-gnu/bar",
                "/usr/share/tool",
                "/usr/games/x",
            ],
        );
        assert_eq!(name(&got[0]), Some("libbar"));
        assert_eq!(name(&got[1]), Some("tool"));
        assert_eq!(got[2], None);
    }

    #[test]
    fn a_folder_with_nothing_listed_inside_has_no_owner() {
        let fx = Fixture::new("dpkg-empty");
        list(&fx, "foo.list", &["/usr/share/foo"]);
        let got = ask(&fx, &["/usr/share/foo/", "/usr/share/nothing"]);
        // Only the folder itself is listed, so there is nothing to weigh.
        assert_eq!(got[0], None);
        assert_eq!(got[1], None);
    }

    #[test]
    fn hostile_lists_are_survived() {
        let fx = Fixture::new("dpkg-hostile");
        // Long lines, a line without a newline, binary bytes, CRLF, no slash.
        let long = format!("/opt/hostile/{}", "x".repeat(10_000));
        fx.write(
            "info/hostile.list",
            &format!(
                "{long}\n/opt/hostile/ok\r\n\u{0}\u{1}binary\nrelative/path\n/opt/hostile/last"
            ),
        );
        let got = ask(&fx, &["/opt/hostile", "/", "", "relative", "/opt//hostile"]);
        let owner = got[0].as_ref().unwrap();
        assert_eq!(owner.name, "hostile");
        // The two usable lines count; the long and broken ones do not.
        assert_eq!(owner.listed, 2);
        assert_eq!(got[1], None);
        assert_eq!(got[2], None);
        assert_eq!(got[3], None);
        assert_eq!(got[4], None);

        // A folder that is not there, and no folders at all.
        assert_eq!(
            owners(
                &fx.path().join("missing"),
                &[PathBuf::from("/opt/x")],
                &AtomicBool::new(false),
                &|_| None
            ),
            vec![None]
        );
        assert!(
            owners(
                &fx.path().join("info"),
                &[],
                &AtomicBool::new(false),
                &|_| None
            )
            .is_empty()
        );
    }

    #[test]
    fn non_utf8_paths_match_byte_for_byte() {
        let fx = Fixture::new("dpkg-bytes");
        let mut text = b"/opt/odd\xff".to_vec();
        text.extend_from_slice(b"\n/opt/odd\xff/file\n");
        std::fs::create_dir_all(fx.path().join("info")).unwrap();
        std::fs::write(fx.path().join("info/odd.list"), text).unwrap();
        let folder = PathBuf::from(OsStr::from_bytes(b"/opt/odd\xff"));
        let got = owners(
            &fx.path().join("info"),
            &[folder],
            &AtomicBool::new(false),
            &|_| None,
        );
        assert_eq!(name(&got[0]), Some("odd"));
    }

    #[test]
    fn cancelling_stops_the_pass() {
        let fx = Fixture::new("dpkg-cancel");
        list(&fx, "foo.list", &["/opt/foo", "/opt/foo/x"]);
        let folders = [PathBuf::from("/opt/foo")];
        let got = owners(
            &fx.path().join("info"),
            &folders,
            &AtomicBool::new(true),
            &|_| None,
        );
        assert_eq!(got, vec![None]);
    }

    #[test]
    fn the_component_id_comes_from_the_metainfo_file_name() {
        let fx = Fixture::new("dpkg-appstream");
        list(
            &fx,
            "one.list",
            &[
                "/usr/bin/one",
                "/usr/share/metainfo/org.example.One.metainfo.xml",
                "/usr/share/applications/org.example.One.desktop",
            ],
        );
        list(
            &fx,
            "legacy.list",
            &["/usr/share/appdata/org.example.Legacy.appdata.xml"],
        );
        list(
            &fx,
            "many.list",
            &[
                "/usr/share/metainfo/org.example.A.metainfo.xml",
                "/usr/share/metainfo/org.example.B.metainfo.xml",
            ],
        );
        list(&fx, "none.list", &["/usr/bin/none"]);
        list(
            &fx,
            "bad.list",
            &["/usr/share/metainfo/-evil id.metainfo.xml"],
        );
        let id = |f: &str| appstream_id(&fx.path().join(format!("info/{f}.list")));
        assert_eq!(id("one").as_deref(), Some("org.example.One"));
        assert_eq!(id("legacy").as_deref(), Some("org.example.Legacy"));
        assert_eq!(id("many"), None);
        assert_eq!(id("none"), None);
        assert_eq!(id("bad"), None);
        assert_eq!(appstream_id(&fx.path().join("info/missing.list")), None);
    }

    #[test]
    fn a_folder_that_is_mostly_something_else_has_no_owner() {
        let fx = Fixture::new("dpkg-on-disk");
        let app = fx.path().join("app");
        let first = app.display().to_string();
        // The package lists the folder and three of its files.
        list(
            &fx,
            "plugin.list",
            &[
                &first,
                &format!("{first}/f0"),
                &format!("{first}/f1"),
                &format!("{first}/f2"),
            ],
        );
        let ask_disk = |folder: &Path| {
            owners(
                &fx.path().join("info"),
                &[folder.to_path_buf()],
                &AtomicBool::new(false),
                &children_on_disk,
            )
            .remove(0)
        };
        // Ten files are there; only three are the package's.
        for n in 0..10 {
            fx.write(&format!("app/f{n}"), "x");
        }
        assert_eq!(ask_disk(&app), None);

        // Only the package's own files are there.
        std::fs::remove_dir_all(&app).unwrap();
        for n in 0..3 {
            fx.write(&format!("app/f{n}"), "x");
        }
        assert_eq!(name(&ask_disk(&app)), Some("plugin"));
        // One unlisted name in four is more than one in ten.
        fx.write("app/cache", "x");
        assert_eq!(ask_disk(&app), None);
    }

    #[test]
    fn the_unlisted_share_is_one_in_ten() {
        assert!(mostly_listed(0, 0));
        assert!(mostly_listed(1, 10));
        assert!(mostly_listed(0, 5));
        assert!(!mostly_listed(1, 9));
        assert!(!mostly_listed(1, 4));
        assert!(!mostly_listed(7, 10));
        assert!(mostly_listed(5, 50));
        assert!(!mostly_listed(6, 50));
    }

    /// The speed of one pass over many lists, printed for the record. The
    /// bound is far looser than the measured time; it only catches a pass
    /// that has become quadratic.
    #[test]
    fn ten_thousand_lists_are_read_quickly() {
        let fx = Fixture::new("dpkg-speed");
        std::fs::create_dir_all(fx.path().join("info")).unwrap();
        let mut total_lines = 0u64;
        for n in 0..10_000u32 {
            let mut text = String::new();
            for k in 0..100u32 {
                text.push_str(&format!("/usr/share/pkg{n}/dir{}/file{k}\n", k % 7));
                total_lines += 1;
            }
            text.push_str(&format!("/usr/lib/shared/lib{n}.so\n"));
            total_lines += 1;
            std::fs::write(fx.path().join(format!("info/pkg{n}.list")), text).unwrap();
        }
        let folders: Vec<PathBuf> = (0..50u32)
            .map(|n| PathBuf::from(format!("/usr/share/pkg{}", n * 97)))
            .chain([PathBuf::from("/usr/lib/shared")])
            .collect();
        let started = Instant::now();
        let got = owners(
            &fx.path().join("info"),
            &folders,
            &AtomicBool::new(false),
            &|_| None,
        );
        let elapsed = started.elapsed();
        eprintln!("10000 lists, {total_lines} lines, 51 folders: {elapsed:?}");
        assert_eq!(name(&got[1]), Some("pkg97"));
        assert_eq!(got[50], None);
        assert!(elapsed.as_secs() < 30, "{elapsed:?}");
    }
}
