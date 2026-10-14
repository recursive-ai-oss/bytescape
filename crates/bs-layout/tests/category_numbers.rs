//! The tree numbers categories and the layout maps those numbers to palette
//! entries. The numbers come from the knowledge tables, so the three must
//! agree.

use bs_knowledge::Category as Known;
use bs_layout::{CATEGORY_COUNT, CategoryId};
use bs_tree::Category;

#[test]
fn the_palette_order_follows_the_knowledge_numbers() {
    // 0 is unclassified and drawn as Other (the last entry); 1 to 8 are
    // palette entries 0 to 7 in the order of the tables.
    assert_eq!(usize::from(CATEGORY_COUNT), Known::ALL.len());
    for (position, known) in Known::ALL.iter().enumerate() {
        assert_eq!(usize::from(known.number()), position, "{known:?}");
        let palette = CategoryId::from_tree(Category(known.number()));
        match known {
            Known::Other => assert_eq!(palette, CategoryId::OTHER),
            colored => assert_eq!(
                usize::from(palette.0) + 1,
                usize::from(colored.number()),
                "{colored:?}"
            ),
        }
    }
    // Numbers outside the tables are drawn as Other.
    for n in 9..=255u8 {
        assert_eq!(CategoryId::from_tree(Category(n)), CategoryId::OTHER, "{n}");
        assert_eq!(Known::from_number(n), None);
    }
}
