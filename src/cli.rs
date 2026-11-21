//! The command line, `seer.exe`: argument parsing, output formatting and exit
//! codes.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

use crate::error::ErrorCode;

/// Seer's command line.
#[derive(Debug, Parser)]
#[command(
    name = "seer",
    version,
    about = "Drive the Seer image viewer from the command line",
    arg_required_else_help = true
)]
struct Cli {}

/// The entry point of `seer.exe`.
pub fn main() -> ExitCode {
    ExitCode::from(run(std::env::args_os()))
}

/// Runs the command line, program name first, and returns the exit code.
pub fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(Cli {}) => 0,
        Err(error) => {
            let _ = error.print();
            if error.use_stderr() {
                ErrorCode::Usage.exit_code()
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn version_matches_the_library() {
        assert_eq!(
            Cli::command().render_version().to_string(),
            format!("{}\n", crate::version_line())
        );
        assert_eq!(run(["seer", "--version"]), 0);
    }

    #[test]
    fn usage_errors_exit_with_code_2() {
        assert_eq!(run(["seer"]), 2);
        assert_eq!(run(["seer", "--frobnicate"]), 2);
    }
}
