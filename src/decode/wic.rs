//! Images through the Windows Imaging Component.
//!
//! [`Decoder`] opens a file as a [`Source`], which can be decoded at any
//! [`DecodeTarget`]: scaled to cover a size, at full resolution, or as a
//! thumbnail. Every result is premultiplied BGRA with the EXIF orientation
//! applied, ready to become a Direct2D bitmap.
//!
//! Most formats are still images, and only the first frame is decoded. GIFs
//! are composed by [`gif`](crate::decode::gif): an animated one decodes to all
//! its frames at full size, whatever the target, so it can play at once. SVG
//! files, known by their extension, are rasterised by
//! [`svg`](crate::decode::svg), and carry their text for the renderer to draw
//! as vectors.
//!
//! Scaled decodes are cheap for JPEG: the codec can scale by 1/2, 1/4 or 1/8
//! while decoding, so a [`Source`] asks it for the smallest such size that is
//! still large enough, and finishes with WIC's scaler.

use std::cell::OnceCell;
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, WINCODEC_ERR_COMPONENTINITIALIZEFAILURE,
    WINCODEC_ERR_COMPONENTNOTFOUND, WINCODEC_ERR_UNKNOWNIMAGEFORMAT,
};
use windows::Win32::Graphics::Direct2D::ID2D1Factory1;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatGif, GUID_WICPixelFormat32bppPBGRA,
    IWICBitmapCodecInfo, IWICBitmapFrameDecode, IWICBitmapSource, IWICBitmapSourceTransform,
    IWICImagingFactory, IWICPalette, IWICPixelFormatInfo, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICBitmapTransformRotate0,
    WICComponentEnumerateDefault, WICDecodeMetadataCacheOnDemand, WICDecoder,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::VT_UI2;
use windows::core::{GUID, HRESULT, IUnknown, Interface, w};

use crate::decode::gif::{Animation, Gif};
use crate::decode::svg::{self, Svg};
use crate::error::{Error, ErrorCode, Result};
use crate::model::app::DecodeTarget;
use crate::model::geometry::PixelSize;

/// A decoded image: premultiplied BGRA, rows top to bottom, tightly packed,
/// with the EXIF orientation applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The image's full size, after orientation.
    pub size: PixelSize,
    /// The size of `pixels`, which is `size` or smaller.
    pub bitmap_size: PixelSize,
    /// `bitmap_size.width × bitmap_size.height × 4` bytes.
    pub pixels: Vec<u8>,
    /// What the image is beyond `pixels`.
    pub content: Content,
}

/// What a decoded image is beyond its pixels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Content {
    /// A still image: the pixels are all there is.
    #[default]
    Still,
    /// An animation, whose first frame is the pixels.
    Animation(Animation),
    /// An SVG document, whose text this is; the pixels are a rasterisation.
    Vector(Arc<[u8]>),
}

/// Opens image files. Each thread that decodes needs its own.
pub struct Decoder {
    factory: IWICImagingFactory,
    d2d: OnceCell<ID2D1Factory1>,
}

impl Decoder {
    /// Initialises COM on this thread, if it is not already, and creates the
    /// WIC factory.
    pub fn new() -> Result<Self> {
        Ok(Decoder {
            factory: wic_factory()?,
            d2d: OnceCell::new(),
        })
    }

    /// Opens `path` and reads the image's size and orientation.
    ///
    /// The file is opened sharing reading, writing and deleting, so holding a
    /// [`Source`] never stops another program from rewriting or deleting it.
    pub fn open(&self, path: &Path) -> Result<Source> {
        let failed = |error: windows::core::Error| open_error(path, &error);
        let io_failed = |error: std::io::Error| {
            let code = error.raw_os_error().unwrap_or_default() as u32;
            failed(HRESULT::from_win32(code).into())
        };
        // The standard library shares reading, writing and deleting.
        let mut file = File::open(path).map_err(io_failed)?;
        let stamp = Stamp::of(&file.metadata().map_err(io_failed)?);
        let (kind, file) = if svg::is_svg(path) {
            let mut source = Vec::new();
            file.read_to_end(&mut source).map_err(io_failed)?;
            (self.open_svg(path, source)?, None)
        } else {
            (self.open_wic(&file).map_err(failed)?, Some(file))
        };
        Ok(Source {
            factory: self.factory.clone(),
            path: path.to_owned(),
            kind,
            stamp,
            _file: file,
        })
    }

    fn open_wic(&self, file: &File) -> windows::core::Result<Kind> {
        // SAFETY: the handle is open, and `Source` keeps the file open for as
        // long as the decoder, which its frame holds, lives. No vendor is
        // preferred.
        let decoder = unsafe {
            self.factory.CreateDecoderFromFileHandle(
                file.as_raw_handle() as usize,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )
        }?;
        // SAFETY: GetContainerFormat has no arguments.
        if unsafe { decoder.GetContainerFormat() }? == GUID_ContainerFormatGif {
            return Ok(Kind::Gif(Gif::new(&self.factory, decoder)?));
        }
        // SAFETY: GetFrame takes a plain index.
        let frame = unsafe { decoder.GetFrame(0) }?;
        let (mut width, mut height) = (0, 0);
        // SAFETY: both pointers are to live locals.
        unsafe { frame.GetSize(&mut width, &mut height) }?;
        Ok(Kind::Still(Still {
            factory: self.factory.clone(),
            orientation: Orientation::from_exif(read_orientation(&frame)),
            stored: PixelSize::new(width, height),
            frame,
        }))
    }

    fn open_svg(&self, path: &Path, source: Vec<u8>) -> Result<Kind> {
        let d2d = svg::factory(&self.d2d).map_err(|error| decode_error(path, &error))?;
        let svg =
            Svg::new(&d2d, &self.factory, source).map_err(|error| decode_error(path, &error))?;
        Ok(Kind::Svg(svg))
    }
}

/// A file's length and last-write time, which change when it is rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(metadata: &Metadata) -> Self {
        Stamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// An open image file.
pub struct Source {
    factory: IWICImagingFactory,
    path: PathBuf,
    kind: Kind,
    stamp: Stamp,
    /// The file the decoder reads, closed after the decoder is released. An
    /// SVG file is read whole when opened, and closed then.
    _file: Option<File>,
}

enum Kind {
    Still(Still),
    Gif(Gif),
    Svg(Svg),
}

impl Source {
    /// The path the source was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file at the source's path is still the one it read: not
    /// rewritten, replaced or deleted since it was opened.
    pub fn is_current(&self) -> bool {
        std::fs::metadata(&self.path).is_ok_and(|metadata| Stamp::of(&metadata) == self.stamp)
    }

    /// The image's full size, after orientation.
    pub fn size(&self) -> PixelSize {
        match &self.kind {
            Kind::Still(still) => still.orientation.apply_size(still.stored),
            Kind::Gif(gif) => gif.size(),
            Kind::Svg(svg) => svg.size(),
        }
    }

    /// How the stored pixels are turned to show the image upright.
    pub fn orientation(&self) -> Orientation {
        match &self.kind {
            Kind::Still(still) => still.orientation,
            Kind::Gif(_) | Kind::Svg(_) => Orientation::Normal,
        }
    }

    /// Decodes the image at `target`'s size. An animated GIF decodes to all
    /// its frames at full size, except as a thumbnail.
    pub fn decode(&self, target: DecodeTarget) -> Result<Decoded> {
        let size = self.size();
        let failed = |error: windows::core::Error| decode_error(&self.path, &error);
        match &self.kind {
            Kind::Still(still) => {
                let want = still.orientation.apply_size(target_size(size, target));
                let pixels = still.pixels(want).map_err(failed)?;
                let (bitmap_size, pixels) = still.orientation.apply(want, pixels);
                Ok(Decoded {
                    size,
                    bitmap_size,
                    pixels,
                    content: Content::Still,
                })
            }
            Kind::Gif(gif) => {
                let thumbnail = matches!(target, DecodeTarget::Thumbnail { .. });
                if gif.is_animated() && !thumbnail {
                    let animation = gif.compose(u32::MAX).map_err(failed)?;
                    return Ok(Decoded {
                        size,
                        bitmap_size: size,
                        pixels: animation.frames[0].pixels.clone(),
                        content: Content::Animation(animation),
                    });
                }
                let mut first = gif.compose(1).map_err(failed)?;
                let pixels = std::mem::take(&mut first.frames[0].pixels);
                let want = target_size(size, target);
                let pixels = scale(&self.factory, size, &pixels, want).map_err(failed)?;
                Ok(Decoded {
                    size,
                    bitmap_size: want,
                    pixels,
                    content: Content::Still,
                })
            }
            Kind::Svg(svg) => {
                let mut want = target_size(size, target);
                if want.width > svg::MAX_RASTER || want.height > svg::MAX_RASTER {
                    let at_most = PixelSize::new(svg::MAX_RASTER, svg::MAX_RASTER);
                    want = target_size(size, DecodeTarget::Thumbnail { at_most });
                }
                let pixels = svg.render(want).map_err(failed)?;
                Ok(Decoded {
                    size,
                    bitmap_size: want,
                    pixels,
                    content: Content::Vector(Arc::clone(svg.source())),
                })
            }
        }
    }
}

/// A still image's first frame.
struct Still {
    factory: IWICImagingFactory,
    frame: IWICBitmapFrameDecode,
    stored: PixelSize,
    orientation: Orientation,
}

impl Still {
    /// The stored pixels scaled to `want`, premultiplied BGRA.
    fn pixels(&self, want: PixelSize) -> windows::core::Result<Vec<u8>> {
        let mut source: IWICBitmapSource = self.frame.cast()?;
        let mut have = self.stored;
        if want != self.stored
            && let Some((native, size)) = self.native_scale(want)?
        {
            source = native;
            have = size;
        }
        // SAFETY: CreateFormatConverter has no arguments.
        let converter = unsafe { self.factory.CreateFormatConverter() }?;
        // SAFETY: the source and GUID outlive the call, and no palette is
        // needed to convert to a format without one.
        unsafe {
            converter.Initialize(
                &source,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        }?;
        let mut source: IWICBitmapSource = converter.cast()?;
        if have != want {
            // SAFETY: CreateBitmapScaler has no arguments.
            let scaler = unsafe { self.factory.CreateBitmapScaler() }?;
            // SAFETY: the source outlives the call; the rest are plain values.
            unsafe {
                scaler.Initialize(
                    &source,
                    want.width,
                    want.height,
                    WICBitmapInterpolationModeFant,
                )
            }?;
            source = scaler.cast()?;
        }
        let stride = want.width * 4;
        let mut pixels = vec![0; stride as usize * want.height as usize];
        // SAFETY: a null rectangle copies the whole source, which is `want`
        // in size and fits the buffer at this stride.
        unsafe { source.CopyPixels(std::ptr::null(), stride, &mut pixels) }?;
        Ok(pixels)
    }

    /// The frame scaled by the codec itself to the smallest size it can
    /// produce that is no smaller than `want`, if the codec can scale and that
    /// size is smaller than the stored one.
    fn native_scale(
        &self,
        want: PixelSize,
    ) -> windows::core::Result<Option<(IWICBitmapSource, PixelSize)>> {
        let Ok(transform) = self.frame.cast::<IWICBitmapSourceTransform>() else {
            return Ok(None);
        };
        let mut ask = want;
        let size = loop {
            let (mut width, mut height) = (ask.width, ask.height);
            // SAFETY: both pointers are to live locals.
            unsafe { transform.GetClosestSize(&mut width, &mut height) }?;
            if width >= want.width && height >= want.height {
                break PixelSize::new(width, height);
            }
            if ask == self.stored {
                return Ok(None);
            }
            ask = PixelSize::new(
                (ask.width * 2).min(self.stored.width),
                (ask.height * 2).min(self.stored.height),
            );
        };
        if size.width >= self.stored.width && size.height >= self.stored.height {
            return Ok(None);
        }
        let mut format = GUID_WICPixelFormat32bppPBGRA;
        // SAFETY: the pointer is to a live local, which the codec changes to
        // the nearest format it can produce.
        unsafe { transform.GetClosestPixelFormat(&mut format) }?;
        let stride = (size.width * self.bits_per_pixel(&format)?).div_ceil(32) * 4;
        let mut pixels = vec![0; stride as usize * size.height as usize];
        // SAFETY: a null rectangle decodes the whole frame, scaled to `size`,
        // in `format`, which fits the buffer at this stride.
        unsafe {
            transform.CopyPixels(
                std::ptr::null(),
                size.width,
                size.height,
                &format,
                WICBitmapTransformRotate0,
                stride,
                &mut pixels,
            )
        }?;
        // SAFETY: the buffer holds `size.height` rows of `stride` bytes in
        // `format`; WIC copies it.
        let bitmap = unsafe {
            self.factory
                .CreateBitmapFromMemory(size.width, size.height, &format, stride, &pixels)
        }?;
        Ok(Some((bitmap.cast()?, size)))
    }

    fn bits_per_pixel(&self, format: &GUID) -> windows::core::Result<u32> {
        // SAFETY: the GUID outlives the call.
        let info: IWICPixelFormatInfo =
            unsafe { self.factory.CreateComponentInfo(format) }?.cast()?;
        // SAFETY: GetBitsPerPixel has no arguments.
        unsafe { info.GetBitsPerPixel() }
    }
}

/// Opens and decodes `path` at `target`'s size, for callers that decode a
/// file only once.
pub fn decode(path: &Path, target: DecodeTarget) -> Result<Decoded> {
    Decoder::new()?.open(path)?.decode(target)
}

/// Initialises COM on this thread, if it is not already, and creates a WIC
/// factory.
fn wic_factory() -> Result<IWICImagingFactory> {
    // SAFETY: initialising COM has no preconditions. It reports S_FALSE or
    // RPC_E_CHANGED_MODE when COM is already initialised on this thread, and
    // COM is usable either way, so the result is ignored.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: the class id is a constant and no outer object is given.
    Ok(unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?)
}

/// `pixels`, premultiplied BGRA of `size`, scaled to `want`.
fn scale(
    factory: &IWICImagingFactory,
    size: PixelSize,
    pixels: &[u8],
    want: PixelSize,
) -> windows::core::Result<Vec<u8>> {
    if want == size {
        return Ok(pixels.to_vec());
    }
    // SAFETY: the buffer holds `size.height` rows of `size.width × 4` bytes
    // of premultiplied BGRA; WIC copies it.
    let bitmap = unsafe {
        factory.CreateBitmapFromMemory(
            size.width,
            size.height,
            &GUID_WICPixelFormat32bppPBGRA,
            size.width * 4,
            pixels,
        )
    }?;
    // SAFETY: CreateBitmapScaler has no arguments.
    let scaler = unsafe { factory.CreateBitmapScaler() }?;
    // SAFETY: the bitmap outlives the call; the rest are plain values.
    unsafe {
        scaler.Initialize(
            &bitmap,
            want.width,
            want.height,
            WICBitmapInterpolationModeFant,
        )
    }?;
    let stride = want.width * 4;
    let mut scaled = vec![0; stride as usize * want.height as usize];
    // SAFETY: a null rectangle copies the whole scaled image, which is `want`
    // in size and fits the buffer at this stride.
    unsafe { scaler.CopyPixels(std::ptr::null(), stride, &mut scaled) }?;
    Ok(scaled)
}

/// If `path`'s extension is one whose codec Windows installs only as a
/// Store extension, and no installed codec decodes it, the error that says
/// which extension to install.
///
/// The installed codecs are listed once a process, the first time this is
/// asked about such a file.
pub fn missing_codec(path: &Path) -> Option<Error> {
    let codec = extension_codec(path)?;
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    static DECODABLE: OnceLock<Vec<String>> = OnceLock::new();
    let decodable = DECODABLE.get_or_init(|| decodable_extensions().unwrap_or_default());
    if decodable.contains(&extension) {
        return None;
    }
    Some(codec_missing(path, codec))
}

/// The extensions, lower case and without dots, that the installed WIC
/// decoders claim.
pub fn decodable_extensions() -> Result<Vec<String>> {
    let factory = wic_factory()?;
    // SAFETY: plain flag values.
    let components = unsafe {
        factory
            .CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32)
    }?;
    let mut extensions = Vec::new();
    loop {
        let mut component: [Option<IUnknown>; 1] = [None];
        let mut fetched = 0;
        // SAFETY: the array and count are live locals for one element.
        if unsafe { components.Next(&mut component, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let Some(info) = component[0]
            .take()
            .and_then(|unknown| unknown.cast::<IWICBitmapCodecInfo>().ok())
        else {
            continue;
        };
        let mut buffer = vec![0u16; 256];
        let mut length = 0;
        // SAFETY: the buffer and length are live locals, and the call is told
        // the buffer's length.
        let mut listed = unsafe { info.GetFileExtensions(&mut buffer, &mut length) };
        if listed.is_err() && length as usize > buffer.len() {
            buffer.resize(length as usize, 0);
            // SAFETY: as above, with a buffer as long as WIC asked for.
            listed = unsafe { info.GetFileExtensions(&mut buffer, &mut length) };
        }
        if listed.is_err() {
            continue;
        }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..end]);
        extensions.extend(
            text.split([',', ';'])
                .map(|extension| {
                    extension
                        .trim()
                        .trim_start_matches('.')
                        .to_ascii_lowercase()
                })
                .filter(|extension| !extension.is_empty()),
        );
    }
    Ok(extensions)
}

/// The size to decode an image of `size` (after orientation) at for
/// `target`, keeping the aspect ratio and never exceeding `size`.
pub fn target_size(size: PixelSize, target: DecodeTarget) -> PixelSize {
    let (width, height) = (f64::from(size.width), f64::from(size.height));
    let scaled = |scale: f64, round: fn(f64) -> f64| {
        PixelSize::new(
            (round(width * scale) as u32).clamp(1, size.width.max(1)),
            (round(height * scale) as u32).clamp(1, size.height.max(1)),
        )
    };
    match target {
        DecodeTarget::Full => size,
        DecodeTarget::Fit { within } => {
            let scale = (f64::from(within.width) / width)
                .min(f64::from(within.height) / height)
                .min(1.0);
            if scale >= 1.0 || size.width == 0 || size.height == 0 {
                size
            } else {
                scaled(scale, f64::ceil)
            }
        }
        DecodeTarget::Scaled { at_least } => {
            let scale =
                (f64::from(at_least.width) / width).max(f64::from(at_least.height) / height);
            if scale >= 1.0 || size.width == 0 || size.height == 0 {
                size
            } else {
                scaled(scale, f64::ceil)
            }
        }
        DecodeTarget::Thumbnail { at_most } => {
            let scale = (f64::from(at_most.width) / width)
                .min(f64::from(at_most.height) / height)
                .min(1.0);
            if size.width == 0 || size.height == 0 {
                size
            } else {
                scaled(scale, f64::floor)
            }
        }
    }
}

/// How stored pixels are turned to show an image upright: the eight values of
/// the EXIF orientation tag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Orientation {
    /// 1: as stored.
    #[default]
    Normal,
    /// 2: mirrored left to right.
    MirrorHorizontal,
    /// 3: turned half a turn.
    Rotate180,
    /// 4: mirrored top to bottom.
    MirrorVertical,
    /// 5: mirrored about the diagonal from the top-left.
    Transpose,
    /// 6: turned a quarter turn clockwise.
    Rotate90,
    /// 7: mirrored about the diagonal from the top-right.
    Transverse,
    /// 8: turned a quarter turn anticlockwise.
    Rotate270,
}

impl Orientation {
    /// The orientation an EXIF tag value names; anything but 2 to 8 is
    /// [`Normal`](Orientation::Normal).
    pub fn from_exif(value: u16) -> Self {
        match value {
            2 => Orientation::MirrorHorizontal,
            3 => Orientation::Rotate180,
            4 => Orientation::MirrorVertical,
            5 => Orientation::Transpose,
            6 => Orientation::Rotate90,
            7 => Orientation::Transverse,
            8 => Orientation::Rotate270,
            _ => Orientation::Normal,
        }
    }

    /// Whether the orientation swaps width and height.
    pub fn is_sideways(self) -> bool {
        matches!(
            self,
            Orientation::Transpose
                | Orientation::Rotate90
                | Orientation::Transverse
                | Orientation::Rotate270
        )
    }

    /// The size of an image of `size` once turned. Turning back is the same
    /// operation.
    pub fn apply_size(self, size: PixelSize) -> PixelSize {
        if self.is_sideways() {
            PixelSize::new(size.height, size.width)
        } else {
            size
        }
    }

    /// Turns `pixels`, a tightly packed four-byte-per-pixel image of `size`,
    /// and returns the result with its size.
    pub fn apply(self, size: PixelSize, pixels: Vec<u8>) -> (PixelSize, Vec<u8>) {
        if self == Orientation::Normal {
            return (size, pixels);
        }
        let (w, h) = (size.width as usize, size.height as usize);
        let turned = self.apply_size(size);
        let tw = turned.width as usize;
        let source = pixels.as_chunks::<4>().0;
        let mut out = vec![[0u8; 4]; w * h];
        for y in 0..h {
            for x in 0..w {
                let (tx, ty) = match self {
                    Orientation::Normal => (x, y),
                    Orientation::MirrorHorizontal => (w - 1 - x, y),
                    Orientation::Rotate180 => (w - 1 - x, h - 1 - y),
                    Orientation::MirrorVertical => (x, h - 1 - y),
                    Orientation::Transpose => (y, x),
                    Orientation::Rotate90 => (h - 1 - y, x),
                    Orientation::Transverse => (h - 1 - y, w - 1 - x),
                    Orientation::Rotate270 => (y, w - 1 - x),
                };
                out[ty * tw + tx] = source[y * w + x];
            }
        }
        (turned, out.into_flattened())
    }
}

/// The frame's EXIF orientation tag, or 1 if it has none.
fn read_orientation(frame: &IWICBitmapFrameDecode) -> u16 {
    // SAFETY: GetMetadataQueryReader has no arguments.
    let Ok(reader) = (unsafe { frame.GetMetadataQueryReader() }) else {
        return 1;
    };
    let mut value = PROPVARIANT::default();
    // SAFETY: the name is static and the value a live, zeroed PROPVARIANT for
    // WIC to fill.
    if unsafe { reader.GetMetadataByName(w!("System.Photo.Orientation"), &mut value) }.is_err() {
        return 1;
    }
    // SAFETY: the tag is checked before the union member it selects is read,
    // and the variant is cleared once read.
    unsafe {
        let orientation = if value.Anonymous.Anonymous.vt == VT_UI2 {
            value.Anonymous.Anonymous.Anonymous.uiVal
        } else {
            1
        };
        let _ = PropVariantClear(&mut value);
        orientation
    }
}

/// File extensions whose codecs Windows installs only as Store extensions.
const EXTENSION_CODECS: [(&str, &str); 6] = [
    ("webp", "WebP Image Extensions"),
    ("heic", "HEIF Image Extensions"),
    ("heif", "HEIF Image Extensions"),
    ("avif", "AV1 Video Extension"),
    ("jxl", "JPEG XL Image Extension"),
    ("raw", "Raw Image Extension"),
];

/// The error for a file that could not be opened.
fn open_error(path: &Path, error: &windows::core::Error) -> Error {
    let code = error.code();
    if code == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
        || code == HRESULT::from_win32(ERROR_PATH_NOT_FOUND.0)
    {
        return not_found(path);
    }
    if code == WINCODEC_ERR_COMPONENTNOTFOUND || code == WINCODEC_ERR_UNKNOWNIMAGEFORMAT {
        if let Some(codec) = extension_codec(path) {
            return codec_missing(path, codec);
        }
        return Error::new(
            ErrorCode::UnsupportedFormat,
            format!("{}: not an image format Windows can decode", path.display()),
        );
    }
    decode_error(path, error)
}

/// The Store extension whose codec decodes files like `path`, if Windows
/// installs it only that way.
fn extension_codec(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    EXTENSION_CODECS
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, codec)| *codec)
}

fn not_found(path: &Path) -> Error {
    Error::new(
        ErrorCode::FileNotFound,
        format!("{}: no such file", path.display()),
    )
}

fn codec_missing(path: &Path, codec: &str) -> Error {
    Error::new(
        ErrorCode::CodecMissing,
        format!(
            "{}: decoding it needs the {codec} from the Microsoft Store",
            path.display()
        ),
    )
}

/// The error for a file that could not be decoded. A codec registered for
/// one of the Store extensions' formats that cannot start is as good as
/// missing.
fn decode_error(path: &Path, error: &windows::core::Error) -> Error {
    if error.code() == WINCODEC_ERR_COMPONENTINITIALIZEFAILURE
        && let Some(codec) = extension_codec(path)
    {
        return codec_missing(path, codec);
    }
    Error::new(
        ErrorCode::DecodeFailed,
        format!("{}: {}", path.display(), error.message()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        Format, GifFrame, Pixels, Rgba, TempDir, write_animated_gif, write_image,
        write_jpeg_with_orientation,
    };

    const COLOURS: [Rgba; 4] = [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE];

    fn to_pixels(decoded: &Decoded) -> Pixels {
        Pixels {
            width: decoded.bitmap_size.width,
            height: decoded.bitmap_size.height,
            data: decoded.pixels.clone(),
        }
    }

    /// The colour at the centre of each quadrant: top-left, top-right,
    /// bottom-left, bottom-right.
    fn quadrant_colours(pixels: &Pixels) -> [Rgba; 4] {
        let (w, h) = (pixels.width, pixels.height);
        [
            pixels.get(w / 4, h / 4),
            pixels.get(w * 3 / 4, h / 4),
            pixels.get(w / 4, h * 3 / 4),
            pixels.get(w * 3 / 4, h * 3 / 4),
        ]
    }

    fn near(actual: Rgba, expected: Rgba) -> bool {
        [
            (actual.r, expected.r),
            (actual.g, expected.g),
            (actual.b, expected.b),
            (actual.a, expected.a),
        ]
        .iter()
        .all(|(a, e)| a.abs_diff(*e) <= 8)
    }

    #[test]
    fn every_format_decodes_to_the_generated_pixels() {
        let dir = TempDir::new().unwrap();
        let image = Pixels::quadrants(64, 48, COLOURS);
        for format in Format::ALL {
            let path = dir.join(format!("quadrants.{}", format.extension()));
            write_image(&path, &image, format).unwrap();
            let decoded = decode(&path, DecodeTarget::Full).unwrap();
            assert_eq!(decoded.size, PixelSize::new(64, 48), "{format:?}");
            assert_eq!(decoded.bitmap_size, decoded.size, "{format:?}");
            let actual = to_pixels(&decoded);
            if format == Format::Jpeg {
                for (a, e) in quadrant_colours(&actual).into_iter().zip(COLOURS) {
                    assert!(near(a, e), "{format:?}: {a:?} is not near {e:?}");
                }
            } else {
                crate::testutil::assert_similar(&image, &actual, 0);
            }
        }
    }

    #[test]
    fn alpha_is_premultiplied() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("alpha.png");
        let colour = Rgba {
            r: 200,
            g: 100,
            b: 50,
            a: 128,
        };
        write_image(&path, &Pixels::solid(4, 4, colour), Format::Png).unwrap();
        let decoded = decode(&path, DecodeTarget::Full).unwrap();
        let premultiply = |c: u8| ((u32::from(c) * 128 + 127) / 255) as u8;
        let pixel = to_pixels(&decoded).get(1, 1);
        assert_eq!(pixel.a, 128);
        for (actual, straight) in [(pixel.r, 200), (pixel.g, 100), (pixel.b, 50)] {
            assert!(actual.abs_diff(premultiply(straight)) <= 1);
        }
    }

    #[test]
    fn every_exif_orientation_is_honoured() {
        let dir = TempDir::new().unwrap();
        let [r, g, b, w] = COLOURS;
        // What each orientation shows, as top-left, top-right, bottom-left,
        // bottom-right, for stored quadrants red, green, blue, white.
        let expected = [
            (1, [r, g, b, w]),
            (2, [g, r, w, b]),
            (3, [w, b, g, r]),
            (4, [b, w, r, g]),
            (5, [r, b, g, w]),
            (6, [b, r, w, g]),
            (7, [w, g, b, r]),
            (8, [g, w, r, b]),
        ];
        let stored = Pixels::quadrants(64, 32, COLOURS);
        for (orientation, corners) in expected {
            let path = dir.join(format!("oriented-{orientation}.jpg"));
            write_jpeg_with_orientation(&path, &stored, orientation).unwrap();
            let source = Decoder::new().unwrap().open(&path).unwrap();
            assert_eq!(source.orientation(), Orientation::from_exif(orientation));
            let decoded = source.decode(DecodeTarget::Full).unwrap();
            let sideways = orientation >= 5;
            let size = if sideways { (32, 64) } else { (64, 32) };
            assert_eq!(
                (decoded.size.width, decoded.size.height),
                size,
                "orientation {orientation}"
            );
            let actual = quadrant_colours(&to_pixels(&decoded));
            for (a, e) in actual.into_iter().zip(corners) {
                assert!(near(a, e), "orientation {orientation}: {actual:?}");
            }
        }
    }

    #[test]
    fn orientation_applies_to_raw_pixels() {
        // A 3 × 2 image whose pixels are numbered 0 to 5 in reading order.
        let pixels: Vec<u8> = (0..6u8).flat_map(|i| [i, 0, 0, 0]).collect();
        let size = PixelSize::new(3, 2);
        let order = |orientation: Orientation| {
            let (turned, out) = orientation.apply(size, pixels.clone());
            let numbers: Vec<u8> = out.chunks(4).map(|p| p[0]).collect();
            ((turned.width, turned.height), numbers)
        };
        assert_eq!(order(Orientation::Normal), ((3, 2), vec![0, 1, 2, 3, 4, 5]));
        assert_eq!(
            order(Orientation::Rotate90),
            ((2, 3), vec![3, 0, 4, 1, 5, 2])
        );
        assert_eq!(
            order(Orientation::Rotate270),
            ((2, 3), vec![2, 5, 1, 4, 0, 3])
        );
        assert_eq!(
            order(Orientation::Transpose),
            ((2, 3), vec![0, 3, 1, 4, 2, 5])
        );
        assert_eq!(
            order(Orientation::Transverse),
            ((2, 3), vec![5, 2, 4, 1, 3, 0])
        );
        assert_eq!(
            order(Orientation::Rotate180),
            ((3, 2), vec![5, 4, 3, 2, 1, 0])
        );
        assert_eq!(
            order(Orientation::MirrorHorizontal),
            ((3, 2), vec![2, 1, 0, 5, 4, 3])
        );
        assert_eq!(
            order(Orientation::MirrorVertical),
            ((3, 2), vec![3, 4, 5, 0, 1, 2])
        );
        assert_eq!(Orientation::from_exif(0), Orientation::Normal);
        assert_eq!(Orientation::from_exif(9), Orientation::Normal);
    }

    #[test]
    fn scaled_decodes_are_no_smaller_than_requested() {
        let dir = TempDir::new().unwrap();
        let image = Pixels::quadrants(800, 600, COLOURS);
        for format in [Format::Jpeg, Format::Png] {
            let path = dir.join(format!("large.{}", format.extension()));
            write_image(&path, &image, format).unwrap();
            let source = Decoder::new().unwrap().open(&path).unwrap();
            for at_least in [
                PixelSize::new(90, 10),
                PixelSize::new(150, 150),
                PixelSize::new(401, 1),
                PixelSize::new(1000, 100),
            ] {
                let decoded = source.decode(DecodeTarget::Scaled { at_least }).unwrap();
                let got = decoded.bitmap_size;
                assert_eq!(decoded.size, PixelSize::new(800, 600));
                assert!(
                    got.width >= at_least.width.min(800) && got.height >= at_least.height.min(600),
                    "{format:?}: asked for {at_least:?}, got {got:?}"
                );
                assert_eq!(
                    got,
                    target_size(decoded.size, DecodeTarget::Scaled { at_least })
                );
                let colours = quadrant_colours(&to_pixels(&decoded));
                for (a, e) in colours.into_iter().zip(COLOURS) {
                    assert!(near(a, e), "{format:?} at {got:?}: {colours:?}");
                }
            }
        }
    }

    #[test]
    fn sideways_images_scale_in_their_shown_orientation() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("sideways.jpg");
        write_jpeg_with_orientation(&path, &Pixels::quadrants(800, 400, COLOURS), 6).unwrap();
        let at_least = PixelSize::new(100, 300);
        let decoded = decode(&path, DecodeTarget::Scaled { at_least }).unwrap();
        assert_eq!(decoded.size, PixelSize::new(400, 800));
        assert_eq!(decoded.bitmap_size, PixelSize::new(150, 300));
    }

    #[test]
    fn thumbnails_fit_within_the_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("thumb.bmp");
        write_image(&path, &Pixels::quadrants(300, 100, COLOURS), Format::Bmp).unwrap();
        let at_most = PixelSize::new(64, 64);
        let decoded = decode(&path, DecodeTarget::Thumbnail { at_most }).unwrap();
        assert_eq!(decoded.bitmap_size, PixelSize::new(64, 21));
    }

    #[test]
    fn target_sizes() {
        let size = PixelSize::new(4000, 3000);
        let scaled = |w, h| DecodeTarget::Scaled {
            at_least: PixelSize::new(w, h),
        };
        assert_eq!(target_size(size, DecodeTarget::Full), size);
        let fit = |w, h| DecodeTarget::Fit {
            within: PixelSize::new(w, h),
        };
        assert_eq!(
            target_size(size, fit(2048, 1100)),
            PixelSize::new(1467, 1100)
        );
        assert_eq!(
            target_size(size, fit(1000, 3000)),
            PixelSize::new(1000, 750)
        );
        assert_eq!(target_size(size, fit(5000, 5000)), size);
        assert_eq!(
            target_size(PixelSize::new(0, 0), fit(10, 10)),
            PixelSize::new(0, 0)
        );
        assert_eq!(
            target_size(size, scaled(1000, 100)),
            PixelSize::new(1000, 750)
        );
        assert_eq!(
            target_size(size, scaled(100, 1500)),
            PixelSize::new(2000, 1500)
        );
        assert_eq!(target_size(size, scaled(5000, 10)), size);
        assert_eq!(
            target_size(size, scaled(1001, 1)),
            PixelSize::new(1001, 751)
        );
        let thumbnail = |w, h| DecodeTarget::Thumbnail {
            at_most: PixelSize::new(w, h),
        };
        assert_eq!(
            target_size(size, thumbnail(100, 100)),
            PixelSize::new(100, 75)
        );
        assert_eq!(target_size(size, thumbnail(9000, 9000)), size);
        assert_eq!(
            target_size(PixelSize::new(10000, 1), thumbnail(10, 10)),
            PixelSize::new(10, 1)
        );
        let empty = PixelSize::new(0, 0);
        assert_eq!(target_size(empty, scaled(10, 10)), empty);
        assert_eq!(target_size(empty, thumbnail(10, 10)), empty);
    }

    #[test]
    fn missing_and_unsupported_files_say_why() {
        let dir = TempDir::new().unwrap();
        let error = decode(&dir.join("absent.png"), DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::FileNotFound);
        assert!(error.message.contains("absent.png"), "{}", error.message);

        let text = dir.join("notes.txt");
        std::fs::write(&text, "not an image").unwrap();
        let error = decode(&text, DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedFormat);

        let fake = dir.join("photo.heic");
        std::fs::write(&fake, "not an image either").unwrap();
        let error = decode(&fake, DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::CodecMissing);
        assert!(error.message.contains("HEIF"), "{}", error.message);
    }

    #[test]
    fn truncated_files_fail_to_decode() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("cut.bmp");
        write_image(&path, &Pixels::quadrants(200, 200, COLOURS), Format::Bmp).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        let source = Decoder::new().unwrap().open(&path);
        let error = match source {
            Ok(source) => {
                assert_eq!(source.path(), path);
                source.decode(DecodeTarget::Full).unwrap_err()
            }
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::DecodeFailed, "{}", error.message);
    }

    const CLEAR: Rgba = Rgba {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// Premultiplied BGRA pixels as colours with straight alpha, which for
    /// opaque and fully transparent pixels is the same thing.
    fn frame_pixels(size: PixelSize, pixels: &[u8]) -> Pixels {
        to_pixels(&Decoded {
            size,
            bitmap_size: size,
            pixels: pixels.to_vec(),
            content: Content::Still,
        })
    }

    /// A 4 × 2 animation: red all over; then a blue square with one
    /// transparent pixel at the right, cleared after it is shown; then one
    /// green pixel at the top-left.
    fn write_animation(path: &Path, loop_count: Option<u16>) {
        let mut patch = Pixels::solid(2, 2, Rgba::BLUE);
        patch.set(0, 1, CLEAR);
        let frames = [
            GifFrame::whole(Pixels::solid(4, 2, Rgba::RED), 0),
            GifFrame {
                pixels: patch,
                left: 2,
                top: 0,
                delay: 20,
                disposal: 2,
            },
            GifFrame {
                pixels: Pixels::solid(1, 1, Rgba::GREEN),
                left: 0,
                top: 0,
                delay: 5,
                disposal: 1,
            },
        ];
        write_animated_gif(path, 4, 2, &frames, loop_count).unwrap();
    }

    #[test]
    fn animated_gifs_decode_to_composed_frames() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("animated.gif");
        write_animation(&path, Some(0));
        let size = PixelSize::new(4, 2);
        // Even a preview is the whole animation, so it can play at once.
        let fit = DecodeTarget::Fit {
            within: PixelSize::new(2, 1),
        };
        let decoded = decode(&path, fit).unwrap();
        assert_eq!((decoded.size, decoded.bitmap_size), (size, size));
        let Content::Animation(animation) = &decoded.content else {
            panic!("not an animation: {:?}", decoded.content);
        };
        assert_eq!(animation.plays, None);
        assert_eq!(decoded.pixels, animation.frames[0].pixels);
        let delays: Vec<u64> = animation
            .frames
            .iter()
            .map(|frame| frame.delay.as_millis() as u64)
            .collect();
        assert_eq!(delays, [100, 200, 50]);
        let (r, b, g) = (Rgba::RED, Rgba::BLUE, Rgba::GREEN);
        let expected = [
            [[r, r, r, r], [r, r, r, r]],
            [[r, r, b, b], [r, r, r, b]],
            [[g, r, CLEAR, CLEAR], [r, r, CLEAR, CLEAR]],
        ];
        for (index, (frame, rows)) in animation.frames.iter().zip(expected).enumerate() {
            let pixels = frame_pixels(size, &frame.pixels);
            for (y, row) in rows.iter().enumerate() {
                for (x, colour) in row.iter().enumerate() {
                    assert_eq!(
                        pixels.get(x as u32, y as u32),
                        *colour,
                        "frame {index} at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn gifs_without_a_loop_count_play_once_and_thumbnails_are_still() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("once.gif");
        write_animation(&path, None);
        let source = Decoder::new().unwrap().open(&path).unwrap();
        assert_eq!(source.orientation(), Orientation::Normal);
        let Content::Animation(animation) = source.decode(DecodeTarget::Full).unwrap().content
        else {
            panic!("not an animation");
        };
        assert_eq!(animation.plays, Some(1));
        let at_most = PixelSize::new(2, 2);
        let thumbnail = source.decode(DecodeTarget::Thumbnail { at_most }).unwrap();
        assert_eq!(thumbnail.content, Content::Still);
        assert_eq!(thumbnail.bitmap_size, PixelSize::new(2, 1));
        assert_eq!(to_pixels(&thumbnail).get(0, 0), Rgba::RED);
    }

    #[test]
    fn single_frame_gifs_are_still() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("still.gif");
        write_image(&path, &Pixels::quadrants(8, 8, COLOURS), Format::Gif).unwrap();
        let decoded = decode(&path, DecodeTarget::Full).unwrap();
        assert_eq!(decoded.content, Content::Still);
        assert_eq!(to_pixels(&decoded).get(6, 1), Rgba::GREEN);
    }

    const HALVES: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
        <rect width="20" height="20" fill="red"/>
        <rect x="20" width="20" height="20" fill="blue"/>
    </svg>"#;

    #[test]
    fn svg_documents_rasterise_at_their_intrinsic_size_or_smaller() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("halves.SVG");
        std::fs::write(&path, HALVES).unwrap();
        let source = Decoder::new().unwrap().open(&path).unwrap();
        assert_eq!(source.size(), PixelSize::new(40, 20));
        assert_eq!(source.orientation(), Orientation::Normal);
        let full = source.decode(DecodeTarget::Full).unwrap();
        assert_eq!(full.bitmap_size, PixelSize::new(40, 20));
        assert_eq!(full.content, Content::Vector(HALVES.as_bytes().into()));
        let pixels = to_pixels(&full);
        assert_eq!(pixels.get(5, 10), Rgba::RED);
        assert_eq!(pixels.get(35, 10), Rgba::BLUE);
        let fit = DecodeTarget::Fit {
            within: PixelSize::new(20, 20),
        };
        let small = source.decode(fit).unwrap();
        assert_eq!(small.bitmap_size, PixelSize::new(20, 10));
        assert_eq!(to_pixels(&small).get(15, 5), Rgba::BLUE);
    }

    #[test]
    fn huge_svg_documents_are_rasterised_no_larger_than_the_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("huge.svg");
        std::fs::write(
            &path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20000 100"/>"#,
        )
        .unwrap();
        let decoded = decode(&path, DecodeTarget::Full).unwrap();
        assert_eq!(decoded.size, PixelSize::new(20000, 100));
        assert_eq!(decoded.bitmap_size, PixelSize::new(svg::MAX_RASTER, 40));
    }

    #[test]
    fn broken_and_missing_svg_files_say_why() {
        let dir = TempDir::new().unwrap();
        let error = decode(&dir.join("absent.svg"), DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::FileNotFound);
        let path = dir.join("broken.svg");
        std::fs::write(&path, "<svg").unwrap();
        let error = decode(&path, DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::DecodeFailed, "{}", error.message);
        assert!(error.message.contains("broken.svg"), "{}", error.message);
        let folder = dir.join("folder.svg");
        std::fs::create_dir(&folder).unwrap();
        let error = decode(&folder, DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::DecodeFailed, "{}", error.message);
    }

    #[test]
    fn missing_codecs_are_named_only_when_no_decoder_claims_the_extension() {
        let decodable = decodable_extensions().unwrap();
        for known in ["png", "jpg", "gif", "bmp", "tiff"] {
            assert!(decodable.iter().any(|e| e == known), "{decodable:?}");
        }
        assert!(missing_codec(Path::new("a.png")).is_none());
        assert!(missing_codec(Path::new("noextension")).is_none());
        for (extension, codec) in EXTENSION_CODECS {
            let path = PathBuf::from(format!("image.{extension}"));
            match missing_codec(&path) {
                Some(error) => {
                    assert!(!decodable.iter().any(|e| e == extension));
                    assert_eq!(error.code, ErrorCode::CodecMissing);
                    assert!(error.message.contains(codec), "{}", error.message);
                }
                None => assert!(decodable.iter().any(|e| e == extension)),
            }
        }
    }
}
