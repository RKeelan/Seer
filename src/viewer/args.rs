//! `seerw.exe`'s command line, parsed by hand to keep argument parsing off the
//! cold-start path.
//!
//! ```text
//! seerw [--headless] [--instance <name>] [--no-activate] [--exit-after-first-frame] [--] <files…>
//! seerw --version
//! ```
//!
//! `--exit-after-first-frame` is for the start-up benchmark: the viewer prints
//! its start-up timings as JSON on stdout once the first frame is presented,
//! and exits.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::{Error, Result};

/// What `seerw.exe` was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewerArgs {
    /// Print the version and exit.
    Version,
    /// Run the viewer, or hand the files to the one already running.
    Run(RunArgs),
}

/// The arguments for running the viewer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunArgs {
    /// Create the window without ever showing it, for tests.
    pub headless: bool,
    /// The instance named by `--instance`, unchecked.
    pub instance: Option<String>,
    /// Show the window without activating it. `seer open` starts the viewer
    /// this way unless given `--activate`, so an agent never takes the focus
    /// from the user.
    pub no_activate: bool,
    /// Print the start-up timings and exit once the first frame is presented.
    pub exit_after_first_frame: bool,
    /// The files to open, as given.
    pub files: Vec<PathBuf>,
}

/// Parses the arguments that follow the program name.
pub fn parse<I>(args: I) -> Result<ViewerArgs>
where
    I: IntoIterator<Item = OsString>,
{
    let mut run = RunArgs::default();
    let mut args = args.into_iter();
    let mut options_ended = false;
    while let Some(arg) = args.next() {
        let text = arg.to_str();
        if options_ended || !text.is_some_and(|text| text.starts_with("--")) {
            run.files.push(PathBuf::from(arg));
            continue;
        }
        match text.expect("checked above") {
            "--" => options_ended = true,
            "--version" => return Ok(ViewerArgs::Version),
            "--headless" => run.headless = true,
            "--no-activate" => run.no_activate = true,
            "--exit-after-first-frame" => run.exit_after_first_frame = true,
            "--instance" => {
                let name = args
                    .next()
                    .ok_or_else(|| Error::usage("--instance needs a name"))?;
                run.instance = Some(
                    name.into_string()
                        .map_err(|_| Error::usage("the instance name is not valid Unicode"))?,
                );
            }
            option => match option.strip_prefix("--instance=") {
                Some(name) => run.instance = Some(name.to_owned()),
                None => return Err(Error::usage(format!("unknown option {option}"))),
            },
        }
    }
    Ok(ViewerArgs::Run(run))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn parse_strs(args: &[&str]) -> Result<ViewerArgs> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn files_and_options() {
        assert_eq!(
            parse_strs(&["--headless", "a.png", "--instance", "t1", r"C:\b.jpg"]).unwrap(),
            ViewerArgs::Run(RunArgs {
                headless: true,
                instance: Some("t1".into()),
                no_activate: false,
                files: vec!["a.png".into(), r"C:\b.jpg".into()],
                ..RunArgs::default()
            })
        );
        assert_eq!(
            parse_strs(&["--instance=t2"]).unwrap(),
            ViewerArgs::Run(RunArgs {
                instance: Some("t2".into()),
                ..RunArgs::default()
            })
        );
        assert_eq!(
            parse_strs(&["--no-activate"]).unwrap(),
            ViewerArgs::Run(RunArgs {
                no_activate: true,
                ..RunArgs::default()
            })
        );
        assert_eq!(
            parse_strs(&["--exit-after-first-frame"]).unwrap(),
            ViewerArgs::Run(RunArgs {
                exit_after_first_frame: true,
                ..RunArgs::default()
            })
        );
        assert_eq!(
            parse_strs(&[]).unwrap(),
            ViewerArgs::Run(RunArgs::default())
        );
    }

    #[test]
    fn version() {
        assert_eq!(parse_strs(&["--version"]).unwrap(), ViewerArgs::Version);
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse_strs(&["--", "--headless", "-x.png"]).unwrap(),
            ViewerArgs::Run(RunArgs {
                files: vec!["--headless".into(), "-x.png".into()],
                ..RunArgs::default()
            })
        );
    }

    #[test]
    fn usage_errors() {
        for args in [&["--instance"][..], &["--frobnicate"]] {
            assert_eq!(parse_strs(args).unwrap_err().code, ErrorCode::Usage);
        }
    }

    #[test]
    fn non_unicode_arguments_are_files_or_errors() {
        use std::os::windows::ffi::OsStringExt;
        let unpaired = OsString::from_wide(&[0xD800]);
        assert_eq!(
            parse([unpaired.clone()]).unwrap(),
            ViewerArgs::Run(RunArgs {
                files: vec![PathBuf::from(unpaired.clone())],
                ..RunArgs::default()
            })
        );
        let error = parse([OsString::from("--instance"), unpaired]).unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
    }
}
