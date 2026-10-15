// Windows resources (icon, version information, application manifest) shared
// by the build scripts of both programs. Each build script includes this file,
// and the xtask tests include it too, so the script text has one home and is
// tested.
//
// The resource script is written into the build folder with the version and
// the file paths filled in, then compiled by the `embed-resource` crate for
// the named program only (not for its test programs). On other systems
// nothing happens.

include!("windows-resource-script.rs");

/// Compiles the resources into the program `bin` when building for Windows.
pub fn embed_windows_resources(bin: &str, description: &str, with_icon: bool) {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let packaging = std::path::Path::new(&manifest_dir).join("../../packaging");
    let manifest = packaging.join("windows").join(format!("{bin}.manifest"));
    let icon = packaging.join("icons").join("bytescape.ico");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let script = resource_script(
        &version,
        bin,
        description,
        with_icon.then(|| icon.to_string_lossy().into_owned()).as_deref(),
        &manifest.to_string_lossy(),
    );
    let rc = std::path::Path::new(&out_dir).join(format!("{bin}.rc"));
    if let Err(error) = std::fs::write(&rc, script) {
        eprintln!("could not write {}: {error}", rc.display());
        std::process::exit(1);
    }
    if embed_resource::compile_for(&rc, [bin], embed_resource::NONE)
        .manifest_required()
        .is_err()
    {
        eprintln!("could not compile the Windows resources for {bin}");
        std::process::exit(1);
    }
}
