//! The thumbnail strip: thumbnails in set order, the selected one highlighted
//! and groups bracketed.
//!
//! [`items`] turns a [`Layout`] into scene items. A thumbnail not yet decoded
//! is drawn as an empty box, so the strip has its final shape at once.

use crate::model::ImageId;
use crate::model::geometry::{PixelSize, Rect};
use crate::model::strip::Layout;
use crate::render::scene::{Align, Colour, Item};

/// The strip's colours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Behind the thumbnails.
    pub background: Colour,
    /// Group labels, brackets and empty boxes, at reduced opacity.
    pub text: Colour,
    /// The selected thumbnail's outline.
    pub accent: Colour,
}

/// Adds the strip's items to `items`. `thumbnail` gives the full size of each
/// image whose thumbnail has been decoded, which sets the thumbnail's shape.
pub fn items(
    layout: &Layout,
    selected: Option<ImageId>,
    thumbnail: impl Fn(ImageId) -> Option<PixelSize>,
    palette: Palette,
    items: &mut Vec<Item>,
) {
    let scale = layout.scale;
    let bounds = layout.bounds;
    items.push(Item::FillRect {
        rect: bounds,
        colour: palette.background,
    });
    items.push(Item::FillRect {
        rect: Rect::new(bounds.x, bounds.y, bounds.width, scale),
        colour: palette.text.with_alpha(0.15),
    });
    let shown = |rect: Rect| rect.right() > bounds.x && rect.x < bounds.right();
    for bracket in layout.brackets.iter().filter(|b| shown(b.rect)) {
        items.push(Item::StrokeRect {
            rect: bracket.rect,
            colour: palette.text.with_alpha(0.4),
            width: scale,
            dashed: false,
        });
        items.push(Item::Text {
            text: bracket.group.clone(),
            rect: Rect::new(
                bracket.label.x + 2.0 * scale,
                bracket.label.y,
                (bracket.label.width - 2.0 * scale).max(0.0),
                bracket.label.height,
            ),
            colour: palette.text.with_alpha(0.7),
            size: 11.0 * scale,
            align: Align::Start,
            bold: false,
        });
    }
    for cell in layout.visible() {
        match thumbnail(cell.image) {
            Some(size) => items.push(Item::Thumbnail {
                image: cell.image,
                rect: fit(size, cell.rect),
            }),
            None => items.push(Item::FillRect {
                rect: cell.rect,
                colour: palette.text.with_alpha(0.08),
            }),
        }
        if Some(cell.image) == selected {
            let inset = 2.0 * scale;
            items.push(Item::StrokeRect {
                rect: Rect::new(
                    cell.rect.x - inset,
                    cell.rect.y - inset,
                    cell.rect.width + 2.0 * inset,
                    cell.rect.height + 2.0 * inset,
                ),
                colour: palette.accent,
                width: 2.0 * scale,
                dashed: false,
            });
        }
    }
}

/// An image of `size` fitted within `cell`, centred, never enlarged, on whole
/// pixels.
fn fit(size: PixelSize, cell: Rect) -> Rect {
    if size.width == 0 || size.height == 0 {
        return cell;
    }
    let (width, height) = (f64::from(size.width), f64::from(size.height));
    let scale = (cell.width / width).min(cell.height / height).min(1.0);
    let (width, height) = ((width * scale).round(), (height * scale).round());
    Rect::new(
        (cell.x + (cell.width - width) / 2.0).round(),
        (cell.y + (cell.height - height) / 2.0).round(),
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::strip::arrange;

    const PALETTE: Palette = Palette {
        background: Colour::rgb(1, 1, 1),
        text: Colour::rgb(2, 2, 2),
        accent: Colour::rgb(3, 3, 3),
    };

    #[test]
    fn thumbnails_are_fitted_and_the_selection_outlined() {
        let layout = arrange(
            [
                (ImageId(1), None),
                (ImageId(2), Some("pair")),
                (ImageId(3), Some("pair")),
            ],
            Rect::new(0.0, 100.0, 400.0, 84.0),
            1.0,
        );
        let mut drawn = Vec::new();
        let sizes = |image: ImageId| match image.0 {
            1 => Some(PixelSize::new(1600, 400)),
            2 => Some(PixelSize::new(16, 16)),
            _ => None,
        };
        items(&layout, Some(ImageId(2)), sizes, PALETTE, &mut drawn);
        let thumbnails: Vec<_> = drawn
            .iter()
            .filter_map(|item| match item {
                Item::Thumbnail { image, rect } => Some((image.0, *rect)),
                _ => None,
            })
            .collect();
        // A wide image fills the box's width; a small one is not enlarged.
        assert_eq!(
            thumbnails,
            [
                (1, Rect::new(8.0, 140.0, 64.0, 16.0)),
                (2, Rect::new(108.0, 140.0, 16.0, 16.0)),
            ]
        );
        assert!(drawn.contains(&Item::FillRect {
            rect: layout.cells[2].rect,
            colour: PALETTE.text.with_alpha(0.08),
        }));
        assert!(drawn.contains(&Item::StrokeRect {
            rect: Rect::new(82.0, 122.0, 68.0, 52.0),
            colour: PALETTE.accent,
            width: 2.0,
            dashed: false,
        }));
        assert!(drawn.iter().any(|item| matches!(
            item,
            Item::Text { text, .. } if text == "pair"
        )));
        assert_eq!(
            fit(PixelSize::new(0, 0), layout.cells[0].rect),
            layout.cells[0].rect
        );
    }
}
