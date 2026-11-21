//! The pipe client: connects to the viewer for an instance, retrying while it
//! starts, sends one request and reads the response.
//!
//! `seer.exe` uses it for every command, and a second `seerw.exe` uses it to
//! hand its files to the viewer that owns the instance. A connection carries
//! one request and one response, so a client waiting on `marks --wait` holds
//! only its own connection.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE};
use windows::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::HSTRING;

use crate::error::{Error, ErrorCode, Result};
use crate::ipc::instance::InstanceName;
use crate::protocol::{self, Reply, Request};

/// How long a client waits for the pipe of a viewer that holds the instance's
/// mutex but has not opened its pipe yet.
pub const STARTING_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `seer open` waits for a viewer it started to open its pipe.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client keeps retrying while every instance of the pipe is busy.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to sleep between attempts while the pipe does not exist.
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// The viewer's executable, which `seer open` looks for beside `seer.exe`.
pub const VIEWER_EXECUTABLE: &str = "seerw.exe";

/// A viewer process that has been started but may not have opened its pipe.
pub trait Starting {
    /// `None` while the process runs; once it has exited, a description of
    /// how.
    fn exited(&mut self) -> Option<String>;
}

impl Starting for Child {
    fn exited(&mut self) -> Option<String> {
        match self.try_wait() {
            Ok(None) => None,
            Ok(Some(status)) => Some(status.to_string()),
            Err(error) => Some(error.to_string()),
        }
    }
}

/// An open connection to a viewer, ready for one request.
#[derive(Debug)]
pub struct Connection {
    pipe: File,
}

/// Connects to the viewer on an instance.
///
/// If the pipe does not exist but the instance's mutex is held, the viewer is
/// starting, and this retries for up to [`STARTING_TIMEOUT`]. Otherwise it
/// fails at once with [`ErrorCode::NoViewer`].
pub fn connect(instance: &InstanceName) -> Result<Connection> {
    connect_until(instance, Instant::now() + STARTING_TIMEOUT, || {
        Ok(instance.is_taken())
    })
}

/// Connects to the viewer on an instance, waiting up to `timeout` for a viewer
/// that `viewer` is starting to open its pipe.
///
/// It stops waiting early if the process exits while no other viewer holds the
/// instance. A process that exits because another viewer won the instance is
/// not a failure: the client connects to the winner.
pub fn connect_to_started(
    instance: &InstanceName,
    viewer: &mut dyn Starting,
    timeout: Duration,
) -> Result<Connection> {
    connect_until(instance, Instant::now() + timeout, || {
        match viewer.exited() {
            None => Ok(true),
            Some(_) if instance.is_taken() => Ok(true),
            Some(how) => Err(Error::unexpected(format!(
                "the viewer exited ({how}) before it was ready"
            ))),
        }
    })
}

/// Connects, retrying while the pipe is busy, and while it does not exist for
/// as long as `may_appear` says it might and the deadline has not passed.
fn connect_until(
    instance: &InstanceName,
    deadline: Instant,
    mut may_appear: impl FnMut() -> Result<bool>,
) -> Result<Connection> {
    let name = instance.pipe_name();
    let busy_deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        let error = match open_pipe(&name) {
            Ok(pipe) => return Ok(Connection { pipe }),
            Err(error) => error,
        };
        let code = error.raw_os_error().map(|code| code as u32);
        if code == Some(ERROR_PIPE_BUSY.0) {
            let now = Instant::now();
            if now >= busy_deadline {
                return Err(Error::unexpected(format!(
                    "the viewer on instance {instance} is too busy to answer"
                )));
            }
            let wait = (busy_deadline - now).as_millis().clamp(1, 1000) as u32;
            // SAFETY: WaitNamedPipeW reads the name, which outlives the call.
            // Its result does not matter: the loop tries again either way.
            let _ = unsafe { WaitNamedPipeW(&HSTRING::from(name.as_str()), wait) };
        } else if code == Some(ERROR_FILE_NOT_FOUND.0) {
            if Instant::now() >= deadline || !may_appear()? {
                return Err(Error::new(
                    ErrorCode::NoViewer,
                    format!("no viewer is running on instance {instance}"),
                ));
            }
            thread::sleep(RETRY_INTERVAL);
        } else {
            return Err(error.into());
        }
    }
}

/// Opens the client end of a pipe, letting the server identify this client
/// but not act as it.
pub(crate) fn open_pipe(name: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(SECURITY_IDENTIFICATION.0)
        .open(name)
}

impl Connection {
    /// The process id of the viewer at the other end.
    pub fn server_process_id(&self) -> Result<u32> {
        let mut process = 0;
        // SAFETY: the handle is the open pipe this connection owns, and
        // `process` outlives the call.
        unsafe { GetNamedPipeServerProcessId(self.handle(), &mut process) }?;
        Ok(process)
    }

    /// Lets the viewer bring its window to the foreground.
    ///
    /// Windows lets a process take the foreground only if the foreground
    /// process allows it. A client the user launched, such as `seerw.exe`
    /// started from Explorer, passes its permission on this way. A client
    /// without the permission has none to give, and the call does nothing.
    pub fn allow_foreground(&self) {
        if let Ok(process) = self.server_process_id() {
            // SAFETY: AllowSetForegroundWindow takes a plain value. Failure
            // means this process had no permission to pass on, which is fine.
            let _ = unsafe { AllowSetForegroundWindow(process) };
        }
    }

    /// Sends a request and waits for the response, however long the viewer
    /// takes.
    ///
    /// An [`Request::Open`] that asks to activate the window first passes on
    /// the permission to take the foreground (see
    /// [`Connection::allow_foreground`]).
    pub fn send(self, request: &Request) -> Result<Reply> {
        if matches!(request, Request::Open { activate: true, .. }) {
            self.allow_foreground();
        }
        let mut line = protocol::encode_request(request)?;
        line.push('\n');
        (&self.pipe).write_all(line.as_bytes())?;

        let mut reply = String::new();
        BufReader::new(&self.pipe).read_line(&mut reply)?;
        if reply.is_empty() {
            return Err(Error::unexpected(
                "the viewer closed the connection without answering",
            ));
        }
        protocol::decode_response(&reply)?.into()
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.pipe.as_raw_handle())
    }
}

/// Sends a request to the viewer on an instance (see [`connect`]).
pub fn send(instance: &InstanceName, request: &Request) -> Result<Reply> {
    connect(instance)?.send(request)
}

/// Sends a request to the viewer on an instance, starting one with `start` if
/// none is running, and waiting up to [`LAUNCH_TIMEOUT`] for it.
pub fn send_or_start(
    instance: &InstanceName,
    request: &Request,
    start: impl FnOnce() -> Result<Box<dyn Starting>>,
) -> Result<Reply> {
    let connection = match connect(instance) {
        Err(error) if error.code == ErrorCode::NoViewer => {
            let mut viewer = start()?;
            connect_to_started(instance, viewer.as_mut(), LAUNCH_TIMEOUT)?
        }
        other => other?,
    };
    connection.send(request)
}

/// Where `seer open` looks for the viewer: beside the running executable.
pub fn viewer_path() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    Ok(current.with_file_name(VIEWER_EXECUTABLE))
}

/// The arguments that start a viewer on an instance.
///
/// Without `activate` the window shows without taking the focus, so a viewer
/// started by an agent does not interrupt the user's typing.
pub fn viewer_arguments(instance: &InstanceName, activate: bool) -> Vec<String> {
    let mut arguments = vec!["--instance".to_owned(), instance.to_string()];
    if !activate {
        arguments.push("--no-activate".to_owned());
    }
    arguments
}

/// Starts the viewer at `program` on an instance, with no files: the files
/// follow over the pipe, so the reply can report their ids.
///
/// Its standard handles are closed, so a caller that captures `seer.exe`'s
/// output is not kept waiting by the viewer.
pub fn start_viewer(
    program: &Path,
    instance: &InstanceName,
    activate: bool,
    extra: &[&str],
) -> Result<Child> {
    Command::new(program)
        .args(viewer_arguments(instance, activate))
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            Error::unexpected(format!(
                "could not start the viewer {}: {error}",
                program.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::instance::InstanceMutex;
    use crate::testutil::unique_instance;

    fn instance() -> InstanceName {
        InstanceName::new(&unique_instance()).unwrap()
    }

    struct Exited;

    impl Starting for Exited {
        fn exited(&mut self) -> Option<String> {
            Some("exit code: 0".into())
        }
    }

    #[test]
    fn no_viewer_fails_at_once() {
        let started = Instant::now();
        let error = send(&instance(), &Request::List).unwrap_err();
        assert_eq!(error.code, ErrorCode::NoViewer);
        assert_eq!(error.exit_code(), 3);
        assert!(started.elapsed() < STARTING_TIMEOUT);
    }

    #[test]
    fn a_starting_viewer_is_waited_for_then_given_up_on() {
        let instance = instance();
        let _mutex = InstanceMutex::acquire_or_fail(&instance).unwrap();
        let started = Instant::now();
        let error = connect(&instance).unwrap_err();
        assert_eq!(error.code, ErrorCode::NoViewer);
        assert!(started.elapsed() >= STARTING_TIMEOUT);
    }

    #[test]
    fn a_started_viewer_that_exits_is_reported() {
        let error = connect_to_started(&instance(), &mut Exited, LAUNCH_TIMEOUT).unwrap_err();
        assert_eq!(error.code, ErrorCode::Unexpected);
        assert!(error.message.contains("exited (exit code: 0)"), "{error}");
    }

    #[test]
    fn a_started_viewer_that_loses_the_race_is_not_a_failure() {
        // The started process exited, but another viewer holds the instance,
        // so the client waits for that one until the timeout.
        let instance = instance();
        let _mutex = InstanceMutex::acquire_or_fail(&instance).unwrap();
        let error =
            connect_to_started(&instance, &mut Exited, Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.code, ErrorCode::NoViewer);
    }

    #[test]
    fn failing_to_start_is_reported() {
        let instance = instance();
        let error = send_or_start(&instance, &Request::List, || {
            Err(Error::unexpected("no such program"))
        })
        .unwrap_err();
        assert_eq!(error.message, "no such program");

        let missing = Path::new(r"C:\nonexistent\seerw.exe");
        let error = start_viewer(missing, &instance, true, &[]).unwrap_err();
        assert!(
            error.message.contains("could not start the viewer"),
            "{error}"
        );
    }

    #[test]
    fn the_viewer_is_found_beside_the_executable() {
        let path = viewer_path().unwrap();
        assert_eq!(path.file_name().unwrap(), VIEWER_EXECUTABLE);
        assert_eq!(path.parent(), std::env::current_exe().unwrap().parent());
    }

    #[test]
    fn viewer_arguments_carry_the_instance_and_focus() {
        let instance = InstanceName::new("t").unwrap();
        assert_eq!(viewer_arguments(&instance, true), ["--instance", "t"]);
        assert_eq!(
            viewer_arguments(&instance, false),
            ["--instance", "t", "--no-activate"]
        );
    }
}
