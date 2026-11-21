//! The thumbnail strip's layout: where each thumbnail and group bracket goes
//! across the bottom of the client area, how far the strip is scrolled, and
//! which thumbnail lies under a point.
//!
//! Thumbnails sit in boxes of one size, in set order, whatever the shape of
//! their images, so the layout does not move as thumbnails arrive. The
//! members of a group, which the set keeps next to each other, are enclosed
//! in a bracket labelled with the group's name. Dimensions here are at 100%
//! scale; [`arrange`] multiplies them by the window's scale.

use crate::model::ImageId;
use crate::model::geometry::{PixelSize, Point, Rect};

/// The strip's height.
pub const HEIGHT: f64 = 84.0;
/// The width of a thumbnail's box.
pub const THUMBNAIL_WIDTH: f64 = 64.0;
/// The height of a thumbnail's box.
pub const THUMBNAIL_HEIGHT: f64 = 48.0;

/// Space at either end of the strip.
const MARGIN: f64 = 8.0;
/// Space between thumbnails and brackets.
const GAP: f64 = 8.0;
/// Space between thumbnails in a group.
const GROUP_GAP: f64 = 4.0;
/// Space between a bracket and the thumbnails inside it.
const BRACKET_PADDING: f64 = 4.0;
/// From the strip's top to the top of a group's label.
const LABEL_TOP: f64 = 4.0;
/// A group label's height.
const LABEL_HEIGHT: f64 = 16.0;
/// From the strip's top to the top of each thumbnail's box.
const THUMBNAIL_TOP: f64 = LABEL_TOP + LABEL_HEIGHT + BRACKET_PADDING;

/// The thumbnails and brackets, placed in client space.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// The strip's band across the client area.
    pub bounds: Rect,
    /// Each image's thumbnail box, in set order.
    pub cells: Vec<Cell>,
    /// Each group's bracket.
    pub brackets: Vec<Bracket>,
    /// The width of everything in the strip, from the start of the first
    /// thumbnail's margin to the end of the last's.
    pub width: f64,
    /// The window's scale: 1 at 96 DPI.
    pub scale: f64,
}

/// Where an image's thumbnail goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    /// The image.
    pub image: ImageId,
    /// The thumbnail's box. The thumbnail is fitted inside it.
    pub rect: Rect,
}

/// The bracket enclosing a group's thumbnails.
#[derive(Clone, Debug, PartialEq)]
pub struct Bracket {
    /// The group's name.
    pub group: String,
    /// The bracket's outline.
    pub rect: Rect,
    /// Where the group's name goes, above the bracket.
    pub label: Rect,
}

impl Layout {
    /// The image whose thumbnail box, or the gap around it, is under `at`.
    pub fn image_at(&self, at: Point) -> Option<ImageId> {
        if !self.bounds.contains(at) {
            return None;
        }
        let reach = self.scale * GROUP_GAP / 2.0;
        self.cells
            .iter()
            .find(|cell| {
                let rect = cell.rect;
                at.x >= rect.x - reach
                    && at.x < rect.right() + reach
                    && at.y >= rect.y - reach
                    && at.y < rect.bottom() + reach
            })
            .map(|cell| cell.image)
    }

    /// The cells at least partly inside the strip's bounds.
    pub fn visible(&self) -> impl Iterator<Item = &Cell> {
        self.cells
            .iter()
            .filter(|cell| cell.rect.right() > self.bounds.x && cell.rect.x < self.bounds.right())
    }

    /// The size, in whole pixels, that thumbnails are decoded to fit.
    pub fn thumbnail_size(&self) -> PixelSize {
        let scale = self.scale;
        PixelSize::new(
            (THUMBNAIL_WIDTH * scale).round() as u32,
            (THUMBNAIL_HEIGHT * scale).round() as u32,
        )
    }

    /// The same layout moved `by` to the left.
    fn shifted(mut self, by: f64) -> Self {
        for cell in &mut self.cells {
            cell.rect.x -= by;
        }
        for bracket in &mut self.brackets {
            bracket.rect.x -= by;
            bracket.label.x -= by;
        }
        self
    }

    /// The furthest the strip can scroll.
    fn max_scroll(&self) -> f64 {
        (self.width - self.bounds.width).max(0.0)
    }
}

/// The strip's band across the bottom of a client area of `size`.
pub fn bounds(size: PixelSize, scale: f64) -> Rect {
    let height = (HEIGHT * scale).min(f64::from(size.height));
    Rect::new(
        0.0,
        f64::from(size.height) - height,
        f64::from(size.width),
        height,
    )
}

/// Lays out the strip for `images`, each with its group, in set order, as if
/// unscrolled.
pub fn arrange<'a>(
    images: impl IntoIterator<Item = (ImageId, Option<&'a str>)>,
    bounds: Rect,
    scale: f64,
) -> Layout {
    let (width, height) = (THUMBNAIL_WIDTH * scale, THUMBNAIL_HEIGHT * scale);
    let top = bounds.y + THUMBNAIL_TOP * scale;
    let padding = BRACKET_PADDING * scale;
    let mut cells = Vec::new();
    let mut brackets: Vec<Bracket> = Vec::new();
    let mut x = bounds.x + MARGIN * scale;
    let mut group: Option<&str> = None;
    for (image, member_of) in images {
        if member_of.is_some() && member_of == group {
            x += GROUP_GAP * scale;
        } else {
            if group.is_some() {
                x += padding;
                let bracket = brackets.last_mut().expect("opened with the group");
                bracket.rect.width = x - bracket.rect.x;
            }
            if !cells.is_empty() {
                x += GAP * scale;
            }
            if let Some(name) = member_of {
                brackets.push(Bracket {
                    group: name.to_string(),
                    rect: Rect::new(x, top - padding, 0.0, height + 2.0 * padding),
                    label: Rect::new(x, bounds.y + LABEL_TOP * scale, 0.0, LABEL_HEIGHT * scale),
                });
                x += padding;
            }
            group = member_of;
        }
        cells.push(Cell {
            image,
            rect: Rect::new(x, top, width, height),
        });
        x += width;
    }
    if group.is_some() {
        x += padding;
        let bracket = brackets.last_mut().expect("opened with the group");
        bracket.rect.width = x - bracket.rect.x;
    }
    for bracket in &mut brackets {
        bracket.label.width = bracket.rect.width;
    }
    let width = if cells.is_empty() {
        0.0
    } else {
        x + MARGIN * scale - bounds.x
    };
    Layout {
        bounds,
        cells,
        brackets,
        width,
        scale,
    }
}

/// How far the strip is scrolled, and which image it last scrolled to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Strip {
    scroll: f64,
    revealed: Option<ImageId>,
}

impl Strip {
    /// `arranged` scrolled to where the strip is.
    pub fn place(&self, arranged: Layout) -> Layout {
        let scroll = self.scroll.clamp(0.0, arranged.max_scroll());
        arranged.shifted(scroll)
    }

    /// Scrolls by `by` pixels; positive moves the thumbnails to the left.
    pub fn scroll_by(&mut self, by: f64, arranged: &Layout) {
        self.scroll = (self.scroll + by).clamp(0.0, arranged.max_scroll());
    }

    /// Scrolls the least distance that shows all of `selected`'s thumbnail,
    /// if the selection changed since the last call, and keeps the scroll
    /// within the strip.
    pub fn reveal(&mut self, selected: Option<ImageId>, arranged: &Layout) {
        if selected != self.revealed {
            self.revealed = selected;
            if let Some(cell) = arranged
                .cells
                .iter()
                .find(|cell| Some(cell.image) == selected)
            {
                let margin = MARGIN * arranged.scale;
                let start = cell.rect.x - margin - arranged.bounds.x;
                let end = cell.rect.right() + margin - arranged.bounds.x;
                if start < self.scroll {
                    self.scroll = start;
                } else if end > self.scroll + arranged.bounds.width {
                    self.scroll = end - arranged.bounds.width;
                }
            }
        }
        self.scroll = self.scroll.clamp(0.0, arranged.max_scroll());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Rect = Rect::new(0.0, 516.0, 800.0, 84.0);

    fn ids(layout: &Layout) -> Vec<u32> {
        layout.cells.iter().map(|cell| cell.image.0).collect()
    }

    #[test]
    fn the_strip_runs_along_the_bottom() {
        assert_eq!(bounds(PixelSize::new(800, 600), 1.0), BOUNDS);
        assert_eq!(
            bounds(PixelSize::new(800, 600), 1.5),
            Rect::new(0.0, 474.0, 800.0, 126.0)
        );
        assert_eq!(
            bounds(PixelSize::new(100, 50), 1.0),
            Rect::new(0.0, 0.0, 100.0, 50.0)
        );
    }

    #[test]
    fn thumbnails_follow_set_order_with_groups_bracketed() {
        let images = [
            (ImageId(1), None),
            (ImageId(2), Some("a")),
            (ImageId(3), Some("a")),
            (ImageId(4), Some("b")),
            (ImageId(5), None),
        ];
        let layout = arrange(images, BOUNDS, 1.0);
        assert_eq!(ids(&layout), [1, 2, 3, 4, 5]);
        let xs: Vec<f64> = layout.cells.iter().map(|cell| cell.rect.x).collect();
        // 8 margin; 64 thumbnail; 8 gap, 4 padding; 64, 4 gap, 64; 4 padding,
        // 8 gap, 4 padding; 64; 4 padding, 8 gap; 64; 8 margin.
        assert_eq!(xs, [8.0, 84.0, 152.0, 232.0, 308.0]);
        for cell in &layout.cells {
            assert_eq!(cell.rect.y, 540.0);
            assert_eq!((cell.rect.width, cell.rect.height), (64.0, 48.0));
        }
        assert_eq!(layout.width, 380.0);
        assert_eq!(
            layout.brackets,
            [
                Bracket {
                    group: "a".into(),
                    rect: Rect::new(80.0, 536.0, 140.0, 56.0),
                    label: Rect::new(80.0, 520.0, 140.0, 16.0),
                },
                Bracket {
                    group: "b".into(),
                    rect: Rect::new(228.0, 536.0, 72.0, 56.0),
                    label: Rect::new(228.0, 520.0, 72.0, 16.0),
                },
            ]
        );
        assert_eq!(layout.visible().count(), 5);
        assert_eq!(layout.thumbnail_size(), PixelSize::new(64, 48));
    }

    #[test]
    fn a_group_at_the_end_is_closed() {
        let layout = arrange([(ImageId(1), Some("a"))], BOUNDS, 2.0);
        assert_eq!(layout.cells[0].rect, Rect::new(24.0, 564.0, 128.0, 96.0));
        assert_eq!(
            layout.brackets[0].rect,
            Rect::new(16.0, 556.0, 144.0, 112.0)
        );
        assert_eq!(layout.width, 176.0);
        assert_eq!(layout.thumbnail_size(), PixelSize::new(128, 96));
        assert_eq!(arrange([], BOUNDS, 1.0).width, 0.0);
    }

    #[test]
    fn the_image_under_a_point() {
        let images = [(ImageId(1), None), (ImageId(2), None)];
        let layout = arrange(images, BOUNDS, 1.0);
        let at = |x, y| layout.image_at(Point::new(x, y));
        assert_eq!(at(10.0, 560.0), Some(ImageId(1)));
        assert_eq!(at(71.0, 560.0), Some(ImageId(1)));
        assert_eq!(at(76.0, 560.0), None);
        assert_eq!(at(100.0, 539.0), Some(ImageId(2)));
        assert_eq!(at(100.0, 520.0), None);
        assert_eq!(at(100.0, 400.0), None);
    }

    #[test]
    fn scrolling_reveals_the_selection_and_stays_in_range() {
        let images: Vec<_> = (1..=20).map(|id| (ImageId(id), None)).collect();
        let narrow = Rect::new(0.0, 516.0, 300.0, 84.0);
        let arranged = arrange(images, narrow, 1.0);
        // 20 thumbnails of 64 with 19 gaps of 8 and two margins of 8.
        assert_eq!(arranged.width, 1448.0);
        let mut strip = Strip::default();
        strip.reveal(Some(ImageId(1)), &arranged);
        assert_eq!(strip.place(arranged.clone()).cells[0].rect.x, 8.0);

        // The tenth thumbnail ends at 8 + 10 × 64 + 9 × 8 = 720; with its
        // margin, the strip scrolls 728 − 300.
        strip.reveal(Some(ImageId(10)), &arranged);
        assert_eq!(strip.scroll, 428.0);
        let placed = strip.place(arranged.clone());
        assert_eq!(placed.cells[9].rect.right(), 292.0);
        assert_eq!(
            placed
                .visible()
                .map(|cell| cell.image.0)
                .collect::<Vec<_>>(),
            [6, 7, 8, 9, 10]
        );
        assert_eq!(placed.image_at(Point::new(260.0, 560.0)), Some(ImageId(10)));

        // Revealing the same selection again leaves a scroll the user made.
        strip.scroll_by(-100.0, &arranged);
        strip.reveal(Some(ImageId(10)), &arranged);
        assert_eq!(strip.scroll, 328.0);

        // Going back scrolls the other way, as little as it can.
        strip.reveal(Some(ImageId(3)), &arranged);
        assert_eq!(strip.scroll, 144.0);

        strip.scroll_by(1e6, &arranged);
        assert_eq!(strip.scroll, 1148.0);
        strip.scroll_by(-1e6, &arranged);
        assert_eq!(strip.scroll, 0.0);

        // A strip that has grown narrower content keeps its scroll in range.
        let few = arrange([(ImageId(1), None), (ImageId(2), None)], narrow, 1.0);
        strip.scroll = 500.0;
        assert_eq!(strip.place(few.clone()).cells[0].rect.x, 8.0);
        strip.reveal(Some(ImageId(3)), &few);
        assert_eq!(strip.scroll, 0.0);
    }
}
