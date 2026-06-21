# Rduel phase-one integration notes

## Clipboard isolation patch point

Zed editor clipboard behavior is concentrated in `crates/editor/src/clipboard.rs`, with several direct calls in `crates/editor/src/input.rs` for line-cut style actions. The platform clipboard boundary is `gpui::App::read_from_clipboard` / `write_to_clipboard`, backed by `gpui::Platform`.

For Rduel, the low-risk patch should be scoped to editor instances or buffers created by `crates/rduel/src/rduel.rs`:

1. Add an editor-scoped clipboard provider or policy on `Editor`, defaulting to the existing platform clipboard.
2. Route `copy`, `cut`, and `paste` in `crates/editor/src/clipboard.rs` through that provider.
3. Route `cut_to_end_of_line` and related direct writes in `crates/editor/src/input.rs` through the same provider.
4. Store the Rduel provider in `RduelView`, and install it only on the embedded Rust editor.

This avoids changing ordinary Zed editors. It also keeps Vim copy/paste behavior covered because Vim dispatches editor actions that land in the same editor clipboard code.

## acr-based local runner integration

Rduel should not require users to install `acr` locally. The prototype vendors the relevant runner behavior from `t-seki/acr` (MIT) inside `crates/rduel/src/rduel.rs`:

- `~/.rduel/abc001/a/src/main.rs` and `~/.rduel/abc001/a/Cargo.toml` follow acr's problem workspace shape.
- Sample inputs live under `~/.rduel/abc001/a/tests/{n}.in` and expected outputs under `{n}.out`.
- Test builds with Cargo and runs the compiled binary against those sample files using acr-style result comparison.
- Submit currently prepares the source path and problem URL after tests pass; browser submit filling remains a later patch.

The vendored behavior is attributed in `crates/rduel/THIRD_PARTY.md`.
