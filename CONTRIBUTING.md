# Contributing to Bytescape

Thank you for helping. Bug reports, documentation fixes, test cases and code are all welcome.

## Before you start

- **Found a bug?** Open an issue with the [bug report form](https://github.com/recursive-ai-oss/bytescape/issues/new/choose).
- **Found a security problem?** Don't open an issue. Follow [SECURITY.md](SECURITY.md).
- **Want to add or change behavior?** Open an issue first, so the idea can be agreed before you write code. Some things are out of scope by design; the README's "Why use it, and why not" section lists them.

## Set up

You need [rustup](https://rustup.rs) and `git`. The repository pins an exact Rust release in `rust-toolchain.toml`, and rustup installs it the first time you run `cargo` here.

```sh
git clone https://github.com/recursive-ai-oss/bytescape
cd bytescape
cargo test --workspace
```

The workspace has the two programs (`bytescape`, the interface, and `bytescape-helper`, the elevated helper), the `bs-*` library crates, and `xtask`, the automation crate. [docs/building.md](docs/building.md) has the details.

## The checks every change must pass

Continuous integration runs exactly these commands, so you can run them yourself before you push. Nothing happens in CI that you cannot reproduce locally.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny check                 # install once: cargo install --locked cargo-deny
cargo xtask net-ban
cargo xtask helper-deps
cargo xtask wording
cargo xtask public-text
cargo xtask unsafe-allow
```

`cargo xtask guards` runs the five `xtask` checks together.

## Product principles

These are rules of the product, and the checks above enforce them.

### No network, ever

Bytescape never connects to the internet. Do not add a dependency that can open a network connection (an HTTP client or server, a TLS library, a DNS library, a websocket library, an async runtime with networking, a D-Bus crate, or the Windows networking APIs behind the `Win32_Networking` features), and do not use the standard library's network module or the system's socket, name-lookup and download calls. Links open the user's browser. `cargo xtask net-ban` (the dependency graph of every target, its features, and the Rust sources) and the `[bans]` section of `deny.toml` (crate names) reject the ones we know of. Feature bans live in `net-ban` only, because it resolves features exactly for each target. These checks are a backstop for review, not a proof.

The one kind of exception is the local connection to the display server that the windowing libraries make. It is allowed in the interface only, through an entry in `xtask/net-ban.allow` that names the windowing crate that pulls it in; the file explains the format. The entry holds only while every path to the item goes through that crate, and never inside the helper's dependencies. Every entry needs a written reason and a maintainer's explicit approval.

### The elevated helper stays small

`bytescape-helper` runs with administrator rights and reads untrusted disk data. Its dependencies are limited to `xtask/helper-deps.allow`. Adding a line there needs explicit review, and a change that adds one should say why in the pull request.

### Hostile data never panics

The crates that read raw disk structures or sit on the privilege boundary (`bs-scan-ntfs`, `bs-scan-ext4`, `bs-scan-btrfs`, `bs-scan-xfs`, `bs-ipc`, `bytescape-helper`) deny `unwrap`, `expect`, `panic!`, `unreachable!`, unchecked indexing, unchecked arithmetic and truncating casts at the crate root. Do not loosen a lint to make code pass. If a lint is in the way, ask in the pull request.

### Unsafe code is local and explained

`unsafe` is denied across the workspace. Only the crates that call the operating system or read raw disk structures may allow it (`bytescape-helper`, `bs-platform`, `bs-scan`, `bs-scan-ntfs`, `bs-scan-ext4`, `bs-scan-btrfs`, `bs-scan-xfs` and, if its drawing surface needs it, `bs-render`), in a small module, and every `unsafe` block needs a `// SAFETY:` comment saying why it is sound. `cargo xtask unsafe-allow` fails when any other crate allows it, or when anyone switches off the safety-comment lint.

### Wording: Bytescape is a visualizer, not a cleanup advisor

Notes and text say what something is and which program manages it. They never tell the user that something is safe to remove, never call data junk, never offer a reclaimable total or a cleanup list, and never say that space was "freed" when an item was only moved to the Recycle Bin or Trash. `cargo xtask wording` rejects the banned words ("safe", "junk", "reclaim*", "clean up" and "freed", whole words, any case) in the README, the changelog (it becomes the release notes), `docs/`, the issue forms and the package listings, and it fails on any such file that is not UTF-8 text. It also covers the words inside the program: every string literal in `src/` of every crate under `crates/` (outside test code, so a new crate is covered from its first commit), and the notes in the knowledge tables. No crate is left out. Comments and identifiers are not text anyone reads, so they are not checked. The product name "Disk Cleanup" passes, because it is one word; "clean up" as two words does not. A real exception goes in `xtask/wording.allow` (`path | word | text`, one per line), is reviewed like any other change, and fails the check once it matches nothing.

### Numbers are measured

A performance figure in the documentation needs a recorded measurement next to it: the hardware, whether the cache was cold or warm, and the dataset. Do not write a number you did not measure.

### Public text stays about the product

Everything in this repository is public, including comments, workflow files and generated output. Explain a rule in plain words where it shapes a file. `cargo xtask public-text` rejects references to private planning material, internal identifiers, process-role vocabulary and commit hashes. Maintainers also run it with an extra pattern file (`--extra-patterns FILE`) that lists names which must not be written in this repository at all.

## Tests that touch the machine

Some tests move files to the Recycle Bin or Trash, or call the operating system in other ways that change state. They run only when `BYTESCAPE_DISPOSABLE_HOST=1` is set, which CI sets on its throwaway runners. Set it on your own machine only if it is a virtual machine or another disposable system. Three Linux tests need more than that: two run inside a private user namespace and one mounts file systems as root. `BYTESCAPE_DISPOSABLE_HOST=1 packaging/ci/linux-privileged-tests.sh` runs them (it needs passwordless `sudo`, `btrfs-progs`, and on Ubuntu 24.04 `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`) and fails if any of them reports `NOT RUN`. CI runs it on the Linux legs. The btrfs reader's image tests need a `mkfs.btrfs` that knows `--subvol`, which Ubuntu 24.04's `btrfs-progs` (6.6) does not; `packaging/ci/build-btrfs-progs.sh [PREFIX]` builds a pinned, checksummed upstream release, and CI uses it.

## Pull requests

The pull request template lists what the maintainers look for. In short: the checks above pass, behavior changes come with a test, `CHANGELOG.md` has an entry under "Unreleased", and documentation is updated in the same change.

By contributing you agree that your work is licensed as described in the README: `MIT OR Apache-2.0`.
