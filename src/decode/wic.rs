//! Still images through the Windows Imaging Component.
//!
//! [`Decoder`] opens a file as a [`Source`], whose first frame can be decoded
//! at any [`DecodeTarget`]: scaled to cover a size, at full resolution, or as
//! a thumbnail. Every result is premultiplied BGRA with the EXIF orientation
//! applied, ready to become a Direct2D bitmap.
//!
//! Scaled decodes are cheap for JPEG: the codec can scale by 1/2, 1/4 or 1/8
//! while decoding, so a [`Source`] asks it for the smallest such size that is
//! still large enough, and finishes with WIC's scaler.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ, WINCODEC_ERR_COMPONENTNOTFOUND,
    WINCODEC_ERR_UNKNOWNIMAGEFORMAT,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICBitmapFrameDecode,
    IWICBitmapSource, IWICBitmapSourceTransform, IWICImagingFactory, IWICPalette,
    IWICPixelFormatInfo, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom, WICBitmapTransformRotate0, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::VT_UI2;
use windows::core::{GUID, HRESULT, HSTRING, Interface, w};

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
}

/// Opens image files. Each thread that decodes needs its own.
pub struct Decoder {
    factory: IWICImagingFactory,
}

impl Decoder {
    /// Initialises COM on this thread, if it is not already, and creates the
    /// WIC factory.
    pub fn new() -> Result<Self> {
        // SAFETY: initialising COM has no preconditions. It reports S_FALSE or
        // RPC_E_CHANGED_MODE when COM is already initialised on this thread,
        // and COM is usable either way, so the result is ignored.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // SAFETY: the class id is a constant and no outer object is given.
        let factory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
        Ok(Decoder { factory })
    }

    /// Opens `path` and reads its first frame's size and orientation.
    pub fn open(&self, path: &Path) -> Result<Source> {
        let failed = |error: windows::core::Error| open_error(path, &error);
        // SAFETY: the path outlives the call and no vendor is preferred.
        let decoder = unsafe {
            self.factory.CreateDecoderFromFilename(
                &HSTRING::from(path),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
        }
        .map_err(failed)?;
        // SAFETY: GetFrame takes a plain index.
        let frame = unsafe { decoder.GetFrame(0) }.map_err(failed)?;
        let (mut width, mut height) = (0, 0);
        // SAFETY: both pointers are to live locals.
        unsafe { frame.GetSize(&mut width, &mut height) }.map_err(failed)?;
        Ok(Source {
            factory: self.factory.clone(),
            path: path.to_owned(),
            orientation: Orientation::from_exif(read_orientation(&frame)),
            stored: PixelSize::new(width, height),
            frame,
        })
    }
}

/// An open image file's first frame.
pub struct Source {
    factory: IWICImagingFactory,
    frame: IWICBitmapFrameDecode,
    path: PathBuf,
    stored: PixelSize,
    orientation: Orientation,
}

impl Source {
    /// The path the source was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The image's full size, after orientation.
    pub fn size(&self) -> PixelSize {
        self.orientation.apply_size(self.stored)
    }

    /// How the stored pixels are turned to show the image upright.
    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Decodes the frame at `target`'s size.
    pub fn decode(&self, target: DecodeTarget) -> Result<Decoded> {
        let size = self.size();
        let want = self.orientation.apply_size(target_size(size, target));
        let pixels = self
            .pixels(want)
            .map_err(|error| decode_error(&self.path, &error))?;
        let (bitmap_size, pixels) = self.orientation.apply(want, pixels);
        Ok(Decoded {
            size,
            bitmap_size,
            pixels,
        })
    }

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
    let shown = path.display();
    let code = error.code();
    if code == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
        || code == HRESULT::from_win32(ERROR_PATH_NOT_FOUND.0)
    {
        return Error::new(ErrorCode::FileNotFound, format!("{shown}: no such file"));
    }
    if code == WINCODEC_ERR_COMPONENTNOTFOUND || code == WINCODEC_ERR_UNKNOWNIMAGEFORMAT {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if let Some((_, codec)) = EXTENSION_CODECS
            .iter()
            .find(|(known, _)| Some(*known) == extension.as_deref())
        {
            return Error::new(
                ErrorCode::CodecMissing,
                format!("{shown}: decoding it needs the {codec} from the Microsoft Store"),
            );
        }
        return Error::new(
            ErrorCode::UnsupportedFormat,
            format!("{shown}: not an image format Windows can decode"),
        );
    }
    decode_error(path, error)
}

fn decode_error(path: &Path, error: &windows::core::Error) -> Error {
    Error::new(
        ErrorCode::DecodeFailed,
        format!("{}: {}", path.display(), error.message()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        Format, Pixels, Rgba, TempDir, write_image, write_jpeg_with_orientation,
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
}
