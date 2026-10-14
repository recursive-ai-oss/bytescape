//! The wording rule for notes, as a function the tests apply.
//!
//! A note says what a thing is and who manages it. It never calls anything
//! safe, junk or reclaimable, never says space was freed, and never tells the
//! reader to delete or remove. The banned words are matched case-insensitively
//! as whole words, so "unsafe" and "unremoved" are different words and pass.

/// One reason a note was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Violation {
    pub word: String,
}

/// Words that are banned whole.
const WORDS: [&str; 29] = [
    "safe",
    "safely",
    "junk",
    "freed",
    "frees",
    "freeing",
    "unneeded",
    "unnecessary",
    "useless",
    "worthless",
    "should",
    "obsolete",
    "delete",
    "deleted",
    "deletes",
    "deleting",
    "deletion",
    "remove",
    "removed",
    "removes",
    "removing",
    "removal",
    "purge",
    "purged",
    "purging",
    "disposable",
    "bloat",
    "recommended",
    "recommend",
];

/// Words that are banned with any ending: "reclaim" covers "reclaimable".
const STEMS: [&str; 2] = ["reclaim", "recommend"];

/// Two-word phrases that are banned.
const PHRASES: [(&str, &str); 5] = [
    ("clean", "up"),
    ("free", "up"),
    ("can", "be"),
    ("you", "can"),
    ("safe", "to"),
];

/// The words of a text, lower case: runs of letters and digits.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Everything wrong with one note.
pub(crate) fn check(note: &str) -> Vec<Violation> {
    let words = words(note);
    let mut found = Vec::new();
    for (index, word) in words.iter().enumerate() {
        if WORDS.contains(&word.as_str()) || STEMS.iter().any(|s| word.starts_with(s)) {
            found.push(Violation { word: word.clone() });
        }
        if let Some(next) = words.get(index + 1)
            && PHRASES.iter().any(|(a, b)| word == a && next == b)
        {
            found.push(Violation {
                word: format!("{word} {next}"),
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flagged(text: &str) -> bool {
        !check(text).is_empty()
    }

    #[test]
    fn the_banned_words_are_caught_in_any_case_and_spelling() {
        let bad = [
            "Safe to delete.",
            "this is SAFE",
            "Junk files.",
            "junk",
            "JUNK-like",
            "Reclaimable space.",
            "reclaim it",
            "Reclaiming space",
            "Clean up this folder.",
            "clean  up",
            "Clean\nup",
            "clean-up",
            "CLEAN UP",
            "Freed on exit.",
            "Frees space.",
            "Freeing space.",
            "Free up space.",
            "You can remove it.",
            "Delete it.",
            "Deleting is fine.",
            "Removed by hand.",
            "Safely removed.",
            "Unnecessary files.",
            "Disposable data.",
            "Purge the folder.",
            "Bloat.",
            "We recommend it.",
            "It should go.",
            "Can be rebuilt.",
        ];
        for text in bad {
            assert!(flagged(text), "should be flagged: {text:?}");
        }
    }

    #[test]
    fn plain_factual_notes_pass() {
        let good = [
            "Package cache managed by APT.",
            "Previous Windows installation kept by Windows Setup; Disk Cleanup and Storage settings manage it.",
            "Per-user cache folder from the XDG base directory specification.",
            "Unsafe code is a different word.",
            "Cleanup is a different word.",
            "Safeguard and freedom are different words.",
            "Folder of a program that Windows keeps.",
            "Mount points of removable media.",
        ];
        for text in good {
            assert!(!flagged(text), "should pass: {text:?} -> {:?}", check(text));
        }
    }

    #[test]
    fn the_violation_names_the_word() {
        assert_eq!(
            check("Safe to delete."),
            vec![
                Violation {
                    word: "safe".into()
                },
                Violation {
                    word: "safe to".into()
                },
                Violation {
                    word: "delete".into()
                },
            ]
        );
    }
}
