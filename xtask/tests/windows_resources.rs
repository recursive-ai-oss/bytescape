//! The Windows resource script written by the two build scripts: version
//! numbers, paths and the manifest reference.

#[allow(dead_code)]
mod script {
    include!("../../packaging/windows-resource-script.rs");
}

#[test]
fn the_resources_are_skipped_only_on_an_explicit_request_outside_ci() {
    assert_eq!(script::winres_skip(None, false, false), Ok(false));
    assert_eq!(script::winres_skip(Some(""), false, false), Ok(false));
    assert_eq!(script::winres_skip(Some("1"), false, false), Ok(true));
    // CI and release builds never skip, and a loose value is not taken as a
    // request.
    assert_eq!(script::winres_skip(None, true, true), Ok(false));
    assert!(script::winres_skip(Some("1"), true, false).is_err());
    assert!(script::winres_skip(Some("1"), false, true).is_err());
    assert!(script::winres_skip(Some("0"), false, false).is_err());
    assert!(script::winres_skip(Some("yes"), false, false).is_err());
}

fn make(version: &str) -> String {
    script::resource_script(
        version,
        "bytescape",
        "A description",
        Some("C:\\work\\icons\\bytescape.ico"),
        "C:\\work\\windows\\bytescape.manifest",
    )
}

#[test]
fn the_version_fills_both_the_numbers_and_the_text() {
    let text = make("1.22.3");
    assert!(text.contains("FILEVERSION 1,22,3,0"), "{text}");
    assert!(text.contains("PRODUCTVERSION 1,22,3,0"));
    assert!(text.contains("VALUE \"FileVersion\", \"1.22.3\\0\""));
    assert!(text.contains("VALUE \"ProductVersion\", \"1.22.3\\0\""));
    assert!(text.contains("VALUE \"OriginalFilename\", \"bytescape.exe\\0\""));
}

#[test]
fn a_pre_release_or_short_version_still_gives_four_numbers() {
    assert!(make("0.1.0-rc.1").contains("FILEVERSION 0,1,0,0"));
    assert!(make("2").contains("FILEVERSION 2,0,0,0"));
    assert!(make("").contains("FILEVERSION 0,0,0,0"));
}

#[test]
fn paths_use_forward_slashes_and_the_manifest_is_resource_one() {
    let text = make("0.1.0");
    assert!(
        text.contains("1 ICON \"C:/work/icons/bytescape.ico\""),
        "{text}"
    );
    assert!(text.contains("1 24 \"C:/work/windows/bytescape.manifest\""));
    assert!(
        text.lines()
            .filter(|line| line.starts_with("1 "))
            .all(|line| !line.contains('\\')),
        "{text}"
    );
}

#[test]
fn the_helper_has_no_icon_line() {
    let text = script::resource_script("0.1.0", "bytescape-helper", "d", None, "m.manifest");
    assert!(!text.contains("ICON"), "{text}");
    assert!(text.contains("1 24 \"m.manifest\""));
}

#[test]
fn the_shipped_manifests_ask_for_the_right_rights() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../packaging/windows/");
    let ui = std::fs::read_to_string(format!("{root}bytescape.manifest")).expect("ui manifest");
    let helper = std::fs::read_to_string(format!("{root}bytescape-helper.manifest"))
        .expect("helper manifest");
    assert!(ui.contains("level=\"asInvoker\""));
    assert!(!ui.contains("requireAdministrator"));
    assert!(helper.contains("level=\"requireAdministrator\""));
    assert!(!helper.contains("asInvoker"));
}

#[test]
fn the_installer_script_keeps_the_helper_in_a_folder_only_administrators_write() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../packaging/windows/bytescape.iss"
    ))
    .expect("installer script");
    assert!(text.contains("PrivilegesRequired=admin"));
    assert!(text.contains("DefaultDirName={autopf}"));
    assert!(!text.contains("PrivilegesRequiredOverridesAllowed"));
    assert!(!text.contains("{localappdata}") && !text.contains("{userpf}"));
    for verb in [
        "Directory\\shell",
        "Directory\\Background\\shell",
        "Drive\\shell",
    ] {
        assert!(text.contains(verb), "{verb}");
    }
}
