// Exports the build version, date and target as compile-time variables.
include!("../../packaging/build-metadata.rs");

fn main() {
    emit_build_metadata();
}
