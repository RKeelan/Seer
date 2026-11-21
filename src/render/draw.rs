//! Drawing a [`Scene`] to a Direct2D device context.
//!
//! The context's target may be a swap chain's back buffer or an offscreen
//! bitmap; [`Renderer::draw`] neither knows nor cares. It draws in client
//! space, one pixel to one device-independent pixel, so the context must be at
//! 96 DPI, which is Direct2D's default.

use std::collections::HashMap;

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_CAP_STYLE_FLAT, D2D1_DASH_STYLE_DASH,
    D2D1_INTERPOLATION_MODE, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_STROKE_STYLE_PROPERTIES, ID2D1Bitmap1,
    ID2D1DeviceContext, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::error::Result;
use crate::model::ImageId;
use crate::model::geometry::{Point, Rect, Transform};
use crate::render::scene::{Colour, Interpolation, Item, Scene};
use crate::render::text::{Line, Text};

/// The decoded bitmaps the renderer can draw, by image.
pub type Bitmaps = HashMap<ImageId, ID2D1Bitmap1>;

/// Draws scenes. It keeps what is worth keeping between frames: DirectWrite
/// state and the dash style.
#[derive(Default)]
pub struct Renderer {
    text: Text,
    dashes: Option<ID2D1StrokeStyle>,
}

impl Renderer {
    /// Draws `scene` to `context`'s current target, drawing each image with
    /// its bitmap in `bitmaps` and each thumbnail with its bitmap in
    /// `thumbnails`, and skipping those with none.
    pub fn draw(
        &mut self,
        context: &ID2D1DeviceContext,
        scene: &Scene,
        bitmaps: &Bitmaps,
        thumbnails: &Bitmaps,
    ) -> Result<()> {
        // SAFETY: BeginDraw has no arguments; EndDraw below closes it.
        unsafe { context.BeginDraw() };
        let drawn = self.items(context, scene, bitmaps, thumbnails);
        // SAFETY: the transform is a live local.
        unsafe { context.SetTransform(&Matrix3x2::identity()) };
        // SAFETY: no tags are asked for. EndDraw reports errors from the
        // whole frame, including device loss.
        let ended = unsafe { context.EndDraw(None, None) };
        drawn?;
        Ok(ended?)
    }

    fn items(
        &mut self,
        context: &ID2D1DeviceContext,
        scene: &Scene,
        bitmaps: &Bitmaps,
        thumbnails: &Bitmaps,
    ) -> Result<()> {
        // SAFETY: the transform and colour are live locals.
        unsafe {
            context.SetTransform(&Matrix3x2::identity());
            context.Clear(Some(&colour(scene.background)));
        }
        for item in &scene.items {
            match item {
                Item::Image {
                    image,
                    size,
                    transform,
                    clip,
                    interpolation,
                    opacity,
                } => {
                    let Some(bitmap) = bitmaps.get(image) else {
                        continue;
                    };
                    // Stretching the bitmap over the image's full size scales
                    // a preview up to the size the transform expects.
                    let destination = rect(Rect::new(
                        0.0,
                        0.0,
                        f64::from(size.width),
                        f64::from(size.height),
                    ));
                    // SAFETY: the clip, transform and bitmap are live, and
                    // the clip is popped before the next item.
                    unsafe {
                        context
                            .PushAxisAlignedClip(&rect(*clip), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                        context.SetTransform(&matrix(transform));
                        context.DrawBitmap(
                            bitmap,
                            Some(&destination),
                            *opacity,
                            interpolation_mode(*interpolation),
                            None,
                            None,
                        );
                        context.SetTransform(&Matrix3x2::identity());
                        context.PopAxisAlignedClip();
                    }
                }
                Item::Thumbnail { image, rect: r } => {
                    let Some(bitmap) = thumbnails.get(image) else {
                        continue;
                    };
                    // SAFETY: the bitmap and rectangle are live; the whole
                    // bitmap is drawn.
                    unsafe {
                        context.DrawBitmap(
                            bitmap,
                            Some(&rect(*r)),
                            1.0,
                            interpolation_mode(Interpolation::Smooth),
                            None,
                            None,
                        );
                    }
                }
                Item::FillRect { rect: r, colour: c } => {
                    let brush = brush(context, *c)?;
                    // SAFETY: the rectangle and brush are live.
                    unsafe { context.FillRectangle(&rect(*r), &brush) };
                }
                Item::StrokeRect {
                    rect: r,
                    colour: c,
                    width,
                    dashed,
                } => {
                    let brush = brush(context, *c)?;
                    let style = if *dashed {
                        Some(self.dashes(context)?)
                    } else {
                        None
                    };
                    // SAFETY: the rectangle, brush and style are live.
                    unsafe {
                        context.DrawRectangle(&rect(*r), &brush, *width as f32, style.as_ref())
                    };
                }
                Item::Line {
                    from,
                    to,
                    colour: c,
                    width,
                } => {
                    let brush = brush(context, *c)?;
                    // SAFETY: the brush is live and no stroke style is given.
                    unsafe {
                        context.DrawLine(
                            vector(*from),
                            vector(*to),
                            &brush,
                            *width as f32,
                            None::<&ID2D1StrokeStyle>,
                        )
                    };
                }
                Item::Text {
                    text,
                    rect: r,
                    colour: c,
                    size,
                    align,
                    bold,
                } => {
                    let brush = brush(context, *c)?;
                    let line = Line {
                        text,
                        rect: *r,
                        size: *size,
                        align: *align,
                        bold: *bold,
                    };
                    self.text.draw(context, &line, &brush)?;
                }
            }
        }
        Ok(())
    }

    fn dashes(&mut self, context: &ID2D1DeviceContext) -> Result<ID2D1StrokeStyle> {
        if let Some(style) = &self.dashes {
            return Ok(style.clone());
        }
        // SAFETY: GetFactory has no arguments.
        let factory = unsafe { context.GetFactory() }?;
        let properties = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: D2D1_CAP_STYLE_FLAT,
            dashStyle: D2D1_DASH_STYLE_DASH,
            ..Default::default()
        };
        // SAFETY: the properties are a live local; no custom dashes.
        let style = unsafe { factory.CreateStrokeStyle(&properties, None) }?;
        self.dashes = Some(style.clone());
        Ok(style)
    }
}

fn brush(context: &ID2D1DeviceContext, c: Colour) -> Result<ID2D1SolidColorBrush> {
    // SAFETY: the colour is a live local and no brush properties are given.
    Ok(unsafe { context.CreateSolidColorBrush(&colour(c), None) }?)
}

fn colour(c: Colour) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r,
        g: c.g,
        b: c.b,
        a: c.a,
    }
}

fn rect(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x as f32,
        top: r.y as f32,
        right: r.right() as f32,
        bottom: r.bottom() as f32,
    }
}

fn vector(p: Point) -> Vector2 {
    Vector2 {
        X: p.x as f32,
        Y: p.y as f32,
    }
}

fn matrix(t: &Transform) -> Matrix3x2 {
    Matrix3x2 {
        M11: t.m11 as f32,
        M12: t.m12 as f32,
        M21: t.m21 as f32,
        M22: t.m22 as f32,
        M31: t.dx as f32,
        M32: t.dy as f32,
    }
}

fn interpolation_mode(interpolation: Interpolation) -> D2D1_INTERPOLATION_MODE {
    match interpolation {
        Interpolation::Smooth => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
        Interpolation::Nearest => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    }
}
