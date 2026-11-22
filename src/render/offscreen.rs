//! Drawing a scene to a bitmap in memory, reading bitmaps back from the
//! device, and encoding the result as PNG, for `screenshot`, copying to the
//! clipboard and the render tests. None of it needs a window, let alone a
//! visible one.

use std::path::Path;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_U;
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_CPU_READ, D2D1_MAP_OPTIONS_READ,
    ID2D1Bitmap,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag2;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, STREAM_SEEK_END,
    STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::error::{Error, Result};
use crate::model::geometry::{PixelRect, PixelSize};
use crate::render::device::{Device, bitmap_properties, d2d_size};
use crate::render::draw::{Bitmaps, Renderer};
use crate::render::scene::Scene;

/// A rendered image: rows top to bottom, each pixel four bytes in the order
/// blue, green, red, alpha. Its alpha is straight, as in WIC's `32bppBGRA`,
/// except as [`read_back`] returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    /// The size in pixels.
    pub size: PixelSize,
    /// `width × height × 4` bytes.
    pub pixels: Vec<u8>,
}

/// Draws `scene` to an offscreen bitmap of the scene's size, with the images'
/// and thumbnails' bitmaps, and reads it back.
/// The device context's target is left unset.
pub fn render(
    device: &Device,
    renderer: &mut Renderer,
    scene: &Scene,
    bitmaps: &Bitmaps,
    thumbnails: &Bitmaps,
) -> Result<Capture> {
    let size = PixelSize::new(scene.size.width.max(1), scene.size.height.max(1));
    let context = device.context();
    let target = device.create_target(size)?;
    // SAFETY: the target was made on this context's device.
    unsafe { context.SetTarget(&target) };
    let drawn = renderer.draw(context, scene, bitmaps, thumbnails);
    // SAFETY: releasing the target only drops the context's reference.
    unsafe { context.SetTarget(None) };
    drawn?;
    let mut capture = read_back(device, &target, PixelRect::whole(size))?;
    unpremultiply(&mut capture.pixels);
    Ok(capture)
}

/// Reads `region` of `bitmap`, a premultiplied bitmap on `device`, back to
/// memory, still premultiplied; [`unpremultiply`] makes the alpha straight.
/// `region` must lie within the bitmap.
pub fn read_back(device: &Device, bitmap: &ID2D1Bitmap, region: PixelRect) -> Result<Capture> {
    let size = PixelSize::new(region.width, region.height);
    let readable =
        bitmap_properties(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
    // SAFETY: no source data is given and the properties are a live local.
    let copy = unsafe {
        device
            .context()
            .CreateBitmap(d2d_size(size), None, 0, &readable)
    }?;
    let source = D2D_RECT_U {
        left: region.x,
        top: region.y,
        right: region.x + region.width,
        bottom: region.y + region.height,
    };
    // SAFETY: both bitmaps are on the same device, and the source rectangle,
    // a live local, is the copy's size; a null point copies to its top-left.
    unsafe { copy.CopyFromBitmap(None, bitmap, Some(&source)) }?;
    // SAFETY: the bitmap was created for CPU reads.
    let mapped = unsafe { copy.Map(D2D1_MAP_OPTIONS_READ) }?;
    let row = size.width as usize * 4;
    let mut pixels = Vec::with_capacity(row * size.height as usize);
    for y in 0..size.height as usize {
        // SAFETY: Map returned a buffer of `height` rows `pitch` bytes apart,
        // each holding at least `width × 4` bytes, valid until Unmap.
        let line =
            unsafe { std::slice::from_raw_parts(mapped.bits.add(y * mapped.pitch as usize), row) };
        pixels.extend_from_slice(line);
    }
    // SAFETY: the bitmap is mapped, and the slices above are no longer used.
    unsafe { copy.Unmap() }?;
    Ok(Capture { size, pixels })
}

/// Writes `capture` to `path` as a PNG.
pub fn write_png(path: &Path, capture: &Capture) -> Result<()> {
    std::fs::write(path, encode_png(capture)?)?;
    Ok(())
}

/// Encodes `capture` as a PNG file's bytes.
pub fn encode_png(capture: &Capture) -> Result<Vec<u8>> {
    // SAFETY: initialising COM has no preconditions. It reports S_FALSE or
    // RPC_E_CHANGED_MODE when COM is already initialised on this thread, and
    // COM is usable either way, so the result is ignored.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: the class id is a constant and no outer object is given.
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
    // SAFETY: no initial contents are given.
    let memory = unsafe { SHCreateMemStream(None) }
        .ok_or_else(|| Error::unexpected("could not make a stream in memory"))?;
    // SAFETY: CreateStream has no arguments.
    let stream = unsafe { factory.CreateStream() }?;
    // SAFETY: the memory stream is live.
    unsafe { stream.InitializeFromIStream(&memory) }?;
    // SAFETY: the container GUID outlives the call; no vendor is preferred.
    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()) }?;
    // SAFETY: the stream is live.
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }?;
    let mut frame = None;
    let mut options: Option<IPropertyBag2> = None;
    // SAFETY: both pointers are to live locals, which receive owned
    // references.
    unsafe { encoder.CreateNewFrame(&mut frame, &mut options) }?;
    let frame = frame.ok_or_else(|| Error::unexpected("WIC returned no frame"))?;
    let size = capture.size;
    let mut format = GUID_WICPixelFormat32bppBGRA;
    // SAFETY: the options came from CreateNewFrame; the rest are plain values
    // or live locals.
    unsafe {
        frame.Initialize(options.as_ref())?;
        frame.SetSize(size.width, size.height)?;
        frame.SetPixelFormat(&mut format)?;
    }
    if format != GUID_WICPixelFormat32bppBGRA {
        return Err(Error::unexpected("the PNG encoder refused 32-bit BGRA"));
    }
    // SAFETY: the buffer holds `height` rows of `width × 4` bytes of BGRA.
    unsafe { frame.WritePixels(size.height, size.width * 4, &capture.pixels) }?;
    // SAFETY: Commit has no arguments.
    unsafe { frame.Commit() }?;
    // SAFETY: Commit has no arguments.
    unsafe { encoder.Commit() }?;
    let mut length = 0u64;
    // SAFETY: the out pointer is to a live local.
    unsafe { memory.Seek(0, STREAM_SEEK_END, Some(&mut length)) }?;
    // SAFETY: no out pointer is given.
    unsafe { memory.Seek(0, STREAM_SEEK_SET, None) }?;
    let length = u32::try_from(length).map_err(|_| Error::unexpected("the PNG is over 4 GB"))?;
    let mut bytes = vec![0u8; length as usize];
    let mut read = 0u32;
    // SAFETY: the buffer holds `length` bytes, and the out pointer is to a
    // live local.
    unsafe { memory.Read(bytes.as_mut_ptr().cast(), length, Some(&mut read)) }.ok()?;
    bytes.truncate(read as usize);
    Ok(bytes)
}

/// Converts premultiplied BGRA to straight BGRA in place.
pub fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha != 0 && alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::wic::{Content, Decoded};
    use crate::model::ImageId;
    use crate::model::geometry::{Point, Rect, Transform};
    use crate::render::scene::{Align, Colour, Interpolation, Item};
    use crate::testutil::{Pixels, Rgba, TempDir, assert_similar, read_image};

    fn to_pixels(capture: &Capture) -> Pixels {
        Pixels {
            width: capture.size.width,
            height: capture.size.height,
            data: capture.pixels.clone(),
        }
    }

    fn quadrants() -> Decoded {
        let pixels = Pixels::quadrants(4, 4, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
        Decoded {
            size: PixelSize::new(4, 4),
            bitmap_size: PixelSize::new(4, 4),
            pixels: pixels.data,
            content: Content::Still,
        }
    }

    fn image_item(transform: Transform, clip: Rect) -> Item {
        Item::Image {
            image: ImageId(1),
            size: PixelSize::new(4, 4),
            transform,
            clip,
            interpolation: Interpolation::Nearest,
            opacity: 1.0,
        }
    }

    #[test]
    fn scenes_render_offscreen() {
        let device = Device::warp().unwrap();
        let mut bitmaps = Bitmaps::new();
        bitmaps.insert(ImageId(1), device.create_bitmap(&quadrants()).unwrap());
        let grey = Colour::rgb(40, 40, 40);
        let scene = Scene {
            size: PixelSize::new(16, 12),
            background: grey,
            items: vec![
                image_item(Transform::scale(2.0, 2.0), Rect::new(0.0, 0.0, 16.0, 12.0)),
                Item::FillRect {
                    rect: Rect::new(12.0, 0.0, 4.0, 4.0),
                    colour: Colour::rgb(255, 255, 0),
                },
                // An image with no bitmap draws nothing.
                Item::Image {
                    image: ImageId(2),
                    size: PixelSize::new(4, 4),
                    transform: Transform::IDENTITY,
                    clip: Rect::new(0.0, 0.0, 16.0, 12.0),
                    interpolation: Interpolation::Smooth,
                    opacity: 1.0,
                },
                // A thumbnail is drawn from its own bitmaps; one with none
                // draws nothing.
                Item::Thumbnail {
                    image: ImageId(1),
                    rect: Rect::new(12.0, 8.0, 4.0, 4.0),
                },
                Item::Thumbnail {
                    image: ImageId(2),
                    rect: Rect::new(8.0, 8.0, 4.0, 4.0),
                },
            ],
        };
        let mut thumbnails = Bitmaps::new();
        thumbnails.insert(ImageId(1), device.create_bitmap(&quadrants()).unwrap());
        let capture = render(
            &device,
            &mut Renderer::default(),
            &scene,
            &bitmaps,
            &thumbnails,
        )
        .unwrap();
        let expected = Pixels::from_fn(16, 12, |x, y| match (x, y) {
            (12..14, 8..10) => Rgba::RED,
            (14..16, 8..10) => Rgba::GREEN,
            (12..14, 10..12) => Rgba::BLUE,
            (14..16, 10..12) => Rgba::WHITE,
            (12.., 0..4) => Rgba::opaque(255, 255, 0),
            (0..4, 0..4) => Rgba::RED,
            (4..8, 0..4) => Rgba::GREEN,
            (0..4, 4..8) => Rgba::BLUE,
            (4..8, 4..8) => Rgba::WHITE,
            _ => Rgba::opaque(40, 40, 40),
        });
        assert_similar(&expected, &to_pixels(&capture), 0);
    }

    #[test]
    fn previews_are_stretched_to_the_full_size_and_clipped() {
        let device = Device::warp().unwrap();
        let mut bitmaps = Bitmaps::new();
        // A 4 × 4 preview of an image whose full size is 8 × 8.
        let mut preview = quadrants();
        preview.size = PixelSize::new(8, 8);
        bitmaps.insert(ImageId(1), device.create_bitmap(&preview).unwrap());
        let scene = Scene {
            size: PixelSize::new(8, 8),
            background: Colour::rgb(0, 0, 0),
            items: vec![Item::Image {
                image: ImageId(1),
                size: PixelSize::new(8, 8),
                transform: Transform::IDENTITY,
                clip: Rect::new(0.0, 0.0, 4.0, 8.0),
                interpolation: Interpolation::Nearest,
                opacity: 1.0,
            }],
        };
        let capture = render(
            &device,
            &mut Renderer::default(),
            &scene,
            &bitmaps,
            &Bitmaps::new(),
        )
        .unwrap();
        let expected = Pixels::from_fn(8, 8, |x, y| match (x, y) {
            (0..4, 0..4) => Rgba::RED,
            (0..4, 4..8) => Rgba::BLUE,
            _ => Rgba::BLACK,
        });
        assert_similar(&expected, &to_pixels(&capture), 0);
    }

    #[test]
    fn lines_outlines_and_text_draw_something() {
        let device = Device::warp().unwrap();
        let white = Colour::rgb(255, 255, 255);
        let scene = Scene {
            size: PixelSize::new(200, 60),
            background: Colour::rgb(0, 0, 0),
            items: vec![
                Item::Line {
                    from: Point::new(0.0, 1.0),
                    to: Point::new(200.0, 1.0),
                    colour: white,
                    width: 2.0,
                },
                Item::StrokeRect {
                    rect: Rect::new(10.0, 10.0, 50.0, 40.0),
                    colour: white,
                    width: 2.0,
                    dashed: true,
                },
                Item::StrokeRect {
                    rect: Rect::new(70.0, 10.0, 50.0, 40.0),
                    colour: white,
                    width: 2.0,
                    dashed: false,
                },
                Item::Text {
                    text: "Seer".into(),
                    rect: Rect::new(130.0, 10.0, 70.0, 40.0),
                    colour: white,
                    size: 20.0,
                    align: Align::Centre,
                    bold: true,
                },
            ],
        };
        let mut renderer = Renderer::default();
        let capture = render(
            &device,
            &mut renderer,
            &scene,
            &Bitmaps::new(),
            &Bitmaps::new(),
        )
        .unwrap();
        let pixels = to_pixels(&capture);
        assert_eq!(pixels.get(100, 1), Rgba::WHITE);
        assert_eq!(pixels.get(95, 10).r, 255);
        let lit = |x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (10..50).map(move |y| (x, y)))
                .filter(|&(x, y)| pixels.get(x, y).r > 128)
                .count()
        };
        assert!(lit(130, 200) > 20, "the text drew nothing");
        // The dashed outline lights fewer pixels than the solid one.
        assert!(lit(8, 62) < lit(68, 122));
        // Formats and the dash style are reused on the next frame.
        let again = render(
            &device,
            &mut renderer,
            &scene,
            &Bitmaps::new(),
            &Bitmaps::new(),
        )
        .unwrap();
        assert_eq!(again, capture);
        for align in [Align::Start, Align::End] {
            let scene = Scene {
                items: vec![Item::Text {
                    text: "x".into(),
                    rect: Rect::new(0.0, 0.0, 200.0, 60.0),
                    colour: white,
                    size: 12.0,
                    align,
                    bold: false,
                }],
                ..scene.clone()
            };
            render(
                &device,
                &mut renderer,
                &scene,
                &Bitmaps::new(),
                &Bitmaps::new(),
            )
            .unwrap();
        }
    }

    #[test]
    fn captures_round_trip_through_png() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("capture.png");
        let pixels = Pixels::quadrants(6, 4, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
        let capture = Capture {
            size: PixelSize::new(6, 4),
            pixels: pixels.data.clone(),
        };
        write_png(&path, &capture).unwrap();
        assert_similar(&pixels, &read_image(&path).unwrap(), 0);
        assert!(write_png(&dir.join("no-such-dir").join("x.png"), &capture).is_err());
    }

    #[test]
    fn regions_of_bitmaps_read_back_premultiplied() {
        let device = Device::warp().unwrap();
        let mut decoded = quadrants();
        // The top-left pixel is half-transparent red, premultiplied.
        decoded.pixels[..4].copy_from_slice(&[0, 0, 128, 128]);
        let bitmap = device.create_bitmap(&decoded).unwrap();
        let mut capture = read_back(&device, &bitmap, PixelRect::new(0, 0, 2, 4)).unwrap();
        assert_eq!(capture.pixels[..4], [0, 0, 128, 128]);
        unpremultiply(&mut capture.pixels);
        assert_eq!(capture.size, PixelSize::new(2, 4));
        let mut expected = Pixels::from_fn(2, 4, |_, y| if y < 2 { Rgba::RED } else { Rgba::BLUE });
        expected.data[..4].copy_from_slice(&[0, 0, 255, 128]);
        assert_eq!(to_pixels(&capture), expected);
        let corner = read_back(&device, &bitmap, PixelRect::new(3, 3, 1, 1)).unwrap();
        assert_eq!(corner.pixels, [255, 255, 255, 255]);
    }

    #[test]
    fn unpremultiplying() {
        let mut pixels = [64, 32, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut pixels);
        assert_eq!(pixels, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }
}
