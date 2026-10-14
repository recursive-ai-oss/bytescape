//! Turns the TOML data files into Rust tables at build time.
//!
//! Parsing happens here, so the library itself has no parser and no
//! dependencies, which keeps it fit for the elevated helper. The data files use
//! a small subset of TOML: `[[table]]` headers, `key = "string"`,
//! `key = ["list", "of", "strings"]`, with `#` comments. Anything else stops the build with the file and line.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
enum Value {
    Text(String),
    List(Vec<String>),
}

#[derive(Debug)]
struct Table {
    name: String,
    line: usize,
    fields: Vec<(String, Value, usize)>,
}

struct Reader<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
    file: &'a str,
}

impl<'a> Reader<'a> {
    fn new(file: &'a str, text: &'a str) -> Self {
        Self {
            chars: text.chars().peekable(),
            line: 1,
            file,
        }
    }

    fn error(&self, message: &str) -> String {
        format!("{}:{}: {message}", self.file, self.line)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c == Some('\n') {
            self.line += 1;
        }
        c
    }

    /// Skips spaces, tabs and comments on the current line.
    fn skip_inline(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c == ' ' || c == '\t' || c == '\r' {
                self.bump();
            } else if c == '#' {
                while let Some(&c) = self.chars.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    /// Skips blanks, comments and line breaks.
    fn skip_all(&mut self) {
        loop {
            self.skip_inline();
            if self.chars.peek() == Some(&'\n') {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.bump() != Some('"') {
            return Err(self.error("expected a string in double quotes"));
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.error("unterminated string")),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    _ => return Err(self.error("unsupported escape in string")),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.chars.peek() {
            Some('"') => Ok(Value::Text(self.string()?)),
            Some('[') => {
                self.bump();
                let mut items = Vec::new();
                loop {
                    self.skip_all();
                    match self.chars.peek() {
                        Some(']') => {
                            self.bump();
                            return Ok(Value::List(items));
                        }
                        Some('"') => {
                            items.push(self.string()?);
                            self.skip_all();
                            match self.chars.peek() {
                                Some(',') => {
                                    self.bump();
                                }
                                Some(']') => {}
                                _ => return Err(self.error("expected `,` or `]` in list")),
                            }
                        }
                        _ => return Err(self.error("expected a string or `]` in list")),
                    }
                }
            }
            _ => Err(self.error("expected a string or a list of strings")),
        }
    }
}

fn parse(file: &str, text: &str) -> Result<Vec<Table>, String> {
    let mut reader = Reader::new(file, text);
    let mut tables: Vec<Table> = Vec::new();
    loop {
        reader.skip_all();
        let Some(&c) = reader.chars.peek() else {
            return Ok(tables);
        };
        if c == '[' {
            reader.bump();
            if reader.bump() != Some('[') {
                return Err(reader.error("only [[table]] headers are supported"));
            }
            let line = reader.line;
            let mut name = String::new();
            loop {
                match reader.bump() {
                    Some(']') => break,
                    Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => name.push(c),
                    _ => return Err(reader.error("bad table name")),
                }
            }
            if reader.bump() != Some(']') {
                return Err(reader.error("expected `]]`"));
            }
            tables.push(Table {
                name,
                line,
                fields: Vec::new(),
            });
        } else if c.is_ascii_alphabetic() {
            let line = reader.line;
            let mut key = String::new();
            while let Some(&c) = reader.chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    key.push(c);
                    reader.bump();
                } else {
                    break;
                }
            }
            reader.skip_inline();
            if reader.bump() != Some('=') {
                return Err(reader.error("expected `=` after the key"));
            }
            reader.skip_inline();
            let value = reader.value()?;
            reader.skip_inline();
            match reader.chars.peek() {
                None | Some('\n') => {}
                _ => return Err(reader.error("unexpected text after the value")),
            }
            let Some(table) = tables.last_mut() else {
                return Err(reader.error("a key needs a [[table]] before it"));
            };
            if table.fields.iter().any(|(k, _, _)| *k == key) {
                return Err(format!("{file}:{line}: key `{key}` is given twice"));
            }
            table.fields.push((key, value, line));
        } else {
            return Err(reader.error("unexpected character"));
        }
    }
}

// ---------------------------------------------------------------- checks

const CATEGORIES: [(&str, &str); 9] = [
    ("other", "Other"),
    ("video", "Video"),
    ("images", "Images"),
    ("audio", "Audio"),
    ("documents", "Documents"),
    ("archives", "Archives"),
    ("apps-system", "AppsSystem"),
    ("code-dev", "CodeDev"),
    ("cache-temp", "CacheTemp"),
];

fn category(file: &str, table: &Table, text: &str) -> Result<&'static str, String> {
    CATEGORIES
        .iter()
        .find(|(name, _)| *name == text)
        .map(|(_, variant)| *variant)
        .ok_or_else(|| format!("{file}:{}: unknown category `{text}`", table.line))
}

struct Fields<'a> {
    file: &'a str,
    table: &'a Table,
}

impl<'a> Fields<'a> {
    fn check_keys(&self, allowed: &[&str]) -> Result<(), String> {
        for (key, _, line) in &self.table.fields {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("{}:{line}: unknown key `{key}`", self.file));
            }
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.table
            .fields
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v)
    }

    fn text(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Text(t)) => Ok(Some(t.as_str())),
            Some(_) => Err(format!(
                "{}:{}: `{key}` must be a string",
                self.file, self.table.line
            )),
        }
    }

    fn required(&self, key: &str) -> Result<&'a str, String> {
        match self.text(key)? {
            Some(t) if !t.trim().is_empty() => Ok(t),
            _ => Err(format!(
                "{}:{}: `{key}` is required",
                self.file, self.table.line
            )),
        }
    }

    fn os(&self) -> Result<&'static str, String> {
        match self.text("os")?.unwrap_or("any") {
            "any" => Ok("Os::Any"),
            "windows" => Ok("Os::Windows"),
            "linux" => Ok("Os::Linux"),
            other => Err(format!(
                "{}:{}: unknown os `{other}`",
                self.file, self.table.line
            )),
        }
    }
}

fn pattern_check(file: &str, table: &Table, pattern: &str) -> Result<(), String> {
    let bad = pattern.is_empty()
        || pattern.contains('\\')
        || pattern.contains('\0')
        || pattern.trim() != pattern;
    if bad {
        return Err(format!(
            "{file}:{}: pattern `{pattern}` is empty, has a backslash or stray spaces",
            table.line
        ));
    }
    let percent = pattern.matches('%').count();
    if !percent.is_multiple_of(2) {
        return Err(format!(
            "{file}:{}: unbalanced `%` in `{pattern}`",
            table.line
        ));
    }
    Ok(())
}

fn variables_in(pattern: &str, into: &mut Vec<String>) {
    let mut parts = pattern.split('%');
    parts.next();
    for (index, part) in parts.enumerate() {
        if index % 2 == 0 && !part.is_empty() {
            into.push(part.to_ascii_uppercase());
        }
    }
    if pattern.starts_with('~') {
        into.push("HOME".into());
        into.push("USERPROFILE".into());
    }
}

fn q(text: &str) -> String {
    format!("{text:?}")
}

fn generate(dir: &std::path::Path) -> Result<String, String> {
    let read = |name: &str| -> Result<(String, String), String> {
        let path = dir.join(name);
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((name.to_owned(), text))
    };
    let mut out = String::new();
    let mut variables: Vec<String> = Vec::new();

    // ---- categories
    let (file, text) = read("categories.toml")?;
    let tables = parse(&file, &text)?;
    let mut extensions: Vec<(String, &'static str)> = Vec::new();
    let mut paths = String::new();
    for table in &tables {
        let f = Fields { file: &file, table };
        match table.name.as_str() {
            "extensions" => {
                f.check_keys(&["category", "list"])?;
                let cat = category(&file, table, f.required("category")?)?;
                let Some(Value::List(list)) = f.get("list") else {
                    return Err(format!("{file}:{}: `list` must be a list", table.line));
                };
                for ext in list {
                    let ok = !ext.is_empty()
                        && ext.len() <= 15
                        && ext
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
                    if !ok {
                        return Err(format!(
                            "{file}:{}: extension `{ext}` must be lower case letters and digits, \
                             up to 15, without a dot",
                            table.line
                        ));
                    }
                    if let Some((_, other)) = extensions.iter().find(|(e, _)| e == ext) {
                        return Err(format!(
                            "{file}:{}: extension `{ext}` is already listed under {other}",
                            table.line
                        ));
                    }
                    extensions.push((ext.clone(), cat));
                }
            }
            "paths" => {
                f.check_keys(&["pattern", "os", "category"])?;
                let pattern = f.required("pattern")?;
                pattern_check(&file, table, pattern)?;
                variables_in(pattern, &mut variables);
                let cat = category(&file, table, f.required("category")?)?;
                writeln!(
                    paths,
                    "    PathRule {{ pattern: {}, os: {}, category: Category::{cat} }},",
                    q(pattern),
                    f.os()?
                )
                .unwrap();
            }
            other => {
                return Err(format!("{file}:{}: unknown table [[{other}]]", table.line));
            }
        }
    }
    extensions.sort();
    writeln!(
        out,
        "pub(crate) static EXTENSIONS: &[(&str, Category)] = &["
    )
    .unwrap();
    for (ext, cat) in &extensions {
        writeln!(out, "    ({}, Category::{cat}),", q(ext)).unwrap();
    }
    writeln!(out, "];\n").unwrap();
    writeln!(
        out,
        "pub(crate) static PATH_RULES: &[PathRule] = &[\n{paths}];\n"
    )
    .unwrap();

    // ---- recognition
    let (file, text) = read("recognition.toml")?;
    let tables = parse(&file, &text)?;
    writeln!(out, "pub(crate) static RECOGNITION: &[Entry] = &[").unwrap();
    for table in &tables {
        if table.name != "entry" {
            return Err(format!(
                "{file}:{}: unknown table [[{}]]",
                table.line, table.name
            ));
        }
        let f = Fields { file: &file, table };
        f.check_keys(&["pattern", "os", "category", "contents", "note", "handoff"])?;
        let pattern = f.required("pattern")?;
        pattern_check(&file, table, pattern)?;
        variables_in(pattern, &mut variables);
        let cat = category(&file, table, f.required("category")?)?;
        let inherit = match f.text("contents")?.unwrap_or("inherit") {
            "inherit" => true,
            "own" => false,
            other => {
                return Err(format!("{file}:{}: unknown contents `{other}`", table.line));
            }
        };
        let handoff = match f.text("handoff")? {
            None => "None",
            Some("uninstall") => "Some(Handoff::Uninstall)",
            Some("disk-cleanup") => "Some(Handoff::DiskCleanup)",
            Some("package") => "Some(Handoff::Package)",
            Some(other) => {
                return Err(format!("{file}:{}: unknown handoff `{other}`", table.line));
            }
        };
        writeln!(
            out,
            "    Entry {{ pattern: {}, os: {}, category: Category::{cat}, inherit: {inherit}, \
             note: {}, handoff: {handoff} }},",
            q(pattern),
            f.os()?,
            q(f.required("note")?),
        )
        .unwrap();
    }
    writeln!(out, "];\n").unwrap();

    // ---- protected
    let (file, text) = read("protected.toml")?;
    let tables = parse(&file, &text)?;
    writeln!(out, "pub(crate) static PROTECTED: &[ProtectedEntry] = &[").unwrap();
    for table in &tables {
        if table.name != "entry" {
            return Err(format!(
                "{file}:{}: unknown table [[{}]]",
                table.line, table.name
            ));
        }
        let f = Fields { file: &file, table };
        f.check_keys(&["pattern", "os", "level", "scope", "note"])?;
        let pattern = f.required("pattern")?;
        pattern_check(&file, table, pattern)?;
        variables_in(pattern, &mut variables);
        let level = match f.required("level")? {
            "blocked" => "Level::Blocked",
            "protected" => "Level::Protected",
            other => return Err(format!("{file}:{}: unknown level `{other}`", table.line)),
        };
        let exact = match f.text("scope")?.unwrap_or("subtree") {
            "subtree" => false,
            "exact" => true,
            other => return Err(format!("{file}:{}: unknown scope `{other}`", table.line)),
        };
        let note = match f.text("note")? {
            Some(n) if !n.trim().is_empty() => format!("Some({})", q(n)),
            _ if level == "Level::Protected" => {
                return Err(format!(
                    "{file}:{}: a protected entry needs a note",
                    table.line
                ));
            }
            _ => "None".to_owned(),
        };
        writeln!(
            out,
            "    ProtectedEntry {{ pattern: {}, os: {}, level: {level}, exact: {exact}, note: {note} }},",
            q(pattern),
            f.os()?,
        )
        .unwrap();
    }
    writeln!(out, "];\n").unwrap();

    variables.sort();
    variables.dedup();
    writeln!(out, "pub(crate) static VARIABLES: &[&str] = &[").unwrap();
    for name in &variables {
        writeln!(out, "    {},", q(name)).unwrap();
    }
    writeln!(out, "];").unwrap();
    Ok(out)
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let data = manifest.join("data");
    println!("cargo:rerun-if-changed=data");
    println!("cargo:rerun-if-changed=build.rs");
    match generate(&data) {
        Ok(source) => {
            let out =
                PathBuf::from(std::env::var_os("OUT_DIR").expect("out dir")).join("tables.rs");
            fs::write(out, source).expect("write generated tables");
        }
        Err(message) => {
            eprintln!("error in the knowledge data files: {message}");
            std::process::exit(1);
        }
    }
}
