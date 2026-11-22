# AGENTS.md

Seer is a fast, single-window image viewer for Windows, written in Rust as a native Win32 program, that agents can drive from the command line. See README.md for what it does and how to build it.

## Commands

README.md's "Developing Seer" lists the commands for building, testing, measuring coverage, formatting, linting and benchmarking. Before finishing a change, run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and the coverage check, `cargo llvm-cov --all-targets --ignore-filename-regex 'src[\\/]bin[\\/]' --fail-under-lines 75`, which CI enforces.

Never push a `v*` tag: it publishes a release (see README.md's "Releasing"), and releasing is the maintainer's call.

Keep `src/bin/seer.rs` and `src/bin/seerw.rs` thin entry points that call into the library: they are left out of the coverage measurement, so logic there goes untested.

Every `unsafe` block carries a `// SAFETY:` comment saying why it is sound; clippy enforces this.

## Testing the viewer without the desktop

Never test against the developer's own viewer or show a window on their desktop. Drive a headless viewer on an instance of your own, using only `seer.exe`; README.md describes each command.

- Pick a unique instance name and pass it as `--instance <name>` to every `seerw` and `seer` call (or set `SEER_INSTANCE`). Viewers on different instances never meet.
- Start the viewer with `seerw --headless --instance <name> [files…]`. It creates its window but never shows it or takes the focus, and draws on WARP, so screenshots are the same on every machine. `seer open --instance <name> …` then adds images to it.
- Poll `seer --json list` until the shown image's `state` is `loaded` before taking a screenshot; decoding happens in the background. Only the shown image and the ones either side of it are decoded, so in a set of more than three, images further away stay `loading` until the selection nears them.
- `seer debug input <event>…` feeds keys, pointer events, wheel turns and typed note text to a headless viewer, as JSON events (see `model::app::Input`). `{"type":"resize","size":{"width":128,"height":96}}` gives the client area a known size, which makes screenshots predictable.
- `seer screenshot <out.png>` writes what the window shows, hidden or not.
- End the viewer with `seer quit`, never by killing it: `cargo llvm-cov` collects coverage only from processes that exit normally.

In Rust tests, `testutil::HeadlessViewer` does all of this: it starts a headless viewer on a unique instance, runs `seer` against it, waits for images to load, and quits it on drop. It also gives the viewer a log directory of its own, since a headless viewer logs only when `SEER_LOG_DIR` is set, and `HeadlessViewer::log` reads what it wrote. `tests/viewer.rs` shows its use. To test copying or pasting, start it with `HeadlessViewer::start_with_env` and `SEER_CLIPBOARD_DIR` set to a temporary directory, as `tests/clipboard.rs` and `tests/paste.rs` do; never test against the real clipboard, which is the developer's.

## Testing installation

Never run `seer install` or `seer uninstall` while developing or testing: they change the user's real file associations and `PATH`. Test `install::install` and `install::uninstall` with an `install::Target` pointing at a `registry::TestRoot` (a throwaway key under `HKEY_CURRENT_USER\Software`, deleted on drop) and a temporary directory, as the tests in `src/install.rs` and `tests/install.rs` do.

## Public repository

This repository is public.

## Dependency Policy

All dependencies are pinned to exact versions in `Cargo.toml` (`windows = "0.62.2"`, not `windows = "0.62"`), and `Cargo.lock` is checked in. Do not use version ranges (`^`, `~`, `>=`, `*`). Dependabot handles upgrades.
