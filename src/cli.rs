//! The command line, `seer.exe`: argument parsing, output formatting and exit
//! codes.
//!
//! Each command becomes one [`Request`] to the viewer on the instance named by
//! `--instance` or `SEER_INSTANCE`. Paths are made absolute before they are
//! sent. Results go to stdout, as JSON with `--json` and as short lines of
//! text otherwise; errors go to stderr, and with `--json` also to stdout as
//! `{"error":{"code":"…","message":"…"}}`.
//!
//! Exit codes:
//!
//! - 0: success, including a `close` that names images not in the set (they
//!   are reported on stderr and in the JSON's `not_found`).
//! - 1: unexpected failure.
//! - 2: usage error.
//! - 3: no viewer is running (every command except `open` and `paste`, which
//!   start one).
//! - 4: one or more files could not be opened; the others were. Also when
//!   `paste` finds no image on the clipboard (`no_images`).
//! - 5: `marks --wait` timed out.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::error::{Error, ErrorCode, Result};
use crate::install;
use crate::ipc::client::{self, Starting};
use crate::ipc::instance::InstanceName;
use crate::model::ImageId;
use crate::model::app::Input;
use crate::protocol::{ImageRef, ListReply, OpenFile, OpenReply, Reply, Request};

/// Seer's command line.
#[derive(Debug, Parser)]
#[command(
    name = "seer",
    version,
    about = "Drive the Seer image viewer from the command line",
    arg_required_else_help = true
)]
struct Cli {
    /// Print the result as JSON.
    #[arg(long, global = true)]
    json: bool,

    /// The viewer instance to talk to [default: $SEER_INSTANCE, else "default"].
    #[arg(long, global = true, value_name = "NAME")]
    instance: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Add images to the viewer's window, starting the viewer if none is
    /// running.
    Open(OpenArgs),
    /// Open a temporary copy of the files or the image on the clipboard,
    /// starting the viewer if none is running.
    Paste(PasteArgs),
    /// Report the open images and how the selected one is shown.
    List,
    /// Remove images by id or path.
    Close(CloseArgs),
    /// Remove a group's images.
    Clear {
        /// The group.
        group: String,
    },
    /// Report the marks the user has drawn.
    Marks(MarksArgs),
    /// Save what the window shows as a PNG.
    Screenshot {
        /// The PNG file to write.
        #[arg(value_name = "OUT.png")]
        path: PathBuf,
    },
    /// Close the viewer.
    Quit,
    /// Install Seer for the current user: copy seer.exe and seerw.exe to
    /// %LOCALAPPDATA%\Programs\Seer, add that directory to PATH, register
    /// Seer for its image types, and add it to the Start menu.
    Install,
    /// Undo `seer install`.
    Uninstall,
    /// Commands for testing the viewer.
    #[command(hide = true, subcommand)]
    Debug(DebugCommand),
}

#[derive(Debug, Args)]
struct OpenArgs {
    /// Put the images in this group.
    #[arg(long, value_name = "G")]
    group: Option<String>,
    /// A caption; the nth caption belongs to the nth file.
    #[arg(long = "caption", value_name = "C")]
    captions: Vec<String>,
    /// Show the group side by side once opened.
    #[arg(long)]
    compare: bool,
    /// Bring the window to the foreground instead of flashing its taskbar
    /// button.
    #[arg(long)]
    activate: bool,
    /// The image files. A folder stands for the images directly in it, in
    /// the order Explorer sorts them by name.
    #[arg(required = true, value_name = "FILES")]
    files: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct PasteArgs {
    /// Put the image in this group.
    #[arg(long, value_name = "G")]
    group: Option<String>,
    /// The image's caption.
    #[arg(long, value_name = "C")]
    caption: Option<String>,
    /// Bring the window to the foreground instead of flashing its taskbar
    /// button.
    #[arg(long)]
    activate: bool,
}

#[derive(Debug, Args)]
struct CloseArgs {
    /// Remove every image.
    #[arg(long, conflicts_with = "images")]
    all: bool,
    /// Image ids or paths. A bare number is an id; write `.\3` for a file
    /// named 3.
    #[arg(required_unless_present = "all", value_name = "ID or PATH")]
    images: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct MarksArgs {
    /// Answer only once the user has finished marking (Ctrl+Enter).
    #[arg(long)]
    wait: bool,
    /// With --wait, give up after this many seconds (exit code 5).
    #[arg(long, requires = "wait", value_name = "SECONDS", value_parser = parse_seconds)]
    timeout: Option<u64>,
    /// Remove the marks once reported.
    #[arg(long)]
    clear: bool,
}

#[derive(Debug, Subcommand)]
enum DebugCommand {
    /// Feed synthetic input to a viewer started with --headless.
    ///
    /// Each argument is a JSON input event, or a JSON array of them, such as
    /// '{"type":"key","key":"right"}'. An argument of - reads events from
    /// stdin: one JSON value per line, or arrays.
    Input {
        /// The events, in order.
        #[arg(required = true, value_name = "EVENT")]
        events: Vec<String>,
    },
    /// Report the viewer's start-up timings.
    Timings,
}

/// Parses a number of seconds into milliseconds.
fn parse_seconds(text: &str) -> std::result::Result<u64, String> {
    match text.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds >= 0.0 => Ok((seconds * 1000.0) as u64),
        _ => Err(format!("{text} is not a number of seconds")),
    }
}

/// Starts a viewer on an instance, activating its window or not.
pub(crate) type StartViewer<'a> = dyn FnMut(&InstanceName, bool) -> Result<Box<dyn Starting>> + 'a;

/// What a command reads and writes, so tests can stand in for the console and
/// for starting the viewer.
pub(crate) struct Context<'a> {
    pub(crate) out: &'a mut dyn Write,
    pub(crate) err: &'a mut dyn Write,
    pub(crate) input: &'a mut dyn Read,
    pub(crate) start_viewer: &'a mut StartViewer<'a>,
}

/// The entry point of `seer.exe`.
pub fn main() -> ExitCode {
    ExitCode::from(run(std::env::args_os()))
}

/// Runs the command line, program name first, and returns the exit code.
/// `open` starts `seerw.exe` from beside the running executable when no viewer
/// is running.
pub fn run<I, T>(args: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut start_viewer = |instance: &InstanceName, activate: bool| {
        let program = client::viewer_path()?;
        let viewer = client::start_viewer(&program, instance, activate, &[])?;
        Ok(Box::new(viewer) as Box<dyn Starting>)
    };
    let mut context = Context {
        out: &mut std::io::stdout(),
        err: &mut std::io::stderr(),
        input: &mut std::io::stdin(),
        start_viewer: &mut start_viewer,
    };
    run_with(args, &mut context)
}

/// Runs the command line in a context and returns the exit code.
pub(crate) fn run_with<I, T>(args: I, context: &mut Context) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let json = args
        .iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            let text = error.render().to_string();
            if !error.use_stderr() {
                let _ = context.out.write_all(text.as_bytes());
                return 0;
            }
            let _ = context.err.write_all(text.as_bytes());
            if json {
                let message = text.lines().next().unwrap_or_default();
                let message = message.strip_prefix("error: ").unwrap_or(message);
                print_json_error(context, &Error::usage(message));
            }
            return ErrorCode::Usage.exit_code();
        }
    };
    let outcome = match cli.command {
        Command::Install | Command::Uninstall => installation(&cli.command).map(|(json, text)| {
            let out = if cli.json { json + "\n" } else { text };
            let _ = context.out.write_all(out.as_bytes());
            0
        }),
        _ => execute(&cli, context).map(|reply| report(&reply, cli.json, context)),
    };
    match outcome {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(context.err, "seer: {error}");
            if cli.json {
                print_json_error(context, &error);
            }
            error.exit_code()
        }
    }
}

/// Runs `install` or `uninstall` for the current user, returning the report
/// as JSON and as text.
fn installation(command: &Command) -> Result<(String, String)> {
    let target = install::Target::for_user()?;
    let (json, text) = if matches!(command, Command::Install) {
        let source = std::env::current_exe()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let installed = install::install(&target, &source)?;
        (to_json(&installed), install::describe_installed(&installed))
    } else {
        let uninstalled = install::uninstall(&target)?;
        (
            to_json(&uninstalled),
            install::describe_uninstalled(&uninstalled),
        )
    };
    install::announce();
    Ok((json, text))
}

/// Sends the command's request and returns the viewer's reply.
fn execute(cli: &Cli, context: &mut Context) -> Result<Reply> {
    let instance = InstanceName::from_environment(cli.instance.as_deref())?;
    let request = build_request(&cli.command, context.input)?;
    match request {
        Request::Open { activate, .. } | Request::Paste { activate, .. } => {
            client::send_or_start(&instance, &request, || {
                (context.start_viewer)(&instance, activate)
            })
        }
        _ => client::send(&instance, &request),
    }
}

/// Turns a parsed command into a request, making paths absolute.
fn build_request(command: &Command, input: &mut dyn Read) -> Result<Request> {
    Ok(match command {
        Command::Open(open) => {
            if open.captions.len() > open.files.len() {
                return Err(Error::usage(format!(
                    "{} captions were given for {} files",
                    open.captions.len(),
                    open.files.len()
                )));
            }
            let files = open
                .files
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    let caption = open.captions.get(index).cloned();
                    if caption.is_some() && path.is_dir() {
                        return Err(Error::usage(format!(
                            "{} is a folder, which cannot have a caption",
                            path.display()
                        )));
                    }
                    Ok(OpenFile {
                        path: absolute(path)?,
                        caption,
                    })
                })
                .collect::<Result<_>>()?;
            Request::Open {
                files,
                group: open.group.clone(),
                compare: open.compare,
                activate: open.activate,
                select: None,
            }
        }
        Command::Paste(paste) => Request::Paste {
            group: paste.group.clone(),
            caption: paste.caption.clone(),
            activate: paste.activate,
        },
        Command::List => Request::List,
        Command::Close(close) if close.all => Request::CloseAll,
        Command::Close(close) => Request::Close {
            images: close
                .images
                .iter()
                .map(|image| image_ref(image))
                .collect::<Result<_>>()?,
        },
        Command::Clear { group } => Request::Clear {
            group: group.clone(),
        },
        Command::Marks(marks) => Request::Marks {
            wait: marks.wait,
            timeout_ms: marks.timeout,
            clear: marks.clear,
        },
        Command::Screenshot { path } => Request::Screenshot {
            path: absolute(path)?,
        },
        Command::Quit => Request::Quit,
        Command::Install | Command::Uninstall => {
            unreachable!("install and uninstall do not talk to the viewer")
        }
        Command::Debug(DebugCommand::Input { events }) => Request::DebugInput {
            events: parse_events(events, input)?,
        },
        Command::Debug(DebugCommand::Timings) => Request::DebugTimings,
    })
}

/// Makes a path absolute against the current directory, without resolving
/// links or checking that it exists.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| {
        Error::usage(format!(
            "{} cannot be made absolute: {error}",
            path.display()
        ))
    })
}

/// Reads `close`'s argument: a bare number is an id, anything else a path.
fn image_ref(argument: &Path) -> Result<ImageRef> {
    match argument.to_str().and_then(|text| text.parse::<u32>().ok()) {
        Some(id) => Ok(ImageRef::Id(ImageId(id))),
        None => Ok(ImageRef::Path(absolute(argument)?)),
    }
}

/// Parses `debug input`'s events: JSON objects or arrays of them, with `-`
/// standing for stdin.
fn parse_events(arguments: &[String], input: &mut dyn Read) -> Result<Vec<Input>> {
    let mut events = Vec::new();
    for argument in arguments {
        let text = if argument == "-" {
            let mut text = String::new();
            input.read_to_string(&mut text)?;
            text
        } else {
            argument.clone()
        };
        for value in serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>() {
            let value = value.map_err(|error| bad_event(&text, &error))?;
            let values = match value {
                serde_json::Value::Array(values) => values,
                value => vec![value],
            };
            for value in values {
                let event = serde_json::from_value::<Input>(value.clone())
                    .map_err(|error| bad_event(&value.to_string(), &error))?;
                events.push(event);
            }
        }
    }
    Ok(events)
}

fn bad_event(text: &str, error: &serde_json::Error) -> Error {
    Error::usage(format!("{} is not an input event: {error}", text.trim()))
}

/// Prints a reply and returns the exit code it calls for.
fn report(reply: &Reply, json: bool, context: &mut Context) -> u8 {
    let mut exit_code = 0;
    match reply {
        Reply::Opened(OpenReply { failed, .. }) if !failed.is_empty() => {
            for failure in failed {
                let _ = writeln!(
                    context.err,
                    "seer: could not open {}: {}",
                    failure.path.display(),
                    failure.error
                );
            }
            exit_code = ErrorCode::OpenFailed.exit_code();
        }
        Reply::Closed { not_found, .. } => {
            for image in not_found {
                let _ = writeln!(context.err, "seer: not open: {}", describe(image));
            }
        }
        _ => {}
    }
    let text = if json {
        let mut text = to_json(reply);
        text.push('\n');
        text
    } else {
        format_reply(reply)
    };
    let _ = context.out.write_all(text.as_bytes());
    exit_code
}

fn print_json_error(context: &mut Context, error: &Error) {
    #[derive(Serialize)]
    struct ErrorOutput<'a> {
        error: &'a Error,
    }
    let _ = writeln!(context.out, "{}", to_json(&ErrorOutput { error }));
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("replies always serialise")
}

fn describe(image: &ImageRef) -> String {
    match image {
        ImageRef::Id(id) => id.0.to_string(),
        ImageRef::Path(path) => path.display().to_string(),
    }
}

/// The short, human-readable form of a reply. Every line ends in a newline.
fn format_reply(reply: &Reply) -> String {
    let mut text = String::new();
    match reply {
        Reply::Opened(opened) => {
            for image in &opened.images {
                let existing = if image.existing {
                    "  (already open)"
                } else {
                    ""
                };
                let _ = writeln!(text, "{}  {}{existing}", image.id.0, image.path.display());
            }
        }
        Reply::List(list) => format_list(&mut text, list),
        Reply::Closed { closed, .. } if closed.is_empty() => text.push_str("nothing closed\n"),
        Reply::Closed { closed, .. } => {
            let ids: Vec<_> = closed.iter().map(|id| id.0.to_string()).collect();
            let _ = writeln!(text, "closed {}", ids.join(", "));
        }
        Reply::Marks { marks } if marks.is_empty() => text.push_str("no marks\n"),
        Reply::Marks { marks } => {
            for mark in marks {
                let rect = &mark.rect;
                let _ = writeln!(
                    text,
                    "mark {}  image {}  {}  {},{} {}x{}  {}",
                    mark.id.0,
                    mark.image.0,
                    mark.path.display(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    mark.note
                );
            }
        }
        Reply::Screenshot { path, size } => {
            let _ = writeln!(text, "{}  {}x{}", path.display(), size.width, size.height);
        }
        Reply::Timings(timings) => {
            for phase in &timings.phases {
                let _ = writeln!(text, "{}  {:.1} ms", phase.name, phase.ms);
            }
        }
        Reply::Done => {}
    }
    text
}

fn format_list(text: &mut String, list: &ListReply) {
    if list.images.is_empty() {
        text.push_str("no images open\n");
        return;
    }
    for image in &list.images {
        let marker = if list.selected == Some(image.id) {
            '*'
        } else {
            ' '
        };
        let size = image.size.map_or("-".to_owned(), |size| {
            format!("{}x{}", size.width, size.height)
        });
        let state = serde_json::to_value(image.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let _ = write!(
            text,
            "{marker} {}  {}  {size}  {state}",
            image.id.0,
            image.path.display()
        );
        if let Some(group) = &image.group {
            let _ = write!(text, "  group \"{group}\"");
        }
        if let Some(caption) = &image.caption {
            let _ = write!(text, "  caption \"{caption}\"");
        }
        if image.missing {
            text.push_str("  missing");
        }
        if let Some(error) = &image.error {
            let _ = write!(text, "  ({error})");
        }
        text.push('\n');
    }
    if let Some(view) = &list.view {
        let fit = if view.fit { " (fit)" } else { "" };
        let visible = &view.visible;
        let _ = writeln!(
            text,
            "zoom {:.0}%{fit}, rotation {}°, visible {:.0},{:.0} {:.0}x{:.0}",
            view.zoom * 100.0,
            view.rotation.degrees(),
            visible.x,
            visible.y,
            visible.width,
            visible.height
        );
    }
    if let Some(layout) = list.compare {
        let layout = to_json(&layout);
        let _ = writeln!(text, "compare {}", layout.trim_matches('"'));
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use clap::CommandFactory;

    use super::*;
    use crate::ipc::server::{Responder, Server};
    use crate::model::MarkId;
    use crate::model::app::{Key, Modifiers};
    use crate::model::geometry::{PixelRect, PixelSize, Rect, Rotation};
    use crate::protocol::{
        CompareLayout, ImageInfo, LoadState, MarkInfo, OpenFailure, OpenedImage, Phase, Timings,
        ViewInfo,
    };
    use crate::testutil::unique_instance;

    /// The result of running the command line in a test.
    struct Outcome {
        code: u8,
        out: String,
        err: String,
        started: Vec<(String, bool)>,
    }

    fn run_in(args: &[&str], stdin: &str) -> Outcome {
        run_starting(args, stdin, &mut |_, _| {
            Err(Error::unexpected("no viewer may be started here"))
        })
    }

    fn run_starting(args: &[&str], stdin: &str, start: &mut StartViewer) -> Outcome {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = Cursor::new(stdin.as_bytes().to_vec());
        let mut started = Vec::new();
        let mut start_viewer = |instance: &InstanceName, activate: bool| {
            started.push((instance.to_string(), activate));
            start(instance, activate)
        };
        let mut context = Context {
            out: &mut out,
            err: &mut err,
            input: &mut input,
            start_viewer: &mut start_viewer,
        };
        let code = run_with(
            std::iter::once("seer").chain(args.iter().copied()),
            &mut context,
        );
        Outcome {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            started,
        }
    }

    /// A server that records each request and answers with `reply`.
    fn serve(
        instance: &str,
        reply: impl Fn(&Request) -> Result<Reply> + Send + Sync + 'static,
    ) -> (Server, Arc<Mutex<Vec<Request>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let seen = Arc::clone(&seen);
            Server::start(
                &InstanceName::new(instance).unwrap(),
                move |request: Request, responder: Responder| {
                    responder.respond(reply(&request));
                    seen.lock().unwrap().push(request);
                },
            )
            .unwrap()
        };
        (server, seen)
    }

    fn parse(args: &[&str]) -> Result<Request> {
        let cli = Cli::try_parse_from(std::iter::once("seer").chain(args.iter().copied()))
            .map_err(|error| Error::usage(error.to_string()))?;
        build_request(&cli.command, &mut std::io::empty())
    }

    struct Running;

    impl Starting for Running {
        fn exited(&mut self) -> Option<String> {
            None
        }
    }

    #[test]
    fn version_matches_the_library() {
        assert_eq!(
            Cli::command().render_version().to_string(),
            format!("{}\n", crate::version_line())
        );
        assert_eq!(run(["seer", "--version"]), 0);
        let outcome = run_in(&["--help"], "");
        assert_eq!(outcome.code, 0);
        assert!(outcome.out.contains("screenshot"));
        assert!(!outcome.out.contains("debug"), "debug is hidden");
    }

    #[test]
    fn usage_errors_exit_with_code_2() {
        assert_eq!(run(["seer"]), 2);
        assert_eq!(run(["seer", "--frobnicate"]), 2);
        for args in [
            &["open"][..],
            &["close"],
            &["close", "--all", "3"],
            &["marks", "--timeout", "5"],
            &["marks", "--wait", "--timeout", "soon"],
            &["open", "--caption", "a", "--caption", "b", "x.png"],
            &["open", "--caption", "a", "."],
            &["--instance", "bad name", "list"],
            &["debug", "input", "{\"type\":\"dance\"}"],
            &["debug", "input", "not json"],
        ] {
            let outcome = run_in(args, "");
            assert_eq!(outcome.code, 2, "{args:?}: {}", outcome.err);
            assert!(!outcome.err.is_empty());
        }
    }

    #[test]
    fn usage_errors_are_json_with_json() {
        let outcome = run_in(&["--json", "close"], "");
        assert_eq!(outcome.code, 2);
        let output: serde_json::Value = serde_json::from_str(&outcome.out).unwrap();
        assert_eq!(output["error"]["code"], "usage");
        assert!(output["error"]["message"].as_str().unwrap().len() > 5);
    }

    #[test]
    fn exit_code_3_with_no_viewer() {
        let instance = unique_instance();
        for command in [
            &["list"][..],
            &["close", "--all"],
            &["clear", "g"],
            &["marks"],
            &["screenshot", "x.png"],
            &["quit"],
            &["debug", "timings"],
        ] {
            let mut args = vec!["--instance", &instance];
            args.extend_from_slice(command);
            let outcome = run_in(&args, "");
            assert_eq!(outcome.code, 3, "{command:?}");
            assert!(outcome.err.contains("no viewer"), "{}", outcome.err);
        }
        let outcome = run_in(&["--json", "--instance", &instance, "list"], "");
        assert_eq!(outcome.code, 3);
        assert_eq!(
            outcome.out,
            format!(
                "{{\"error\":{{\"code\":\"no_viewer\",\"message\":\"no viewer is running on instance {instance}\"}}}}\n"
            )
        );
    }

    #[test]
    fn relative_paths_arrive_absolute() {
        let instance = unique_instance();
        let (_server, seen) = serve(&instance, |_| Ok(Reply::Done));
        let current = std::env::current_dir().unwrap();
        for args in [
            &["open", "a.png", r"sub\b.png"][..],
            &["close", r"..\c.png"],
            &["screenshot", "out.png"],
        ] {
            let mut full = vec!["--instance", &instance];
            full.extend_from_slice(args);
            assert_eq!(run_in(&full, "").code, 0);
        }
        let seen = seen.lock().unwrap();
        let Request::Open { files, .. } = &seen[0] else {
            panic!("{:?}", seen[0])
        };
        assert_eq!(files[0].path, current.join("a.png"));
        assert_eq!(files[1].path, current.join(r"sub\b.png"));
        assert_eq!(
            seen[1],
            Request::Close {
                images: vec![ImageRef::Path(
                    std::path::absolute(current.join(r"..\c.png")).unwrap()
                )]
            }
        );
        assert_eq!(
            seen[2],
            Request::Screenshot {
                path: current.join("out.png")
            }
        );
        assert!(files.iter().all(|file| file.path.is_absolute()));
    }

    #[test]
    fn commands_become_requests() {
        let open = parse(&[
            "open",
            "--group",
            "run 3",
            "--caption",
            "before",
            "--compare",
            "--activate",
            r"C:\a.png",
            r"C:\b.png",
        ])
        .unwrap();
        assert_eq!(
            open,
            Request::Open {
                files: vec![
                    OpenFile {
                        path: r"C:\a.png".into(),
                        caption: Some("before".into())
                    },
                    OpenFile {
                        path: r"C:\b.png".into(),
                        caption: None
                    },
                ],
                group: Some("run 3".into()),
                compare: true,
                activate: true,
                select: None,
            }
        );
        assert_eq!(
            parse(&["paste", "--group", "g", "--caption", "menu", "--activate"]).unwrap(),
            Request::Paste {
                group: Some("g".into()),
                caption: Some("menu".into()),
                activate: true,
            }
        );
        assert_eq!(
            parse(&["paste"]).unwrap(),
            Request::Paste {
                group: None,
                caption: None,
                activate: false,
            }
        );
        assert_eq!(parse(&["list"]).unwrap(), Request::List);
        assert_eq!(parse(&["close", "--all"]).unwrap(), Request::CloseAll);
        assert_eq!(
            parse(&["close", "3", r"C:\x.png"]).unwrap(),
            Request::Close {
                images: vec![ImageRef::Id(ImageId(3)), ImageRef::Path(r"C:\x.png".into())]
            }
        );
        assert_eq!(
            parse(&["clear", "g"]).unwrap(),
            Request::Clear { group: "g".into() }
        );
        assert_eq!(
            parse(&["marks", "--wait", "--timeout", "1.5", "--clear"]).unwrap(),
            Request::Marks {
                wait: true,
                timeout_ms: Some(1500),
                clear: true
            }
        );
        assert_eq!(
            parse(&["marks"]).unwrap(),
            Request::Marks {
                wait: false,
                timeout_ms: None,
                clear: false
            }
        );
        assert_eq!(parse(&["quit"]).unwrap(), Request::Quit);
        assert_eq!(parse(&["debug", "timings"]).unwrap(), Request::DebugTimings);
    }

    #[test]
    fn debug_input_reads_events_from_arguments_and_stdin() {
        let instance = unique_instance();
        let (_server, seen) = serve(&instance, |_| Ok(Reply::Done));
        let stdin = "{\"type\":\"key\",\"key\":\"home\"}\n[{\"type\":\"key\",\"key\":\"end\"}]\n";
        let outcome = run_in(
            &[
                "--instance",
                &instance,
                "debug",
                "input",
                r#"{"type":"key","key":"right"}"#,
                r#"[{"type":"key","key":"r","shift":true},{"type":"note_text","text":"x"}]"#,
                "-",
            ],
            stdin,
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert_eq!(outcome.out, "");
        let key = |key, modifiers| Input::Key { key, modifiers };
        assert_eq!(
            seen.lock().unwrap()[0],
            Request::DebugInput {
                events: vec![
                    key(Key::Right, Modifiers::NONE),
                    key(Key::Char('r'), Modifiers::SHIFT),
                    Input::NoteText { text: "x".into() },
                    key(Key::Home, Modifiers::NONE),
                    key(Key::End, Modifiers::NONE),
                ]
            }
        );
    }

    #[test]
    fn viewer_errors_set_the_exit_code() {
        let instance = unique_instance();
        let (_server, _) = serve(&instance, |request| match request {
            Request::Marks { .. } => Err(Error::new(ErrorCode::Timeout, "no marks in time")),
            _ => Err(Error::new(ErrorCode::NotImplemented, "not implemented")),
        });
        let outcome = run_in(&["--instance", &instance, "marks", "--wait"], "");
        assert_eq!(outcome.code, 5);
        assert_eq!(outcome.err, "seer: no marks in time\n");
        let outcome = run_in(&["--json", "--instance", &instance, "list"], "");
        assert_eq!(outcome.code, 1);
        assert_eq!(
            outcome.out,
            "{\"error\":{\"code\":\"not_implemented\",\"message\":\"not implemented\"}}\n"
        );
    }

    #[test]
    fn open_reports_ids_and_failures() {
        let instance = unique_instance();
        let (_server, _) = serve(&instance, |_| {
            Ok(Reply::Opened(OpenReply {
                images: vec![
                    OpenedImage {
                        id: ImageId(1),
                        path: r"C:\a.png".into(),
                        existing: false,
                    },
                    OpenedImage {
                        id: ImageId(2),
                        path: r"C:\b.png".into(),
                        existing: true,
                    },
                ],
                failed: vec![OpenFailure {
                    path: r"C:\gone.png".into(),
                    error: Error::new(ErrorCode::FileNotFound, "no such file"),
                }],
            }))
        });
        let outcome = run_in(&["--instance", &instance, "open", "a.png"], "");
        assert_eq!(outcome.code, 4);
        assert_eq!(outcome.out, "1  C:\\a.png\n2  C:\\b.png  (already open)\n");
        assert_eq!(
            outcome.err,
            "seer: could not open C:\\gone.png: no such file\n"
        );
        assert!(outcome.started.is_empty());

        let outcome = run_in(&["--json", "--instance", &instance, "open", "a.png"], "");
        assert_eq!(outcome.code, 4);
        let output: serde_json::Value = serde_json::from_str(&outcome.out).unwrap();
        assert_eq!(output["images"][1]["existing"], true);
        assert_eq!(output["failed"][0]["error"]["code"], "file_not_found");
    }

    #[test]
    fn open_starts_a_viewer_when_none_runs() {
        let instance = unique_instance();
        let server = Arc::new(Mutex::new(None));
        let outcome = {
            let server = Arc::clone(&server);
            let instance = instance.clone();
            run_starting(
                &["--instance", &instance.clone(), "open", "a.png"],
                "",
                &mut move |_, _| {
                    // The started viewer opens its pipe a little later.
                    let server = Arc::clone(&server);
                    let instance = instance.clone();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(200));
                        let (started, _) = serve(&instance, |_| {
                            Ok(Reply::Opened(OpenReply {
                                images: vec![],
                                failed: vec![],
                            }))
                        });
                        *server.lock().unwrap() = Some(started);
                    });
                    Ok(Box::new(Running))
                },
            )
        };
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert_eq!(outcome.started, [(instance.clone(), false)]);

        let outcome = run_in(
            &["--instance", &instance, "open", "--activate", "a.png"],
            "",
        );
        assert_eq!(outcome.code, 0, "a running viewer is not started again");
        assert!(outcome.started.is_empty());
    }

    #[test]
    fn open_reports_a_viewer_that_fails_to_start() {
        let instance = unique_instance();
        let outcome = run_in(
            &["--instance", &instance, "open", "--activate", "a.png"],
            "",
        );
        assert_eq!(outcome.code, 1);
        assert_eq!(outcome.started, [(instance, true)]);
        assert_eq!(outcome.err, "seer: no viewer may be started here\n");
    }

    #[test]
    fn replies_have_a_human_form() {
        let list = Reply::List(ListReply {
            images: vec![
                ImageInfo {
                    id: ImageId(1),
                    path: r"C:\a.png".into(),
                    caption: Some("before".into()),
                    group: Some("g".into()),
                    size: Some(PixelSize::new(640, 480)),
                    state: LoadState::Loaded,
                    error: None,
                    missing: true,
                },
                ImageInfo {
                    id: ImageId(2),
                    path: r"C:\b.png".into(),
                    caption: None,
                    group: None,
                    size: None,
                    state: LoadState::Failed,
                    error: Some(Error::new(ErrorCode::DecodeFailed, "truncated")),
                    missing: false,
                },
            ],
            selected: Some(ImageId(1)),
            view: Some(ViewInfo {
                zoom: 0.5,
                fit: true,
                rotation: Rotation::Clockwise90,
                visible: Rect::new(0.0, 0.0, 640.0, 480.0),
            }),
            compare: Some(CompareLayout::Tiles),
        });
        assert_eq!(
            format_reply(&list),
            "* 1  C:\\a.png  640x480  loaded  group \"g\"  caption \"before\"  missing\n\
             \x20 2  C:\\b.png  -  failed  (truncated)\n\
             zoom 50% (fit), rotation 90°, visible 0,0 640x480\n\
             compare tiles\n"
        );
        let empty = Reply::List(ListReply {
            images: vec![],
            selected: None,
            view: None,
            compare: None,
        });
        assert_eq!(format_reply(&empty), "no images open\n");
        assert_eq!(
            format_reply(&Reply::Closed {
                closed: vec![ImageId(1), ImageId(2)],
                not_found: vec![]
            }),
            "closed 1, 2\n"
        );
        assert_eq!(
            format_reply(&Reply::Closed {
                closed: vec![],
                not_found: vec![]
            }),
            "nothing closed\n"
        );
        assert_eq!(format_reply(&Reply::Marks { marks: vec![] }), "no marks\n");
        assert_eq!(
            format_reply(&Reply::Marks {
                marks: vec![MarkInfo {
                    id: MarkId(4),
                    image: ImageId(1),
                    path: r"C:\a.png".into(),
                    rect: PixelRect::new(10, 20, 30, 40),
                    note: "wrong title".into(),
                }]
            }),
            "mark 4  image 1  C:\\a.png  10,20 30x40  wrong title\n"
        );
        assert_eq!(
            format_reply(&Reply::Screenshot {
                path: r"C:\out.png".into(),
                size: PixelSize::new(800, 600)
            }),
            "C:\\out.png  800x600\n"
        );
        assert_eq!(
            format_reply(&Reply::Timings(Timings {
                phases: vec![Phase {
                    name: "first_present".into(),
                    ms: 92.54
                }]
            })),
            "first_present  92.5 ms\n"
        );
        assert_eq!(format_reply(&Reply::Done), "");
    }

    #[test]
    fn closing_images_not_open_succeeds_with_a_warning() {
        let instance = unique_instance();
        let (_server, _) = serve(&instance, |_| {
            Ok(Reply::Closed {
                closed: vec![ImageId(1)],
                not_found: vec![ImageRef::Id(ImageId(9)), ImageRef::Path(r"C:\x.png".into())],
            })
        });
        let outcome = run_in(
            &["--instance", &instance, "close", "1", "9", r"C:\x.png"],
            "",
        );
        assert_eq!(outcome.code, 0);
        assert_eq!(outcome.out, "closed 1\n");
        assert_eq!(
            outcome.err,
            "seer: not open: 9\nseer: not open: C:\\x.png\n"
        );
        let outcome = run_in(&["--json", "--instance", &instance, "close", "9"], "");
        let output: serde_json::Value = serde_json::from_str(&outcome.out).unwrap();
        assert_eq!(output["not_found"][0], 9);
    }
}
