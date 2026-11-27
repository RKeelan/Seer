//! Reloading images when their files change, by watching each open image's
//! parent directory and filtering for the open files' names.
//!
//! A [`Watcher`] runs one thread per directory that holds an open image. Each
//! thread waits on `ReadDirectoryChangesW`, which reports the names of files
//! created, written, renamed or deleted in the directory without the
//! directory ever being listed. The watcher keeps the names that belong to
//! open images and drops the rest.
//!
//! [`Reloads`] decides when to act on a change. A burst of writes, as an
//! editor or a program saving a large file makes, is one reload once the
//! file has been quiet for [`DEBOUNCE`]. A reload that fails, because the
//! file is locked or only partly written, is tried again after a delay that
//! doubles each time, up to [`RETRIES`] times; the old image stays on screen
//! meanwhile.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::HSTRING;

use crate::error::Result;
use crate::model::ImageId;

/// How long a file must be quiet after a change before it is reloaded.
pub const DEBOUNCE: Duration = Duration::from_millis(100);

/// The delay before the first retry of a failed reload. Each further retry
/// waits twice as long as the one before.
pub const FIRST_RETRY: Duration = Duration::from_millis(100);

/// How many times a failed reload is tried again before it is given up.
pub const RETRIES: u32 = 8;

/// The size of each directory's change buffer, in bytes.
const BUFFER_BYTES: usize = 16 * 1024;

/// Watches the files of open images, and reports which have changed.
///
/// Nothing is watched until [`Watcher::start`], so a viewer can record what
/// to watch while it starts and begin watching once its first frame is
/// shown.
pub struct Watcher {
    notify: Arc<dyn Fn() + Send + Sync>,
    send: Sender<Event>,
    events: Receiver<Event>,
    started: bool,
    images: HashMap<ImageId, PathBuf>,
    /// The watched directories, by [`key`].
    directories: HashMap<String, Directory>,
}

impl Watcher {
    /// A watcher that calls `notify`, from its own threads, whenever
    /// [`Watcher::changes`] has something to report.
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        let (send, events) = mpsc::channel();
        Watcher {
            notify: Arc::new(notify),
            send,
            events,
            started: false,
            images: HashMap::new(),
            directories: HashMap::new(),
        }
    }

    /// Starts watching the files recorded so far, and any recorded later.
    pub fn start(&mut self) {
        self.started = true;
        let directories: Vec<PathBuf> = self
            .images
            .values()
            .filter_map(|path| path.parent().map(Path::to_owned))
            .collect();
        for directory in directories {
            self.watch_directory(&directory);
        }
    }

    /// Watches an image's file. A file whose directory cannot be watched is
    /// passed over: the image is shown as it is, and is not reloaded.
    pub fn watch(&mut self, image: ImageId, path: PathBuf) {
        let directory = path.parent().map(Path::to_owned);
        self.images.insert(image, path);
        if self.started
            && let Some(directory) = directory
        {
            self.watch_directory(&directory);
        }
    }

    /// Stops watching an image's file, and its directory once no other open
    /// image is in it.
    pub fn unwatch(&mut self, image: ImageId) {
        let Some(path) = self.images.remove(&image) else {
            return;
        };
        let Some(directory) = path.parent().map(key) else {
            return;
        };
        let in_use = self
            .images
            .values()
            .any(|path| path.parent().map(key).as_ref() == Some(&directory));
        if !in_use {
            self.directories.remove(&directory);
        }
    }

    /// Whether a file's directory is being watched.
    pub fn is_watching(&self, path: &Path) -> bool {
        path.parent()
            .is_some_and(|directory| self.directories.contains_key(&key(directory)))
    }

    /// The images whose files have changed since the last call, each once.
    pub fn changes(&mut self) -> Vec<ImageId> {
        let mut changed = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            for (image, path) in &self.images {
                let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
                    continue;
                };
                if key(directory) != event.directory {
                    continue;
                }
                let name = key(Path::new(name));
                let hit = match &event.names {
                    Some(names) => names.iter().any(|changed| key(Path::new(changed)) == name),
                    None => true,
                };
                if hit && !changed.contains(image) {
                    changed.push(*image);
                }
            }
        }
        changed.sort();
        changed
    }

    fn watch_directory(&mut self, directory: &Path) {
        let key = key(directory);
        if self.directories.contains_key(&key) {
            return;
        }
        if let Ok(watch) = Directory::start(directory, key.clone(), &self.send, &self.notify) {
            self.directories.insert(key, watch);
        }
    }
}

/// How the watcher names a directory or file: Windows compares names without
/// regard to case.
fn key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// What a directory's thread reports.
struct Event {
    /// The directory, by [`key`].
    directory: String,
    /// The names of the files that changed, or `None` when so much changed
    /// that the system lost track and any file may have.
    names: Option<Vec<OsString>>,
}

/// A directory being watched, by a thread that ends when this is dropped.
struct Directory {
    stop: Arc<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
}

impl Directory {
    /// Opens the directory, asks for its first changes, and starts the
    /// thread that waits for them, so no change made after this returns is
    /// missed.
    fn start(
        directory: &Path,
        key: String,
        send: &Sender<Event>,
        notify: &Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let mut reader = Reader::open(directory)?;
        reader.arm()?;
        // SAFETY: CreateEventW has no pointer arguments here; a manual-reset
        // event, initially clear.
        let stop = unsafe { CreateEventW(None, true, false, None) }?;
        // SAFETY: the handle was just created and nothing else owns it.
        let stop = Arc::new(unsafe { OwnedHandle::from_raw_handle(stop.0) });
        let thread = {
            let stop = Arc::clone(&stop);
            let send = send.clone();
            let notify = Arc::clone(notify);
            std::thread::Builder::new()
                .name("watch".into())
                .spawn(move || reader.run(&stop, &key, &send, &*notify))?
        };
        Ok(Directory {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: the event is open for as long as `self.stop` lives.
        let _ = unsafe { SetEvent(HANDLE(self.stop.as_raw_handle())) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A directory handle and the overlapped read of its changes.
struct Reader {
    directory: OwnedHandle,
    /// Signalled when the read completes.
    done: OwnedHandle,
    /// Boxed, as are the buffer's contents, so their addresses stay fixed
    /// while a read is pending, however the reader moves.
    overlapped: Box<OVERLAPPED>,
    buffer: Box<[u32]>,
    pending: bool,
}

// SAFETY: the reader owns its handles and the memory the pending read writes
// to, and it is used by one thread at a time.
unsafe impl Send for Reader {}

impl Reader {
    fn open(directory: &Path) -> Result<Self> {
        // SAFETY: the path outlives the call; no security attributes or
        // template are given.
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(directory),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }?;
        // SAFETY: the handle was just opened and nothing else owns it.
        let directory = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        // SAFETY: as for the stop event; this one is reset by each read.
        let done = unsafe { CreateEventW(None, true, false, None) }?;
        // SAFETY: the handle was just created and nothing else owns it.
        let done = unsafe { OwnedHandle::from_raw_handle(done.0) };
        Ok(Reader {
            directory,
            done,
            overlapped: Box::default(),
            buffer: vec![0; BUFFER_BYTES / 4].into_boxed_slice(),
            pending: false,
        })
    }

    /// Asks for the next changes in the directory.
    fn arm(&mut self) -> Result<()> {
        *self.overlapped = OVERLAPPED {
            hEvent: HANDLE(self.done.as_raw_handle()),
            ..OVERLAPPED::default()
        };
        // SAFETY: the buffer is DWORD-aligned and, like the OVERLAPPED, is
        // heap memory the reader owns; `Drop` waits for a pending read to
        // finish before either is freed.
        unsafe {
            ReadDirectoryChangesW(
                HANDLE(self.directory.as_raw_handle()),
                self.buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_SIZE,
                None,
                Some(&mut *self.overlapped),
                None,
            )
        }?;
        self.pending = true;
        Ok(())
    }

    /// Waits for the pending read to finish, and returns how many bytes it
    /// wrote to the buffer.
    fn finish(&mut self) -> Result<u32> {
        self.pending = false;
        let mut bytes = 0;
        // SAFETY: the OVERLAPPED is the one the read was started with, and
        // the pointer is to a live local.
        unsafe {
            GetOverlappedResult(
                HANDLE(self.directory.as_raw_handle()),
                &*self.overlapped,
                &mut bytes,
                true,
            )
        }?;
        Ok(bytes)
    }

    /// Reports changes until `stop` is signalled, the receiver goes, or the
    /// directory can no longer be read, as when it is deleted.
    fn run(mut self, stop: &OwnedHandle, key: &str, send: &Sender<Event>, notify: &dyn Fn()) {
        let handles = [
            HANDLE(stop.as_raw_handle()),
            HANDLE(self.done.as_raw_handle()),
        ];
        loop {
            // SAFETY: both handles stay open until this function returns.
            let woken = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            // The second handle, `done`, is the only one that means go on.
            if woken.0 != WAIT_OBJECT_0.0 + 1 {
                return;
            }
            let names = match self.finish() {
                // A read that returns nothing has overflowed its buffer.
                Ok(0) => None,
                Ok(bytes) => Some(parse(&self.buffer, bytes as usize)),
                Err(_) => return,
            };
            let event = Event {
                directory: key.to_owned(),
                names,
            };
            if send.send(event).is_err() {
                return;
            }
            notify();
            if self.arm().is_err() {
                return;
            }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.pending {
            // SAFETY: the handle is open; cancelling a read that has already
            // finished does nothing.
            let _ = unsafe { CancelIoEx(HANDLE(self.directory.as_raw_handle()), None) };
            let _ = self.finish();
        }
    }
}

/// The file names in the first `bytes` of a change buffer.
fn parse(buffer: &[u32], bytes: usize) -> Vec<OsString> {
    let bytes = bytes.min(std::mem::size_of_val(buffer));
    let base = buffer.as_ptr().cast::<u8>();
    let header = std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
    let mut names = Vec::new();
    let mut offset = 0;
    while offset + header <= bytes {
        // SAFETY: the record lies within the buffer, which the system filled
        // with DWORD-aligned FILE_NOTIFY_INFORMATION records.
        let record = unsafe { &*base.add(offset).cast::<FILE_NOTIFY_INFORMATION>() };
        let length = record.FileNameLength as usize;
        if offset + header + length > bytes {
            break;
        }
        // SAFETY: the name's `length` bytes follow the header within the
        // buffer, and are UTF-16 code units aligned to two bytes.
        let name = unsafe {
            std::slice::from_raw_parts(base.add(offset + header).cast::<u16>(), length / 2)
        };
        names.push(OsString::from_wide(name));
        if record.NextEntryOffset == 0 {
            break;
        }
        offset += record.NextEntryOffset as usize;
    }
    names
}

/// When to reload each changed image, and when to try again after a reload
/// fails.
#[derive(Debug, Default)]
pub struct Reloads {
    pending: HashMap<ImageId, Pending>,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    /// When to reload, or `None` once the reload has been asked for and its
    /// result is awaited.
    due: Option<Instant>,
    /// How many times the reload has failed.
    failures: u32,
}

impl Reloads {
    /// No reloads pending.
    pub fn new() -> Self {
        Reloads::default()
    }

    /// An image's file changed at `now`: reload it once it has been quiet
    /// for [`DEBOUNCE`].
    pub fn changed(&mut self, image: ImageId, now: Instant) {
        self.pending.insert(
            image,
            Pending {
                due: Some(now + DEBOUNCE),
                failures: 0,
            },
        );
    }

    /// The images due for reloading at `now`, in order. Each is then awaited
    /// until [`Reloads::decoded`] or [`Reloads::failed`].
    pub fn due(&mut self, now: Instant) -> Vec<ImageId> {
        let mut due: Vec<ImageId> = self
            .pending
            .iter_mut()
            .filter(|(_, pending)| pending.due.is_some_and(|due| due <= now))
            .map(|(image, pending)| {
                pending.due = None;
                *image
            })
            .collect();
        due.sort();
        due
    }

    /// When the next reload is due, if any is waiting.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter_map(|pending| pending.due)
            .min()
    }

    /// Whether an image is being reloaded: waiting to be, or awaiting the
    /// result.
    pub fn contains(&self, image: ImageId) -> bool {
        self.pending.contains_key(&image)
    }

    /// An image was decoded. A reload that was awaited is done; one that is
    /// waiting, for a change that came since, still is.
    pub fn decoded(&mut self, image: ImageId) {
        if self
            .pending
            .get(&image)
            .is_some_and(|pending| pending.due.is_none())
        {
            self.pending.remove(&image);
        }
    }

    /// An awaited reload failed at `now`: try again later, or, after
    /// [`RETRIES`] retries, give up.
    pub fn failed(&mut self, image: ImageId, now: Instant) {
        let Some(pending) = self.pending.get_mut(&image) else {
            return;
        };
        if pending.due.is_some() {
            return;
        }
        if pending.failures >= RETRIES {
            self.pending.remove(&image);
            return;
        }
        pending.due = Some(now + FIRST_RETRY * 2u32.pow(pending.failures));
        pending.failures += 1;
    }

    /// Drops any reload of an image.
    pub fn forget(&mut self, image: ImageId) {
        self.pending.remove(&image);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::testutil::TempDir;

    const WAIT: Duration = Duration::from_secs(10);

    /// Collects changes until `done` holds of all those seen so far.
    fn wait_for(watcher: &mut Watcher, done: impl Fn(&[ImageId]) -> bool) -> Vec<ImageId> {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            for image in watcher.changes() {
                if !seen.contains(&image) {
                    seen.push(image);
                }
            }
            if done(&seen) {
                seen.sort();
                return seen;
            }
            assert!(Instant::now() < deadline, "saw only {seen:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn changes_to_watched_files_are_reported_and_others_are_not() {
        let dir = TempDir::new().unwrap();
        let other_dir = TempDir::new().unwrap();
        let one = dir.join("One.png");
        let two = dir.join("two.png");
        let three = other_dir.join("three.png");
        for path in [&one, &two, &three] {
            std::fs::write(path, b"before").unwrap();
        }
        let notified = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&notified);
        let mut watcher = Watcher::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        watcher.watch(ImageId(1), one.clone());
        watcher.watch(ImageId(2), two.clone());
        // Nothing is watched before the start.
        assert!(!watcher.is_watching(&one));
        watcher.start();
        assert!(watcher.is_watching(&one));
        watcher.watch(ImageId(3), three.clone());
        assert!(watcher.is_watching(&three));

        // A file that is not open, and a name differing only in case.
        std::fs::write(dir.join("unrelated.png"), b"x").unwrap();
        std::fs::write(dir.join("one.PNG"), b"after").unwrap();
        assert_eq!(
            wait_for(&mut watcher, |seen| !seen.is_empty()),
            [ImageId(1)]
        );
        assert!(notified.load(Ordering::SeqCst) > 0);

        // A directory stays watched while an image in it is open. Closing the
        // first image also drops any late report of its write, which the
        // system may deliver in a later read than the first.
        watcher.unwatch(ImageId(1));
        assert!(watcher.is_watching(&two));

        std::fs::remove_file(&two).unwrap();
        std::fs::write(&three, b"after").unwrap();
        assert_eq!(
            wait_for(&mut watcher, |seen| seen.len() >= 2),
            [ImageId(2), ImageId(3)]
        );

        watcher.unwatch(ImageId(2));
        assert!(!watcher.is_watching(&two));
        watcher.unwatch(ImageId(2));
        watcher.unwatch(ImageId(3));
        assert!(!watcher.is_watching(&three));
    }

    #[test]
    fn an_overflow_reports_every_file_in_the_directory() {
        let mut watcher = Watcher::new(|| {});
        let (send, events) = mpsc::channel();
        watcher.events = events;
        watcher.watch(ImageId(1), PathBuf::from(r"C:\shots\a.png"));
        watcher.watch(ImageId(2), PathBuf::from(r"C:\shots\b.png"));
        watcher.watch(ImageId(3), PathBuf::from(r"C:\other\c.png"));
        watcher.watch(ImageId(4), PathBuf::from(r"C:\"));
        send.send(Event {
            directory: r"c:\shots".into(),
            names: None,
        })
        .unwrap();
        send.send(Event {
            directory: r"c:\shots".into(),
            names: Some(vec![OsString::from("A.png")]),
        })
        .unwrap();
        assert_eq!(watcher.changes(), [ImageId(1), ImageId(2)]);
        assert!(watcher.changes().is_empty());
    }

    #[test]
    fn a_directory_that_cannot_be_opened_is_passed_over() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("absent").join("image.png");
        let mut watcher = Watcher::new(|| {});
        watcher.start();
        watcher.watch(ImageId(1), path.clone());
        assert!(!watcher.is_watching(&path));
    }

    #[test]
    fn change_records_are_parsed_and_truncated_ones_ignored() {
        let mut buffer = vec![0u32; 16];
        let names = ["ab", "c"];
        let mut offset = 0;
        for (index, name) in names.iter().enumerate() {
            let wide: Vec<u16> = name.encode_utf16().collect();
            let next = if index + 1 < names.len() { 16 } else { 0 };
            buffer[offset] = next;
            buffer[offset + 1] = 3;
            buffer[offset + 2] = (wide.len() * 2) as u32;
            let bytes: Vec<u8> = wide.iter().flat_map(|unit| unit.to_le_bytes()).collect();
            for (at, chunk) in bytes.chunks(4).enumerate() {
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                buffer[offset + 3 + at] = u32::from_le_bytes(word);
            }
            offset += 4;
        }
        assert_eq!(parse(&buffer, 64), [OsString::from("ab"), "c".into()]);
        assert_eq!(parse(&buffer, 18), [OsString::from("ab")]);
        assert_eq!(parse(&buffer, 14), Vec::<OsString>::new());
    }

    #[test]
    fn reloads_wait_for_quiet_and_retry_with_back_off() {
        let start = Instant::now();
        let mut reloads = Reloads::new();
        assert_eq!(reloads.next_due(), None);
        reloads.changed(ImageId(1), start);
        // A second change restarts the wait.
        reloads.changed(ImageId(1), start + DEBOUNCE / 2);
        assert!(reloads.due(start + DEBOUNCE).is_empty());
        assert_eq!(reloads.next_due(), Some(start + DEBOUNCE * 3 / 2));
        let now = start + DEBOUNCE * 2;
        assert_eq!(reloads.due(now), [ImageId(1)]);
        assert!(reloads.contains(ImageId(1)));
        assert_eq!(reloads.next_due(), None);

        // Each failure waits twice as long as the last.
        reloads.failed(ImageId(1), now);
        assert_eq!(reloads.next_due(), Some(now + FIRST_RETRY));
        // A failure of a reload that is not awaited changes nothing.
        reloads.failed(ImageId(1), now);
        assert_eq!(reloads.next_due(), Some(now + FIRST_RETRY));
        assert_eq!(reloads.due(now + FIRST_RETRY), [ImageId(1)]);
        reloads.failed(ImageId(1), now);
        assert_eq!(reloads.next_due(), Some(now + FIRST_RETRY * 2));
        for _ in 2..RETRIES {
            assert_eq!(reloads.due(now + Duration::from_secs(3600)), [ImageId(1)]);
            reloads.failed(ImageId(1), now);
        }
        assert_eq!(
            reloads.next_due(),
            Some(now + FIRST_RETRY * 2u32.pow(RETRIES - 1))
        );
        reloads.due(now + Duration::from_secs(3600));
        reloads.failed(ImageId(1), now);
        assert!(!reloads.contains(ImageId(1)));
        reloads.failed(ImageId(2), now);
        assert!(!reloads.contains(ImageId(2)));
    }

    #[test]
    fn a_decode_ends_an_awaited_reload_but_not_a_waiting_one() {
        let now = Instant::now();
        let mut reloads = Reloads::new();
        reloads.changed(ImageId(1), now);
        reloads.decoded(ImageId(1));
        assert!(reloads.contains(ImageId(1)));
        reloads.due(now + DEBOUNCE);
        reloads.decoded(ImageId(1));
        assert!(!reloads.contains(ImageId(1)));
        reloads.changed(ImageId(2), now);
        reloads.forget(ImageId(2));
        assert!(!reloads.contains(ImageId(2)));
    }
}
