//! The system's authorization policy for the helper says what the code
//! relies on: one action per kind of request, both naming the installed helper
//! and its first argument, a scan that may keep its authorization and a delete
//! that never does.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(target_os = "linux")]

use std::path::Path;

fn policy() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("packaging/linux/org.recursiveai.bytescape.helper.policy");
    std::fs::read_to_string(path).unwrap()
}

/// The text of the first `<tag>...</tag>` in `xml`.
fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

/// The value of the annotation `key` in an action.
fn annotation<'a>(action: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("<annotate key=\"{key}\">");
    let start = action.find(&marker)? + marker.len();
    let end = action[start..].find("</annotate>")? + start;
    Some(action[start..end].trim())
}

/// Each `<action ...>...</action>` as (id, body). Comments are left out first.
fn actions(xml: &str) -> Vec<(String, String)> {
    let mut text = xml.to_owned();
    while let Some(start) = text.find("<!--") {
        let end = text[start..]
            .find("-->")
            .map_or(text.len(), |e| start + e + 3);
        text.replace_range(start..end, "");
    }
    let mut out = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<action id=\"") {
        let after = &rest[start + "<action id=\"".len()..];
        let id_end = after.find('"').unwrap();
        let body_end = after.find("</action>").unwrap();
        out.push((
            after[..id_end].to_owned(),
            after[id_end..body_end].to_owned(),
        ));
        rest = &after[body_end..];
    }
    out
}

#[test]
fn there_is_one_action_per_kind_of_request() {
    let xml = policy();
    let found = actions(&xml);
    let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "org.recursiveai.bytescape.helper.scan",
            "org.recursiveai.bytescape.helper.delete"
        ]
    );
    for (id, _) in &found {
        assert!(id.starts_with("org.recursiveai.bytescape."), "{id}");
    }
}

#[test]
fn both_actions_name_the_installed_helper_and_their_first_argument() {
    let installed = bs_platform::elevate::Elevation::system()
        .unwrap()
        .helper_path()
        .to_path_buf();
    for (id, body) in actions(&policy()) {
        assert_eq!(
            annotation(&body, "org.freedesktop.policykit.exec.path"),
            installed.to_str(),
            "{id}"
        );
        let mode = id.rsplit('.').next().unwrap();
        assert_eq!(
            annotation(&body, "org.freedesktop.policykit.exec.argv1"),
            Some(mode),
            "{id}"
        );
        assert_eq!(
            annotation(&body, "org.freedesktop.policykit.exec.allow_gui"),
            Some("true"),
            "{id}"
        );
    }
}

#[test]
fn a_delete_always_asks_and_nothing_is_allowed_without_asking() {
    let xml = policy();
    for (id, body) in actions(&xml) {
        for default in ["allow_any", "allow_inactive", "allow_active"] {
            let value = element(&body, default).unwrap_or_else(|| panic!("{id} lacks {default}"));
            assert!(
                matches!(value, "auth_admin" | "auth_admin_keep"),
                "{id} {default} = {value}"
            );
            if id.ends_with(".delete") {
                assert_eq!(
                    value, "auth_admin",
                    "{id} {default}: a delete asks every time"
                );
            }
            if default != "allow_active" {
                // Only the person at the console may keep an authorization.
                assert_eq!(value, "auth_admin", "{id} {default}");
            }
        }
    }
    assert!(!xml.contains(">yes<"));
}

#[test]
fn the_first_arguments_in_the_policy_are_the_ones_the_helper_accepts() {
    use bytescape_helper::args::{Mode, parse};
    for (id, mode) in [("scan", Mode::Scan), ("delete", Mode::Delete)] {
        assert!(actions(&policy()).iter().any(|(a, _)| a.ends_with(id)));
        let args: Vec<std::ffi::OsString> = [
            id,
            "--protocol",
            "1",
            "--nonce",
            "01234567890123450123456789012345",
        ]
        .iter()
        .map(Into::into)
        .collect();
        assert_eq!(parse(&args, false).unwrap().mode, mode);
    }
}
