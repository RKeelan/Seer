//! SVG documents, loaded and drawn through Direct2D's SVG support.
//!
//! An [`Svg`] is a file's text and its intrinsic size, which becomes the
//! image's size in pixels: the root element's `width` and `height`, or its
//! `viewBox` where they are missing, or 300 × 150 as in a browser. Decoding
//! rasterises the document at the size asked for, in software, so previews,
//! thumbnails and screenshots work as for any image; the renderer draws the
//! document itself as vectors wherever it is shown, so it stays sharp at any
//! zoom.
//!
//! Direct2D draws what it supports of SVG: no text elements, and only some
//! of CSS.

use std::cell::OnceCell;
use std::path::Path;
use std::sync::Arc;

use windows::Win32::Graphics::Direct2D::Common::{D2D_SIZE_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_SVG_ATTRIBUTE_POD_TYPE,
    D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH, D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX, D2D1_SVG_LENGTH,
    D2D1_SVG_LENGTH_UNITS_NUMBER, D2D1_SVG_VIEWBOX, D2D1CreateFactory, ID2D1DeviceContext5,
    ID2D1Factory1, ID2D1SvgDocument, ID2D1SvgElement,
};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory, WICBitmapCacheOnLoad,
};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{BOOL, Interface, PCWSTR, w};
use windows_numerics::Matrix3x2;

use crate::model::geometry::PixelSize;
use crate::render::device::PIXEL_FORMAT;

/// The size of an SVG document with neither a size nor a `viewBox`, as
/// browsers give it.
pub const DEFAULT_SIZE: PixelSize = PixelSize::new(300, 150);

/// The largest a document is rasterised, in either dimension.
pub const MAX_RASTER: u32 = 8192;

/// Whether `path` names an SVG file, by its extension.
pub fn is_svg(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
}

/// An SVG document's intrinsic size in pixels, from its root element's
/// `width` and `height`, where they are plain numbers, and its `viewBox`'s
/// width and height. A missing dimension follows the `viewBox`'s aspect
/// ratio, or the default size.
pub fn intrinsic_size(
    width: Option<f32>,
    height: Option<f32>,
    view_box: Option<(f32, f32)>,
) -> PixelSize {
    let positive = |value: Option<f32>| value.filter(|v| v.is_finite() && *v > 0.0);
    let (width, height) = (positive(width), positive(height));
    let view_box =
        view_box.filter(|(w, h)| positive(Some(*w)).is_some() && positive(Some(*h)).is_some());
    let (width, height) = match (width, height, view_box) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some((vw, vh))) => (w, w * vh / vw),
        (None, Some(h), Some((vw, vh))) => (h * vw / vh, h),
        (None, None, Some((vw, vh))) => (vw, vh),
        (w, h, None) => (
            w.unwrap_or(DEFAULT_SIZE.width as f32),
            h.unwrap_or(DEFAULT_SIZE.height as f32),
        ),
    };
    let pixels = |value: f32| (value.ceil() as u32).clamp(1, u32::from(u16::MAX));
    PixelSize::new(pixels(width), pixels(height))
}

/// Parses `source` into a document on `context`, with a viewport of
/// `viewport`.
pub fn document(
    context: &ID2D1DeviceContext5,
    source: &[u8],
    viewport: PixelSize,
) -> windows::core::Result<ID2D1SvgDocument> {
    // SAFETY: the stream copies the bytes.
    let stream = unsafe { SHCreateMemStream(Some(source)) };
    let size = D2D_SIZE_F {
        width: viewport.width as f32,
        height: viewport.height as f32,
    };
    // SAFETY: the stream is live, or absent, which gives an empty document.
    unsafe { context.CreateSvgDocument(stream.as_ref(), size) }
}

/// An SVG file's text and intrinsic size.
pub struct Svg {
    d2d: ID2D1Factory1,
    wic: IWICImagingFactory,
    source: Arc<[u8]>,
    size: PixelSize,
}

impl Svg {
    /// Reads the document in `source`, using `d2d` and `wic` to parse and
    /// rasterise it.
    pub fn new(
        d2d: &ID2D1Factory1,
        wic: &IWICImagingFactory,
        source: Vec<u8>,
    ) -> windows::core::Result<Self> {
        let mut svg = Svg {
            d2d: d2d.clone(),
            wic: wic.clone(),
            source: source.into(),
            size: PixelSize::new(1, 1),
        };
        let (_, context) = svg.target(svg.size)?;
        let document = document(&context, &svg.source, svg.size)?;
        // SAFETY: GetRoot has no arguments.
        let root = unsafe { document.GetRoot() }?;
        svg.size = intrinsic_size(
            length(&root, w!("width")),
            length(&root, w!("height")),
            view_box(&root),
        );
        Ok(svg)
    }

    /// The document's intrinsic size.
    pub fn size(&self) -> PixelSize {
        self.size
    }

    /// The file's text.
    pub fn source(&self) -> &Arc<[u8]> {
        &self.source
    }

    /// Rasterises the document at `size`, as premultiplied BGRA.
    pub fn render(&self, size: PixelSize) -> windows::core::Result<Vec<u8>> {
        let (bitmap, context) = self.target(size)?;
        let document = document(&context, &self.source, self.size)?;
        let scale = Matrix3x2 {
            M11: size.width as f32 / self.size.width as f32,
            M22: size.height as f32 / self.size.height as f32,
            ..Matrix3x2::identity()
        };
        // SAFETY: the transform, colour and document are live, and EndDraw
        // closes BeginDraw.
        unsafe {
            context.BeginDraw();
            context.SetTransform(&scale);
            context.Clear(Some(&D2D1_COLOR_F::default()));
            context.DrawSvgDocument(&document);
            context.EndDraw(None, None)?;
        }
        let stride = size.width * 4;
        let mut pixels = vec![0; stride as usize * size.height as usize];
        // SAFETY: a null rectangle copies the whole bitmap, which is `size`
        // in premultiplied BGRA and fits the buffer at this stride.
        unsafe { bitmap.CopyPixels(std::ptr::null(), stride, &mut pixels) }?;
        Ok(pixels)
    }

    /// A WIC bitmap of `size` and a software Direct2D context that draws to
    /// it.
    fn target(
        &self,
        size: PixelSize,
    ) -> windows::core::Result<(
        windows::Win32::Graphics::Imaging::IWICBitmap,
        ID2D1DeviceContext5,
    )> {
        // SAFETY: the GUID outlives the call; the rest are plain values.
        let bitmap = unsafe {
            self.wic.CreateBitmap(
                size.width,
                size.height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )
        }?;
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: PIXEL_FORMAT,
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        // SAFETY: the bitmap and properties are live.
        let target = unsafe { self.d2d.CreateWicBitmapRenderTarget(&bitmap, &properties) }?;
        Ok((bitmap, target.cast()?))
    }
}

/// Creates the Direct2D factory an [`Svg`] needs, once per thread that asks.
pub fn factory(cell: &OnceCell<ID2D1Factory1>) -> windows::core::Result<ID2D1Factory1> {
    if let Some(factory) = cell.get() {
        return Ok(factory.clone());
    }
    // SAFETY: no options are given.
    let factory: ID2D1Factory1 =
        unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
    Ok(cell.get_or_init(|| factory).clone())
}

/// The value of a length attribute of `element`, if it is specified as a
/// plain number.
fn length(element: &ID2D1SvgElement, name: PCWSTR) -> Option<f32> {
    let mut value = D2D1_SVG_LENGTH::default();
    attribute(
        element,
        name,
        D2D1_SVG_ATTRIBUTE_POD_TYPE_LENGTH,
        &mut value,
    )?;
    (value.units == D2D1_SVG_LENGTH_UNITS_NUMBER).then_some(value.value)
}

/// The width and height of `element`'s `viewBox`, if it has one.
fn view_box(element: &ID2D1SvgElement) -> Option<(f32, f32)> {
    let mut value = D2D1_SVG_VIEWBOX::default();
    attribute(
        element,
        w!("viewBox"),
        D2D1_SVG_ATTRIBUTE_POD_TYPE_VIEWBOX,
        &mut value,
    )?;
    Some((value.width, value.height))
}

/// Reads an attribute of `element` that is specified on it into `value`,
/// whose type matches `kind`.
fn attribute<T>(
    element: &ID2D1SvgElement,
    name: PCWSTR,
    kind: D2D1_SVG_ATTRIBUTE_POD_TYPE,
    value: &mut T,
) -> Option<()> {
    // SAFETY: the name is a static string and no inherited flag is asked for.
    let specified: BOOL = unsafe { element.IsAttributeSpecified(name, None) };
    if !specified.as_bool() {
        return None;
    }
    // SAFETY: `value` is a live `T` of the size passed, which is the type
    // `kind` names.
    unsafe {
        element.GetAttributeValue2(
            name,
            kind,
            std::ptr::from_mut(value).cast(),
            size_of::<T>() as u32,
        )
    }
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_intrinsic_size_comes_from_the_size_then_the_view_box() {
        assert_eq!(
            intrinsic_size(Some(40.0), Some(20.5), Some((10.0, 10.0))),
            PixelSize::new(40, 21)
        );
        assert_eq!(
            intrinsic_size(Some(40.0), None, Some((200.0, 100.0))),
            PixelSize::new(40, 20)
        );
        assert_eq!(
            intrinsic_size(None, Some(40.0), Some((200.0, 100.0))),
            PixelSize::new(80, 40)
        );
        assert_eq!(
            intrinsic_size(None, None, Some((64.0, 48.0))),
            PixelSize::new(64, 48)
        );
        assert_eq!(intrinsic_size(None, None, None), DEFAULT_SIZE);
        assert_eq!(
            intrinsic_size(Some(10.0), None, None),
            PixelSize::new(10, 150)
        );
        // Nonsense values count as missing.
        assert_eq!(
            intrinsic_size(Some(-5.0), Some(f32::NAN), Some((0.0, 10.0))),
            DEFAULT_SIZE
        );
        assert_eq!(
            intrinsic_size(Some(1e9), Some(0.25), None),
            PixelSize::new(65535, 1)
        );
    }

    #[test]
    fn svg_files_are_known_by_their_extension() {
        assert!(is_svg(Path::new("a.svg")));
        assert!(is_svg(Path::new("B.SVG")));
        assert!(!is_svg(Path::new("svg")));
        assert!(!is_svg(Path::new("a.png")));
    }
}
