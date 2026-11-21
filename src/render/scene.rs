//! What to draw, as plain data.
//!
//! A [`Scene`] is a list of items drawn in order over a background, in client
//! space: physical pixels from the client area's top-left. The renderer draws
//! it to any Direct2D device context, so the window, `screenshot` and the
//! render tests all draw the same scene. Images are named by id; the renderer
//! draws the best decoded bitmap it holds for each, and nothing for an image
//! with none yet. Thumbnails are bitmaps of their own, drawn the same way.

use crate::model::ImageId;
use crate::model::geometry::{PixelSize, Point, Rect, Transform};

/// Everything to draw in one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// The client area's size.
    pub size: PixelSize,
    /// The colour the client area is cleared to.
    pub background: Colour,
    /// What to draw over the background, bottom first.
    pub items: Vec<Item>,
}

/// One thing to draw.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// An image, placed by a transform and clipped to a rectangle.
    Image {
        /// The image.
        image: ImageId,
        /// The image's full size in pixels. The renderer scales a smaller
        /// decoded bitmap, such as a preview, up to this size before applying
        /// `transform`.
        size: PixelSize,
        /// Maps image space to client space, including zoom, pan and rotation.
        transform: Transform,
        /// The part of the client area the image may cover.
        clip: Rect,
        /// How to sample the image.
        interpolation: Interpolation,
        /// From 0 (invisible) to 1 (opaque).
        opacity: f32,
    },
    /// An image's thumbnail, stretched over a rectangle.
    Thumbnail {
        /// The image.
        image: ImageId,
        /// Where.
        rect: Rect,
    },
    /// A filled rectangle.
    FillRect {
        /// Where.
        rect: Rect,
        /// The fill.
        colour: Colour,
    },
    /// A rectangle's outline, centred on its edges.
    StrokeRect {
        /// Where.
        rect: Rect,
        /// The line's colour.
        colour: Colour,
        /// The line's width in pixels.
        width: f64,
        /// Whether the line is dashed.
        dashed: bool,
    },
    /// A straight line.
    Line {
        /// One end.
        from: Point,
        /// The other end.
        to: Point,
        /// The line's colour.
        colour: Colour,
        /// The line's width in pixels.
        width: f64,
    },
    /// A line of text in the system UI font, vertically centred in a
    /// rectangle and trimmed with an ellipsis if it does not fit.
    Text {
        /// The text.
        text: String,
        /// Where.
        rect: Rect,
        /// The text's colour.
        colour: Colour,
        /// The font size in pixels.
        size: f64,
        /// The horizontal alignment.
        align: Align,
        /// Whether the text is bold.
        bold: bool,
    },
}

/// How to sample an image when drawing it at a different size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Interpolation {
    /// High-quality filtering, for zoomed-out and moderately zoomed images.
    #[default]
    Smooth,
    /// Nearest neighbour, so pixels can be inspected when zoomed far in.
    Nearest,
}

/// Horizontal text alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Align {
    /// Against the left edge.
    #[default]
    Start,
    /// Centred.
    Centre,
    /// Against the right edge.
    End,
}

/// A colour in sRGB with straight (not premultiplied) alpha, each channel
/// from 0 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Colour {
    /// Red.
    pub r: f32,
    /// Green.
    pub g: f32,
    /// Blue.
    pub b: f32,
    /// Alpha: 0 is transparent, 1 opaque.
    pub a: f32,
}

impl Colour {
    /// An opaque colour from 8-bit channels.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Colour {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    /// This colour with a different alpha.
    pub const fn with_alpha(self, a: f32) -> Self {
        Colour { a, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_from_bytes() {
        let colour = Colour::rgb(255, 0, 51).with_alpha(0.5);
        assert_eq!(
            colour,
            Colour {
                r: 1.0,
                g: 0.0,
                b: 0.2,
                a: 0.5
            }
        );
    }
}
