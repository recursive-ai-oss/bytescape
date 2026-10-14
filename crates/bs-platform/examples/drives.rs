//! Prints this machine's drives and appearance setting.
//!
//! ```text
//! cargo run -p bs-platform --example drives            # all listed drives
//! cargo run -p bs-platform --example drives -- <path>  # the drive holding <path>
//! ```

use bs_platform::appearance;
use bs_platform::drives::{self, Drive};
use std::path::Path;

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

fn print_drive(drive: &Drive) {
    let (total, used, free, reserved) = match drive.capacity {
        Some(c) => (gib(c.total), gib(c.used), gib(c.free), gib(c.reserved)),
        None => {
            let note = format!("{:?}", drive.capacity_problem);
            (note, "-".into(), "-".into(), "-".into())
        }
    };
    println!(
        "{:<9?} {:<5} {:<6} {:<8} {:>11} {:>11} {:>11} {:>10}  {}  [{}]{}",
        drive.kind,
        if drive.eligible_for_background_scan {
            "scan"
        } else {
            "-"
        },
        if drive.read_only { "ro" } else { "rw" },
        drive.fs_type,
        total,
        used,
        free,
        reserved,
        drive.mount_point.display(),
        drive.id,
        drive
            .label
            .as_deref()
            .map(|l| format!("  label \"{l}\""))
            .unwrap_or_default(),
    );
    if let Some(c) = drive.capacity {
        println!("            reserved is a {:?}", c.reserved_kind);
    }
    if drive.mounts.len() > 1 {
        for mount in &drive.mounts {
            println!(
                "            mount {} (id {:?}, root {}){}",
                mount.mount_point.display(),
                mount.mount_id,
                mount.root.display(),
                if mount.covered_by_other_mount {
                    ", already shown by another mount"
                } else {
                    ""
                },
            );
        }
    }
}

fn main() {
    let header = format!(
        "{:<9} {:<5} {:<6} {:<8} {:>11} {:>11} {:>11} {:>10}  mount point  [id]",
        "kind", "bg", "mode", "fs", "total", "used", "free", "reserved"
    );
    match std::env::args_os().nth(1) {
        Some(path) => match drives::volume_for_path(Path::new(&path)) {
            Ok(drive) => {
                println!("{header}");
                print_drive(&drive);
            }
            Err(error) => {
                eprintln!("{}: {error}", Path::new(&path).display());
                std::process::exit(1);
            }
        },
        None => match drives::list_drives() {
            Ok(list) => {
                println!("{header}");
                list.iter().for_each(print_drive);
            }
            Err(error) => {
                eprintln!("cannot list drives: {error}");
                std::process::exit(1);
            }
        },
    }
    let look = appearance::current();
    let accent = look.accent_or_default();
    println!(
        "\nappearance: {:?}, accent #{:02X}{:02X}{:02X} ({})",
        look.scheme,
        accent.r,
        accent.g,
        accent.b,
        if look.accent.is_some() {
            "from the system"
        } else {
            "default"
        }
    );
}
