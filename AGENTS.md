# AGENTS.md

Seer is a fast, single-window image viewer for Windows, written in Rust as a native Win32 program, that agents can drive from the command line. See README.md for what it does and how to build it.

## Commands

- `cargo build` — build
- `cargo test` — run the tests
- `cargo llvm-cov --all-targets --ignore-filename-regex 'src[\\/]bin[\\/]' --fail-under-lines 75` — run the tests with the coverage check CI enforces
- `cargo fmt --all` — format
- `cargo clippy --all-targets -- -D warnings` — lint

Keep `src/bin/seer.rs` and `src/bin/seerw.rs` thin entry points that call into the library: they are left out of the coverage measurement, so logic there goes untested.

Every `unsafe` block carries a `// SAFETY:` comment saying why it is sound; clippy enforces this.

## Public repository

This repository is public.

## Dependency Policy

All dependencies are pinned to exact versions in `Cargo.toml` (`windows = "0.62.2"`, not `windows = "0.62"`), and `Cargo.lock` is checked in. Do not use version ranges (`^`, `~`, `>=`, `*`). Dependabot handles upgrades.
