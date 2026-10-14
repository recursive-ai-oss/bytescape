//! `cargo xtask <command> [--root DIR] [--extra-patterns FILE] [PATH...]`
//!
//! Commands: `net-ban`, `helper-deps`, `wording`, `public-text`,
//! `unsafe-allow`, and `guards` (all five). Exit status 0 when clean, 1 when a
//! check found problems, 2 when a check could not run.

use std::path::PathBuf;
use std::process::ExitCode;
use xtask::public_text::Extra;
use xtask::util::{Finding, default_root};
use xtask::{helper_deps, net_ban, public_text, unsafe_allow, wording};

const USAGE: &str = "usage: cargo xtask <net-ban|helper-deps|wording|public-text|unsafe-allow|guards> \
    [--root DIR] [--extra-patterns FILE] [PATH...]\n\
    PATH arguments are extra files or directories for `wording` (for example store listing text).\n\
    --extra-patterns FILE adds names to `public-text` that must not be written in this repository.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let mut root = default_root();
    let mut extra = Vec::new();
    let mut extra_patterns = Extra::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => match args.next() {
                Some(dir) => root = PathBuf::from(dir),
                None => {
                    eprintln!("--root needs a directory\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--extra-patterns" => {
                let Some(file) = args.next() else {
                    eprintln!("--extra-patterns needs a file\n{USAGE}");
                    return ExitCode::from(2);
                };
                let parsed = std::fs::read_to_string(&file)
                    .map_err(|error| error.to_string())
                    .and_then(|text| Extra::parse(&text));
                match parsed {
                    Ok(patterns) => extra_patterns = patterns,
                    Err(message) => {
                        eprintln!("could not use {file}: {message}");
                        return ExitCode::from(2);
                    }
                }
            }
            _ => extra.push(PathBuf::from(arg)),
        }
    }

    let commands: Vec<&str> = match command.as_str() {
        "net-ban" | "helper-deps" | "wording" | "public-text" | "unsafe-allow" => {
            vec![command.as_str()]
        }
        "guards" => vec![
            "net-ban",
            "helper-deps",
            "wording",
            "public-text",
            "unsafe-allow",
        ],
        _ => {
            eprintln!("unknown command `{command}`\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let mut worst = 0u8;
    for name in commands {
        let result: Result<Vec<Finding>, String> = match name {
            "net-ban" => net_ban::run(&root),
            "helper-deps" => helper_deps::run(&root),
            "wording" => wording::run(&root, &extra),
            "unsafe-allow" => unsafe_allow::run(&root),
            _ => public_text::run(&root, &extra_patterns),
        };
        match result {
            Ok(findings) if findings.is_empty() => println!("{name}: ok"),
            Ok(findings) => {
                for finding in &findings {
                    eprintln!("{name}: {finding}");
                }
                eprintln!("{name}: FAILED ({} problem(s))", findings.len());
                worst = worst.max(1);
            }
            Err(message) => {
                eprintln!("{name}: could not run: {message}");
                worst = worst.max(2);
            }
        }
    }
    ExitCode::from(worst)
}
