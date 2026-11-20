# Seer

[![CI](https://github.com/RKeelan/Seer/actions/workflows/ci.yml/badge.svg)](https://github.com/RKeelan/Seer/actions/workflows/ci.yml)

Seer is a fast image viewer for Windows that shows the images it was asked to open and nothing else, in one window, and that agents can drive and read back from.

## Using Seer

Seer runs on Windows 11, x64.

- `seer --version` prints the program's name and version.

## Developing Seer

Seer is a native Win32 program written in Rust. Building it needs the stable Rust toolchain; measuring test coverage also needs [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov).

- `cargo build` builds the program.
- `cargo run` runs it.
- `cargo test` runs the tests.
- `cargo llvm-cov --ignore-filename-regex 'src[\\/]main\.rs$' --fail-under-lines 75` runs the tests and fails if they cover less than 75% of lines. CI runs this check.
- `cargo fmt` formats the code, and `cargo clippy --all-targets -- -D warnings` lints it.

CI runs on pull requests and on demand, on Windows.

## Licence

Seer is released under the [MIT licence](LICENSE).
