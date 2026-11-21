//! A small, size-capped log file, opened after the first frame, with a panic
//! hook that records panics there.
//!
//! The log is `seerw.log` in `%LOCALAPPDATA%\Seer\logs`, or in the directory
//! `SEER_LOG_DIR` names. Every viewer on every instance appends to it, one
//! line per event, each starting with the local time and the process id. When
//! a line would take it past [`CAP`] bytes, it is renamed to `seerw.old.log`,
//! replacing the one before, and a new log is started; the two together never
//! hold much more than twice the cap.
//!
//! Only the viewer that owns its instance logs, and a headless one only when
//! `SEER_LOG_DIR` is set, so tests leave the user's log alone (see
//! [`wanted`]).
//!
//! Nothing touches the disk before the first frame: [`init`] only installs
//! the panic hook, and lines written before [`open`] are held in memory and
//! written when it is called. A panic opens the log at once, whenever it
//! happens.
//!
//! File names stay out of the log unless `SEER_LOG_PATHS` is `1`. Error
//! messages and panic messages can hold file names, so without it the log
//! records an error's code and a panic's location only.

use std::any::Any;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once, OnceLock, TryLockError};

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::error::Error;

/// The size in bytes past which the log is started again.
pub const CAP: u64 = 1024 * 1024;

/// The log's file name.
pub const FILE_NAME: &str = "seerw.log";

/// The name the full log is renamed to when a new one is started.
pub const OLD_FILE_NAME: &str = "seerw.old.log";

/// How many lines are held before [`open`]; later ones are dropped.
const PENDING_LIMIT: usize = 256;

/// A log file in a directory, started again when it reaches its cap.
#[derive(Debug)]
pub struct Log {
    directory: PathBuf,
    cap: u64,
    file: File,
    len: u64,
}

impl Log {
    /// Opens the log in `directory` for appending, creating the directory if
    /// need be.
    pub fn open(directory: &Path, cap: u64) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let file = append(&directory.join(FILE_NAME))?;
        let len = file.metadata()?.len();
        Ok(Log {
            directory: directory.to_owned(),
            cap,
            file,
            len,
        })
    }

    /// The log file's path.
    pub fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    /// Appends `line` and a line break, first starting the log again if the
    /// line would take it past its cap.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let bytes = format!("{line}\r\n");
        let size = bytes.len() as u64;
        if self.len > 0 && self.len + size > self.cap {
            self.rotate()?;
        }
        self.file.write_all(bytes.as_bytes())?;
        self.len += size;
        Ok(())
    }

    /// Renames the log to [`OLD_FILE_NAME`] and starts a new one.
    fn rotate(&mut self) -> io::Result<()> {
        let path = self.path();
        // Another viewer may have rotated it already; then this one carries
        // on in the new file.
        let _ = std::fs::rename(&path, self.directory.join(OLD_FILE_NAME));
        self.file = append(&path)?;
        self.len = self.file.metadata()?.len();
        Ok(())
    }
}

fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Where the process's lines go.
enum Sink {
    /// [`init`] has not been called; lines are dropped.
    Off,
    /// Held until [`open`].
    Pending(Vec<String>),
    Open(Log),
    /// The log could not be opened; lines are dropped.
    Closed,
}

static SINK: Mutex<Sink> = Mutex::new(Sink::Off);

fn sink() -> MutexGuard<'static, Sink> {
    SINK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The log's directory: `SEER_LOG_DIR`, or `%LOCALAPPDATA%\Seer\logs`.
pub fn directory() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("SEER_LOG_DIR").filter(|value| !value.is_empty()) {
        return Some(directory.into());
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(Path::new(&local).join("Seer").join("logs"))
}

/// Whether a viewer should log: any viewer the user starts, and a headless
/// one only when `SEER_LOG_DIR` gives it somewhere of its own to write.
pub fn wanted(headless: bool) -> bool {
    !headless || std::env::var_os("SEER_LOG_DIR").is_some_and(|value| !value.is_empty())
}

/// Whether file names may appear in the log: `SEER_LOG_PATHS` is `1`.
pub fn paths_allowed() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| std::env::var_os("SEER_LOG_PATHS").is_some_and(|value| value == "1"))
}

/// Turns the log on, holding lines until [`open`], and installs the panic
/// hook. The hook records the panic in the log, opening the log if it is not
/// open yet, and then hands the panic to the hook that was installed before.
/// Until this is called, nothing is logged.
pub fn init() {
    static HOOKED: Once = Once::new();
    {
        let mut sink = sink();
        if matches!(*sink, Sink::Off) {
            *sink = Sink::Pending(Vec::new());
        }
    }
    HOOKED.call_once(install_hook);
}

fn install_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let line = panic_line(
            thread.name(),
            info.location(),
            info.payload(),
            paths_allowed(),
        );
        // A panic while this thread holds the log leaves the log alone,
        // rather than wait for itself.
        let mut sink = match SINK.try_lock() {
            Ok(sink) => Some(sink),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(sink) = sink.as_deref_mut() {
            push(sink, stamped(format_args!("{line}")));
            open_in(sink);
        }
        drop(sink);
        previous(info);
    }));
}

/// A panic as the log shows it: the thread and the place in Seer's source,
/// and the message too if file names are allowed.
fn panic_line(
    thread: Option<&str>,
    location: Option<&Location<'_>>,
    payload: &(dyn Any + Send),
    paths: bool,
) -> String {
    let thread = thread.unwrap_or("unnamed");
    let location = location.map_or_else(|| "an unknown location".to_owned(), ToString::to_string);
    let mut line = format!("panic on thread {thread} at {location}");
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
    if paths && let Some(message) = message {
        line.push_str(": ");
        line.push_str(message);
    }
    line
}

/// Opens the log in [`directory`] and writes the lines held so far. Does
/// nothing if it is open already.
pub fn open() {
    open_in(&mut sink());
}

fn open_in(sink: &mut Sink) {
    let Sink::Pending(pending) = sink else {
        return;
    };
    let pending = std::mem::take(pending);
    let Some(mut log) = directory().and_then(|directory| Log::open(&directory, CAP).ok()) else {
        *sink = Sink::Closed;
        return;
    };
    for line in &pending {
        let _ = log.write_line(line);
    }
    *sink = Sink::Open(log);
}

/// Writes a line to the log, or holds it until [`open`].
pub fn write(message: fmt::Arguments<'_>) {
    push(&mut sink(), stamped(message));
}

fn stamped(message: fmt::Arguments<'_>) -> String {
    format!("{} [{}] {message}", timestamp(), std::process::id())
}

fn push(sink: &mut Sink, line: String) {
    match sink {
        Sink::Pending(pending) => {
            if pending.len() < PENDING_LIMIT {
                pending.push(line);
            }
        }
        Sink::Open(log) => {
            let _ = log.write_line(&line);
        }
        Sink::Off | Sink::Closed => {}
    }
}

/// An error as the log shows it: its code, and its message too if file names
/// are allowed.
pub fn error(error: &Error) -> String {
    error_with(error, paths_allowed())
}

fn error_with(error: &Error, paths: bool) -> String {
    let code = serde_json::to_value(error.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    if paths {
        format!("{code}: {}", error.message)
    } else {
        code
    }
}

/// The local time, to the millisecond.
fn timestamp() -> String {
    // SAFETY: GetLocalTime only writes the structure it returns.
    let now = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond, now.wMilliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::testutil::TempDir;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn lines_are_appended_to_what_is_there() {
        let directory = TempDir::new().unwrap();
        let logs = directory.join("logs");
        Log::open(&logs, CAP).unwrap().write_line("one").unwrap();
        let mut log = Log::open(&logs, CAP).unwrap();
        log.write_line("two").unwrap();
        assert_eq!(read(&log.path()), "one\r\ntwo\r\n");
    }

    #[test]
    fn a_full_log_is_renamed_and_started_again() {
        let directory = TempDir::new().unwrap();
        let mut log = Log::open(directory.path(), 12).unwrap();
        log.write_line("first").unwrap();
        log.write_line("second").unwrap();
        log.write_line("third").unwrap();
        assert_eq!(read(&log.path()), "third\r\n");
        assert_eq!(read(&directory.join(OLD_FILE_NAME)), "second\r\n");
    }

    #[test]
    fn a_line_longer_than_the_cap_is_still_written() {
        let directory = TempDir::new().unwrap();
        let mut log = Log::open(directory.path(), 4).unwrap();
        log.write_line("longer than the cap").unwrap();
        assert_eq!(read(&log.path()), "longer than the cap\r\n");
    }

    #[test]
    fn an_error_shows_its_message_only_when_paths_are_allowed() {
        let error = Error::new(ErrorCode::FileNotFound, r"C:\photos\cat.png does not exist");
        assert_eq!(error_with(&error, false), "file_not_found");
        assert_eq!(
            error_with(&error, true),
            r"file_not_found: C:\photos\cat.png does not exist"
        );
    }

    #[test]
    fn a_panic_shows_its_message_only_when_paths_are_allowed() {
        let location = Location::caller();
        let message = String::from(r"C:\photos\cat.png is broken");
        let line = panic_line(Some("main"), Some(location), &message, false);
        assert_eq!(line, format!("panic on thread main at {location}"));
        let line = panic_line(None, None, &"static message", true);
        assert_eq!(
            line,
            "panic on thread unnamed at an unknown location: static message"
        );
        let line = panic_line(Some("main"), Some(location), &message, true);
        assert!(line.ends_with(r": C:\photos\cat.png is broken"), "{line}");
    }

    #[test]
    fn lines_are_dropped_held_or_written_by_the_state_of_the_log() {
        let mut sink = Sink::Off;
        push(&mut sink, "dropped".into());
        assert!(matches!(sink, Sink::Off));

        let mut sink = Sink::Pending(Vec::new());
        for _ in 0..PENDING_LIMIT + 1 {
            push(&mut sink, "held".into());
        }
        let Sink::Pending(held) = &sink else {
            panic!("not pending");
        };
        assert_eq!(held.len(), PENDING_LIMIT);

        let directory = TempDir::new().unwrap();
        let mut sink = Sink::Open(Log::open(directory.path(), CAP).unwrap());
        push(&mut sink, "written".into());
        assert_eq!(read(&directory.join(FILE_NAME)), "written\r\n");
    }

    #[test]
    fn timestamps_have_a_fixed_shape() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), "2026-01-01 00:00:00.000".len(), "{stamp}");
    }
}
