// Exports the build version, date and target as compile-time variables, and
// compiles the Windows version information and manifest (which asks for
// administrator rights) into the helper, and restricts where it looks for DLLs.
include!("../../packaging/build-metadata.rs");
include!("../../packaging/windows-resources.rs");

fn main() {
    emit_build_metadata();
    embed_windows_resources(
        "bytescape-helper",
        "Bytescape helper (administrator)",
        false,
    );
    // The helper runs with administrator rights: it loads DLLs from the system
    // folder only, never from the folder it sits in.
    restrict_dependent_dll_search();
}
