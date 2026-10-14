## What this changes

<!-- One or two sentences. Link the issue (`Fixes #...`). -->

## How it is tested

<!-- The test that fails without this change. -->

## Checklist

- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --locked --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test --locked --workspace` passes.
- [ ] `cargo deny check` and `cargo xtask guards` pass.
- [ ] No networking dependency or code, and no new line in `xtask/helper-deps.allow` without a reason given here.
- [ ] No lint was loosened to make code pass.
- [ ] Text shown to users says what something is, never that it is safe to remove, and never that space was freed by a Recycle Bin or Trash move.
- [ ] `CHANGELOG.md` has an entry under "Unreleased".
- [ ] Documentation is updated, and every number in it comes from a recorded measurement.
