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

Building for Windows also compiles the icon, the version information and the application manifest into each program, and that needs a resource compiler for the target (`windres` from llvm-mingw or MinGW-w64 for the `gnullvm` and `gnu` targets, or the Windows SDK on Windows). Without one the build stops, which is the right default: a Windows program without its manifest must not be built by accident.

If you only want to lint or type-check Windows code on a machine that has no resource compiler, you can say so explicitly:

```sh
BYTESCAPE_SKIP_WINRES=1 cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnullvm -- -D warnings
```

The two build scripts then skip the resources and print a warning saying so. Use it only for `clippy` and `check`. Programs built this way have no icon, no version information and no administrator manifest, so they must never be run for real or released. The variable only accepts `1`. CI and the release builds fail at the first step if it is set, `packaging/windows/build-windows.sh` refuses it, and the build scripts refuse it when they see they run in CI or build with the release profile.
