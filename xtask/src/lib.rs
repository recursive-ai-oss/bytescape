//! Project automation for Bytescape, run as `cargo xtask <command>`.
//!
//! Each command is a check that CI and contributors run the same way:
//! `net-ban`, `helper-deps`, `wording`, `public-text` and `unsafe-allow`. The logic lives
//! here, not in the CI configuration.

pub mod helper_deps;
pub mod net_ban;
pub mod public_text;
pub mod unsafe_allow;
pub mod util;
pub mod wording;
