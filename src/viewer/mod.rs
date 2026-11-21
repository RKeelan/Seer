//! The viewer, `seerw.exe`: the window, and the machinery around it.
//!
//! - [`args`]: the command line.
//! - [`window`]: the window class and procedure.
//! - [`theme`]: light and dark.
//! - [`watch`]: reloading images that change on disk.
//! - [`timing`]: start-up timing.
//! - [`log`]: the log file and panic hook.

pub mod args;
pub mod log;
pub mod theme;
pub mod timing;
pub mod watch;
pub mod window;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

use self::args::ViewerArgs;

/// The entry point of `seerw.exe`.
pub fn main() -> ExitCode {
    ExitCode::from(run(std::env::args_os().skip(1)))
}

/// Runs the viewer with the arguments that follow the program name, and
/// returns the exit code.
pub fn run<I>(args: I) -> u8
where
    I: IntoIterator<Item = OsString>,
{
    match args::parse(args) {
        Ok(ViewerArgs::Version) => {
            attach_parent_console();
            let _ = writeln!(std::io::stdout(), "{}", crate::version_line());
            0
        }
        Ok(ViewerArgs::Run(_)) => 0,
        Err(error) => {
            attach_parent_console();
            let _ = writeln!(std::io::stderr(), "seerw: {error}");
            error.exit_code()
        }
    }
}

/// Lets a GUI-subsystem process print to the console it was started from.
///
/// Windows gives such a process no standard handles unless its parent
/// redirected them. When neither stdout nor stderr is set, this attaches to
/// the parent's console, if it has one, so `seerw --version` typed at a prompt
/// prints there.
fn attach_parent_console() {
    let unset = |which| {
        // SAFETY: GetStdHandle only reads the process's standard handle table.
        unsafe { GetStdHandle(which) }.map_or(true, |handle| handle.is_invalid())
    };
    if unset(STD_OUTPUT_HANDLE) && unset(STD_ERROR_HANDLE) {
        // SAFETY: AttachConsole has no pointer arguments. It fails harmlessly
        // when the parent has no console or this process already has one.
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_usage_errors_set_the_exit_code() {
        assert_eq!(run([OsString::from("--version")]), 0);
        assert_eq!(run([OsString::from("--frobnicate")]), 2);
        assert_eq!(run([OsString::from("--headless")]), 0);
    }
}
