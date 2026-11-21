//! Plain geometry shared by the model, the protocol and the scene.
//!
//! Two coordinate spaces meet here. Image space is the image's own pixels,
//! with the origin at the top-left of the image as stored (after EXIF
//! orientation, before any view rotation). Client space is the window's client
//! area in physical pixels, with the origin at its top-left. In both, `y`
//! grows downwards. A [`Transform`] maps one to the other.

use serde::{Deserialize, Serialize};

/// A point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Distance from the left.
    pub x: f64,
    /// Distance from the top.
    pub y: f64,
}

impl Point {
    /// The point `(x, y)`.
    pub const fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }
}

/// A size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

impl Size {
    /// The size `width × height`.
    pub const fn new(width: f64, height: f64) -> Self {
        Size { width, height }
    }
}

/// An axis-aligned rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width, never negative.
    pub width: f64,
    /// Height, never negative.
    pub height: f64,
}

impl Rect {
    /// The rectangle with top-left `(x, y)` and the given size.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// The right edge.
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    /// The bottom edge.
    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// Whether the point lies inside, counting the top and left edges but not
    /// the bottom and right.
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }
}

/// A size in whole pixels, such as an image's pixel size or a window's client
/// size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PixelSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl PixelSize {
    /// The size `width × height`.
    pub const fn new(width: u32, height: u32) -> Self {
        PixelSize { width, height }
    }
}

/// A rectangle in whole image pixels, such as a mark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PixelRect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl PixelRect {
    /// The rectangle with top-left `(x, y)` and the given size.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        PixelRect {
            x,
            y,
            width,
            height,
        }
    }
}

impl From<PixelRect> for Rect {
    fn from(rect: PixelRect) -> Self {
        Rect::new(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.width),
            f64::from(rect.height),
        )
    }
}

/// A view rotation, clockwise, in quarter turns.
///
/// Serialised as degrees: `0`, `90`, `180` or `270`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "u16", try_from = "u16")]
pub enum Rotation {
    /// Upright.
    #[default]
    None,
    /// A quarter turn clockwise.
    Clockwise90,
    /// A half turn.
    Half,
    /// A quarter turn anticlockwise.
    Clockwise270,
}

impl Rotation {
    /// The rotation in degrees clockwise.
    pub fn degrees(self) -> u16 {
        match self {
            Rotation::None => 0,
            Rotation::Clockwise90 => 90,
            Rotation::Half => 180,
            Rotation::Clockwise270 => 270,
        }
    }

    /// This rotation followed by a further quarter turn clockwise.
    pub fn clockwise(self) -> Self {
        match self {
            Rotation::None => Rotation::Clockwise90,
            Rotation::Clockwise90 => Rotation::Half,
            Rotation::Half => Rotation::Clockwise270,
            Rotation::Clockwise270 => Rotation::None,
        }
    }

    /// This rotation followed by a further quarter turn anticlockwise.
    pub fn anticlockwise(self) -> Self {
        self.clockwise().clockwise().clockwise()
    }

    /// Whether the rotation swaps width and height.
    pub fn is_sideways(self) -> bool {
        matches!(self, Rotation::Clockwise90 | Rotation::Clockwise270)
    }
}

impl From<Rotation> for u16 {
    fn from(rotation: Rotation) -> Self {
        rotation.degrees()
    }
}

impl TryFrom<u16> for Rotation {
    type Error = String;

    fn try_from(degrees: u16) -> Result<Self, Self::Error> {
        match degrees {
            0 => Ok(Rotation::None),
            90 => Ok(Rotation::Clockwise90),
            180 => Ok(Rotation::Half),
            270 => Ok(Rotation::Clockwise270),
            _ => Err(format!("rotation must be 0, 90, 180 or 270, not {degrees}")),
        }
    }
}

/// An affine transform, laid out as Direct2D's `Matrix3x2` so the renderer can
/// use it as is.
///
/// A point maps as `x' = x·m11 + y·m21 + dx` and `y' = x·m12 + y·m22 + dy`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    /// Row 1, column 1.
    pub m11: f64,
    /// Row 1, column 2.
    pub m12: f64,
    /// Row 2, column 1.
    pub m21: f64,
    /// Row 2, column 2.
    pub m22: f64,
    /// Translation along x.
    pub dx: f64,
    /// Translation along y.
    pub dy: f64,
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    /// The transform that leaves every point where it is.
    pub const IDENTITY: Transform = Transform {
        m11: 1.0,
        m12: 0.0,
        m21: 0.0,
        m22: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    /// A translation by `(dx, dy)`.
    pub const fn translation(dx: f64, dy: f64) -> Self {
        Transform {
            dx,
            dy,
            ..Transform::IDENTITY
        }
    }

    /// A scale about the origin.
    pub const fn scale(sx: f64, sy: f64) -> Self {
        Transform {
            m11: sx,
            m22: sy,
            ..Transform::IDENTITY
        }
    }

    /// A rotation about the origin, clockwise on screen (where `y` grows
    /// downwards).
    pub const fn rotation(rotation: Rotation) -> Self {
        let (cos, sin) = match rotation {
            Rotation::None => (1.0, 0.0),
            Rotation::Clockwise90 => (0.0, 1.0),
            Rotation::Half => (-1.0, 0.0),
            Rotation::Clockwise270 => (0.0, -1.0),
        };
        Transform {
            m11: cos,
            m12: sin,
            m21: -sin,
            m22: cos,
            dx: 0.0,
            dy: 0.0,
        }
    }

    /// This transform followed by `next`.
    pub fn then(&self, next: &Transform) -> Transform {
        Transform {
            m11: self.m11 * next.m11 + self.m12 * next.m21,
            m12: self.m11 * next.m12 + self.m12 * next.m22,
            m21: self.m21 * next.m11 + self.m22 * next.m21,
            m22: self.m21 * next.m12 + self.m22 * next.m22,
            dx: self.dx * next.m11 + self.dy * next.m21 + next.dx,
            dy: self.dx * next.m12 + self.dy * next.m22 + next.dy,
        }
    }

    /// Where the transform takes a point.
    pub fn apply(&self, point: Point) -> Point {
        Point {
            x: point.x * self.m11 + point.y * self.m21 + self.dx,
            y: point.x * self.m12 + point.y * self.m22 + self.dy,
        }
    }

    /// The transform that undoes this one, or `None` if it collapses the plane
    /// (a zero scale, say).
    pub fn invert(&self) -> Option<Transform> {
        let determinant = self.m11 * self.m22 - self.m12 * self.m21;
        if determinant == 0.0 || !determinant.is_finite() {
            return None;
        }
        let m11 = self.m22 / determinant;
        let m12 = -self.m12 / determinant;
        let m21 = -self.m21 / determinant;
        let m22 = self.m11 / determinant;
        Some(Transform {
            m11,
            m12,
            m21,
            m22,
            dx: -(self.dx * m11 + self.dy * m21),
            dy: -(self.dx * m12 + self.dy * m22),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
    }

    #[test]
    fn rect_edges_and_containment() {
        let rect = Rect::new(10.0, 20.0, 30.0, 40.0);
        assert_eq!(rect.right(), 40.0);
        assert_eq!(rect.bottom(), 60.0);
        assert!(rect.contains(Point::new(10.0, 20.0)));
        assert!(!rect.contains(Point::new(40.0, 30.0)));
        assert!(!rect.contains(Point::new(15.0, 60.0)));
        assert_eq!(
            Rect::from(PixelRect::new(1, 2, 3, 4)),
            Rect::new(1.0, 2.0, 3.0, 4.0)
        );
        assert_eq!(PixelSize::new(3, 4).width, 3);
        assert_eq!(Size::new(3.0, 4.0).height, 4.0);
    }

    #[test]
    fn rotation_turns_and_serialises_as_degrees() {
        let mut rotation = Rotation::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(rotation.degrees());
            rotation = rotation.clockwise();
        }
        assert_eq!(seen, [0, 90, 180, 270]);
        assert_eq!(rotation, Rotation::None);
        assert_eq!(Rotation::None.anticlockwise(), Rotation::Clockwise270);
        assert!(Rotation::Clockwise90.is_sideways());
        assert!(!Rotation::Half.is_sideways());

        assert_eq!(serde_json::to_string(&Rotation::Half).unwrap(), "180");
        assert_eq!(
            serde_json::from_str::<Rotation>("270").unwrap(),
            Rotation::Clockwise270
        );
        assert!(serde_json::from_str::<Rotation>("45").is_err());
    }

    #[test]
    fn quarter_turn_clockwise_takes_right_to_down() {
        let turned = Transform::rotation(Rotation::Clockwise90).apply(Point::new(1.0, 0.0));
        assert!(close(turned, Point::new(0.0, 1.0)), "{turned:?}");
        for rotation in [Rotation::None, Rotation::Half, Rotation::Clockwise270] {
            let there = Transform::rotation(rotation).apply(Point::new(2.0, 3.0));
            let back = Transform::rotation(rotation).invert().unwrap().apply(there);
            assert!(close(back, Point::new(2.0, 3.0)));
        }
    }

    #[test]
    fn then_applies_in_order_and_invert_undoes() {
        let transform = Transform::scale(2.0, 3.0)
            .then(&Transform::rotation(Rotation::Clockwise90))
            .then(&Transform::translation(10.0, 20.0));
        let point = Point::new(1.0, 1.0);
        // Scale to (2, 3), turn to (-3, 2), move to (7, 22).
        assert!(close(transform.apply(point), Point::new(7.0, 22.0)));
        let inverse = transform.invert().unwrap();
        assert!(close(inverse.apply(transform.apply(point)), point));
        assert_eq!(Transform::default(), Transform::IDENTITY);
        assert!(Transform::scale(0.0, 1.0).invert().is_none());
    }
}
