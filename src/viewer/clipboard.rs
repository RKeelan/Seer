//! Copying an image, or part of one, to the clipboard.
//!
//! The pixels go on the clipboard twice, both with straight alpha: as a PNG,
//! under the registered format `PNG` that browsers and image editors read,
//! and as `CF_DIBV5`, a bottom-up 32-bit bitmap with an alpha mask, from
//! which Windows makes `CF_DIB` and `CF_BITMAP` for programs that ask for
//! those.
//!
//! The UI thread reads the pixels back from the graphics device, which is
//! quick, and hands them to a [`Copier`]. Its thread does the slow part:
//! turning the pixels, encoding the PNG and building the bitmap, each in
//! memory the clipboard can take. The UI thread then only opens the
//! clipboard and hands that memory over, so copying a large image never
//! stalls the window.
//!
//! A headless viewer never touches the clipboard, which belongs to the
//! user's desktop. When `SEER_CLIPBOARD_DIR` is set, it writes what it would
//! have put there to [`SINK_PNG`] and [`SINK_DIB`] in that directory
//! instead, which is how the tests see what was copied; otherwise copying
//! fails. Pasting reads the same directory: see [`super::paste`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_DIBV5;
use windows::core::w;

use crate::error::{Error, Result};
use crate::model::geometry::{PixelRect, PixelSize, Rotation};
use crate::render::offscreen::{Capture, encode_png, unpremultiply};

/// The file a headless viewer writes the PNG it copies to.
pub const SINK_PNG: &str = "clipboard.png";

/// The file a headless viewer writes the `CF_DIBV5` bitmap it copies to.
pub const SINK_DIB: &str = "clipboard.dib";

/// How many times to try to open the clipboard, which another program may
/// hold for a moment, and how long to wait between tries.
const OPEN_TRIES: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(10);

/// The size of a `BITMAPV5HEADER`.
const DIBV5_HEADER: usize = 124;

/// The clipboard a viewer copies to and pastes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clipboard {
    /// The clipboard.
    System,
    /// Files in this directory, for a headless viewer under test.
    Sink(PathBuf),
    /// None: a headless viewer with no `SEER_CLIPBOARD_DIR`.
    Unavailable,
}

impl Clipboard {
    /// A viewer's clipboard: the system's, or for a headless viewer the
    /// directory `SEER_CLIPBOARD_DIR` names, if any.
    pub fn for_viewer(headless: bool) -> Self {
        if !headless {
            return Clipboard::System;
        }
        match std::env::var_os("SEER_CLIPBOARD_DIR").filter(|value| !value.is_empty()) {
            Some(directory) => Clipboard::Sink(directory.into()),
            None => Clipboard::Unavailable,
        }
    }

    /// The error for a headless viewer with no clipboard.
    pub fn unavailable() -> Error {
        Error::unexpected("a headless viewer has no clipboard unless SEER_CLIPBOARD_DIR is set")
    }
}

/// A copy prepared on the [`Copier`]'s thread, for the UI thread to finish
/// with [`Ready::finish`].
pub struct Ready {
    size: PixelSize,
    /// The PNG and the bitmap, for the clipboard; `None` once written to a
    /// sink.
    contents: Option<(Global, Global)>,
}

impl Ready {
    /// Puts the copy on the clipboard, owned by `hwnd`, if it is for the
    /// clipboard, and returns the size of what was copied.
    pub fn finish(self, hwnd: HWND) -> Result<PixelSize> {
        if let Some((png, dib)) = self.contents {
            put_on_clipboard(hwnd, png, dib)?;
        }
        Ok(self.size)
    }
}

/// A copy for the [`Copier`]'s thread to prepare.
struct Job {
    /// Premultiplied, as read back from the device.
    capture: Capture,
    rotation: Rotation,
    clipboard: Clipboard,
}

/// Does work on a thread of its own, one job at a time in the order given,
/// and calls its `notify` after each result is ready. The thread starts with
/// the first job; if it cannot, the job is done on the caller's thread.
/// Dropped, it lets the thread finish the jobs given to it.
pub struct Background<J, R> {
    name: &'static str,
    work: Arc<dyn Fn(J) -> R + Send + Sync>,
    notify: Arc<dyn Fn() + Send + Sync>,
    jobs: Option<Sender<J>>,
    ready: Receiver<R>,
    send_ready: Sender<R>,
    thread: Option<JoinHandle<()>>,
}

impl<J: Send + 'static, R: Send + 'static> Background<J, R> {
    /// A thread named `name` that does `work` on each job, calling `notify`
    /// after each.
    pub fn new(
        name: &'static str,
        work: impl Fn(J) -> R + Send + Sync + 'static,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let (send_ready, ready) = mpsc::channel();
        Background {
            name,
            work: Arc::new(work),
            notify: Arc::new(notify),
            jobs: None,
            ready,
            send_ready,
            thread: None,
        }
    }

    /// Queues `job`.
    pub fn submit(&mut self, job: J) {
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (send, jobs) = mpsc::channel::<J>();
        let ready = self.send_ready.clone();
        let work = Arc::clone(&self.work);
        let notify = Arc::clone(&self.notify);
        let thread = std::thread::Builder::new()
            .name(self.name.into())
            .spawn(move || {
                for job in jobs {
                    let _ = ready.send(work(job));
                    notify();
                }
            });
        match thread {
            Ok(thread) => {
                let _ = send.send(job);
                self.jobs = Some(send);
                self.thread = Some(thread);
            }
            Err(_) => {
                let _ = self.send_ready.send((self.work)(job));
                (self.notify)();
            }
        }
    }

    /// The next result ready.
    pub fn try_recv(&self) -> Option<R> {
        self.ready.try_recv().ok()
    }
}

impl<J, R> Drop for Background<J, R> {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Prepares copies on a thread of its own, one at a time in the order they
/// are given, and calls its `notify` after each is ready.
pub struct Copier {
    clipboard: Clipboard,
    thread: Background<Job, Result<Ready>>,
}

impl Copier {
    /// A copier sending copies to `clipboard`, calling `notify` after each
    /// is ready.
    pub fn new(clipboard: Clipboard, notify: impl Fn() + Send + Sync + 'static) -> Self {
        Copier {
            clipboard,
            thread: Background::new("copy", prepare, notify),
        }
    }

    /// Sends later copies to `clipboard`.
    pub fn set_clipboard(&mut self, clipboard: Clipboard) {
        self.clipboard = clipboard;
    }

    /// Prepares a copy of `capture`, premultiplied pixels as read back from
    /// the device, turned by `rotation`.
    pub fn submit(&mut self, capture: Capture, rotation: Rotation) {
        self.thread.submit(Job {
            capture,
            rotation,
            clipboard: self.clipboard.clone(),
        });
    }

    /// The next copy ready, or the reason it could not be made.
    pub fn try_recv(&self) -> Option<Result<Ready>> {
        self.thread.try_recv()
    }
}

/// Turns, encodes and places a copy: in memory for the clipboard, or in a
/// sink's files.
fn prepare(job: Job) -> Result<Ready> {
    if job.clipboard == Clipboard::Unavailable {
        return Err(Clipboard::unavailable());
    }
    let mut capture = job.capture;
    unpremultiply(&mut capture.pixels);
    let capture = rotate(capture, job.rotation);
    let png = encode_png(&capture)?;
    let dib = dibv5(&capture);
    let contents = match &job.clipboard {
        Clipboard::Sink(directory) => {
            // The PNG goes last, so a reader that waits for it finds both.
            write_whole(directory, SINK_DIB, &dib)?;
            write_whole(directory, SINK_PNG, &png)?;
            None
        }
        _ => Some((Global::holding(&png)?, Global::holding(&dib)?)),
    };
    Ok(Ready {
        size: capture.size,
        contents,
    })
}

/// Writes `bytes` to `name` in `directory` under another name first, so the
/// file never appears partly written.
fn write_whole(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let partial = directory.join(format!("{name}.partial"));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, directory.join(name))?;
    Ok(())
}

/// A movable global memory block, as the clipboard takes, freed on drop
/// unless the clipboard has taken it.
struct Global(HGLOBAL);

// SAFETY: a global memory handle names memory in the process's heap, which
// any thread may use, and a `Global` is its only owner.
unsafe impl Send for Global {}

impl Global {
    /// A block holding a copy of `bytes`.
    fn holding(bytes: &[u8]) -> Result<Self> {
        // SAFETY: GlobalAlloc has no pointer arguments.
        let block = Global(unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }?);
        // SAFETY: the block was just allocated and is owned here.
        let locked = unsafe { GlobalLock(block.0) };
        if locked.is_null() {
            return Err(Error::unexpected("could not lock memory for the clipboard"));
        }
        // SAFETY: the locked block holds at least `bytes.len()` bytes and does
        // not overlap `bytes`.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), locked.cast::<u8>(), bytes.len()) };
        // SAFETY: the block was locked above. GlobalUnlock reports an error
        // when the lock count reaches zero, which is the expected outcome.
        let _ = unsafe { GlobalUnlock(block.0) };
        Ok(block)
    }

    /// Puts the block on the open clipboard as `format`; the clipboard owns
    /// it from then on.
    fn give(self, format: u32) -> Result<()> {
        // SAFETY: the clipboard is open on this thread and the block is
        // movable global memory.
        unsafe { SetClipboardData(format, Some(HANDLE(self.0.0))) }?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        // SAFETY: the block is this process's and nothing else holds it.
        let _ = unsafe { GlobalFree(Some(self.0)) };
    }
}

/// The part of a bitmap of `bitmap` pixels covering `region` of an image of
/// `size`, which the bitmap shows scaled. Usually the two sizes are the same;
/// a large SVG document is rasterised smaller than its size.
pub fn scale_region(region: PixelRect, size: PixelSize, bitmap: PixelSize) -> PixelRect {
    if size == bitmap {
        return region;
    }
    let scale = |value: u32, from: u32, to: u32, round: fn(f64) -> f64| {
        let scaled = round(f64::from(value) * f64::from(to) / f64::from(from.max(1)));
        // Clamped to the bitmap, so it fits in u32.
        scaled.clamp(0.0, f64::from(to)) as u32
    };
    let left = scale(region.x, size.width, bitmap.width, f64::floor);
    let top = scale(region.y, size.height, bitmap.height, f64::floor);
    let right = scale(region.x + region.width, size.width, bitmap.width, f64::ceil);
    let bottom = scale(
        region.y + region.height,
        size.height,
        bitmap.height,
        f64::ceil,
    );
    // Never empty: a region smaller than a bitmap pixel takes that pixel.
    let left = left.min(bitmap.width.saturating_sub(1));
    let top = top.min(bitmap.height.saturating_sub(1));
    PixelRect::new(
        left,
        top,
        right.max(left + 1) - left,
        bottom.max(top + 1) - top,
    )
}

/// `capture` turned by `rotation`, clockwise.
pub fn rotate(capture: Capture, rotation: Rotation) -> Capture {
    let (width, height) = (capture.size.width as usize, capture.size.height as usize);
    let size = if rotation.is_sideways() {
        PixelSize::new(capture.size.height, capture.size.width)
    } else {
        capture.size
    };
    if rotation == Rotation::None {
        return capture;
    }
    let mut pixels = vec![0u8; capture.pixels.len()];
    let turned_width = size.width as usize;
    for y in 0..height {
        for x in 0..width {
            let (to_x, to_y) = match rotation {
                Rotation::None => (x, y),
                Rotation::Clockwise90 => (height - 1 - y, x),
                Rotation::Half => (width - 1 - x, height - 1 - y),
                Rotation::Clockwise270 => (y, width - 1 - x),
            };
            let from = (y * width + x) * 4;
            let to = (to_y * turned_width + to_x) * 4;
            pixels[to..to + 4].copy_from_slice(&capture.pixels[from..from + 4]);
        }
    }
    Capture { size, pixels }
}

/// `capture` as the contents of a `CF_DIBV5` clipboard entry: a
/// `BITMAPV5HEADER` for a bottom-up 32-bit sRGB bitmap with red, green, blue
/// and alpha masks, then the rows, bottom first.
pub fn dibv5(capture: &Capture) -> Vec<u8> {
    let (width, height) = (capture.size.width, capture.size.height);
    let row = width as usize * 4;
    let image_size = row * height as usize;
    // 96 dots per inch, in pixels per metre.
    let resolution = 3780;
    let fields: [u32; 31] = [
        DIBV5_HEADER as u32,
        width,
        // Positive: the rows run bottom to top.
        height,
        // One plane, then 32 bits per pixel.
        1 | 32 << 16,
        3, // BI_BITFIELDS
        u32::try_from(image_size).unwrap_or(0),
        resolution,
        resolution,
        0, // colours used
        0, // colours important
        0x00FF_0000,
        0x0000_FF00,
        0x0000_00FF,
        0xFF00_0000,
        0x7352_4742, // LCS_sRGB: 'sRGB'
        // The endpoints and gammas, which sRGB does not use.
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        4, // LCS_GM_IMAGES
        0, // profile data
        0, // profile size
        0, // reserved
    ];
    let mut bytes = Vec::with_capacity(DIBV5_HEADER + image_size);
    for field in fields {
        bytes.extend_from_slice(&field.to_le_bytes());
    }
    debug_assert_eq!(bytes.len(), DIBV5_HEADER);
    if row > 0 {
        for line in capture.pixels.chunks_exact(row).rev() {
            bytes.extend_from_slice(line);
        }
    }
    bytes
}

/// Replaces the clipboard's contents with `png` and `dib`, owned by `hwnd`.
fn put_on_clipboard(hwnd: HWND, png: Global, dib: Global) -> Result<()> {
    let png_format = png_format()?;
    with_clipboard(hwnd, || {
        // SAFETY: this thread has the clipboard open.
        unsafe { EmptyClipboard() }?;
        png.give(png_format)?;
        dib.give(u32::from(CF_DIBV5.0))
    })
}

/// The registered clipboard format `PNG`.
pub fn png_format() -> Result<u32> {
    // SAFETY: the format name is a static string.
    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if format == 0 {
        return Err(windows::core::Error::from_thread().into());
    }
    Ok(format)
}

/// Opens the clipboard for `hwnd`, a window of this thread, waiting a little
/// if another program has it, runs `act`, and closes it again.
pub fn with_clipboard<T>(hwnd: HWND, act: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut tries = 0;
    // SAFETY: the window is this thread's, and the clipboard is closed below.
    while let Err(error) = unsafe { OpenClipboard(Some(hwnd)) } {
        tries += 1;
        if tries == OPEN_TRIES {
            return Err(Error::unexpected(format!(
                "another program is using the clipboard: {}",
                Error::from(error).message
            )));
        }
        std::thread::sleep(OPEN_RETRY);
    }
    let result = act();
    // SAFETY: this thread opened the clipboard above.
    let closed = unsafe { CloseClipboard() };
    let value = result?;
    closed?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Gdi::BITMAPV5HEADER;

    use super::*;
    use crate::testutil::{Pixels, Rgba, TempDir, VIEWER_TIMEOUT, read_image};

    /// A 3 × 2 capture whose pixels are numbered 0 to 5 in their blue
    /// channel, row by row.
    fn numbered() -> Capture {
        Capture {
            size: PixelSize::new(3, 2),
            pixels: (0..6).flat_map(|n| [n, 0, 0, 255]).collect(),
        }
    }

    fn order(capture: &Capture) -> Vec<u8> {
        capture
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[0])
            .collect()
    }

    #[test]
    fn captures_turn_clockwise() {
        // 0 1 2
        // 3 4 5
        let turned = rotate(numbered(), Rotation::None);
        assert_eq!(turned, numbered());
        let turned = rotate(numbered(), Rotation::Clockwise90);
        assert_eq!(turned.size, PixelSize::new(2, 3));
        assert_eq!(order(&turned), [3, 0, 4, 1, 5, 2]);
        let turned = rotate(numbered(), Rotation::Half);
        assert_eq!(turned.size, PixelSize::new(3, 2));
        assert_eq!(order(&turned), [5, 4, 3, 2, 1, 0]);
        let turned = rotate(numbered(), Rotation::Clockwise270);
        assert_eq!(turned.size, PixelSize::new(2, 3));
        assert_eq!(order(&turned), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn regions_scale_to_a_smaller_bitmap() {
        let region = PixelRect::new(10, 20, 30, 40);
        let size = PixelSize::new(100, 100);
        assert_eq!(scale_region(region, size, size), region);
        assert_eq!(
            scale_region(region, size, PixelSize::new(50, 50)),
            PixelRect::new(5, 10, 15, 20)
        );
        // Partly covered bitmap pixels are taken whole.
        assert_eq!(
            scale_region(PixelRect::new(1, 1, 3, 3), size, PixelSize::new(50, 50)),
            PixelRect::new(0, 0, 2, 2)
        );
        // A region smaller than a bitmap pixel takes that pixel.
        assert_eq!(
            scale_region(PixelRect::new(99, 99, 1, 1), size, PixelSize::new(10, 10)),
            PixelRect::new(9, 9, 1, 1)
        );
    }

    #[test]
    fn dibs_have_a_v5_header_and_rows_bottom_first() {
        let dib = dibv5(&numbered());
        assert_eq!(DIBV5_HEADER, size_of::<BITMAPV5HEADER>());
        assert_eq!(dib.len(), DIBV5_HEADER + 6 * 4);
        let u32_at =
            |offset: usize| u32::from_le_bytes(dib[offset..offset + 4].try_into().unwrap());
        assert_eq!(u32_at(0), 124);
        assert_eq!(u32_at(4), 3);
        assert_eq!(u32_at(8), 2);
        assert_eq!(&dib[12..16], &[1, 0, 32, 0]);
        assert_eq!(u32_at(16), 3);
        assert_eq!(u32_at(20), 24);
        assert_eq!(u32_at(52), 0xFF00_0000);
        assert_eq!(u32_at(56), 0x7352_4742);
        assert_eq!(u32_at(108), 4);
        let rows: Vec<u8> = dib[DIBV5_HEADER..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[0])
            .collect();
        assert_eq!(rows, [3, 4, 5, 0, 1, 2]);
        let empty = Capture {
            size: PixelSize::new(0, 0),
            pixels: Vec::new(),
        };
        assert_eq!(dibv5(&empty).len(), DIBV5_HEADER);
    }

    /// Waits for the copier's next result.
    fn next(copier: &Copier) -> Result<Ready> {
        let deadline = std::time::Instant::now() + VIEWER_TIMEOUT;
        loop {
            if let Some(ready) = copier.try_recv() {
                return ready;
            }
            assert!(std::time::Instant::now() < deadline, "no copy came");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_copier_prepares_copies_in_order_on_its_thread() {
        let dir = TempDir::new().unwrap();
        let pixels = Pixels::quadrants(4, 2, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
        let capture = Capture {
            size: PixelSize::new(4, 2),
            pixels: pixels.data.clone(),
        };
        let notified = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut copier = {
            let notified = Arc::clone(&notified);
            Copier::new(Clipboard::Sink(dir.path().to_owned()), move || {
                notified.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };
        assert!(copier.try_recv().is_none());

        // A sink gets the PNG and the bitmap.
        copier.submit(capture.clone(), Rotation::None);
        let ready = next(&copier).unwrap();
        assert!(ready.contents.is_none());
        assert_eq!(ready.finish(HWND::default()).unwrap(), PixelSize::new(4, 2));
        assert_eq!(read_image(&dir.join(SINK_PNG)).unwrap(), pixels);
        assert_eq!(std::fs::read(dir.join(SINK_DIB)).unwrap(), dibv5(&capture));

        // The pixels arrive premultiplied, and are turned.
        let half = Capture {
            size: PixelSize::new(2, 1),
            pixels: vec![0, 0, 128, 128, 255, 0, 0, 255],
        };
        copier.submit(half, Rotation::Clockwise90);
        assert_eq!(next(&copier).unwrap().size, PixelSize::new(1, 2));
        let copied = read_image(&dir.join(SINK_PNG)).unwrap();
        assert_eq!(
            copied.get(0, 0),
            Rgba {
                a: 128,
                ..Rgba::RED
            }
        );
        assert_eq!(copied.get(0, 1), Rgba::BLUE);

        // Copies for the clipboard are left in memory for the UI thread;
        // dropped unused, the memory is freed.
        copier.set_clipboard(Clipboard::System);
        copier.submit(capture.clone(), Rotation::Half);
        let ready = next(&copier).unwrap();
        assert!(ready.contents.is_some());
        drop(ready);

        copier.set_clipboard(Clipboard::Unavailable);
        copier.submit(capture.clone(), Rotation::None);
        let error = next(&copier).err().unwrap();
        assert!(error.message.contains("SEER_CLIPBOARD_DIR"));
        copier.set_clipboard(Clipboard::Sink(dir.join("no-such-dir")));
        copier.submit(capture, Rotation::None);
        assert!(next(&copier).is_err());
        drop(copier);
        assert_eq!(notified.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[test]
    fn destinations_follow_the_headless_flag() {
        assert_eq!(Clipboard::for_viewer(false), Clipboard::System);
        assert_ne!(Clipboard::for_viewer(true), Clipboard::System);
    }
}
