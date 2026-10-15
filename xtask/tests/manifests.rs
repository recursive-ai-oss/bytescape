//! The winget and Chocolatey files written from a release.

use std::collections::BTreeMap;
use xtask::manifests;

const X64: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ARM: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn sums() -> BTreeMap<String, String> {
    let text = format!(
        "{X64}  bytescape-1.2.3-windows-x64-setup.exe\n{ARM} *bytescape-1.2.3-windows-arm64-setup.exe\n{X64}  bytescape-1.2.3-linux-x64.deb\n"
    );
    manifests::parse_sums(&text).expect("sums")
}

fn find<'a>(files: &'a [manifests::File], suffix: &str) -> &'a str {
    &files
        .iter()
        .find(|f| f.path.ends_with(suffix))
        .unwrap_or_else(|| panic!("no file ending {suffix}"))
        .text
}

#[test]
fn sums_are_read_and_bad_lines_are_refused() {
    let parsed = sums();
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed["bytescape-1.2.3-windows-arm64-setup.exe"], ARM);
    assert!(manifests::parse_sums("nothex  file\n").is_err());
    assert!(manifests::parse_sums("abc\n").is_err());
    assert!(manifests::parse_sums(&format!("{}  f\n", "a".repeat(63))).is_err());
    assert!(manifests::parse_sums("").expect("empty").is_empty());
}

#[test]
fn versions_that_would_break_a_file_name_or_address_are_refused() {
    for good in ["0.1.0", "1.22.333", "1.0.0-rc.1"] {
        assert!(manifests::check_version(good).is_ok(), "{good}");
    }
    for bad in [
        "1.0",
        "1.0.0.0",
        "v1.0.0",
        "1.0.0/../x",
        "1.0.0 ",
        "1.0.x",
        "1.0.0-",
        "",
    ] {
        assert!(manifests::check_version(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn winget_has_one_installer_per_architecture_with_upper_case_hashes() {
    let files = manifests::winget("1.2.3", Some("2026-10-15"), &sums()).expect("winget");
    assert_eq!(files.len(), 3);
    assert!(files.iter().all(|f| {
        f.path
            .starts_with("winget/manifests/r/RecursiveAI/Bytescape/1.2.3/RecursiveAI.Bytescape")
    }));
    let installer = find(&files, ".installer.yaml");
    assert!(installer.contains("InstallerType: inno"));
    assert!(installer.contains("Scope: machine"));
    assert!(installer.contains("Silent: /VERYSILENT /SUPPRESSMSGBOXES /NORESTART"));
    assert!(installer.contains("ReleaseDate: 2026-10-15"));
    assert_eq!(installer.matches("- Architecture:").count(), 2);
    assert!(installer.contains(&format!("InstallerSha256: {}", X64.to_uppercase())));
    assert!(installer.contains(&format!("InstallerSha256: {}", ARM.to_uppercase())));
    assert!(installer.contains(
        "https://github.com/recursive-ai-oss/bytescape/releases/download/v1.2.3/bytescape-1.2.3-windows-arm64-setup.exe"
    ));
    assert!(installer.contains("ManifestVersion: 1.12.0"));
    assert!(find(&files, "Bytescape.yaml").contains("ManifestType: version"));
    assert!(find(&files, ".locale.en-US.yaml").contains("ManifestType: defaultLocale"));
}

#[test]
fn a_missing_installer_hash_stops_the_command() {
    let mut partial = sums();
    partial.remove("bytescape-1.2.3-windows-arm64-setup.exe");
    let error = manifests::winget("1.2.3", None, &partial).expect_err("must fail");
    assert!(error.contains("windows-arm64-setup.exe"), "{error}");
    partial.remove("bytescape-1.2.3-windows-x64-setup.exe");
    assert!(manifests::chocolatey("1.2.3", &partial).is_err());
    // A hash for another version does not stand in for this one.
    assert!(manifests::winget("1.2.4", None, &sums()).is_err());
}

#[test]
fn chocolatey_downloads_the_versioned_release_and_checks_the_hash() {
    let files = manifests::chocolatey("1.2.3", &sums()).expect("chocolatey");
    let install = find(&files, "chocolateyinstall.ps1");
    assert!(install.contains(
        "url64bit       = 'https://github.com/recursive-ai-oss/bytescape/releases/download/v1.2.3/bytescape-1.2.3-windows-x64-setup.exe'"
    ));
    assert!(install.contains(&format!("checksum64     = '{X64}'")));
    assert!(install.contains("checksumType64 = 'sha256'"));
    assert!(install.contains("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART"));
    let nuspec = find(&files, "bytescape.nuspec");
    assert!(nuspec.contains("<id>bytescape</id>"));
    assert!(nuspec.contains("<version>1.2.3</version>"));
    assert!(find(&files, "chocolateyuninstall.ps1").contains("Get-UninstallRegistryKey"));
}

#[test]
fn generated_text_obeys_the_wording_rule_and_names_no_private_files() {
    let mut files = manifests::winget("1.2.3", None, &sums()).expect("winget");
    files.extend(manifests::chocolatey("1.2.3", &sums()).expect("chocolatey"));
    for file in &files {
        let hits = xtask::wording::scan_text(&file.text);
        assert!(hits.is_empty(), "{}: {hits:?}", file.path);
        let found =
            xtask::public_text::scan_text(&file.text, &xtask::public_text::Extra::default());
        assert!(found.is_empty(), "{}: {found:?}", file.path);
    }
}

#[test]
fn the_files_are_written_below_the_output_folder() {
    let dir = std::env::temp_dir().join(format!("xtask-manifests-{}", std::process::id()));
    let sums_path = dir.join("SHA256SUMS");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        &sums_path,
        format!("{X64}  bytescape-1.2.3-windows-x64-setup.exe\n{ARM}  bytescape-1.2.3-windows-arm64-setup.exe\n"),
    )
    .expect("sums file");
    let written = manifests::run("1.2.3", &sums_path, None, &dir.join("out")).expect("run");
    assert_eq!(written.len(), 6);
    for path in written {
        assert!(dir.join("out").join(path).is_file());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
