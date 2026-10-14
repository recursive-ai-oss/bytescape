//! The generic shapes that must not appear in public text: the private
//! planning files' names, decision, rule, gate and phase id shapes, and
//! process-role words. Names specific to this project's private material
//! (the private repository, host paths and the like) are not listed here;
//! they are loaded at run time from a file with `--extra-patterns`.
//!
//! This is the one file `cargo xtask public-text` does not scan, because it
//! has to spell the forbidden words out.

/// Path of this file, relative to the repository root.
pub const EXEMPT_PATH: &str = "xtask/src/public_text/patterns.rs";

/// Whole words (case-insensitive, with a plural or past form) that belong to
/// the vocabulary of how the work is organised rather than to the product.
pub const PROCESS_WORDS: [&str; 6] = [
    "orchestrator",
    "specialist",
    "architect",
    "reviewer",
    "ruling",
    "waive",
];

/// Endings that still count as the same word ("rulings", "waived").
pub const WORD_ENDINGS: [&str; 5] = ["", "s", "d", "ed", "es"];

/// The word "agent" is also an ordinary word. It is allowed after these words.
pub const AGENT_OK_AFTER: [&str; 3] = ["authentication", "polkit", "user"];

/// The word that needs the exception above.
pub const AGENT_WORD: &str = "agent";

/// Whole words (case-sensitive) naming the private planning files.
pub const PRIVATE_FILE_WORDS: [&str; 4] = ["REQUIREMENTS", "DESIGN", "RULES", "PLAN"];
