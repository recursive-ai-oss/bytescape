//! What kind of thing each file is, decided while the walker reads it.
//!
//! The category tables live in `bs-knowledge`. A walker asks them two
//! questions at different costs:
//!
//! * once per directory, whether the *place* gives a category to everything
//!   in it (a cache folder, say), which needs the directory's full path;
//! * once per file, what its extension says, which needs only the name and
//!   makes no allocation.
//!
//! A place wins over an extension, the way [`bs_knowledge::Knowledge::classify`]
//! does it. Directories keep the unclassified category: the map colors files
//! and groups, not folders.

use bs_knowledge::{Environment, Knowledge, classify_name};
use bs_tree::{Category, NodeKind};

/// The knowledge tables resolved for this process, built once per scan.
pub struct Classifier {
    knowledge: Knowledge,
}

impl Classifier {
    /// Resolves the tables against the process environment. Read once, on
    /// the thread that starts the scan, before any worker exists.
    pub fn from_process_environment() -> Classifier {
        Classifier::with_environment(&Environment::from_lookup(|name| std::env::var_os(name)))
    }

    pub fn with_environment(env: &Environment) -> Classifier {
        Classifier {
            knowledge: Knowledge::new(env),
        }
    }

    /// The category that a directory's place gives to what is inside it.
    /// `path` is the directory's full path as the scan saw it.
    pub fn place(&self, path: &[u8]) -> Option<Category> {
        self.knowledge.place_bytes(path).map(to_tree)
    }

    /// The category of an entry that is not a directory. The place of its
    /// directory decides when it has an answer; otherwise a regular file is
    /// told by its extension. Links and special files are not "a video" because
    /// of what they are called, so they take only the place.
    pub fn category(&self, place: Option<Category>, kind: NodeKind, name: &[u8]) -> Category {
        match (place, kind) {
            (Some(category), _) => category,
            (None, NodeKind::File) => to_tree(classify_name(name)),
            (None, _) => Category::OTHER,
        }
    }
}

fn to_tree(category: bs_knowledge::Category) -> Category {
    Category(category.number())
}

/// What the walker knows about the directory it is listing.
pub(crate) struct Here<'a> {
    /// Its full path, or empty when nothing is being classified.
    pub path: &'a [u8],
    /// The category its place gives to what is inside.
    pub place: Option<Category>,
}

/// The longest path kept for the lookup. A tree can be nested far deeper than
/// any path can name (directories are opened relative to their parent), and
/// copying an ever longer path into every level would take time quadratic in
/// the depth. Past this length a directory keeps its parent's path, so its
/// place is the place of that prefix: places apply to everything below them,
/// so only a folder that sits deeper than this in a path of very long names
/// can be missed.
pub const MAX_PATH_BYTES: usize = 4096;

/// The path of an entry inside a directory: `parent`, a separator unless it
/// ends in one already (a drive root), and `name`. Once `parent` is
/// [`MAX_PATH_BYTES`] long the result is `parent` itself.
pub fn join(parent: &[u8], name: &[u8], separator: u8) -> Box<[u8]> {
    if parent.len() >= MAX_PATH_BYTES {
        return parent.into();
    }
    let mut out = Vec::with_capacity(parent.len() + 1 + name.len());
    out.extend_from_slice(parent);
    if out.last().is_some_and(|&b| b != separator) {
        out.push(separator);
    }
    out.extend_from_slice(name);
    out.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_knowledge::Category as Known;

    #[test]
    fn join_adds_one_separator_and_only_when_needed() {
        assert_eq!(&*join(b"/home/a", b"b", b'/'), b"/home/a/b");
        assert_eq!(&*join(b"/", b"usr", b'/'), b"/usr");
        assert_eq!(&*join(b"C:\\", b"Users", b'\\'), b"C:\\Users");
        assert_eq!(&*join(b"C:\\Users", b"me", b'\\'), b"C:\\Users\\me");
        assert_eq!(&*join(b"", b"x", b'/'), b"x");
    }

    #[test]
    fn a_path_stops_growing_at_the_cap() {
        let mut path: Box<[u8]> = b"/r".as_slice().into();
        let mut steps = 0;
        while path.len() < MAX_PATH_BYTES {
            path = join(&path, &[b'n'; 200], b'/');
            steps += 1;
        }
        assert!(steps < 30, "{steps}");
        // At the cap the path is handed on as it is, however long the name.
        let capped = join(&path, &[b'n'; 255], b'/');
        assert_eq!(capped, path);
        assert!(capped.len() < MAX_PATH_BYTES + 256);
        // Just under the cap a name is still added, whole.
        let almost = vec![b'a'; MAX_PATH_BYTES - 1];
        assert_eq!(join(&almost, b"b", b'/').len(), MAX_PATH_BYTES + 1);
    }

    #[test]
    fn the_tree_uses_the_numbers_of_the_knowledge_tables() {
        // The tree stores plain numbers, so a renumbering on either side would
        // silently recolor the map.
        for known in Known::ALL {
            assert_eq!(to_tree(known).0, known.number());
            assert_eq!(Known::from_number(to_tree(known).0), Some(known));
        }
        assert_eq!(to_tree(Known::Other), Category::OTHER);
    }

    fn classifier() -> Classifier {
        Classifier::with_environment(&Environment::new().with("HOME", "/home/alice"))
    }

    #[test]
    fn a_file_is_told_by_its_extension_unless_its_place_says_otherwise() {
        let c = classifier();
        let video = Category(Known::Video.number());
        let cache = Category(Known::CacheTemp.number());
        assert_eq!(c.category(None, NodeKind::File, b"movie.MKV"), video);
        assert_eq!(c.category(None, NodeKind::File, b"README"), Category::OTHER);
        assert_eq!(
            c.category(None, NodeKind::File, b".bashrc"),
            Category::OTHER
        );
        assert_eq!(c.category(Some(cache), NodeKind::File, b"movie.mkv"), cache);
        // A link is not a video because of its name; a place still applies.
        assert_eq!(
            c.category(None, NodeKind::Symlink, b"movie.mkv"),
            Category::OTHER
        );
        assert_eq!(c.category(None, NodeKind::Other, b"a.mp3"), Category::OTHER);
        assert_eq!(c.category(Some(cache), NodeKind::Symlink, b"x"), cache);
    }

    #[test]
    fn places_come_from_the_tables() {
        let c = classifier();
        let cache = Category(Known::CacheTemp.number());
        assert_eq!(c.place(b"/home/alice/.cache"), Some(cache));
        assert_eq!(c.place(b"/home/alice/.cache/mozilla/x"), Some(cache));
        assert_eq!(c.place(b"/home/alice/Documents"), None);
        assert_eq!(c.place(b"/"), None);
    }
}
