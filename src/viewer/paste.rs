//! Pasting: opening what is on the clipboard.
//!
//! What is pasted is always a copy, in the viewer's pasted folder, so that it
//! has a path like any other image, for `seer list` and `seer marks` to report
//! and for an agent to read, and so that nothing done to it reaches the
//! original. Files on the clipboard (`CF_HDROP`, as Explorer's Copy leaves
//! them) are copied there under their own names, the images in a folder
//! among them included; files Seer does not read are left out. Otherwise the
//! image on the clipboard is saved there as a PNG file named for the local
//! time, such as `pasted-2026-11-21-153012.png`. It is taken from the
//! registered format `PNG` as it is, or failing that from a bitmap,
//! `CF_DIBV5` and then `CF_DIB`, which Windows makes from the plain bitmap
//! that Print Screen leaves. A name already taken gets `-2`, `-3` and so on.
//!
//! Pasted files are temporary. The model asks for each to be deleted when its
//! image is closed; the viewer deletes its pasted folder when it exits; and a
//! viewer starting deletes its own leftovers and those of any instance no
//! viewer is running, after a crash. Each instance has a folder of its own,
//! `%LOCALAPPDATA%\Seer\pasted\<instance>`, so a viewer never deletes
//! another's pastes.
//!
//! The UI thread reads the clipboard, which only a thread with a window can
//! open, and copies its contents out. A [`Paster`] does the rest on a thread
//! of its own: converting a bitmap to PNG, which takes a moment for a large
//! screenshot, and writing or copying the files.
//!
//! A headless viewer never reads the clipboard. Given `SEER_CLIPBOARD_DIR`,
//! it reads that directory instead, the same one copies are written to:
//! [`SINK_FILES`], a list of paths one per line, else [`SINK_PNG`], else
//! [`SINK_DIB`]. Its pasted folders are in a `pasted` folder inside it.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, SYSTEMTIME};
use windows::Win32::System::DataExchange::{GetClipboardData, IsClipboardFormatAvailable};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use super::clipboard::{Background, Clipboard, SINK_DIB, SINK_PNG, png_format, with_clipboard};
use crate::error::{Error, ErrorCode, Result};
use crate::ipc::instance::InstanceName;
use crate::model::app::Paste;
use crate::model::geometry::PixelSize;
use crate::model::set::{DiskFiles, Files, is_image_name};
use crate::protocol::OpenFailure;
use crate::render::offscreen::{Capture, encode_png};

/// The file in a headless viewer's clipboard directory that lists the files
/// on its clipboard, one absolute path per line.
pub const SINK_FILES: &str = "clipboard.files";

/// The folder that holds a headless viewer's pasted folders, inside its
/// clipboard directory.
pub const SINK_PASTED: &str = "pasted";

/// The signature every PNG file starts with.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];

/// The largest bitmap accepted from the clipboard, in pixels: more than any
/// screen, and few enough that its pixels fit in memory.
const MAX_PIXELS: u64 = 1 << 28;

/// What is on the clipboard, copied out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clip {
    /// Files, by absolute path.
    Files(Vec<PathBuf>),
    /// A PNG file's bytes.
    Png(Vec<u8>),
    /// A packed device-independent bitmap: a `BITMAPINFOHEADER` or a later
    /// version of it, any colour masks, then the rows.
    Dib(Vec<u8>),
}

/// The error for a clipboard with nothing Seer can open.
fn nothing() -> Error {
    Error::new(ErrorCode::NoImages, "the clipboard holds no image")
}

/// Reads `clipboard`, opening the system's for `hwnd`, a window of this
/// thread.
pub fn read(clipboard: &Clipboard, hwnd: HWND) -> Result<Clip> {
    match clipboard {
        Clipboard::System => read_system(hwnd),
        Clipboard::Sink(directory) => read_sink(directory),
        Clipboard::Unavailable => Err(Clipboard::unavailable()),
    }
}

/// Reads a headless viewer's clipboard directory.
fn read_sink(directory: &Path) -> Result<Clip> {
    if let Ok(list) = std::fs::read_to_string(directory.join(SINK_FILES)) {
        let files: Vec<PathBuf> = list
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
        if !files.is_empty() {
            return Ok(Clip::Files(files));
        }
    }
    if let Ok(png) = std::fs::read(directory.join(SINK_PNG)) {
        return Ok(Clip::Png(png));
    }
    if let Ok(dib) = std::fs::read(directory.join(SINK_DIB)) {
        return Ok(Clip::Dib(dib));
    }
    Err(nothing())
}

/// Reads the system's clipboard: files, else a PNG, else a bitmap.
fn read_system(hwnd: HWND) -> Result<Clip> {
    let png = png_format()?;
    with_clipboard(hwnd, || {
        let available = |format: u32| {
            // SAFETY: IsClipboardFormatAvailable has no pointer arguments.
            unsafe { IsClipboardFormatAvailable(format) }.is_ok()
        };
        if available(u32::from(CF_HDROP.0)) {
            let files = dropped_files(u32::from(CF_HDROP.0))?;
            if !files.is_empty() {
                return Ok(Clip::Files(files));
            }
        }
        if available(png) {
            return Ok(Clip::Png(clipboard_bytes(png)?));
        }
        for format in [CF_DIBV5, CF_DIB] {
            let format = u32::from(format.0);
            if available(format) {
                return Ok(Clip::Dib(clipboard_bytes(format)?));
            }
        }
        Err(nothing())
    })
}

/// The bytes the open clipboard holds as `format`.
fn clipboard_bytes(format: u32) -> Result<Vec<u8>> {
    // SAFETY: this thread has the clipboard open; the handle stays the
    // clipboard's and is used only until it is closed.
    let handle = unsafe { GetClipboardData(format) }?;
    let global = HGLOBAL(handle.0);
    // SAFETY: the clipboard keeps its data in global memory.
    let size = unsafe { GlobalSize(global) };
    // SAFETY: as above; the lock is released below.
    let locked = unsafe { GlobalLock(global) };
    if locked.is_null() {
        return Err(Error::unexpected("could not read the clipboard's memory"));
    }
    // SAFETY: the locked block holds `size` bytes, read before it is unlocked.
    let bytes = unsafe { std::slice::from_raw_parts(locked.cast::<u8>(), size) }.to_vec();
    // SAFETY: the block was locked above. GlobalUnlock reports an error when
    // the lock count reaches zero, which is the expected outcome.
    let _ = unsafe { GlobalUnlock(global) };
    Ok(bytes)
}

/// The files the open clipboard holds as `CF_HDROP`, `format`.
fn dropped_files(format: u32) -> Result<Vec<PathBuf>> {
    // SAFETY: this thread has the clipboard open, and the handle is used only
    // until it is closed.
    let handle: HANDLE = unsafe { GetClipboardData(format) }?;
    let list = HDROP(handle.0);
    // SAFETY: an index of all ones asks for the count; no buffer is given.
    let count = unsafe { DragQueryFileW(list, u32::MAX, None) };
    let mut files = Vec::new();
    for index in 0..count {
        // SAFETY: no buffer is given, so this returns the name's length.
        let length = unsafe { DragQueryFileW(list, index, None) } as usize;
        let mut name = vec![0u16; length + 1];
        // SAFETY: the buffer, a live local, has room for the name and its
        // terminator, and its length is passed with it.
        let written = unsafe { DragQueryFileW(list, index, Some(&mut name)) } as usize;
        files.push(PathBuf::from(OsString::from_wide(
            &name[..written.min(length)],
        )));
    }
    Ok(files)
}

/// The folder that holds every instance's pasted folder for a viewer with
/// `clipboard`.
pub fn root(clipboard: &Clipboard) -> Result<PathBuf> {
    match clipboard {
        Clipboard::System => {
            let local = std::env::var_os("LOCALAPPDATA")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Error::unexpected("LOCALAPPDATA is not set"))?;
            Ok(Path::new(&local).join("Seer").join("pasted"))
        }
        Clipboard::Sink(directory) => Ok(directory.join(SINK_PASTED)),
        Clipboard::Unavailable => Err(Clipboard::unavailable()),
    }
}

/// Deletes, in `root`, the pasted folder of `own`, the instance this viewer
/// owns, and those of instances `running` says no viewer runs, left behind by
/// viewers that did not exit cleanly. Anything it cannot delete is left.
pub fn clean_up(root: &Path, own: &str, running: impl Fn(&InstanceName) -> bool) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(instance) = name.to_str().and_then(|name| InstanceName::new(name).ok()) else {
            continue;
        };
        if instance.as_str() == own || !running(&instance) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// What is on the clipboard, for the [`Paster`] to save.
pub struct Job {
    /// The paste.
    pub paste: Paste,
    /// What to save.
    pub clip: Clip,
    /// The viewer's pasted folder, made if it does not exist.
    pub folder: PathBuf,
}

/// The files a paste saved, for the model to open, and the files on the
/// clipboard that could not be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The copies, in order.
    pub files: Vec<PathBuf>,
    /// The originals that were not copied, and why.
    pub failed: Vec<OpenFailure>,
}

/// Saves what is pasted on a thread of its own, one paste at a time in the
/// order given, and calls its `notify` after each is saved.
pub struct Paster {
    thread: Background<Job, (Paste, Result<Saved>)>,
}

impl Paster {
    /// A paster calling `notify` after each paste is saved.
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Paster {
            thread: Background::new("paste", save, notify),
        }
    }

    /// Saves what `job` holds.
    pub fn submit(&mut self, job: Job) {
        self.thread.submit(job);
    }

    /// The next paste done, with the files saved or the reason none were.
    pub fn try_recv(&self) -> Option<(Paste, Result<Saved>)> {
        self.thread.try_recv()
    }
}

/// Saves a job's image as a PNG in its folder, or copies its files there.
fn save(job: Job) -> (Paste, Result<Saved>) {
    let saved = (|| {
        std::fs::create_dir_all(&job.folder)?;
        let png = match job.clip {
            Clip::Png(png) if png.starts_with(&PNG_SIGNATURE) => png,
            Clip::Png(_) => return Err(Error::unexpected("the clipboard's PNG is not a PNG")),
            Clip::Dib(dib) => encode_png(&from_dib(&dib)?)?,
            Clip::Files(files) => return copy_files(&files, &job.folder),
        };
        // SAFETY: GetLocalTime has no arguments.
        let now = unsafe { GetLocalTime() };
        let path = create_new(&job.folder, &stem(&now), "png", |file| file.write_all(&png))?;
        Ok(Saved {
            files: vec![path],
            failed: Vec::new(),
        })
    })();
    (job.paste, saved)
}

/// Copies `files` to `folder` under their own names, each folder among them
/// standing for the images directly in it. A file Seer does not read is not
/// copied but reported, as is one that cannot be.
fn copy_files(files: &[PathBuf], folder: &Path) -> Result<Saved> {
    let mut saved = Saved {
        files: Vec::new(),
        failed: Vec::new(),
    };
    for file in files {
        let images = match DiskFiles.folder_images(file) {
            Some(Ok(images)) => images,
            Some(Err(error)) => {
                saved.failed.push(OpenFailure {
                    path: file.clone(),
                    error,
                });
                continue;
            }
            None => vec![file.clone()],
        };
        for image in images {
            match copy_file(&image, folder) {
                Ok(copy) => saved.files.push(copy),
                Err(error) => saved.failed.push(OpenFailure { path: image, error }),
            }
        }
    }
    if saved.files.is_empty() && saved.failed.is_empty() {
        return Err(nothing());
    }
    Ok(saved)
}

/// Copies an image file to `folder`, under its own name if that is free.
fn copy_file(image: &Path, folder: &Path) -> Result<PathBuf> {
    if !is_image_name(image) {
        return Err(Error::new(
            ErrorCode::UnsupportedFormat,
            format!("{} is not an image Seer reads", image.display()),
        ));
    }
    let mut source = std::fs::File::open(image).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => Error::new(
            ErrorCode::FileNotFound,
            format!("{} does not exist", image.display()),
        ),
        _ => Error::new(
            ErrorCode::OpenFailed,
            format!("cannot open {}: {error}", image.display()),
        ),
    })?;
    let stem = image.file_stem().unwrap_or_default().to_string_lossy();
    let extension = image.extension().unwrap_or_default().to_string_lossy();
    create_new(folder, &stem, &extension, |file| {
        std::io::copy(&mut source, file).map(|_| ())
    })
}

/// The name, without extension, of a file pasted at local time `time`.
pub fn stem(time: &SYSTEMTIME) -> String {
    format!(
        "pasted-{:04}-{:02}-{:02}-{:02}{:02}{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

/// Makes a new file in `folder` named `stem.extension`, or if that is taken
/// `stem-2.extension` and so on, fills it with `fill`, and returns its path.
/// A file `fill` fails on is deleted.
fn create_new(
    folder: &Path,
    stem: &str,
    extension: &str,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<PathBuf> {
    for n in 1..=1000 {
        let name = match n {
            1 => format!("{stem}.{extension}"),
            _ => format!("{stem}-{n}.{extension}"),
        };
        let path = folder.join(name);
        let mut file = match std::fs::File::create_new(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = fill(&mut file) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        return Ok(path);
    }
    Err(Error::unexpected(format!(
        "{} has too many files named {stem}",
        folder.display()
    )))
}

/// The pixels of a packed device-independent bitmap with 24 or 32 bits per
/// pixel, uncompressed or with colour masks, as straight BGRA.
///
/// A 32-bit bitmap's fourth byte, or what its alpha mask selects, is its
/// alpha, except that a bitmap whose alpha is zero everywhere is opaque: that
/// is how bitmaps that have no alpha, such as screenshots, usually fill the
/// byte.
pub fn from_dib(bytes: &[u8]) -> Result<Capture> {
    let unreadable = |why: &str| {
        Error::new(
            ErrorCode::UnsupportedFormat,
            format!("the bitmap on the clipboard {why}"),
        )
    };
    let u32_at = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let short = || unreadable("is cut short");
    let header = u32_at(0).ok_or_else(short)? as usize;
    if header < 40 || bytes.len() < header {
        return Err(short());
    }
    let width = u32_at(4).ok_or_else(short)? as i32;
    let height = u32_at(8).ok_or_else(short)? as i32;
    let bits = u16::from_le_bytes([bytes[14], bytes[15]]);
    let compression = u32_at(16).ok_or_else(short)?;
    let colours_used = u32_at(32).ok_or_else(short)? as usize;
    if width <= 0 || height == 0 {
        return Err(unreadable("is empty"));
    }
    let (width, top_down, height) = (width.unsigned_abs(), height < 0, height.unsigned_abs());
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(unreadable("is too large"));
    }

    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    const BI_ALPHABITFIELDS: u32 = 6;
    // The red, green, blue and alpha masks, and where the colour table, then
    // the rows, start.
    let (masks, mut rows) = match compression {
        BI_RGB => ([0xFF_0000, 0xFF00, 0xFF, 0xFF00_0000], header),
        BI_BITFIELDS | BI_ALPHABITFIELDS => {
            // A BITMAPINFOHEADER is followed by the masks; later headers hold
            // them, alpha included.
            let (at, count, after) = if header == 40 {
                let count = if compression == BI_ALPHABITFIELDS {
                    4
                } else {
                    3
                };
                (header, count, header + count * 4)
            } else {
                (40, if header >= 56 { 4 } else { 3 }, header)
            };
            let mut masks = [0u32; 4];
            for (index, mask) in masks.iter_mut().enumerate().take(count) {
                *mask = u32_at(at + index * 4).ok_or_else(short)?;
            }
            (masks, after)
        }
        _ => return Err(unreadable("is compressed")),
    };
    if bits != 24 && bits != 32 {
        return Err(unreadable(&format!("has {bits} bits per pixel")));
    }
    if bits == 24 && compression != BI_RGB {
        return Err(unreadable("has colour masks with 24 bits per pixel"));
    }
    rows += colours_used * 4;
    let stride = (width as usize * usize::from(bits)).div_ceil(32) * 4;
    let end = stride
        .checked_mul(height as usize)
        .and_then(|size| size.checked_add(rows))
        .ok_or_else(short)?;
    if bytes.len() < end {
        return Err(short());
    }

    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let source = if top_down { y } else { height as usize - 1 - y };
        let row = &bytes[rows + source * stride..][..stride];
        if bits == 24 {
            for pixel in row.as_chunks::<3>().0.iter().take(width as usize) {
                pixels.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        } else {
            for &pixel in row.as_chunks::<4>().0.iter().take(width as usize) {
                let value = u32::from_le_bytes(pixel);
                let [red, green, blue, alpha] = masks.map(|mask| channel(value, mask));
                pixels.extend_from_slice(&[blue, green, red, alpha]);
            }
        }
    }
    if bits == 32 && pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    Ok(Capture {
        size: PixelSize::new(width, height),
        pixels,
    })
}

/// The bits of `value` that `mask` selects, scaled to 0 to 255. With no
/// mask, 255.
fn channel(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 255;
    }
    let shift = mask.trailing_zeros();
    let max = u64::from(mask >> shift);
    let bits = u64::from((value & mask) >> shift);
    // Rounded to the nearest; at most 255, so the cast is exact.
    ((bits * 255 + max / 2) / max) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::offscreen::encode_png;
    use crate::testutil::{Pixels, Rgba, TempDir, VIEWER_TIMEOUT, read_image};
    use crate::viewer::clipboard::dibv5;

    /// A packed DIB: a header of `header` bytes (40, 108 or 124) with these
    /// fields, the masks after a 40-byte header when `trailing_masks`, then
    /// `rows` as given.
    fn dib(
        header: u32,
        width: i32,
        height: i32,
        bits: u16,
        compression: u32,
        masks: &[u32],
        rows: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(&compression.to_le_bytes());
        bytes.resize(40, 0);
        for mask in masks {
            bytes.extend_from_slice(&mask.to_le_bytes());
        }
        bytes.resize(bytes.len().max(header as usize), 0);
        bytes.extend_from_slice(rows);
        bytes
    }

    fn colours(capture: &Capture) -> Vec<[u8; 4]> {
        capture.pixels.as_chunks::<4>().0.to_vec()
    }

    #[test]
    fn bitmaps_of_24_bits_are_read_bottom_up_with_padded_rows() {
        // 2 × 2: each row is 6 bytes of pixels and 2 of padding. The bottom
        // row comes first: blue then green; the top is red then white.
        let rows = [
            255, 0, 0, 0, 255, 0, 0, 0, //
            0, 0, 255, 255, 255, 255, 0, 0,
        ];
        let capture = from_dib(&dib(40, 2, 2, 24, 0, &[], &rows)).unwrap();
        assert_eq!(capture.size, PixelSize::new(2, 2));
        assert_eq!(
            colours(&capture),
            [
                [0, 0, 255, 255],
                [255, 255, 255, 255],
                [255, 0, 0, 255],
                [0, 255, 0, 255]
            ]
        );
        // Negative heights run top down.
        let capture = from_dib(&dib(40, 2, -2, 24, 0, &[], &rows)).unwrap();
        assert_eq!(colours(&capture)[0], [255, 0, 0, 255]);
    }

    #[test]
    fn bitmaps_of_32_bits_keep_their_alpha_unless_it_is_all_zero() {
        let translucent = [10, 20, 30, 128, 40, 50, 60, 255];
        let capture = from_dib(&dib(40, 2, 1, 32, 0, &[], &translucent)).unwrap();
        assert_eq!(colours(&capture), [[10, 20, 30, 128], [40, 50, 60, 255]]);
        let screenshot = [10, 20, 30, 0, 40, 50, 60, 0];
        let capture = from_dib(&dib(40, 2, 1, 32, 0, &[], &screenshot)).unwrap();
        assert_eq!(colours(&capture), [[10, 20, 30, 255], [40, 50, 60, 255]]);
    }

    #[test]
    fn colour_masks_are_read_from_after_or_inside_the_header() {
        // Red in the low byte and blue in the third: the reverse of usual.
        let swapped = [0xFF, 0xFF_00, 0xFF_0000];
        let pixel = [1, 2, 3, 0];
        let after = from_dib(&dib(40, 1, 1, 32, 3, &swapped, &pixel)).unwrap();
        assert_eq!(colours(&after), [[3, 2, 1, 255]]);
        let inside = from_dib(&dib(108, 1, 1, 32, 3, &swapped, &pixel)).unwrap();
        assert_eq!(colours(&inside), [[3, 2, 1, 255]]);
        // A 5-bit channel is scaled to 8 bits.
        let five = from_dib(&dib(
            40,
            1,
            1,
            32,
            3,
            &[0x7C00, 0x3E0, 0x1F],
            &[0x1F, 0, 0, 0],
        ))
        .unwrap();
        assert_eq!(colours(&five), [[255, 0, 0, 255]]);
        // With an alpha mask, from BI_ALPHABITFIELDS after a short header.
        let masks = [0xFF_0000, 0xFF00, 0xFF, 0xFF00_0000];
        let alpha = from_dib(&dib(40, 1, 1, 32, 6, &masks, &[1, 2, 3, 4])).unwrap();
        assert_eq!(colours(&alpha), [[1, 2, 3, 4]]);
    }

    #[test]
    fn seers_own_copies_paste_back() {
        let pixels = Pixels::quadrants(4, 2, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
        let mut capture = Capture {
            size: PixelSize::new(4, 2),
            pixels: pixels.data.clone(),
        };
        capture.pixels[3] = 128;
        assert_eq!(from_dib(&dibv5(&capture)).unwrap(), capture);
    }

    #[test]
    fn unreadable_bitmaps_are_refused() {
        let refused = |bytes: &[u8]| from_dib(bytes).unwrap_err().code;
        assert_eq!(refused(&[1, 2, 3]), ErrorCode::UnsupportedFormat);
        assert_eq!(
            refused(&dib(40, 1, 1, 8, 0, &[], &[0, 0, 0, 0])),
            ErrorCode::UnsupportedFormat
        );
        assert!(from_dib(&dib(40, 1, 1, 32, 1, &[], &[0; 4])).is_err());
        assert!(from_dib(&dib(40, 0, 1, 32, 0, &[], &[])).is_err());
        assert!(from_dib(&dib(40, 2, 2, 32, 0, &[], &[0; 12])).is_err());
        assert!(from_dib(&dib(40, 1 << 20, 1 << 20, 32, 0, &[], &[])).is_err());
        assert!(from_dib(&dib(40, 1, 1, 24, 3, &[0, 0, 0], &[0; 4])).is_err());
        assert!(from_dib(&dib(40, 1, 1, 32, 3, &[0, 0], &[])).is_err());
    }

    #[test]
    fn sinks_are_read_files_first_then_png_then_bitmap() {
        let dir = TempDir::new().unwrap();
        let sink = Clipboard::Sink(dir.path().to_owned());
        let hwnd = HWND::default();
        assert_eq!(read(&sink, hwnd).unwrap_err().code, ErrorCode::NoImages);
        std::fs::write(dir.join(SINK_DIB), [1]).unwrap();
        assert_eq!(read(&sink, hwnd).unwrap(), Clip::Dib(vec![1]));
        std::fs::write(dir.join(SINK_PNG), [2]).unwrap();
        assert_eq!(read(&sink, hwnd).unwrap(), Clip::Png(vec![2]));
        std::fs::write(dir.join(SINK_FILES), "\n").unwrap();
        assert_eq!(read(&sink, hwnd).unwrap(), Clip::Png(vec![2]));
        std::fs::write(dir.join(SINK_FILES), "C:\\a.png\r\n C:\\b c.png \n").unwrap();
        assert_eq!(
            read(&sink, hwnd).unwrap(),
            Clip::Files(vec!["C:\\a.png".into(), "C:\\b c.png".into()])
        );
        let unavailable = read(&Clipboard::Unavailable, hwnd).unwrap_err();
        assert!(unavailable.message.contains("SEER_CLIPBOARD_DIR"));
    }

    #[test]
    fn roots_follow_the_clipboard() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            root(&Clipboard::Sink(dir.path().to_owned())).unwrap(),
            dir.join("pasted")
        );
        assert!(root(&Clipboard::System).unwrap().ends_with(r"Seer\pasted"));
        assert!(root(&Clipboard::Unavailable).is_err());
    }

    #[test]
    fn cleaning_up_leaves_only_running_instances_pastes() {
        let dir = TempDir::new().unwrap();
        for instance in ["mine", "running", "crashed"] {
            std::fs::create_dir(dir.join(instance)).unwrap();
            std::fs::write(dir.join(instance).join("a.png"), b"").unwrap();
        }
        std::fs::create_dir(dir.join("not an instance")).unwrap();
        clean_up(dir.path(), "mine", |instance| {
            matches!(instance.as_str(), "mine" | "running")
        });
        assert!(!dir.join("mine").exists());
        assert!(!dir.join("crashed").exists());
        assert!(dir.join("running").join("a.png").exists());
        assert!(dir.join("not an instance").exists());
        clean_up(&dir.join("no-such-dir"), "mine", |_| false);
    }

    #[test]
    fn names_follow_the_local_time_and_never_collide() {
        let time = SYSTEMTIME {
            wYear: 2026,
            wMonth: 3,
            wDay: 7,
            wHour: 9,
            wMinute: 5,
            wSecond: 1,
            ..Default::default()
        };
        assert_eq!(stem(&time), "pasted-2026-03-07-090501");
        let dir = TempDir::new().unwrap();
        let write = |bytes: &'static [u8]| {
            create_new(dir.path(), "pasted-x", "png", |file| file.write_all(bytes))
        };
        let first = write(b"one").unwrap();
        let second = write(b"two").unwrap();
        assert_eq!(first, dir.join("pasted-x.png"));
        assert_eq!(second, dir.join("pasted-x-2.png"));
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert!(create_new(&dir.join("no-such-dir"), "x", "png", |_| Ok(())).is_err());
        // A file that cannot be filled is not left behind.
        let failed = create_new(dir.path(), "broken", "png", |_| {
            Err(std::io::Error::other("full"))
        });
        assert!(failed.is_err());
        assert!(!dir.join("broken.png").exists());
    }

    /// Waits for the paster's next result.
    fn next(paster: &Paster) -> (Paste, Result<Saved>) {
        let deadline = std::time::Instant::now() + VIEWER_TIMEOUT;
        loop {
            if let Some(done) = paster.try_recv() {
                return done;
            }
            assert!(std::time::Instant::now() < deadline, "no paste came");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn the_paster_saves_pngs_and_bitmaps_as_png_files() {
        let dir = TempDir::new().unwrap();
        let folder = dir.join("pasted");
        let paste = Paste {
            reply: None,
            group: None,
            caption: None,
            activate: true,
        };
        let job = |clip| Job {
            paste: paste.clone(),
            clip,
            folder: folder.clone(),
        };
        let mut paster = Paster::new(|| {});
        let pixels = Pixels::quadrants(4, 2, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
        let capture = Capture {
            size: PixelSize::new(4, 2),
            pixels: pixels.data.clone(),
        };
        let png = encode_png(&capture).unwrap();

        paster.submit(job(Clip::Png(png.clone())));
        let (done, saved) = next(&paster);
        assert_eq!(done, paste);
        let saved = saved.unwrap();
        assert!(saved.failed.is_empty());
        let [saved] = &saved.files[..] else {
            panic!("{saved:?}")
        };
        assert!(saved.starts_with(&folder));
        assert!(
            saved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pasted-")
        );
        assert_eq!(std::fs::read(saved).unwrap(), png);

        paster.submit(job(Clip::Dib(dibv5(&capture))));
        let saved = next(&paster).1.unwrap();
        assert_eq!(read_image(&saved.files[0]).unwrap(), pixels);

        paster.submit(job(Clip::Png(vec![1, 2, 3])));
        assert!(next(&paster).1.is_err());
        paster.submit(job(Clip::Dib(vec![1, 2, 3])));
        assert!(next(&paster).1.is_err());
        paster.submit(job(Clip::Files(Vec::new())));
        assert_eq!(next(&paster).1.unwrap_err().code, ErrorCode::NoImages);
    }

    #[test]
    fn the_paster_copies_files_under_their_own_names() {
        let dir = TempDir::new().unwrap();
        let originals = dir.join("originals");
        let shots = originals.join("shots");
        std::fs::create_dir_all(&shots).unwrap();
        for (path, bytes) in [
            (originals.join("a.png"), &b"a"[..]),
            (originals.join("notes.txt"), b"n"),
            (shots.join("shot2.png"), b"2"),
            (shots.join("shot10.png"), b"10"),
            (shots.join("a.png"), b"shots a"),
        ] {
            std::fs::write(path, bytes).unwrap();
        }
        let folder = dir.join("pasted");
        let mut paster = Paster::new(|| {});
        let paste = Paste {
            reply: None,
            group: None,
            caption: None,
            activate: true,
        };
        paster.submit(Job {
            paste,
            clip: Clip::Files(vec![
                originals.join("a.png"),
                originals.join("notes.txt"),
                shots.clone(),
                originals.join("missing.png"),
            ]),
            folder: folder.clone(),
        });
        let saved = next(&paster).1.unwrap();
        // The folder's images come in Explorer's order, and a second a.png
        // gets a name of its own.
        let names: Vec<_> = saved
            .files
            .iter()
            .map(|file| file.strip_prefix(&folder).unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            ["a.png", "a-2.png", "shot2.png", "shot10.png"].map(PathBuf::from)
        );
        assert_eq!(std::fs::read(folder.join("a-2.png")).unwrap(), b"shots a");
        let failed: Vec<_> = saved
            .failed
            .iter()
            .map(|failure| (failure.path.clone(), failure.error.code))
            .collect();
        assert_eq!(
            failed,
            [
                (originals.join("notes.txt"), ErrorCode::UnsupportedFormat),
                (originals.join("missing.png"), ErrorCode::FileNotFound),
            ]
        );
        assert_eq!(std::fs::read(originals.join("a.png")).unwrap(), b"a");
    }
}
