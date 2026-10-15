//! What a delete reports.

use std::io;

/// What a finished delete took away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    /// Bytes the filesystem had reserved for what is gone. A file with several
    /// names counts once, when its last name goes.
    pub allocated: u64,
    /// Names removed.
    pub entries: u64,
    /// Files that still exist under a name outside the item.
    pub kept_by_other_names: u64,
}

/// Why a delete did not complete.
#[derive(Debug)]
#[non_exhaustive]
pub enum DeleteError {
    /// There was nothing at the path any more.
    NotFound,
    /// The path now names something other than what was judged.
    Replaced,
    /// The item could not be examined before anything was removed.
    Look(io::Error),
    /// Some of the item is gone and `error` stopped the rest.
    Partial { removed: Removed, error: io::Error },
}

impl std::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("the item is gone"),
            Self::Replaced => f.write_str("the item was replaced"),
            Self::Look(error) => write!(f, "cannot look at the item: {error}"),
            Self::Partial { removed, error } => write!(
                f,
                "stopped after removing {} entries: {error}",
                removed.entries
            ),
        }
    }
}

impl std::error::Error for DeleteError {}

/// Why an item could not be looked up.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    NotFound,
    Cannot(io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Cannot(error) => write!(f, "cannot look it up: {error}"),
        }
    }
}

impl std::error::Error for OpenError {}
