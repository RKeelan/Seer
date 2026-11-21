//! Helpers for tests: image files generated through WIC's encoders, images
//! read back through its decoders, pixel comparison, temporary directories,
//! unique instance names, shell item arrays such as Explorer hands over, and
//! [`HeadlessViewer`], which runs a viewer for end-to-end tests.
//!
//! Test images are generated, never checked in. Unit tests get this module
//! through `cfg(test)`; integration tests get it through the `testutil`
//! feature, which the crate's dev-dependency on itself turns on.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use windows::Win32::Foundation::{
    E_FAIL, GENERIC_READ, GENERIC_WRITE, VARIANT_FALSE, VARIANT_TRUE,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatBmp, GUID_ContainerFormatGif,
    GUID_ContainerFormatHeif, GUID_ContainerFormatJpeg, GUID_ContainerFormatPng,
    GUID_ContainerFormatTiff, GUID_WICPixelFormat8bppIndexed, GUID_WICPixelFormat24bppBGR,
    GUID_WICPixelFormat32bppBGR, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory, IWICPalette,
    WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{
    CAUB, IPropertyBag2, PROPVARIANT, PropVariantClear,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemAlloc,
    CoTaskMemFree,
};
use windows::Win32::System::Variant::{VT_BOOL, VT_UI1, VT_UI2, VT_VECTOR};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IShellItemArray, SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
};
use windows::core::{GUID, HSTRING, w};

use crate::error::{Error, Result};
use crate::ipc::client;
use crate::ipc::instance::InstanceName;
use crate::protocol::{ListReply, LoadState};

/// A colour with straight (not premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgba {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha: 0 is transparent, 255 opaque.
    pub a: u8,
}

impl Rgba {
    /// Opaque black.
    pub const BLACK: Rgba = Rgba::opaque(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Rgba = Rgba::opaque(255, 255, 255);
    /// Opaque red.
    pub const RED: Rgba = Rgba::opaque(255, 0, 0);
    /// Opaque green.
    pub const GREEN: Rgba = Rgba::opaque(0, 255, 0);
    /// Opaque blue.
    pub const BLUE: Rgba = Rgba::opaque(0, 0, 255);

    /// An opaque colour.
    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Rgba { r, g, b, a: 255 }
    }
}

/// An image in memory: rows top to bottom, each pixel four bytes in the
/// order blue, green, red, alpha, with straight alpha. This is WIC's
/// `32bppBGRA` format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width × height × 4` bytes.
    pub data: Vec<u8>,
}

impl Pixels {
    /// An image with each pixel's colour given by `colour(x, y)`.
    pub fn from_fn(width: u32, height: u32, colour: impl Fn(u32, u32) -> Rgba) -> Self {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let Rgba { r, g, b, a } = colour(x, y);
                data.extend([b, g, r, a]);
            }
        }
        Pixels {
            width,
            height,
            data,
        }
    }

    /// An image of one colour.
    pub fn solid(width: u32, height: u32, colour: Rgba) -> Self {
        Pixels::from_fn(width, height, |_, _| colour)
    }

    /// An image split into four flat quadrants, given top-left, top-right,
    /// bottom-left, bottom-right. Asymmetric colours make rotation and
    /// mirroring visible, and flat areas survive JPEG well away from the
    /// edges between them.
    pub fn quadrants(width: u32, height: u32, colours: [Rgba; 4]) -> Self {
        Pixels::from_fn(width, height, |x, y| {
            let right = usize::from(x >= width / 2);
            let bottom = usize::from(y >= height / 2);
            colours[bottom * 2 + right]
        })
    }

    /// The colour at `(x, y)`.
    pub fn get(&self, x: u32, y: u32) -> Rgba {
        let i = self.offset(x, y);
        let [b, g, r, a] = self.data[i..i + 4].try_into().expect("four bytes");
        Rgba { r, g, b, a }
    }

    /// Sets the colour at `(x, y)`.
    pub fn set(&mut self, x: u32, y: u32, colour: Rgba) {
        let i = self.offset(x, y);
        self.data[i..i + 4].copy_from_slice(&[colour.b, colour.g, colour.r, colour.a]);
    }

    fn offset(&self, x: u32, y: u32) -> usize {
        assert!(x < self.width && y < self.height, "({x}, {y}) is outside");
        (y as usize * self.width as usize + x as usize) * 4
    }
}

/// How far two images of the same size differ.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Difference {
    /// The largest difference in any channel of any pixel.
    pub max_channel: u8,
    /// How many pixels differ in any channel by more than the tolerance
    /// given to [`difference`].
    pub pixels: usize,
}

/// How far `actual` differs from `expected`, counting the pixels with a
/// channel more than `tolerance` away.
///
/// # Panics
///
/// If the images differ in size.
pub fn difference(expected: &Pixels, actual: &Pixels, tolerance: u8) -> Difference {
    assert_eq!(
        (expected.width, expected.height),
        (actual.width, actual.height),
        "the images differ in size"
    );
    let mut result = Difference::default();
    for (e, a) in expected
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.data.as_chunks::<4>().0)
    {
        let worst = e
            .iter()
            .zip(a)
            .map(|(e, a)| e.abs_diff(*a))
            .max()
            .unwrap_or(0);
        result.max_channel = result.max_channel.max(worst);
        if worst > tolerance {
            result.pixels += 1;
        }
    }
    result
}

/// Asserts that no channel of any pixel differs by more than `tolerance`.
///
/// # Panics
///
/// If the images differ in size or in any channel by more than `tolerance`.
#[track_caller]
pub fn assert_similar(expected: &Pixels, actual: &Pixels, tolerance: u8) {
    let difference = difference(expected, actual, tolerance);
    assert!(
        difference.pixels == 0,
        "{} of {} pixels differ by more than {tolerance}; the largest difference is {}",
        difference.pixels,
        expected.width as usize * expected.height as usize,
        difference.max_channel
    );
}

/// An image file format WIC can encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// PNG, lossless, with alpha.
    Png,
    /// JPEG, lossy, without alpha.
    Jpeg,
    /// BMP, lossless, without alpha.
    Bmp,
    /// TIFF, lossless, with alpha.
    Tiff,
    /// GIF, lossless for opaque images of 256 colours or fewer.
    Gif,
}

impl Format {
    /// Every format.
    pub const ALL: [Format; 5] = [
        Format::Png,
        Format::Jpeg,
        Format::Bmp,
        Format::Tiff,
        Format::Gif,
    ];

    /// The usual file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Bmp => "bmp",
            Format::Tiff => "tif",
            Format::Gif => "gif",
        }
    }

    /// The largest channel difference a round trip through the format can
    /// cause for images built from flat areas, such as
    /// [`Pixels::quadrants`], measured away from the edges between areas.
    pub fn tolerance(self) -> u8 {
        match self {
            Format::Jpeg => 8,
            _ => 0,
        }
    }

    fn container(self) -> GUID {
        match self {
            Format::Png => GUID_ContainerFormatPng,
            Format::Jpeg => GUID_ContainerFormatJpeg,
            Format::Bmp => GUID_ContainerFormatBmp,
            Format::Tiff => GUID_ContainerFormatTiff,
            Format::Gif => GUID_ContainerFormatGif,
        }
    }
}

/// Writes `pixels` to `path` in `format`. JPEG and BMP drop alpha. GIF
/// accepts only opaque images of 256 colours or fewer.
pub fn write_image(path: &Path, pixels: &Pixels, format: Format) -> Result<()> {
    let factory = factory()?;
    let width = pixels.width;
    match format {
        Format::Png | Format::Tiff => encode(
            &factory,
            path,
            format,
            Frame {
                width,
                height: pixels.height,
                pixel_format: GUID_WICPixelFormat32bppBGRA,
                stride: width * 4,
                bytes: &pixels.data,
                palette: None,
                orientation: None,
            },
        ),
        Format::Jpeg | Format::Bmp => {
            let bgr = without_alpha(pixels);
            encode(
                &factory,
                path,
                format,
                Frame {
                    width,
                    height: pixels.height,
                    pixel_format: GUID_WICPixelFormat24bppBGR,
                    stride: width * 3,
                    bytes: &bgr,
                    palette: None,
                    orientation: None,
                },
            )
        }
        Format::Gif => {
            let (colours, indices) = indexed(pixels)?;
            // SAFETY: CreatePalette has no arguments.
            let palette = unsafe { factory.CreatePalette() }?;
            // SAFETY: InitializeCustom copies the colours from the slice.
            unsafe { palette.InitializeCustom(&colours) }?;
            encode(
                &factory,
                path,
                format,
                Frame {
                    width,
                    height: pixels.height,
                    pixel_format: GUID_WICPixelFormat8bppIndexed,
                    stride: width,
                    bytes: &indices,
                    palette: Some(&palette),
                    orientation: None,
                },
            )
        }
    }
}

/// Writes `pixels` to `path` as a JPEG whose EXIF orientation tag is
/// `orientation` (1 to 8). The pixels are stored as given; a viewer that
/// honours the tag shows them transformed.
pub fn write_jpeg_with_orientation(path: &Path, pixels: &Pixels, orientation: u16) -> Result<()> {
    if !(1..=8).contains(&orientation) {
        return Err(Error::usage(format!(
            "EXIF orientation must be 1 to 8, not {orientation}"
        )));
    }
    let bgr = without_alpha(pixels);
    encode(
        &factory()?,
        path,
        Format::Jpeg,
        Frame {
            width: pixels.width,
            height: pixels.height,
            pixel_format: GUID_WICPixelFormat24bppBGR,
            stride: pixels.width * 3,
            bytes: &bgr,
            palette: None,
            orientation: Some(orientation),
        },
    )
}

/// Writes `pixels` to `path` as HEIF, which needs the HEIF and HEVC codec
/// extensions. Fails where they are not installed.
pub fn write_heif(path: &Path, pixels: &Pixels) -> Result<()> {
    encode_as(
        &factory()?,
        path,
        GUID_ContainerFormatHeif,
        "HEIF",
        Frame {
            width: pixels.width,
            height: pixels.height,
            pixel_format: GUID_WICPixelFormat32bppBGR,
            stride: pixels.width * 4,
            bytes: &pixels.data,
            palette: None,
            orientation: None,
        },
    )
}

/// One frame of an animated GIF for [`write_animated_gif`].
#[derive(Clone, Debug)]
pub struct GifFrame {
    /// The frame's pixels, which must be opaque or fully transparent, in at
    /// most 255 colours.
    pub pixels: Pixels,
    /// Where the frame's top-left corner is on the logical screen.
    pub left: u16,
    /// Where the frame's top edge is on the logical screen.
    pub top: u16,
    /// How long the frame is shown, in hundredths of a second.
    pub delay: u16,
    /// The GIF disposal method: 1 keeps the frame, 2 clears it to the
    /// background, 3 restores what was there before.
    pub disposal: u8,
}

impl GifFrame {
    /// A frame covering the logical screen from its top-left, shown for
    /// `delay` hundredths of a second and kept.
    pub fn whole(pixels: Pixels, delay: u16) -> Self {
        GifFrame {
            pixels,
            left: 0,
            top: 0,
            delay,
            disposal: 1,
        }
    }
}

/// Writes an animated GIF of `width × height` with `frames`, through WIC's
/// encoder and its frame metadata. With a `loop_count`, the file carries a
/// `NETSCAPE2.0` extension with that count; without one, it plays once.
pub fn write_animated_gif(
    path: &Path,
    width: u16,
    height: u16,
    frames: &[GifFrame],
    loop_count: Option<u16>,
) -> Result<()> {
    let factory = factory()?;
    // SAFETY: CreateStream has no arguments.
    let stream = unsafe { factory.CreateStream() }?;
    // SAFETY: the path outlives the call.
    unsafe { stream.InitializeFromFilename(&HSTRING::from(path), GENERIC_WRITE.0) }?;
    // SAFETY: the container GUID outlives the call, and a null vendor lets
    // WIC choose the encoder.
    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatGif, std::ptr::null()) }?;
    // SAFETY: the stream is live.
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }?;
    // SAFETY: GetMetadataQueryWriter has no arguments.
    let writer = unsafe { encoder.GetMetadataQueryWriter() }?;
    let mut values = vec![
        (w!("/logscrdesc/Width"), propvariant_u16(width)),
        (w!("/logscrdesc/Height"), propvariant_u16(height)),
    ];
    if let Some(count) = loop_count {
        let [low, high] = count.to_le_bytes();
        values.push((w!("/appext/Application"), propvariant_bytes(b"NETSCAPE2.0")));
        values.push((w!("/appext/Data"), propvariant_bytes(&[3, 1, low, high, 0])));
    }
    let mut set = Ok(());
    for (name, value) in &mut values {
        if set.is_ok() {
            // SAFETY: the name is a static string and the value a live
            // PROPVARIANT, which WIC copies.
            set = unsafe { writer.SetMetadataByName(*name, value) };
        }
        // SAFETY: the variant was made above and is not used again.
        let _ = unsafe { PropVariantClear(value) };
    }
    set?;
    for frame in frames {
        let (colours, indices, transparent) = indexed_frame(&frame.pixels)?;
        // SAFETY: CreatePalette has no arguments.
        let palette = unsafe { factory.CreatePalette() }?;
        // SAFETY: InitializeCustom copies the colours from the slice.
        unsafe { palette.InitializeCustom(&colours) }?;
        let mut encode_frame = None;
        let mut options: Option<IPropertyBag2> = None;
        // SAFETY: both pointers are to live locals, which receive owned
        // references.
        unsafe { encoder.CreateNewFrame(&mut encode_frame, &mut options) }?;
        let encode_frame =
            encode_frame.ok_or_else(|| Error::unexpected("WIC returned no frame"))?;
        let mut format = GUID_WICPixelFormat8bppIndexed;
        // SAFETY: the options came from CreateNewFrame; the rest are plain
        // values or live locals.
        unsafe {
            encode_frame.Initialize(options.as_ref())?;
            encode_frame.SetSize(frame.pixels.width, frame.pixels.height)?;
            encode_frame.SetPixelFormat(&mut format)?;
            encode_frame.SetPalette(&palette)?;
        }
        // SAFETY: GetMetadataQueryWriter has no arguments.
        let writer = unsafe { encode_frame.GetMetadataQueryWriter() }?;
        let mut values = vec![
            (w!("/imgdesc/Left"), propvariant_u16(frame.left)),
            (w!("/imgdesc/Top"), propvariant_u16(frame.top)),
            (w!("/grctlext/Delay"), propvariant_u16(frame.delay)),
            (w!("/grctlext/Disposal"), propvariant_u8(frame.disposal)),
        ];
        if let Some(index) = transparent {
            values.push((w!("/grctlext/TransparencyFlag"), propvariant_bool(true)));
            values.push((w!("/grctlext/TransparentColorIndex"), propvariant_u8(index)));
        }
        for (name, value) in &values {
            // SAFETY: the name is a static string and the value a live
            // PROPVARIANT that holds no resources.
            unsafe { writer.SetMetadataByName(*name, value) }?;
        }
        // SAFETY: the buffer holds one index a pixel, `width` to a row.
        unsafe { encode_frame.WritePixels(frame.pixels.height, frame.pixels.width, &indices) }?;
        // SAFETY: Commit has no arguments.
        unsafe { encode_frame.Commit() }?;
    }
    // SAFETY: Commit has no arguments.
    unsafe { encoder.Commit() }?;
    Ok(())
}

/// Reads the first frame of an image file as stored, ignoring any EXIF
/// orientation.
pub fn read_image(path: &Path) -> Result<Pixels> {
    let factory = factory()?;
    // SAFETY: the path outlives the call, and the other arguments are plain
    // values.
    let decoder = unsafe {
        factory.CreateDecoderFromFilename(
            &HSTRING::from(path),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )
    }?;
    // SAFETY: GetFrame takes a plain index.
    let frame = unsafe { decoder.GetFrame(0) }?;
    // SAFETY: CreateFormatConverter has no arguments.
    let converter = unsafe { factory.CreateFormatConverter() }?;
    // SAFETY: the frame and the GUID outlive the call; no palette is needed
    // to convert to a format with no palette.
    unsafe {
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }?;
    let (mut width, mut height) = (0, 0);
    // SAFETY: both pointers are to live locals.
    unsafe { converter.GetSize(&mut width, &mut height) }?;
    let mut data = vec![0; width as usize * height as usize * 4];
    // SAFETY: a null rectangle copies the whole image, which fits the buffer
    // at the given stride.
    unsafe { converter.CopyPixels(std::ptr::null(), width * 4, &mut data) }?;
    Ok(Pixels {
        width,
        height,
        data,
    })
}

/// A directory under the system's temporary directory, removed with its
/// contents when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates an empty directory with a name no other test will use.
    pub fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(unique_name("seer-test"));
        std::fs::create_dir_all(&path)?;
        Ok(TempDir { path })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path for `name` inside the directory.
    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Joins this thread to the multithreaded apartment, if it is in none.
pub fn init_com() {
    // SAFETY: initialising COM on this thread has no preconditions. It reports
    // S_FALSE or RPC_E_CHANGED_MODE when COM is already initialised here, and
    // either way COM is usable, so the result is ignored.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// A shell item array of `paths`, in order, as Explorer hands a selection to
/// a verb. A path may also be a shell namespace name such as `::{CLSID}`.
pub fn shell_items(paths: &[PathBuf]) -> Result<IShellItemArray> {
    init_com();
    let mut lists: Vec<*mut ITEMIDLIST> = Vec::new();
    let mut parsed = Ok(());
    for path in paths {
        let name = HSTRING::from(path.as_os_str());
        let mut list = std::ptr::null_mut();
        // SAFETY: the name outlives the call, and `list` receives an ID list
        // that is freed below.
        parsed = unsafe { SHParseDisplayName(&name, None, &mut list, 0, None) };
        if parsed.is_err() {
            break;
        }
        lists.push(list);
    }
    let items = parsed.and_then(|()| {
        let borrowed: Vec<*const ITEMIDLIST> = lists.iter().map(|list| list.cast_const()).collect();
        // SAFETY: each ID list is a live one made above; the array copies them.
        unsafe { SHCreateShellItemArrayFromIDLists(&borrowed) }
    });
    for list in lists {
        // SAFETY: SHParseDisplayName allocated the list with the COM
        // allocator, and nothing uses it after this.
        unsafe { CoTaskMemFree(Some(list.cast_const().cast())) };
    }
    Ok(items?)
}

/// An instance name no other test, and no viewer the developer is running,
/// will use.
pub fn unique_instance() -> String {
    unique_name("test")
}

fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!(
        "{prefix}-{}-{}-{nanos}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// How long [`HeadlessViewer`] waits for a viewer to start, to load images,
/// or to exit.
pub const VIEWER_TIMEOUT: Duration = Duration::from_secs(30);

/// A headless viewer on an instance of its own, for end-to-end tests.
///
/// It is ended with `seer quit` when dropped, never killed: coverage is
/// collected only from processes that exit normally.
///
/// It logs to a temporary directory of its own, given to it as
/// `SEER_LOG_DIR`, and removed once the viewer has exited.
#[derive(Debug)]
pub struct HeadlessViewer {
    instance: String,
    seer: PathBuf,
    process: Option<Child>,
    logs: TempDir,
}

impl HeadlessViewer {
    /// Starts `seerw --headless` from `seerw` on a new instance, opening
    /// `files`, and waits until it serves its pipe. `seer` is the command
    /// line, used to talk to it. Integration tests pass
    /// `env!("CARGO_BIN_EXE_seerw")` and `env!("CARGO_BIN_EXE_seer")`.
    pub fn start(seerw: &Path, seer: &Path, files: &[&Path]) -> Result<Self> {
        Self::start_with(seerw, seer, files)
    }

    /// [`HeadlessViewer::start`] with other arguments in place of the files,
    /// such as `-Embedding`.
    pub fn start_with<I, S>(seerw: &Path, seer: &Path, args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::start_with_env(seerw, seer, args, &[])
    }

    /// [`HeadlessViewer::start_with`] with these environment variables set
    /// for the viewer, such as `SEER_LOG_PATHS`.
    pub fn start_with_env<I, S>(
        seerw: &Path,
        seer: &Path,
        args: I,
        env: &[(&str, &str)],
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let instance = unique_instance();
        let logs = TempDir::new()?;
        let mut process = Command::new(seerw)
            .args(["--headless", "--instance", &instance])
            .args(args)
            .env("SEER_LOG_DIR", logs.path())
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let name = InstanceName::new(&instance)?;
        let connected = client::connect_to_started(&name, &mut process, VIEWER_TIMEOUT);
        // Made before the check, so a viewer that failed to connect is still
        // ended on drop.
        let viewer = HeadlessViewer {
            instance,
            seer: seer.to_owned(),
            process: Some(process),
            logs,
        };
        connected?;
        Ok(viewer)
    }

    /// What the viewer has written to its log so far, or an empty string if
    /// it has written nothing.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.logs.join("seerw.log")).unwrap_or_default()
    }

    /// The viewer's instance name.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Runs `seer --instance <this viewer's> <args>` and returns its output.
    pub fn seer<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(&self.seer)
            .args(["--instance", &self.instance])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("seer runs")
    }

    /// Runs `seer --json --instance <this viewer's> <args>`, checks that it
    /// succeeded, and parses what it printed.
    pub fn seer_json<T, I, S>(&self, args: I) -> T
    where
        T: DeserializeOwned,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut all: Vec<OsString> = vec!["--json".into()];
        all.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        let output = self.seer(&all);
        assert!(
            output.status.success(),
            "seer {all:?} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "seer {all:?} printed {}: {error}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// `seer list`.
    pub fn list(&self) -> ListReply {
        self.seer_json(["list"])
    }

    /// Polls `seer list` until `done` is true of it, and returns it. Panics
    /// after [`VIEWER_TIMEOUT`].
    pub fn wait_for(&self, done: impl Fn(&ListReply) -> bool) -> ListReply {
        let deadline = Instant::now() + VIEWER_TIMEOUT;
        loop {
            let list = self.list();
            if done(&list) {
                return list;
            }
            assert!(Instant::now() < deadline, "gave up waiting: {list:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Polls `seer list` until `count` images are open and none is still
    /// loading, and returns it. Only the shown image and its neighbours are
    /// decoded, so in a larger set wait for the images of interest with
    /// [`HeadlessViewer::wait_for`] instead.
    pub fn wait_until_loaded(&self, count: usize) -> ListReply {
        self.wait_for(|list| {
            list.images.len() == count
                && list
                    .images
                    .iter()
                    .all(|image| matches!(image.state, LoadState::Loaded | LoadState::Failed))
        })
    }

    /// Ends the viewer with `seer quit` and returns its exit status.
    pub fn quit(mut self) -> ExitStatus {
        self.stop().expect("the viewer exits")
    }

    fn stop(&mut self) -> Option<ExitStatus> {
        let mut process = self.process.take()?;
        let _ = self.seer(["quit"]);
        let deadline = Instant::now() + VIEWER_TIMEOUT;
        loop {
            match process.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return None,
            }
        }
    }
}

impl Drop for HeadlessViewer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One frame to encode.
struct Frame<'a> {
    width: u32,
    height: u32,
    pixel_format: GUID,
    stride: u32,
    bytes: &'a [u8],
    palette: Option<&'a IWICPalette>,
    orientation: Option<u16>,
}

fn factory() -> Result<IWICImagingFactory> {
    init_com();
    // SAFETY: the class id is a constant and no outer object is given.
    Ok(unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?)
}

fn encode(factory: &IWICImagingFactory, path: &Path, format: Format, frame: Frame) -> Result<()> {
    let name = format!("{format:?}");
    encode_as(factory, path, format.container(), &name, frame)
}

fn encode_as(
    factory: &IWICImagingFactory,
    path: &Path,
    container: GUID,
    format: &str,
    frame: Frame,
) -> Result<()> {
    // SAFETY: CreateStream has no arguments.
    let stream = unsafe { factory.CreateStream() }?;
    // SAFETY: the path outlives the call.
    unsafe { stream.InitializeFromFilename(&HSTRING::from(path), GENERIC_WRITE.0) }?;
    // SAFETY: the container GUID outlives the call, and a null vendor lets
    // WIC choose the encoder.
    let encoder = unsafe { factory.CreateEncoder(&container, std::ptr::null()) }?;
    // SAFETY: the stream is live.
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }?;
    let mut encode_frame = None;
    let mut options: Option<IPropertyBag2> = None;
    // SAFETY: both pointers are to live locals, which receive owned
    // references.
    unsafe { encoder.CreateNewFrame(&mut encode_frame, &mut options) }?;
    let encode_frame = encode_frame.ok_or_else(|| Error::unexpected("WIC returned no frame"))?;
    // SAFETY: the options bag came from CreateNewFrame and is live.
    unsafe { encode_frame.Initialize(options.as_ref()) }?;
    // SAFETY: SetSize takes plain values.
    unsafe { encode_frame.SetSize(frame.width, frame.height) }?;
    let mut pixel_format = frame.pixel_format;
    // SAFETY: the pointer is to a live local, which the encoder may change to
    // the closest format it supports.
    unsafe { encode_frame.SetPixelFormat(&mut pixel_format) }?;
    if pixel_format != frame.pixel_format {
        return Err(windows::core::Error::new(
            E_FAIL,
            format!("the {format} encoder does not accept the pixel format"),
        )
        .into());
    }
    if let Some(palette) = frame.palette {
        // SAFETY: the palette is live; the frame copies it.
        unsafe { encode_frame.SetPalette(palette) }?;
    }
    if let Some(orientation) = frame.orientation {
        // SAFETY: GetMetadataQueryWriter has no arguments.
        let writer = unsafe { encode_frame.GetMetadataQueryWriter() }?;
        let value = propvariant_u16(orientation);
        // SAFETY: the name is a static string and the value a live,
        // initialised PROPVARIANT that holds no resources.
        unsafe { writer.SetMetadataByName(w!("/app1/ifd/{ushort=274}"), &value) }?;
    }
    // SAFETY: the buffer holds `height` rows of `stride` bytes in the pixel
    // format set above.
    unsafe { encode_frame.WritePixels(frame.height, frame.stride, frame.bytes) }?;
    // SAFETY: Commit has no arguments.
    unsafe { encode_frame.Commit() }?;
    // SAFETY: Commit has no arguments.
    unsafe { encoder.Commit() }?;
    Ok(())
}

/// A `VT_UI2` PROPVARIANT. It owns nothing, so it needs no
/// `PropVariantClear`.
fn propvariant_u16(value: u16) -> PROPVARIANT {
    let mut variant = PROPVARIANT::default();
    // SAFETY: the variant is zeroed, so writing the type tag and the union
    // member that tag selects leaves it valid.
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_UI2;
        (*variant.Anonymous.Anonymous).Anonymous.uiVal = value;
    }
    variant
}

/// A `VT_UI1` PROPVARIANT. It owns nothing.
fn propvariant_u8(value: u8) -> PROPVARIANT {
    let mut variant = PROPVARIANT::default();
    // SAFETY: the variant is zeroed, so writing the type tag and the union
    // member that tag selects leaves it valid.
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_UI1;
        (*variant.Anonymous.Anonymous).Anonymous.bVal = value;
    }
    variant
}

/// A `VT_BOOL` PROPVARIANT. It owns nothing.
fn propvariant_bool(value: bool) -> PROPVARIANT {
    let mut variant = PROPVARIANT::default();
    // SAFETY: the variant is zeroed, so writing the type tag and the union
    // member that tag selects leaves it valid.
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_BOOL;
        (*variant.Anonymous.Anonymous).Anonymous.boolVal =
            if value { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    variant
}

/// A `VT_VECTOR | VT_UI1` PROPVARIANT holding a copy of `bytes` in COM's task
/// memory, which `PropVariantClear` frees.
fn propvariant_bytes(bytes: &[u8]) -> PROPVARIANT {
    // SAFETY: CoTaskMemAlloc takes a plain size.
    let copy = unsafe { CoTaskMemAlloc(bytes.len()) }.cast::<u8>();
    assert!(!copy.is_null(), "out of memory");
    // SAFETY: the allocation holds `bytes.len()` bytes and does not overlap
    // `bytes`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy, bytes.len()) };
    let mut variant = PROPVARIANT::default();
    // SAFETY: the variant is zeroed, so writing the type tag and the union
    // member that tag selects leaves it valid; it owns the allocation.
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_VECTOR | VT_UI1;
        (*variant.Anonymous.Anonymous).Anonymous.caub = CAUB {
            cElems: bytes.len() as u32,
            pElems: copy,
        };
    }
    variant
}

/// A GIF frame as a palette of WIC colours, one palette index per pixel, and
/// the index of the transparent colour if any pixel is transparent.
fn indexed_frame(pixels: &Pixels) -> Result<(Vec<u32>, Vec<u8>, Option<u8>)> {
    let transparent = pixels.data.as_chunks::<4>().0.iter().any(|p| p[3] == 0);
    let mut opaque = pixels.clone();
    for pixel in opaque.data.as_chunks_mut::<4>().0 {
        if pixel[3] == 0 {
            // A colour no test uses, to stand for transparency.
            *pixel = [1, 2, 3, 255];
        }
    }
    let (mut colours, indices) = indexed(&opaque)?;
    let index = colours.iter().position(|&c| c == 0xFF03_0201);
    if let Some(index) = index {
        colours[index] = 0;
    }
    Ok((
        colours,
        indices,
        index.filter(|_| transparent).map(|i| i as u8),
    ))
}

fn without_alpha(pixels: &Pixels) -> Vec<u8> {
    pixels
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|bgra| [bgra[0], bgra[1], bgra[2]])
        .collect()
}

/// The image as a palette of WIC colours (`0xAARRGGBB`) and one palette index
/// per pixel.
fn indexed(pixels: &Pixels) -> Result<(Vec<u32>, Vec<u8>)> {
    let mut palette: Vec<u32> = Vec::new();
    let mut indices = Vec::with_capacity(pixels.data.len() / 4);
    for bgra in pixels.data.as_chunks::<4>().0 {
        let [b, g, r, a] = [bgra[0], bgra[1], bgra[2], bgra[3]];
        if a != 255 {
            return Err(Error::usage("GIF test images must be opaque"));
        }
        let colour = u32::from_be_bytes([a, r, g, b]);
        let index = match palette.iter().position(|&c| c == colour) {
            Some(index) => index,
            None if palette.len() < 256 => {
                palette.push(colour);
                palette.len() - 1
            }
            None => {
                return Err(Error::usage(
                    "GIF test images must have at most 256 colours",
                ));
            }
        };
        indices.push(index as u8);
    }
    Ok((palette, indices))
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::error::ErrorCode;

    const COLOURS: [Rgba; 4] = [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE];

    /// The image with a margin around each quadrant's edges blanked to match
    /// `actual`, so only the flat interiors are compared.
    fn interiors(expected: &Pixels, actual: &Pixels, margin: u32) -> Pixels {
        let mut masked = expected.clone();
        let (w, h) = (expected.width, expected.height);
        for y in 0..h {
            for x in 0..w {
                let near = |v: u32, edge: u32| v.abs_diff(edge) < margin;
                if near(x, 0)
                    || near(x, w / 2)
                    || near(x + 1, w)
                    || near(y, 0)
                    || near(y, h / 2)
                    || near(y + 1, h)
                {
                    masked.set(x, y, actual.get(x, y));
                }
            }
        }
        masked
    }

    #[test]
    fn every_format_round_trips() {
        let dir = TempDir::new().unwrap();
        let image = Pixels::quadrants(64, 48, COLOURS);
        for format in Format::ALL {
            let path = dir.join(format!("quadrants.{}", format.extension()));
            write_image(&path, &image, format).unwrap();
            let read = read_image(&path).unwrap();
            let expected = if format == Format::Jpeg {
                interiors(&image, &read, 8)
            } else {
                image.clone()
            };
            assert_similar(&expected, &read, format.tolerance());
        }
    }

    #[test]
    fn alpha_survives_png_and_tiff() {
        let dir = TempDir::new().unwrap();
        let image = Pixels::from_fn(8, 8, |x, y| Rgba {
            r: 200,
            g: 100,
            b: 50,
            a: ((x + y) * 16) as u8,
        });
        for format in [Format::Png, Format::Tiff] {
            let path = dir.join(format!("alpha.{}", format.extension()));
            write_image(&path, &image, format).unwrap();
            assert_similar(&image, &read_image(&path).unwrap(), 0);
        }
    }

    #[test]
    fn gif_rejects_what_it_cannot_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("bad.gif");
        let translucent = Pixels::solid(2, 2, Rgba { a: 0, ..Rgba::RED });
        let error = write_image(&path, &translucent, Format::Gif).unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
        let many = Pixels::from_fn(32, 32, |x, y| Rgba::opaque(x as u8, y as u8, 0));
        let error = write_image(&path, &many, Format::Gif).unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
    }

    #[test]
    fn jpeg_orientation_is_written() {
        let dir = TempDir::new().unwrap();
        let image = Pixels::quadrants(32, 16, COLOURS);
        for orientation in 1..=8 {
            let path = dir.join(format!("oriented-{orientation}.jpg"));
            write_jpeg_with_orientation(&path, &image, orientation).unwrap();
            assert_eq!(read_orientation(&path), orientation);
            let read = read_image(&path).unwrap();
            assert_eq!((read.width, read.height), (32, 16));
        }
        let error = write_jpeg_with_orientation(&dir.join("bad.jpg"), &image, 9).unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
    }

    fn read_orientation(path: &Path) -> u16 {
        let factory = factory().unwrap();
        // SAFETY: as in read_image.
        let decoder = unsafe {
            factory.CreateDecoderFromFilename(
                &HSTRING::from(path),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
        }
        .unwrap();
        // SAFETY: GetFrame takes a plain index.
        let frame = unsafe { decoder.GetFrame(0) }.unwrap();
        // SAFETY: GetMetadataQueryReader has no arguments.
        let reader = unsafe { frame.GetMetadataQueryReader() }.unwrap();
        let mut value = PROPVARIANT::default();
        // SAFETY: the name is static and the value a live, zeroed PROPVARIANT
        // for WIC to fill.
        unsafe { reader.GetMetadataByName(w!("/app1/ifd/{ushort=274}"), &mut value) }.unwrap();
        // SAFETY: the tag is checked before the union member it selects is
        // read, and the variant is cleared once read.
        unsafe {
            assert_eq!(value.Anonymous.Anonymous.vt, VT_UI2);
            let orientation = value.Anonymous.Anonymous.Anonymous.uiVal;
            PropVariantClear(&mut value).unwrap();
            orientation
        }
    }

    #[test]
    fn missing_files_fail_to_read() {
        let dir = TempDir::new().unwrap();
        assert!(read_image(&dir.join("absent.png")).is_err());
    }

    #[test]
    fn pixels_and_differences() {
        let mut image = Pixels::solid(4, 4, Rgba::BLACK);
        image.set(1, 2, Rgba::opaque(10, 0, 0));
        assert_eq!(image.get(1, 2), Rgba::opaque(10, 0, 0));
        let black = Pixels::solid(4, 4, Rgba::BLACK);
        assert_eq!(
            difference(&black, &image, 5),
            Difference {
                max_channel: 10,
                pixels: 1
            }
        );
        assert_similar(&black, &image, 10);
        let quadrants = Pixels::quadrants(4, 4, COLOURS);
        assert_eq!(quadrants.get(0, 0), Rgba::RED);
        assert_eq!(quadrants.get(3, 0), Rgba::GREEN);
        assert_eq!(quadrants.get(0, 3), Rgba::BLUE);
        assert_eq!(quadrants.get(3, 3), Rgba::WHITE);
    }

    #[test]
    #[should_panic(expected = "1 of 16 pixels differ by more than 5")]
    fn assert_similar_reports_the_difference() {
        let mut image = Pixels::solid(4, 4, Rgba::BLACK);
        image.set(0, 0, Rgba::opaque(9, 0, 0));
        assert_similar(&Pixels::solid(4, 4, Rgba::BLACK), &image, 5);
    }

    #[test]
    fn temporary_directories_are_unique_and_removed() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        assert_ne!(first.path(), second.path());
        let path = first.path().to_owned();
        std::fs::write(first.join("file"), b"x").unwrap();
        drop(first);
        assert!(!path.exists());
        assert_ne!(unique_instance(), unique_instance());
        assert!(crate::ipc::instance::InstanceName::new(&unique_instance()).is_ok());
    }
}
