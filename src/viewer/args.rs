//! `seerw.exe`'s command line, parsed by hand to keep argument parsing off the
//! cold-start path.
//!
//! ```text
//! seerw [--headless] [--instance <name>] [--no-activate] [--exit-after-first-frame] [--] <files…>
//! seerw [--headless] [--instance <name>] [--clsid <guid>] -Embedding
//! seerw --version
//! ```
//!
//! COM starts `seerw -Embedding` when Explorer opens a selection through the
//! open verb's `DelegateExecute`; the viewer then takes its files from Explorer
//! rather than its command line (see [`super::execute`]). `--clsid` serves a
//! class id other than [`super::execute::CLSID`], so tests never answer
//! Explorer.
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
    /// Started by COM to take the files from Explorer: `-Embedding` or
    /// `/Embedding`.
    pub embedding: bool,
    /// The class id named by `--clsid`, unchecked.
    pub clsid: Option<String>,
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
        if !options_ended && text.is_some_and(is_embedding) {
            run.embedding = true;
            continue;
        }
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
            "--instance" => run.instance = Some(value(&mut args, "--instance")?),
            "--clsid" => run.clsid = Some(value(&mut args, "--clsid")?),
            option => match option.strip_prefix("--instance=") {
                Some(name) => run.instance = Some(name.to_owned()),
                None => return Err(Error::usage(format!("unknown option {option}"))),
            },
        }
    }
    Ok(ViewerArgs::Run(run))
}

/// The value that follows an option.
fn value(args: &mut impl Iterator<Item = OsString>, option: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| Error::usage(format!("{option} needs a value")))?
        .into_string()
        .map_err(|_| Error::usage(format!("the value of {option} is not valid Unicode")))
}

/// Whether an argument is the flag COM adds when it starts a local server.
fn is_embedding(arg: &str) -> bool {
    ["-Embedding", "/Embedding"]
        .iter()
        .any(|flag| arg.eq_ignore_ascii_case(flag))
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
    fn com_starts_the_viewer_with_embedding() {
        for flag in ["-Embedding", "/Embedding", "-embedding"] {
            assert_eq!(
                parse_strs(&["--instance", "t", "--clsid", "{G}", flag]).unwrap(),
                ViewerArgs::Run(RunArgs {
                    instance: Some("t".into()),
                    embedding: true,
                    clsid: Some("{G}".into()),
                    ..RunArgs::default()
                })
            );
        }
        // After `--`, it is a file.
        assert_eq!(
            parse_strs(&["--", "-Embedding"]).unwrap(),
            ViewerArgs::Run(RunArgs {
                files: vec!["-Embedding".into()],
                ..RunArgs::default()
            })
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
        for args in [&["--instance"][..], &["--clsid"], &["--frobnicate"]] {
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
