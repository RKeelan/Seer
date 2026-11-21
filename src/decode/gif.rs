//! Animated GIF: frames composed by hand from WIC's frame metadata (delay,
//! disposal, frame rectangle, transparency).
//!
//! WIC decodes each frame of a GIF on its own, at the size of its own
//! rectangle, and leaves composing them to the caller. [`Gif`] reads the
//! logical screen's size and the loop count from the file, and each frame's
//! position, delay and disposal from the frame, and a [`Composer`] draws the
//! frames one over another as a browser does, giving every frame at the full
//! size of the logical screen.
//!
//! Timing follows browsers: a delay of 10 ms or less is shown for 100 ms, and
//! a file without a loop count plays once, while a loop count of `n` plays
//! `n + 1` times and 0 plays forever.

use std::time::Duration;

use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, IWICBitmapDecoder, IWICBitmapFrameDecode, IWICImagingFactory,
    IWICMetadataQueryReader, IWICPalette, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Variant::{VT_UI1, VT_UI2, VT_VECTOR};
use windows::core::{PCWSTR, w};

use crate::model::geometry::{PixelRect, PixelSize};

/// The most memory an animation's frames may take. Frames past it are left
/// out, and the animation plays those that fit. The decoded-image cache
/// charges an animation for all its frames, but never evicts the image on
/// screen, so it cannot bound a single animation; this does.
pub const MAX_ANIMATION_BYTES: u64 = 1 << 30;

/// The shortest delay a frame is shown for as asked; shorter ones, including
/// none, are shown for [`DEFAULT_DELAY`].
const MIN_DELAY: Duration = Duration::from_millis(10);

/// What a frame with too short a delay is shown for.
pub const DEFAULT_DELAY: Duration = Duration::from_millis(100);

/// A decoded animation: every frame at the full size of the image,
/// premultiplied BGRA, rows top to bottom, tightly packed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Animation {
    /// The frames, in order.
    pub frames: Vec<Frame>,
    /// How many times the animation plays through, or `None` for forever.
    pub plays: Option<u32>,
}

/// One frame of an [`Animation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The image as it stands once this frame is drawn.
    pub pixels: Vec<u8>,
    /// How long the frame is shown.
    pub delay: Duration,
}

/// How long a frame whose GIF delay is `centiseconds` is shown.
pub fn delay(centiseconds: u16) -> Duration {
    let asked = Duration::from_millis(u64::from(centiseconds) * 10);
    if asked <= MIN_DELAY {
        DEFAULT_DELAY
    } else {
        asked
    }
}

/// How many times an animation plays through, from the loop count in its
/// `NETSCAPE2.0` extension, if it has one: `None` for forever.
pub fn plays(loop_count: Option<u16>) -> Option<u32> {
    match loop_count {
        None => Some(1),
        Some(0) => None,
        Some(count) => Some(u32::from(count) + 1),
    }
}

/// What happens to a frame's rectangle before the next frame is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Disposal {
    /// The frame stays, and the next is drawn over it.
    #[default]
    Keep,
    /// The rectangle is cleared to transparent.
    Background,
    /// The rectangle goes back to what it was before the frame was drawn.
    Previous,
}

impl Disposal {
    /// The disposal a GIF's graphic control extension names; anything but 2
    /// or 3 is [`Keep`](Disposal::Keep).
    pub fn from_gif(value: u8) -> Self {
        match value {
            2 => Disposal::Background,
            3 => Disposal::Previous,
            _ => Disposal::Keep,
        }
    }
}

/// Draws frames one over another, as a GIF's logical screen shows them.
#[derive(Clone, Debug)]
pub struct Composer {
    size: PixelSize,
    canvas: Vec<u8>,
    /// The last frame's rectangle and disposal, and for
    /// [`Disposal::Previous`], the canvas before it was drawn.
    last: Option<(PixelRect, Disposal, Option<Vec<u8>>)>,
}

impl Composer {
    /// A transparent canvas of `size`.
    pub fn new(size: PixelSize) -> Self {
        Composer {
            size,
            canvas: vec![0; size.width as usize * size.height as usize * 4],
            last: None,
        }
    }

    /// Disposes of the last frame, then draws `pixels`, premultiplied BGRA of
    /// `rect`'s size, at `rect` over the canvas, and returns the canvas.
    /// What falls outside the canvas is dropped.
    pub fn add(&mut self, rect: PixelRect, pixels: &[u8], disposal: Disposal) -> &[u8] {
        match self.last.take() {
            Some((last, Disposal::Background, _)) => self.clear(last),
            Some((_, Disposal::Previous, Some(saved))) => self.canvas = saved,
            _ => {}
        }
        let saved = (disposal == Disposal::Previous).then(|| self.canvas.clone());
        let (width, height) = (self.size.width as i64, self.size.height as i64);
        for y in 0..i64::from(rect.height) {
            let cy = i64::from(rect.y) + y;
            if !(0..height).contains(&cy) {
                continue;
            }
            for x in 0..i64::from(rect.width) {
                let cx = i64::from(rect.x) + x;
                if !(0..width).contains(&cx) {
                    continue;
                }
                let from = ((y * i64::from(rect.width) + x) * 4) as usize;
                let Some(source) = pixels.get(from..from + 4) else {
                    continue;
                };
                let to = ((cy * width + cx) * 4) as usize;
                let target = &mut self.canvas[to..to + 4];
                let keep = 255 - u32::from(source[3]);
                for (t, s) in target.iter_mut().zip(source) {
                    *t = (u32::from(*s) + (u32::from(*t) * keep + 127) / 255).min(255) as u8;
                }
            }
        }
        self.last = Some((rect, disposal, saved));
        &self.canvas
    }

    fn clear(&mut self, rect: PixelRect) {
        let width = self.size.width;
        let left = rect.x.min(width);
        let right = rect.x.saturating_add(rect.width).min(width);
        let bottom = rect.y.saturating_add(rect.height).min(self.size.height);
        for y in rect.y..bottom {
            let start = (y * width + left) as usize * 4;
            let end = (y * width + right) as usize * 4;
            self.canvas[start..end].fill(0);
        }
    }
}

/// An open GIF file.
pub struct Gif {
    factory: IWICImagingFactory,
    decoder: IWICBitmapDecoder,
    size: PixelSize,
    frames: u32,
    plays: Option<u32>,
}

impl Gif {
    /// Reads the logical screen's size, the number of frames and the loop
    /// count of the GIF `decoder` has open.
    pub fn new(
        factory: &IWICImagingFactory,
        decoder: IWICBitmapDecoder,
    ) -> windows::core::Result<Self> {
        // SAFETY: GetFrameCount has no arguments.
        let frames = unsafe { decoder.GetFrameCount() }?;
        // SAFETY: GetMetadataQueryReader has no arguments.
        let reader = unsafe { decoder.GetMetadataQueryReader() }.ok();
        let read = |name| reader.as_ref().and_then(|reader| read_u16(reader, name));
        let mut size = match (
            read(w!("/logscrdesc/Width")),
            read(w!("/logscrdesc/Height")),
        ) {
            (Some(width), Some(height)) => PixelSize::new(width.into(), height.into()),
            _ => PixelSize::new(0, 0),
        };
        if size.width == 0 || size.height == 0 {
            // SAFETY: GetFrame takes a plain index.
            let first = unsafe { decoder.GetFrame(0) }?;
            let (mut width, mut height) = (0, 0);
            // SAFETY: both pointers are to live locals.
            unsafe { first.GetSize(&mut width, &mut height) }?;
            size = PixelSize::new(width, height);
        }
        let plays = plays(reader.as_ref().and_then(loop_count));
        Ok(Gif {
            factory: factory.clone(),
            decoder,
            size,
            frames,
            plays,
        })
    }

    /// The logical screen's size.
    pub fn size(&self) -> PixelSize {
        self.size
    }

    /// Whether the file has more than one frame.
    pub fn is_animated(&self) -> bool {
        self.frames > 1
    }

    /// Composes the first `count` frames, or every frame if there are fewer,
    /// stopping early if they would take more than [`MAX_ANIMATION_BYTES`].
    pub fn compose(&self, count: u32) -> windows::core::Result<Animation> {
        let frame_bytes = u64::from(self.size.width) * u64::from(self.size.height) * 4;
        let fit = (MAX_ANIMATION_BYTES / frame_bytes.max(1)).max(1);
        let count = u64::from(count.min(self.frames)).min(fit) as u32;
        let mut composer = Composer::new(self.size);
        let mut frames = Vec::with_capacity(count as usize);
        for index in 0..count {
            // SAFETY: GetFrame takes a plain index below the frame count.
            let frame = unsafe { self.decoder.GetFrame(index) }?;
            let (rect, delay, disposal) = frame_metadata(&frame)?;
            let pixels = self.pixels(&frame, rect)?;
            let canvas = composer.add(rect, &pixels, disposal);
            frames.push(Frame {
                pixels: canvas.to_vec(),
                delay,
            });
        }
        Ok(Animation {
            frames,
            plays: self.plays,
        })
    }

    fn pixels(
        &self,
        frame: &IWICBitmapFrameDecode,
        rect: PixelRect,
    ) -> windows::core::Result<Vec<u8>> {
        // SAFETY: CreateFormatConverter has no arguments.
        let converter = unsafe { self.factory.CreateFormatConverter() }?;
        // SAFETY: the frame and GUID outlive the call; the frame's own
        // palette, with its transparent entry, is used.
        unsafe {
            converter.Initialize(
                frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
        }?;
        let stride = rect.width * 4;
        let mut pixels = vec![0; stride as usize * rect.height as usize];
        // SAFETY: a null rectangle copies the whole frame, which is
        // `rect`'s size and fits the buffer at this stride.
        unsafe { converter.CopyPixels(std::ptr::null(), stride, &mut pixels) }?;
        Ok(pixels)
    }
}

/// A frame's rectangle on the logical screen, delay and disposal.
fn frame_metadata(
    frame: &IWICBitmapFrameDecode,
) -> windows::core::Result<(PixelRect, Duration, Disposal)> {
    let (mut width, mut height) = (0, 0);
    // SAFETY: both pointers are to live locals.
    unsafe { frame.GetSize(&mut width, &mut height) }?;
    // SAFETY: GetMetadataQueryReader has no arguments.
    let reader = unsafe { frame.GetMetadataQueryReader() }.ok();
    let reader = reader.as_ref();
    let left = reader.and_then(|r| read_u16(r, w!("/imgdesc/Left")));
    let top = reader.and_then(|r| read_u16(r, w!("/imgdesc/Top")));
    let centiseconds = reader.and_then(|r| read_u16(r, w!("/grctlext/Delay")));
    let disposal = reader.and_then(|r| read_u8(r, w!("/grctlext/Disposal")));
    let rect = PixelRect::new(
        u32::from(left.unwrap_or(0)),
        u32::from(top.unwrap_or(0)),
        width,
        height,
    );
    Ok((
        rect,
        delay(centiseconds.unwrap_or(0)),
        Disposal::from_gif(disposal.unwrap_or(0)),
    ))
}

/// The loop count in the file's `NETSCAPE2.0` application extension.
fn loop_count(reader: &IWICMetadataQueryReader) -> Option<u16> {
    let application = read_bytes(reader, w!("/appext/Application"))?;
    if application != b"NETSCAPE2.0" && application != b"ANIMEXTS1.0" {
        return None;
    }
    let data = read_bytes(reader, w!("/appext/Data"))?;
    // The data is the extension's sub-blocks: a length of 3, then 1, then
    // the loop count, least significant byte first.
    match data.as_slice() {
        [3, 1, low, high, ..] | [1, low, high, ..] => Some(u16::from_le_bytes([*low, *high])),
        _ => None,
    }
}

/// Reads a metadata value, hands it to `read`, and clears it.
fn read_value<T>(
    reader: &IWICMetadataQueryReader,
    name: PCWSTR,
    read: impl FnOnce(&PROPVARIANT) -> Option<T>,
) -> Option<T> {
    let mut value = PROPVARIANT::default();
    // SAFETY: the name is a static string and the value a live, zeroed
    // PROPVARIANT for WIC to fill.
    unsafe { reader.GetMetadataByName(name, &mut value) }.ok()?;
    let result = read(&value);
    // SAFETY: the variant was filled by WIC and is not used again.
    let _ = unsafe { PropVariantClear(&mut value) };
    result
}

fn read_u16(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<u16> {
    read_value(reader, name, |value| {
        // SAFETY: the tag is checked before the union member it selects is
        // read.
        unsafe {
            let inner = &value.Anonymous.Anonymous;
            match inner.vt {
                VT_UI2 => Some(inner.Anonymous.uiVal),
                VT_UI1 => Some(u16::from(inner.Anonymous.bVal)),
                _ => None,
            }
        }
    })
}

fn read_u8(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<u8> {
    read_u16(reader, name).and_then(|value| u8::try_from(value).ok())
}

fn read_bytes(reader: &IWICMetadataQueryReader, name: PCWSTR) -> Option<Vec<u8>> {
    read_value(reader, name, |value| {
        // SAFETY: the tag is checked before the union member it selects is
        // read, and a counted array of bytes holds `cElems` bytes at
        // `pElems`.
        unsafe {
            let inner = &value.Anonymous.Anonymous;
            if inner.vt != VT_VECTOR | VT_UI1 {
                return None;
            }
            let array = inner.Anonymous.caub;
            if array.pElems.is_null() {
                return Some(Vec::new());
            }
            Some(std::slice::from_raw_parts(array.pElems, array.cElems as usize).to_vec())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 4] = [0, 0, 255, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];

    fn solid(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
        colour.repeat(width as usize * height as usize)
    }

    /// The canvas as one colour a pixel, in reading order.
    fn pixels(canvas: &[u8]) -> Vec<[u8; 4]> {
        canvas.as_chunks::<4>().0.to_vec()
    }

    #[test]
    fn short_delays_are_shown_for_a_tenth_of_a_second() {
        assert_eq!(delay(0), DEFAULT_DELAY);
        assert_eq!(delay(1), DEFAULT_DELAY);
        assert_eq!(delay(2), Duration::from_millis(20));
        assert_eq!(delay(50), Duration::from_millis(500));
    }

    #[test]
    fn loop_counts_follow_browsers() {
        assert_eq!(plays(None), Some(1));
        assert_eq!(plays(Some(0)), None);
        assert_eq!(plays(Some(2)), Some(3));
    }

    #[test]
    fn disposals_from_the_file() {
        assert_eq!(Disposal::from_gif(0), Disposal::Keep);
        assert_eq!(Disposal::from_gif(1), Disposal::Keep);
        assert_eq!(Disposal::from_gif(2), Disposal::Background);
        assert_eq!(Disposal::from_gif(3), Disposal::Previous);
        assert_eq!(Disposal::from_gif(7), Disposal::Keep);
    }

    #[test]
    fn frames_are_drawn_over_what_they_keep() {
        let mut composer = Composer::new(PixelSize::new(2, 2));
        let first = composer.add(
            PixelRect::new(0, 0, 2, 2),
            &solid(2, 2, RED),
            Disposal::Keep,
        );
        assert_eq!(pixels(first), vec![RED; 4]);
        // A transparent pixel lets the frame beneath show through.
        let mut patch = BLUE.to_vec();
        patch.extend(CLEAR);
        let second = composer.add(PixelRect::new(1, 1, 2, 1), &patch, Disposal::Keep);
        assert_eq!(pixels(second), vec![RED, RED, RED, BLUE]);
    }

    #[test]
    fn background_disposal_clears_the_rectangle() {
        let mut composer = Composer::new(PixelSize::new(2, 1));
        composer.add(
            PixelRect::new(0, 0, 1, 1),
            &solid(1, 1, RED),
            Disposal::Background,
        );
        let next = composer.add(
            PixelRect::new(1, 0, 1, 1),
            &solid(1, 1, BLUE),
            Disposal::Keep,
        );
        assert_eq!(pixels(next), vec![CLEAR, BLUE]);
    }

    #[test]
    fn previous_disposal_restores_the_canvas() {
        let mut composer = Composer::new(PixelSize::new(2, 1));
        composer.add(
            PixelRect::new(0, 0, 2, 1),
            &solid(2, 1, RED),
            Disposal::Keep,
        );
        let shown = composer.add(
            PixelRect::new(0, 0, 1, 1),
            &solid(1, 1, BLUE),
            Disposal::Previous,
        );
        assert_eq!(pixels(shown), vec![BLUE, RED]);
        let next = composer.add(
            PixelRect::new(1, 0, 1, 1),
            &solid(1, 1, BLUE),
            Disposal::Keep,
        );
        assert_eq!(pixels(next), vec![RED, BLUE]);
    }

    #[test]
    fn frames_beyond_the_canvas_are_clipped() {
        let mut composer = Composer::new(PixelSize::new(2, 1));
        let shown = composer.add(
            PixelRect::new(1, 0, 4, 2),
            &solid(4, 2, BLUE),
            Disposal::Background,
        );
        assert_eq!(pixels(shown), vec![CLEAR, BLUE]);
        // Too few pixels for the rectangle: the missing ones are skipped.
        let next = composer.add(PixelRect::new(0, 0, 2, 1), &RED, Disposal::Keep);
        assert_eq!(pixels(next), vec![RED, CLEAR]);
    }
}
