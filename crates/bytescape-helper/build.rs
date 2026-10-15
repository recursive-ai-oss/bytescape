// Exports the build version, date and target as compile-time variables, and
// compiles the Windows version information and manifest (which asks for
// administrator rights) into the helper.
include!("../../packaging/build-metadata.rs");
include!("../../packaging/windows-resources.rs");

fn main() {
    emit_build_metadata();
    embed_windows_resources(
        "bytescape-helper",
        "Bytescape helper (administrator)",
        false,
    );
}
