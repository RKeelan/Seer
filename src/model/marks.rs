//! Marks: rectangles in image pixels, each with a note, drawn by the user to
//! point an agent at a place in an image.
//!
//! A mark is stored in image space, so zooming, panning and rotating the view
//! never change it, and it is reported in the image's own pixels.

use crate::model::geometry::{PixelRect, PixelSize, Point};
use crate::model::{ImageId, MarkId};

/// A mark on an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Its id.
    pub id: MarkId,
    /// The image it marks.
    pub image: ImageId,
    /// Where, in image pixels.
    pub rect: PixelRect,
    /// What the user wrote about it.
    pub note: String,
}

/// The rectangle of whole pixels covering a drag between two points in image
/// space, in either direction, clamped to the image. `None` if nothing of the
/// image is covered.
pub fn rect_between(a: Point, b: Point, image: PixelSize) -> Option<PixelRect> {
    let width = f64::from(image.width);
    let height = f64::from(image.height);
    let left = a.x.min(b.x).clamp(0.0, width).floor();
    let right = a.x.max(b.x).clamp(0.0, width).ceil();
    let top = a.y.min(b.y).clamp(0.0, height).floor();
    let bottom = a.y.max(b.y).clamp(0.0, height).ceil();
    (right > left && bottom > top).then(|| {
        // The values are whole numbers within the image, so they fit in u32.
        PixelRect::new(
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        )
    })
}

/// Every mark, in the order made.
#[derive(Debug)]
pub struct Marks {
    marks: Vec<Mark>,
    next_id: u32,
}

impl Default for Marks {
    fn default() -> Self {
        Marks::new()
    }
}

impl Marks {
    /// No marks. The first mark made gets id 1.
    pub fn new() -> Self {
        Marks {
            marks: Vec::new(),
            next_id: 1,
        }
    }

    /// Makes a mark and returns its id.
    pub fn add(&mut self, image: ImageId, rect: PixelRect, note: String) -> MarkId {
        let id = MarkId(self.next_id);
        self.next_id += 1;
        self.marks.push(Mark {
            id,
            image,
            rect,
            note,
        });
        id
    }

    /// A mark by id.
    pub fn get(&self, id: MarkId) -> Option<&Mark> {
        self.marks.iter().find(|mark| mark.id == id)
    }

    /// Replaces a mark's note. Returns false if there is no such mark.
    pub fn set_note(&mut self, id: MarkId, note: String) -> bool {
        match self.marks.iter_mut().find(|mark| mark.id == id) {
            Some(mark) => {
                mark.note = note;
                true
            }
            None => false,
        }
    }

    /// Removes a mark.
    pub fn remove(&mut self, id: MarkId) -> Option<Mark> {
        let index = self.marks.iter().position(|mark| mark.id == id)?;
        Some(self.marks.remove(index))
    }

    /// Removes an image's marks.
    pub fn remove_image(&mut self, image: ImageId) {
        self.marks.retain(|mark| mark.image != image);
    }

    /// Removes every mark.
    pub fn clear(&mut self) {
        self.marks.clear();
    }

    /// An image's marks, oldest first.
    pub fn for_image(&self, image: ImageId) -> impl Iterator<Item = &Mark> {
        self.marks.iter().filter(move |mark| mark.image == image)
    }

    /// Every mark, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &Mark> {
        self.marks.iter()
    }

    /// How many marks there are.
    pub fn len(&self) -> usize {
        self.marks.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// The newest of an image's marks within `tolerance` image pixels of a
    /// point in image space.
    pub fn hit(&self, image: ImageId, at: Point, tolerance: f64) -> Option<MarkId> {
        self.for_image(image)
            .filter(|mark| {
                let rect = mark.rect;
                at.x >= f64::from(rect.x) - tolerance
                    && at.x <= f64::from(rect.x + rect.width) + tolerance
                    && at.y >= f64::from(rect.y) - tolerance
                    && at.y <= f64::from(rect.y + rect.height) + tolerance
            })
            .last()
            .map(|mark| mark.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: PixelSize = PixelSize::new(100, 50);

    #[test]
    fn a_drag_covers_whole_pixels_in_any_direction() {
        let expected = Some(PixelRect::new(10, 5, 11, 16));
        let corners = [
            (Point::new(10.2, 5.5), Point::new(20.5, 20.1)),
            (Point::new(20.5, 20.1), Point::new(10.2, 5.5)),
            (Point::new(10.2, 20.1), Point::new(20.5, 5.5)),
            (Point::new(20.5, 5.5), Point::new(10.2, 20.1)),
        ];
        for (a, b) in corners {
            assert_eq!(rect_between(a, b, IMAGE), expected);
        }
    }

    #[test]
    fn a_drag_is_clamped_to_the_image() {
        assert_eq!(
            rect_between(Point::new(-30.0, -1.0), Point::new(150.0, 80.0), IMAGE),
            Some(PixelRect::new(0, 0, 100, 50))
        );
        assert_eq!(
            rect_between(Point::new(110.0, 0.0), Point::new(150.0, 40.0), IMAGE),
            None
        );
        assert_eq!(
            rect_between(Point::new(10.0, 10.0), Point::new(10.0, 40.0), IMAGE),
            None
        );
    }

    #[test]
    fn marks_are_made_edited_found_and_removed() {
        let mut marks = Marks::default();
        assert!(marks.is_empty());
        let a = marks.add(ImageId(1), PixelRect::new(0, 0, 10, 10), "a".into());
        let b = marks.add(ImageId(1), PixelRect::new(5, 5, 10, 10), "b".into());
        let c = marks.add(ImageId(2), PixelRect::new(0, 0, 10, 10), String::new());
        assert_eq!((a, b, c), (MarkId(1), MarkId(2), MarkId(3)));
        assert_eq!(marks.len(), 3);

        // The newest mark wins where marks overlap.
        assert_eq!(marks.hit(ImageId(1), Point::new(7.0, 7.0), 0.0), Some(b));
        assert_eq!(marks.hit(ImageId(1), Point::new(2.0, 2.0), 0.0), Some(a));
        assert_eq!(marks.hit(ImageId(1), Point::new(16.0, 7.0), 0.0), None);
        assert_eq!(marks.hit(ImageId(1), Point::new(16.0, 7.0), 2.0), Some(b));

        assert!(marks.set_note(c, "c".into()));
        assert_eq!(marks.get(c).unwrap().note, "c");
        assert!(!marks.set_note(MarkId(9), "x".into()));

        assert_eq!(marks.remove(a).unwrap().note, "a");
        assert!(marks.remove(a).is_none());
        assert_eq!(marks.for_image(ImageId(1)).count(), 1);
        marks.remove_image(ImageId(1));
        assert_eq!(marks.iter().map(|mark| mark.id).collect::<Vec<_>>(), [c]);
        marks.clear();
        assert!(marks.is_empty());
        // Ids are never reused.
        assert_eq!(
            marks.add(ImageId(2), PixelRect::new(0, 0, 1, 1), String::new()),
            MarkId(4)
        );
    }
}
