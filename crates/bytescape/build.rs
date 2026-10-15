// Exports the build version, date and target as compile-time variables, and
// compiles the Windows icon, version information and manifest into the program.
include!("../../packaging/build-metadata.rs");
include!("../../packaging/windows-resources.rs");

fn main() {
    emit_build_metadata();
    embed_windows_resources("bytescape", "Bytescape disk space visualizer", true);
}
