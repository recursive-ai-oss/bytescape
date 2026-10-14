# Building Bytescape

Bytescape has no release yet, so building from source is the only way to run it, and today the programs are empty skeletons. This page covers what you need to build and test the workspace.

## Requirements

- [rustup](https://rustup.rs). The repository pins an exact Rust release in `rust-toolchain.toml`; rustup installs it on first use.
- `git`.
- Windows: the Visual Studio Build Tools with the C++ workload (the MSVC target). Bytescape links the C runtime statically, so the programs you build need no redistributable package.
- Linux: a C linker such as `gcc` or `clang`.

Supported targets: `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. There is no macOS target.

## Build and test

```sh
git clone https://github.com/recursive-ai-oss/bytescape
cd bytescape
cargo build --workspace
cargo test --workspace
```

Release builds use fat link-time optimization in a single code generation unit, so they take longer than a normal build:

```sh
cargo build --release --workspace
```

The two programs are `bytescape` (the interface) and `bytescape-helper` (the elevated helper). They are separate on purpose; see [SECURITY.md](../SECURITY.md).

## Build metadata

The build records the version, the target and a build date in both programs. The date comes from the `SOURCE_DATE_EPOCH` environment variable (seconds since 1970, UTC), so rebuilding one commit with the same value gives the same date:

```sh
SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" cargo build --release
```

Without the variable the date is recorded as "unknown", and the About box shows no build-age note for that build.

## The checks

The commands that CI runs are listed in [CONTRIBUTING.md](../CONTRIBUTING.md). `cargo xtask guards` runs the four project-specific checks (`net-ban`, `helper-deps`, `wording` and `public-text`) in one go.

## Checking other targets

You can lint code for another operating system without owning it. For example, on Linux:

```sh
rustup target add x86_64-pc-windows-gnullvm
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnullvm -- -D warnings
```

That checks the Windows-only code paths for errors. It does not run them; Windows tests run on Windows in CI.
