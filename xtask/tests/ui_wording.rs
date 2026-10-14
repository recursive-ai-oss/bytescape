//! Tests for the wording check over the words shown inside the program:
//! string literals in the interface crates and the knowledge tables' notes.
//!
//! The banned words are assembled at run time from fragments, so this file
//! stays clean itself.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use xtask::ui_wording::{self, Hit};
use xtask::util::default_root;
use xtask::wording;

fn join(parts: &[&str]) -> String {
    parts.concat()
}

fn freed() -> String {
    join(&["Fre", "ed"])
}

/// A scratch repository below the system temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let number = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ui-wording-{}-{number}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch directory");
        let scratch = Self(dir);
        scratch.write("README.md", "# Bytescape\n");
        scratch.write("crates/bs-ui/src/lib.rs", "");
        scratch
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, text).expect("write file");
    }

    /// Findings of the whole `wording` check as `file:line` strings.
    fn wording(&self) -> Vec<String> {
        wording::run(&self.0, &[])
            .expect("runs")
            .iter()
            .map(ToString::to_string)
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn words_of(source: &str) -> Vec<String> {
    ui_wording::scan_source(source)
        .strings
        .into_iter()
        .map(|literal| literal.text)
        .collect()
}

#[test]
fn a_banned_word_in_a_literal_fails_and_names_file_and_line() {
    let scratch = Scratch::new();
    let source = format!("fn label() -> &'static str {{\n    \"{}\"\n}}\n", freed());
    scratch.write("crates/bs-ui/src/lib.rs", &source);
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].starts_with("crates/bs-ui/src/lib.rs:2: "),
        "{findings:?}"
    );
    assert!(findings[0].contains("freed"), "{findings:?}");
}

#[test]
fn every_crate_under_crates_is_scanned_including_a_new_unlisted_one() {
    for name in [
        "bs-ui",
        "bs-knowledge",
        "bs-gate",
        "bytescape-helper",
        "bs-scan-ext4",
        "a-crate-added-next-year",
    ] {
        let scratch = Scratch::new();
        scratch.write(
            &format!("crates/{name}/src/lib.rs"),
            &format!("const L: &str = \"{}\";\n", freed()),
        );
        let findings = scratch.wording();
        let in_this_crate = format!("crates/{name}/src/lib.rs:1: ");
        assert!(
            findings.iter().any(|f| f.starts_with(&in_this_crate)),
            "{name}: {findings:?}"
        );
    }
}

#[test]
fn a_nested_source_file_of_a_new_crate_is_scanned() {
    let scratch = Scratch::new();
    scratch.write("crates/brand-new/src/lib.rs", "mod deep;\n");
    scratch.write(
        "crates/brand-new/src/deep/inner.rs",
        &format!("pub const L: &str = \"{}\";\n", freed()),
    );
    let findings = scratch.wording();
    assert!(
        findings
            .iter()
            .any(|f| f.starts_with("crates/brand-new/src/deep/inner.rs:1: ")),
        "{findings:?}"
    );
}

#[test]
fn the_legend_labels_of_the_knowledge_crate_are_scanned() {
    let scratch = Scratch::new();
    let label = join(&["Ju", "nk & temp"]);
    scratch.write(
        "crates/bs-knowledge/src/lib.rs",
        &format!(
            "impl Category {{\n    /// The name shown in the legend.\n    pub fn label(self) -> &'static str {{\n        match self {{\n            Category::CacheTemp => \"{label}\",\n            Category::Other => \"Other\",\n        }}\n    }}\n}}\n"
        ),
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].starts_with("crates/bs-knowledge/src/lib.rs:5: "),
        "{findings:?}"
    );
}

#[test]
fn nothing_is_left_out_of_the_scan() {
    assert!(
        ui_wording::SKIPPED_CRATES.is_empty(),
        "a skipped crate needs a reviewed reason and a test that names it"
    );
}

#[test]
fn every_banned_word_is_found_in_a_literal() {
    for text in [
        "Safe to delete",
        "JUNK files",
        "Reclaimable space",
        "reclaim",
        "Clean up now",
        "Freed 3 GB",
    ] {
        let hits = ui_wording::scan_source(&format!("const L: &str = \"{text}\";"));
        let found: usize = hits
            .strings
            .iter()
            .map(|literal| wording::scan_text(&literal.text).len())
            .sum();
        assert_eq!(found, 1, "{text}");
    }
}

#[test]
fn the_same_word_in_a_test_module_does_not_fail() {
    let scratch = Scratch::new();
    let planted = freed();
    let source = format!(
        "pub fn ok() -> &'static str {{ \"Moved to Recycle Bin\" }}\n\n\
         #[cfg(test)]\nmod tests {{\n    pub fn helper() -> &'static str {{ \"{planted}\" }}\n}}\n"
    );
    scratch.write("crates/bs-ui/src/lib.rs", &source);
    assert_eq!(scratch.wording(), Vec::<String>::new());
}

#[test]
fn test_code_in_every_form_is_skipped() {
    let planted = freed();
    let forms = [
        format!("#[cfg(test)]\nmod tests {{ const L: &str = \"{planted}\"; }}"),
        format!("#[cfg(test)]\nconst L: &str = \"{planted}\";"),
        format!("#[cfg(test)]\nfn helper() -> [u8; 2] {{ let _ = \"{planted}\"; [0; 2] }}"),
        format!("#[test]\nfn t() {{ let _ = \"{planted}\"; }}"),
        format!(
            "#[cfg(all(test, target_os = \"linux\"))]\nmod t {{ const L: &str = \"{planted}\"; }}"
        ),
        format!("#![cfg(test)]\nconst L: &str = \"{planted}\";"),
        format!(
            "#[cfg(test)]\nstruct S {{ a: &'static str }}\n#[cfg(test)]\nconst X: S = S {{ a: \"{planted}\" }};"
        ),
    ];
    for form in forms {
        assert!(words_of(&form).is_empty(), "{form}");
    }
}

#[test]
fn code_that_also_builds_outside_tests_is_scanned() {
    let planted = freed();
    for attr in [
        "#[cfg(any(test, feature = \"x\"))]",
        "#[cfg(not(test))]",
        "#[cfg(unix)]",
    ] {
        let source = format!("{attr}\nconst L: &str = \"{planted}\";");
        assert_eq!(words_of(&source), vec![planted.clone()], "{attr}");
    }
}

#[test]
fn code_after_a_test_item_is_scanned_again() {
    let planted = freed();
    let source =
        format!("#[cfg(test)]\nmod tests {{ fn a() {{}} }}\nconst L: &str = \"{planted}\";\n");
    assert_eq!(words_of(&source), vec![planted]);
}

#[test]
fn a_file_declared_as_a_test_module_is_skipped() {
    let scratch = Scratch::new();
    let planted = freed();
    scratch.write(
        "crates/bs-layout/src/lib.rs",
        "#[cfg(test)]\nmod tests;\nmod real;\n",
    );
    scratch.write(
        "crates/bs-layout/src/tests.rs",
        &format!("const L: &str = \"{planted}\";\n"),
    );
    scratch.write(
        "crates/bs-layout/src/real.rs",
        "const L: &str = \"Fine\";\n",
    );
    assert_eq!(scratch.wording(), Vec::<String>::new());

    // The same text in a module that is not a test module fails.
    scratch.write(
        "crates/bs-layout/src/real.rs",
        &format!("const L: &str = \"{planted}\";\n"),
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].starts_with("crates/bs-layout/src/real.rs:1: "));
}

#[test]
fn a_test_module_directory_is_skipped() {
    let scratch = Scratch::new();
    let planted = freed();
    scratch.write("crates/bs-layout/src/view.rs", "#[cfg(test)]\nmod tests;\n");
    scratch.write(
        "crates/bs-layout/src/view/tests.rs",
        &format!("const L: &str = \"{planted}\";\n"),
    );
    assert_eq!(scratch.wording(), Vec::<String>::new());
}

#[test]
fn folders_outside_src_are_not_shipped() {
    let scratch = Scratch::new();
    let line = format!("const L: &str = \"{}\";\n", freed());
    for path in [
        "crates/bs-ui/tests/t.rs",
        "crates/bs-ui/benches/b.rs",
        "crates/bs-ui/examples/e.rs",
        "crates/bs-ui/build.rs",
    ] {
        scratch.write(path, &line);
    }
    assert_eq!(scratch.wording(), Vec::<String>::new());
}

#[test]
fn comments_and_identifiers_do_not_fail() {
    let planted = freed();
    let source = format!(
        "// {planted} in a comment\n/* {planted} /* nested */ still a comment */\n\
         /// {planted} in a doc comment\n//! {planted}\n\
         fn {}_space(safe_mode: bool, junk: u8) {{}}\n",
        planted.to_lowercase()
    );
    assert!(words_of(&source).is_empty());
}

#[test]
fn the_product_name_disk_cleanup_passes_but_clean_up_does_not() {
    assert!(
        wording::scan_text("Open Disk Cleanup").is_empty(),
        "one word: the Windows tool's name"
    );
    let source = r#"const A: &str = "Open Disk Cleanup"; const B: &str = "Clean up"; const C: &str = "clean
up";"#;
    let hits = ui_wording::scan_source(source);
    let found: Vec<_> = hits
        .strings
        .iter()
        .flat_map(|literal| {
            wording::scan_text_positions(&literal.text)
                .into_iter()
                .map(move |(at, w)| (literal.line_of(at), w))
        })
        .collect();
    assert_eq!(found, vec![(1, "clean up"), (1, "clean up")]);
}

#[test]
fn escapes_and_raw_and_byte_strings_are_decoded() {
    let planted = freed();
    let escaped: String = planted
        .chars()
        .map(|c| format!("\\u{{{:x}}}", c as u32))
        .collect();
    for source in [
        format!("const L: &str = \"{escaped}\";"),
        format!("const L: &str = \"{}\\x65d\";", &planted[..3]),
        format!("const L: &str = r#\"{planted}\"#;"),
        format!("const L: &[u8] = b\"{planted}\";"),
        format!("const L: &[u8] = br##\"{planted}\"##;"),
        format!("const L: &str = c\"{planted}\";"),
        format!("const L: &str = \"{planted}\\\n    \";"),
    ] {
        let found: Vec<_> = words_of(&source)
            .iter()
            .flat_map(|text| wording::scan_text(text))
            .collect();
        assert_eq!(found.len(), 1, "{source}");
    }
}

#[test]
fn a_line_continuation_keeps_line_numbers_right() {
    let planted = freed();
    let source = format!("const A: &str = \"one \\\n     two \\\n     {planted}\";\n");
    let scan = ui_wording::scan_source(&source);
    assert_eq!(scan.strings.len(), 1);
    let found = wording::scan_text_positions(&scan.strings[0].text);
    assert_eq!(found.len(), 1);
    assert_eq!(scan.strings[0].line_of(found[0].0), 3);
}

#[test]
fn a_word_split_by_a_line_continuation_is_one_word() {
    // Rust drops the backslash, the break and the white space after it, so
    // this literal is "Space is not freed until the bin is emptied".
    let source = "const A: &str = \"Space is not fre\\\n        ed until\\\n   \\\n  the bin is emptied\";\n";
    let scan = ui_wording::scan_source(source);
    assert_eq!(
        scan.strings[0].text,
        "Space is not freed untilthe bin is emptied"
    );
    let found = wording::scan_text_positions(&scan.strings[0].text);
    assert_eq!(found.len(), 1, "{:?}", scan.strings[0].text);
    assert_eq!(found[0].1, "freed");
    assert_eq!(scan.strings[0].line_of(found[0].0), 1);

    // A phrase split between its words (the break after "clean ") is seen too.
    let phrase = "const A: &str = \"clean \\\n   up\";";
    let scan = ui_wording::scan_source(phrase);
    assert_eq!(wording::scan_text(&scan.strings[0].text).len(), 1);

    // Through the whole check, with the line where the word starts.
    let scratch = Scratch::new();
    scratch.write(
        "crates/bs-ui/src/lib.rs",
        &format!(
            "fn f() -> &'static str {{\n    \"Not {}\\\n       {}\"\n}}\n",
            "fre", "ed"
        ),
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].starts_with("crates/bs-ui/src/lib.rs:2: "),
        "{findings:?}"
    );
}

#[test]
fn an_escaped_newline_is_not_a_source_line() {
    let planted = freed();
    let source = format!("const A: &str = \"one\\n{planted}\";\n");
    let scan = ui_wording::scan_source(&source);
    let found = wording::scan_text_positions(&scan.strings[0].text);
    assert_eq!(scan.strings[0].line_of(found[0].0), 1);
}

#[test]
fn character_literals_and_lifetimes_do_not_derail_the_scan() {
    let planted = freed();
    let source = format!(
        "fn f<'a>(x: &'a str) -> char {{ let q = '\"'; let b = '\\''; let e = '\\u{{41}}'; 'label: loop {{ break 'label; }} let _ = (q, b, e); \"{planted}\"; 'x' }}"
    );
    assert_eq!(words_of(&source), vec![planted]);
}

#[test]
fn raw_identifiers_are_not_strings() {
    assert!(words_of("let r#type = 1; let r#fn = r#type;").is_empty());
}

#[test]
fn strings_in_compiler_attributes_are_ignored_but_other_attributes_are_scanned() {
    let planted = freed();
    let source = format!(
        "#[cfg(feature = \"{planted}\")]\nfn a() {{}}\n\
         #[allow(clippy::x, reason = \"{planted}\")]\nfn b() {{}}\n\
         #[doc = \"{planted}\"]\nfn c() {{}}\n\
         #[error(\"{planted}\")]\nstruct E;\n"
    );
    let scan = ui_wording::scan_source(&source);
    assert_eq!(scan.strings.len(), 1, "{:?}", scan.strings);
    assert_eq!(scan.strings[0].line, 7);
}

#[test]
fn include_str_is_reported() {
    let scratch = Scratch::new();
    scratch.write(
        "crates/bs-ui/src/lib.rs",
        "const T: &str = include_str!(\"text.txt\");\n",
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("include_str!"), "{findings:?}");
    // Inside test code it is fine.
    scratch.write(
        "crates/bs-ui/src/lib.rs",
        "#[cfg(test)]\nmod t { const T: &str = include_str!(\"text.txt\"); }\n",
    );
    assert_eq!(scratch.wording(), Vec::<String>::new());
}

#[test]
fn knowledge_notes_are_scanned_and_comments_in_the_tables_are_not() {
    let scratch = Scratch::new();
    let text = format!(
        "# a note never says {}\n[[entry]]\npattern = \"~/x\"\nnote = \"Cache folder.\"\n\n[[entry]]\npattern = \"~/y\"\nnote = \"\"\"\nline one\nit is {}\n\"\"\"\n",
        freed(),
        freed().to_lowercase()
    );
    scratch.write("crates/bs-knowledge/data/recognition.toml", &text);
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].starts_with("crates/bs-knowledge/data/recognition.toml:10: "),
        "{findings:?}"
    );
}

#[test]
fn toml_strings_cover_the_quoting_forms() {
    let planted = freed();
    let text = format!(
        "a = '{planted}'\nb = \"\\u0046reed\"\nc = '''\n{planted}\n'''\nd = [\"x\", \"y\"] # \"{planted}\"\n"
    );
    let strings = ui_wording::toml_strings(&text);
    let hit: Vec<_> = strings
        .iter()
        .filter(|literal| !wording::scan_text(&literal.text).is_empty())
        .map(|literal| literal.line)
        .collect();
    assert_eq!(hit, vec![1, 2, 3], "{strings:?}");
}

#[test]
fn a_toml_line_ending_backslash_joins_the_words_and_keeps_lines_right() {
    let text = "a = \"\"\"\nIt is not fre\\\n    ed here\"\"\"\nb = \"\"\"\none \\   \n\n  two\"\"\"\nc = \"x\"\n";
    let strings = ui_wording::toml_strings(text);
    assert_eq!(strings.len(), 3, "{strings:?}");
    assert_eq!(strings[0].text, "\nIt is not freed here");
    let found = wording::scan_text_positions(&strings[0].text);
    assert_eq!(found.len(), 1);
    assert_eq!(strings[0].line_of(found[0].0), 2);
    assert_eq!(strings[1].text, "\none two");
    assert_eq!(strings[2].line, 8, "lines after a continuation stay right");
}

#[test]
fn a_banned_word_split_by_a_toml_continuation_is_found_in_the_tables() {
    let scratch = Scratch::new();
    scratch.write(
        "crates/bs-knowledge/data/recognition.toml",
        "[[entry]]\nnote = \"\"\"\nNot fre\\\n   ed.\"\"\"\n",
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].starts_with("crates/bs-knowledge/data/recognition.toml:3: "),
        "{findings:?}"
    );
}

/// The lint in bs-knowledge, in the shape the check reads.
fn knowledge_lint(words: &str, stems: &str, phrases: &str) -> String {
    format!(
        "const WORDS: [&str; 9] = [\n{words}\n];\nconst STEMS: [&str; 2] = [{stems}];\nconst PHRASES: [(&str, &str); 1] = [{phrases}];\n"
    )
}

#[test]
fn the_knowledge_lint_must_ban_everything_the_wording_rule_bans() {
    let full = knowledge_lint(
        "\"safe\", \"junk\", \"freed\", \"safely\"",
        "\"reclaim\"",
        "(\"clean\", \"up\")",
    );
    assert!(ui_wording::knowledge_lint_gaps(&full).is_empty());
    let cases = [
        (
            knowledge_lint("\"junk\", \"freed\"", "\"reclaim\"", "(\"clean\", \"up\")"),
            "safe",
        ),
        (
            knowledge_lint("\"safe\", \"freed\"", "\"reclaim\"", "(\"clean\", \"up\")"),
            "junk",
        ),
        (
            knowledge_lint("\"safe\", \"junk\"", "\"reclaim\"", "(\"clean\", \"up\")"),
            "freed",
        ),
        (
            knowledge_lint(
                "\"safe\", \"junk\", \"freed\"",
                "\"other\"",
                "(\"clean\", \"up\")",
            ),
            "reclaim*",
        ),
        (
            knowledge_lint(
                "\"safe\", \"junk\", \"freed\"",
                "\"reclaim\"",
                "(\"free\", \"up\")",
            ),
            "clean up",
        ),
    ];
    for (lint, missing) in cases {
        assert_eq!(
            ui_wording::knowledge_lint_gaps(&lint),
            vec![missing.to_string()]
        );
    }
    assert_eq!(ui_wording::knowledge_lint_gaps("").len(), 5);
}

#[test]
fn a_commented_out_entry_in_the_knowledge_lint_does_not_count() {
    // `safe` appears only in comments and in an unrelated table.
    let lint = "const WORDS: [&str; 2] = [\n    // \"safe\",\n    /* \"safe\", */\n    \"junk\",\n    \"freed\",\n];\nconst STEMS: [&str; 1] = [\"reclaim\"];\nconst PHRASES: [(&str, &str); 1] = [(\"clean\", \"up\")];\nconst OTHER: [&str; 1] = [\"safe\"];\n// \"safe\"\n";
    assert_eq!(
        ui_wording::knowledge_lint_gaps(lint),
        vec!["safe".to_string()]
    );
}

#[test]
fn the_comparison_follows_the_one_banned_list() {
    // A lint built from the shared constants has no gaps, and dropping any
    // one entry is reported by name, so a word added to the list is noticed.
    let words: Vec<String> = wording::BANNED_WORDS
        .iter()
        .map(|w| format!("\"{w}\""))
        .collect();
    let stems: Vec<String> = wording::BANNED_STEMS
        .iter()
        .map(|(stem, _)| format!("\"{stem}\""))
        .collect();
    let phrases: Vec<String> = wording::BANNED_PHRASES
        .iter()
        .map(|(a, b, _)| format!("(\"{a}\", \"{b}\")"))
        .collect();
    let build = |words: &[String], stems: &[String], phrases: &[String]| {
        knowledge_lint(&words.join(", "), &stems.join(", "), &phrases.join(", "))
    };
    assert!(ui_wording::knowledge_lint_gaps(&build(&words, &stems, &phrases)).is_empty());
    for index in 0..words.len() {
        let mut fewer = words.clone();
        fewer.remove(index);
        assert_eq!(
            ui_wording::knowledge_lint_gaps(&build(&fewer, &stems, &phrases)),
            vec![wording::BANNED_WORDS[index].to_string()]
        );
    }
    assert_eq!(
        ui_wording::knowledge_lint_gaps(&build(&words, &[], &phrases)),
        vec![wording::BANNED_STEMS[0].1.to_string()]
    );
    assert_eq!(
        ui_wording::knowledge_lint_gaps(&build(&words, &stems, &[])),
        vec![wording::BANNED_PHRASES[0].2.to_string()]
    );
}

#[test]
fn the_check_compares_the_real_knowledge_lint_when_the_crate_exists() {
    let scratch = Scratch::new();
    scratch.write("crates/bs-knowledge/Cargo.toml", "[package]\n");
    // As in the real crate, the lint is a test-only module.
    scratch.write(
        "crates/bs-knowledge/src/lib.rs",
        "#[cfg(test)]\nmod lint;\n",
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("lint.rs"), "{findings:?}");

    scratch.write(
        "crates/bs-knowledge/src/lint.rs",
        &knowledge_lint("\"safe\"", "\"reclaim\"", "(\"clean\", \"up\")"),
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 2, "{findings:?}");
    assert!(findings.iter().all(|f| f.contains("does not ban")));
}

#[test]
fn the_repository_knowledge_lint_agrees_with_the_wording_rule() {
    let lint = default_root().join("crates/bs-knowledge/src/lint.rs");
    let source = std::fs::read_to_string(&lint).expect("crates/bs-knowledge/src/lint.rs exists");
    assert_eq!(
        ui_wording::knowledge_lint_gaps(&source),
        Vec::<String>::new()
    );
}

fn hit(path: &str, word: &'static str, literal: &str) -> Hit {
    Hit {
        rel: path.to_string(),
        line: 1,
        word,
        literal: literal.to_string(),
    }
}

#[test]
fn exceptions_are_exact_and_must_be_used() {
    let exceptions = ui_wording::parse_exceptions(
        "# reviewed\ncrates/bs-ui/src/a.rs | safe | Safe Mode boot option\n",
    )
    .expect("parses");
    assert_eq!(exceptions.len(), 1);

    let hits = vec![
        hit(
            "crates/bs-ui/src/a.rs",
            "safe",
            "Windows Safe Mode boot option",
        ),
        hit("crates/bs-ui/src/a.rs", "safe", "Safe to delete"),
        hit("crates/bs-ui/src/b.rs", "safe", "Safe Mode boot option"),
        hit(
            "crates/bs-ui/src/a.rs",
            "junk",
            "Safe Mode boot option junk",
        ),
    ];
    let (remaining, unused) = ui_wording::apply_exceptions(hits, &exceptions);
    assert_eq!(remaining.len(), 3, "only the exact file, word and text");
    assert!(unused.is_empty());

    let (_, unused) = ui_wording::apply_exceptions(Vec::new(), &exceptions);
    assert_eq!(unused, exceptions, "an exception that matches nothing");
}

#[test]
fn the_exception_file_is_read_and_stale_entries_fail() {
    let scratch = Scratch::new();
    let planted = format!("Boot {} Mode", join(&["sa", "fe"]));
    scratch.write(
        "crates/bs-ui/src/lib.rs",
        &format!("const L: &str = \"{planted}\";\n"),
    );
    assert_eq!(scratch.wording().len(), 1);

    scratch.write(
        "xtask/wording.allow",
        &format!("crates/bs-ui/src/lib.rs | safe | {planted}\n"),
    );
    assert_eq!(scratch.wording(), Vec::<String>::new());

    scratch.write(
        "crates/bs-ui/src/lib.rs",
        "const L: &str = \"Boot Mode\";\n",
    );
    let findings = scratch.wording();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("matches no text"), "{findings:?}");
}

#[test]
fn malformed_exceptions_are_refused() {
    for text in ["just one part", "a | b", "a | | c", "| b | c"] {
        assert!(ui_wording::parse_exceptions(text).is_err(), "{text}");
    }
}

#[test]
fn the_repository_passes() {
    let findings = ui_wording::run(&default_root()).expect("runs");
    let lines: Vec<String> = findings.iter().map(ToString::to_string).collect();
    assert!(lines.is_empty(), "{lines:#?}");
}
