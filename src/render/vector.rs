//! SVG images drawn as vectors, so they stay sharp at any zoom.
//!
//! [`Vectors`] holds the text of each SVG image and, once it has been drawn,
//! the Direct2D document parsed from it. A document belongs to the device it
//! was made on, so a renderer for another device starts from
//! [`Vectors::for_new_device`], which keeps the texts and drops the
//! documents. An image whose document cannot be made or drawn falls back to
//! its rasterised bitmap.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, ID2D1DeviceContext, ID2D1DeviceContext5, ID2D1SvgDocument,
};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

use crate::decode::svg;
use crate::model::ImageId;
use crate::model::geometry::PixelSize;

/// The SVG images a renderer draws as vectors.
#[derive(Default)]
pub struct Vectors {
    held: HashMap<ImageId, Held>,
}

struct Held {
    source: Arc<[u8]>,
    /// The document and the viewport it was made for, once drawn; `None`
    /// inside if it could not be made.
    document: Option<(PixelSize, Option<ID2D1SvgDocument>)>,
}

impl Vectors {
    /// Draws `image` from `source` from now on.
    pub fn insert(&mut self, image: ImageId, source: Arc<[u8]>) {
        if self
            .held
            .get(&image)
            .is_some_and(|held| held.source == source)
        {
            return;
        }
        self.held.insert(
            image,
            Held {
                source,
                document: None,
            },
        );
    }

    /// Stops drawing `image` as vectors.
    pub fn remove(&mut self, image: ImageId) {
        self.held.remove(&image);
    }

    /// Whether `image` is drawn as vectors.
    pub fn contains(&self, image: ImageId) -> bool {
        self.held.contains_key(&image)
    }

    /// The same images, for a renderer on another device.
    pub fn for_new_device(&self) -> Self {
        Vectors {
            held: self
                .held
                .iter()
                .map(|(image, held)| {
                    let held = Held {
                        source: Arc::clone(&held.source),
                        document: None,
                    };
                    (*image, held)
                })
                .collect(),
        }
    }

    /// Draws `image`, whose full size is `size`, through `transform` and
    /// clipped to `clip`, if it is held and its document can be drawn.
    /// Returns whether it was drawn.
    pub fn draw(
        &mut self,
        context: &ID2D1DeviceContext,
        image: ImageId,
        size: PixelSize,
        transform: &Matrix3x2,
        clip: &D2D_RECT_F,
    ) -> bool {
        let Some(held) = self.held.get_mut(&image) else {
            return false;
        };
        let Ok(context) = context.cast::<ID2D1DeviceContext5>() else {
            return false;
        };
        if held.document.as_ref().is_none_or(|(made, _)| *made != size) {
            let document = svg::document(&context, &held.source, size).ok();
            held.document = Some((size, document));
        }
        let Some((_, Some(document))) = &held.document else {
            return false;
        };
        // SAFETY: the clip, transform and document are live, the document was
        // made on this context, and the clip is popped and the transform reset
        // before returning.
        unsafe {
            context.PushAxisAlignedClip(clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            context.SetTransform(transform);
            context.DrawSvgDocument(document);
            context.SetTransform(&Matrix3x2::identity());
            context.PopAxisAlignedClip();
        }
        true
    }
}
