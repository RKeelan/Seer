//! Seer, a single-window image viewer for Windows.
//!
//! The crate builds two executables over this one library: `seerw.exe`, the
//! viewer (see [`viewer`]), and `seer.exe`, the command line that drives it
//! over a named pipe (see [`cli`]). Logic that can be tested without the
//! desktop lives in modules with no Win32 types: [`model`], [`protocol`] and
//! the argument parsing in [`cli`] and [`viewer::args`].

pub mod cli;
pub mod decode;
pub mod error;
pub mod install;
pub mod ipc;
pub mod model;
pub mod protocol;
pub mod render;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
pub mod viewer;

pub use error::{Error, ErrorCode, Result};

/// The program's name and version, as shown by `seer --version` and
/// `seerw --version`.
pub fn version_line() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_program_and_version() {
        assert_eq!(
            version_line(),
            format!("seer {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
