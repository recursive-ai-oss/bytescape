//! `cargo xtask manifests`: the winget and Chocolatey package files, written
//! from a finished release.
//!
//! It reads the release's `SHA256SUMS` (the lines `sha256sum` writes) and the
//! version, and fills in the download addresses and hashes. Nothing here
//! reaches the network: the addresses are built from the release's tag, and
//! the hashes come from the checksum file the release publishes.

use std::collections::BTreeMap;
use std::path::Path;

/// Where the release files are published.
pub const REPOSITORY: &str = "https://github.com/recursive-ai-oss/bytescape";

/// The winget manifest schema the files are written for.
pub const WINGET_SCHEMA: &str = "1.12.0";

/// The silent switches of the installer. Winget, Chocolatey and the Microsoft
/// Store all use them.
pub const SILENT: &str = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART";

/// The text shared by the listings. It is prose, so the wording check covers
/// the generated files too.
pub const SHORT_DESCRIPTION: &str = "See where the space on a drive is used";
pub const DESCRIPTION: &str = "Bytescape scans a drive or folder and draws every file and folder as a box whose area is its size, so the biggest things stand out at once. It shows what is there and leaves the decisions to you. It never connects to the internet. Inspired by SpaceMonger 1.4.";

/// File name to hash, from a `SHA256SUMS` file.
pub fn parse_sums(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut sums = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            return Err(format!("line {}: expected `HASH  NAME`", index + 1));
        };
        let name = name.trim().trim_start_matches('*');
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "line {}: `{hash}` is not a SHA-256 hash",
                index + 1
            ));
        }
        sums.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    Ok(sums)
}

/// The installer's file name for an architecture (`x64` or `arm64`).
pub fn installer_name(version: &str, arch: &str) -> String {
    format!("bytescape-{version}-windows-{arch}-setup.exe")
}

/// The release's download address for a file.
pub fn download_url(version: &str, file: &str) -> String {
    format!("{REPOSITORY}/releases/download/v{version}/{file}")
}

/// A version must be plain `MAJOR.MINOR.PATCH` with an optional pre-release
/// part, so it is safe to put in a file name, an address and a YAML file.
pub fn check_version(version: &str) -> Result<(), String> {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if numbers.len() != 3 || !numbers.iter().all(|part| numeric(part)) {
        return Err(format!("`{version}` is not MAJOR.MINOR.PATCH"));
    }
    if let Some(pre) = pre
        && (pre.is_empty() || !pre.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
    {
        return Err(format!("`{version}` has an odd pre-release part"));
    }
    Ok(())
}

/// One generated file: its path below the output folder and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub text: String,
}

/// The three winget files (version, installer, default locale).
pub fn winget(
    version: &str,
    release_date: Option<&str>,
    sums: &BTreeMap<String, String>,
) -> Result<Vec<File>, String> {
    check_version(version)?;
    let id = "RecursiveAI.Bytescape";
    let dir = format!("winget/manifests/r/RecursiveAI/Bytescape/{version}");
    let schema = |kind: &str| {
        format!(
            "# yaml-language-server: $schema=https://aka.ms/winget-manifest.{kind}.{WINGET_SCHEMA}.schema.json\n\n"
        )
    };
    let mut installers = String::new();
    for arch in ["x64", "arm64"] {
        let name = installer_name(version, arch);
        let hash = sums
            .get(&name)
            .ok_or_else(|| format!("SHA256SUMS has no entry for {name}"))?;
        installers.push_str(&format!(
            "- Architecture: {arch}\n  InstallerUrl: {}\n  InstallerSha256: {}\n",
            download_url(version, &name),
            hash.to_ascii_uppercase()
        ));
    }
    let date = release_date
        .map(|date| format!("ReleaseDate: {date}\n"))
        .unwrap_or_default();
    let installer = format!(
        "{}PackageIdentifier: {id}\nPackageVersion: {version}\nInstallerType: inno\nScope: machine\nInstallerSwitches:\n  Silent: {SILENT}\n  SilentWithProgress: /SILENT /SUPPRESSMSGBOXES /NORESTART\nUpgradeBehavior: install\nElevationRequirement: elevationRequired\nMinimumOSVersion: 10.0.0.0\n{date}Installers:\n{installers}ManifestType: installer\nManifestVersion: {WINGET_SCHEMA}\n",
        schema("installer")
    );
    let locale = format!(
        "{}PackageIdentifier: {id}\nPackageVersion: {version}\nPackageLocale: en-US\nPublisher: Recursive AI, LLC\nPublisherUrl: https://recursiveai.org\nPublisherSupportUrl: {REPOSITORY}/issues\nPackageName: Bytescape\nPackageUrl: {REPOSITORY}\nLicense: MIT OR Apache-2.0\nLicenseUrl: {REPOSITORY}/blob/main/LICENSE-MIT\nCopyright: Copyright (c) Recursive AI, LLC\nShortDescription: {SHORT_DESCRIPTION}\nDescription: {DESCRIPTION}\nTags:\n- disk\n- disk-space\n- storage\n- treemap\n- visualization\nReleaseNotesUrl: {REPOSITORY}/releases/tag/v{version}\nManifestType: defaultLocale\nManifestVersion: {WINGET_SCHEMA}\n",
        schema("defaultLocale")
    );
    let version_file = format!(
        "{}PackageIdentifier: {id}\nPackageVersion: {version}\nDefaultLocale: en-US\nManifestType: version\nManifestVersion: {WINGET_SCHEMA}\n",
        schema("version")
    );
    Ok(vec![
        File {
            path: format!("{dir}/{id}.yaml"),
            text: version_file,
        },
        File {
            path: format!("{dir}/{id}.installer.yaml"),
            text: installer,
        },
        File {
            path: format!("{dir}/{id}.locale.en-US.yaml"),
            text: locale,
        },
    ])
}

/// The Chocolatey package files: the `.nuspec` and the two scripts.
///
/// The script downloads the installer from the versioned release address and
/// checks it against `checksum64`. Chocolatey installs the 64-bit installer on
/// ARM64 Windows too, where Windows runs it through its x64 emulation.
pub fn chocolatey(version: &str, sums: &BTreeMap<String, String>) -> Result<Vec<File>, String> {
    check_version(version)?;
    let name = installer_name(version, "x64");
    let hash = sums
        .get(&name)
        .ok_or_else(|| format!("SHA256SUMS has no entry for {name}"))?;
    let url = download_url(version, &name);
    let nuspec = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<package xmlns=\"http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd\">\n  <metadata>\n    <id>bytescape</id>\n    <version>{version}</version>\n    <title>Bytescape</title>\n    <authors>Recursive AI, LLC</authors>\n    <owners>Recursive AI, LLC</owners>\n    <projectUrl>{REPOSITORY}</projectUrl>\n    <packageSourceUrl>{REPOSITORY}</packageSourceUrl>\n    <licenseUrl>{REPOSITORY}/blob/main/LICENSE-MIT</licenseUrl>\n    <requireLicenseAcceptance>false</requireLicenseAcceptance>\n    <projectSourceUrl>{REPOSITORY}</projectSourceUrl>\n    <bugTrackerUrl>{REPOSITORY}/issues</bugTrackerUrl>\n    <releaseNotes>{REPOSITORY}/releases/tag/v{version}</releaseNotes>\n    <tags>disk space storage treemap visualization</tags>\n    <summary>{SHORT_DESCRIPTION}</summary>\n    <description>{DESCRIPTION}</description>\n  </metadata>\n  <files>\n    <file src=\"tools\\**\" target=\"tools\" />\n  </files>\n</package>\n"
    );
    let install = format!(
        "$ErrorActionPreference = 'Stop'\n\n$packageArgs = @{{\n  packageName    = $env:ChocolateyPackageName\n  fileType       = 'exe'\n  url64bit       = '{url}'\n  checksum64     = '{hash}'\n  checksumType64 = 'sha256'\n  silentArgs     = '{SILENT} /SP-'\n  validExitCodes = @(0)\n  softwareName   = 'Bytescape*'\n}}\n\nInstall-ChocolateyPackage @packageArgs\n"
    );
    let uninstall = format!(
        "$ErrorActionPreference = 'Stop'\n\n$keys = Get-UninstallRegistryKey -SoftwareName 'Bytescape*'\nif ($keys.Count -eq 1) {{\n  $key = $keys[0]\n  $packageArgs = @{{\n    packageName    = $env:ChocolateyPackageName\n    fileType       = 'exe'\n    silentArgs     = '{SILENT}'\n    validExitCodes = @(0)\n    file           = $key.UninstallString.Trim('\"')\n  }}\n  Uninstall-ChocolateyPackage @packageArgs\n}} elseif ($keys.Count -eq 0) {{\n  Write-Warning 'Bytescape is not listed as installed; nothing to remove.'\n}} else {{\n  Write-Warning \"$($keys.Count) matching entries were found; remove Bytescape from Settings, Apps.\"\n}}\n"
    );
    Ok(vec![
        File {
            path: String::from("chocolatey/bytescape.nuspec"),
            text: nuspec,
        },
        File {
            path: String::from("chocolatey/tools/chocolateyinstall.ps1"),
            text: install,
        },
        File {
            path: String::from("chocolatey/tools/chocolateyuninstall.ps1"),
            text: uninstall,
        },
    ])
}

/// Writes the generated files below `out`.
pub fn write_files(out: &Path, files: &[File]) -> Result<(), String> {
    for file in files {
        let path = out.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::write(&path, &file.text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

/// The whole command: reads the checksum file and writes both package sets.
pub fn run(
    version: &str,
    sums_file: &Path,
    release_date: Option<&str>,
    out: &Path,
) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(sums_file)
        .map_err(|error| format!("{}: {error}", sums_file.display()))?;
    let sums = parse_sums(&text)?;
    let mut files = winget(version, release_date, &sums)?;
    files.extend(chocolatey(version, &sums)?);
    write_files(out, &files)?;
    Ok(files.into_iter().map(|file| file.path).collect())
}
