//! The flat list of boxes a layout produces.

use crate::geom::Rect;

/// What a box is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// A folder with a title bar and room for children.
    FolderTitled,
    /// A folder too small to nest: one solid block, labeled when big enough.
    FolderBlock,
    /// A file (or link, or special file).
    FileTile,
    /// Several small siblings merged into one block.
    Group,
    /// Free space on the drive. Reserved for the drive-level view.
    Free,
    /// Bytes that could not be read. Reserved for the drive-level view.
    Unreadable,
    /// Space held back by the filesystem. Reserved for the drive-level view.
    Reserved,
    /// The Recycle Bin or Trash. Reserved for the drive-level view.
    RecycleBin,
}

/// State bits of an item. The bit values match the renderer's flags, so
/// they can be copied across.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StateFlags(pub u8);

impl StateFlags {
    /// No state.
    pub const NONE: StateFlags = StateFlags(0);
    /// The pointer is over the box. Set by the UI, never by layout.
    pub const HOVER: StateFlags = StateFlags(1);
    /// The box is selected. Set by the UI, never by layout.
    pub const SELECTED: StateFlags = StateFlags(2);
    /// The box, or a folder above it, was moved to the Recycle Bin or
    /// deleted.
    pub const DELETED: StateFlags = StateFlags(4);
    /// Still being counted: the sizes are a lower bound.
    pub const IN_PROGRESS: StateFlags = StateFlags(8);
    /// Dimmed by the category legend. Set by the UI, never by layout.
    pub const DIMMED: StateFlags = StateFlags(16);

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: StateFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// The flags with `other`'s bits set or cleared.
    pub const fn with(self, other: StateFlags, on: bool) -> StateFlags {
        if on {
            StateFlags(self.0 | other.0)
        } else {
            StateFlags(self.0 & !other.0)
        }
    }
}

impl std::ops::BitOr for StateFlags {
    type Output = StateFlags;
    fn bitor(self, rhs: StateFlags) -> StateFlags {
        StateFlags(self.0 | rhs.0)
    }
}

/// Number of file categories in the palette, including "Other".
pub const CATEGORY_COUNT: u8 = 9;

/// A palette index: 0 to 8, where 8 is "Other". Values of 9 and above are
/// drawn neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CategoryId(pub u8);

impl CategoryId {
    /// The "Other" category.
    pub const OTHER: CategoryId = CategoryId(CATEGORY_COUNT - 1);
    /// No category color: folders, and groups with no dominant category.
    pub const NEUTRAL: CategoryId = CategoryId(255);

    /// Maps the tree's category number to a palette index. The tree uses 0
    /// for "unclassified"; the numbers 1 to 8 select palette entries 0 to 7
    /// and anything else is "Other".
    pub fn from_tree(category: bs_tree::Category) -> CategoryId {
        match category.0 {
            n @ 1..=8 => CategoryId(n - 1),
            _ => CategoryId::OTHER,
        }
    }
}

/// The text of a box: the name, and the size text that goes beside it.
///
/// The size text already carries every qualifier: a leading "≥" while the
/// bytes are a lower bound, and for deleted bytes either the whole text
/// ("Moved to Recycle Bin · 12.3 GB") or a suffix
/// ("41.2 GB · 12.3 GB moved to Recycle Bin").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// The name, converted for display.
    pub name: Box<str>,
    /// The size text.
    pub size: Box<str>,
}

/// One box to draw, in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawItem {
    /// Whole-pixel rectangle.
    pub rect: Rect,
    /// What it is.
    pub kind: ItemKind,
    /// Nesting depth: 0 for the view's own box, saturating at 255.
    pub depth: u8,
    /// Name and size text, present when the box is big enough to carry it.
    pub label: Option<Box<Label>>,
    /// The node id for nodes; for groups, the layout's node count plus the
    /// group's index (see [`crate::Layout::item_ref`]). Unique within one
    /// layout.
    pub id: u32,
    /// Palette index for files and groups; neutral for folders.
    pub category: CategoryId,
    /// Delete and progress state. Hover, selection and dimming are the UI's.
    pub flags: StateFlags,
    /// Allocated bytes the box's area stands for, as counted (a delete does
    /// not change it).
    pub bytes: u64,
}
