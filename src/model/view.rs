//! How an image is shown: zoom, pan, fit and rotation, the visible region in
//! image pixels, and the conversions between client and image coordinates.
//!
//! A [`View`] holds the user's choices. Where they put the image depends also
//! on the image's size and the area it is shown in, which a [`Frame`] gives,
//! so the same view can be applied to images of different sizes (as compare
//! view does) and survives the window being resized.
//!
//! The image is placed by turning it about its centre point, scaling it by the
//! zoom, and putting the view's centre point at the middle of the viewport.
//! When fitted, the zoom is the largest that shows the whole image without
//! enlarging it, and the image is centred. Otherwise the centre is kept where
//! the image covers the viewport as far as it can: along an axis where the
//! image is smaller than the viewport it is centred, and along one where it is
//! larger its edges never come inside the viewport's.

use crate::model::geometry::{PixelSize, Point, Rect, Rotation, Size, Transform};

/// The largest zoom: one image pixel to 64 client pixels.
pub const MAX_ZOOM: f64 = 64.0;

/// The factor one zoom key press, or one notch of the wheel, zooms by.
pub const ZOOM_STEP: f64 = 1.25;

/// The smallest fit zoom, for degenerate sizes.
const MIN_ZOOM: f64 = 1e-3;

/// An image and the part of the client area it is shown in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// The image's size, after EXIF orientation.
    pub image: PixelSize,
    /// The area, in client space.
    pub viewport: Rect,
}

impl Frame {
    /// An image shown in an area.
    pub const fn new(image: PixelSize, viewport: Rect) -> Self {
        Frame { image, viewport }
    }

    fn image_centre(&self) -> Point {
        Point::new(
            f64::from(self.image.width) / 2.0,
            f64::from(self.image.height) / 2.0,
        )
    }

    fn viewport_centre(&self) -> Point {
        Point::new(
            self.viewport.x + self.viewport.width / 2.0,
            self.viewport.y + self.viewport.height / 2.0,
        )
    }

    /// The image's size as shown under a rotation, before zoom.
    fn turned(&self, rotation: Rotation) -> Size {
        let (width, height) = (f64::from(self.image.width), f64::from(self.image.height));
        if rotation.is_sideways() {
            Size::new(height, width)
        } else {
            Size::new(width, height)
        }
    }
}

/// The user's zoom, pan and rotation for an image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    rotation: Rotation,
    fit: bool,
    zoom: f64,
    /// The image point at the viewport's middle, as fractions of the image's
    /// width and height, so it keeps its relative place if the image's size
    /// changes.
    centre: Point,
}

impl Default for View {
    fn default() -> Self {
        View {
            rotation: Rotation::None,
            fit: true,
            zoom: 1.0,
            centre: Point::new(0.5, 0.5),
        }
    }
}

impl View {
    /// The view rotation.
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// Whether the image is fitted to the viewport.
    pub fn is_fit(&self) -> bool {
        self.fit
    }

    /// The zoom that fits the image to the viewport without enlarging it.
    pub fn fit_zoom(&self, frame: Frame) -> f64 {
        let turned = frame.turned(self.rotation);
        if turned.width <= 0.0 || turned.height <= 0.0 {
            return 1.0;
        }
        let zoom = (frame.viewport.width / turned.width)
            .min(frame.viewport.height / turned.height)
            .min(1.0);
        zoom.max(MIN_ZOOM)
    }

    /// The zoom: client pixels per image pixel.
    pub fn zoom(&self, frame: Frame) -> f64 {
        if self.fit {
            self.fit_zoom(frame)
        } else {
            self.zoom
        }
    }

    /// The image point shown at the viewport's middle.
    pub fn centre(&self, frame: Frame) -> Point {
        if self.fit {
            return frame.image_centre();
        }
        let wanted = Point::new(
            self.centre.x * f64::from(frame.image.width),
            self.centre.y * f64::from(frame.image.height),
        );
        self.clamp(frame, self.zoom, wanted)
    }

    /// The transform from image space to client space.
    pub fn transform(&self, frame: Frame) -> Transform {
        let centre = self.centre(frame);
        let zoom = self.zoom(frame);
        let middle = frame.viewport_centre();
        Transform::translation(-centre.x, -centre.y)
            .then(&Transform::rotation(self.rotation))
            .then(&Transform::scale(zoom, zoom))
            .then(&Transform::translation(middle.x, middle.y))
    }

    /// The image point under a client point.
    pub fn to_image(&self, frame: Frame, client: Point) -> Point {
        self.transform(frame)
            .invert()
            .expect("the zoom is never zero")
            .apply(client)
    }

    /// The client point over an image point.
    pub fn to_client(&self, frame: Frame, image: Point) -> Point {
        self.transform(frame).apply(image)
    }

    /// The rectangle in client space covering a rectangle in image space.
    pub fn rect_to_client(&self, frame: Frame, rect: Rect) -> Rect {
        let transform = self.transform(frame);
        bounds(
            transform.apply(Point::new(rect.x, rect.y)),
            transform.apply(Point::new(rect.right(), rect.bottom())),
        )
    }

    /// The part of the image inside the viewport, in image pixels.
    pub fn visible(&self, frame: Frame) -> Rect {
        let viewport = frame.viewport;
        let corners = bounds(
            self.to_image(frame, Point::new(viewport.x, viewport.y)),
            self.to_image(frame, Point::new(viewport.right(), viewport.bottom())),
        );
        let left = corners.x.max(0.0);
        let top = corners.y.max(0.0);
        let right = corners.right().min(f64::from(frame.image.width));
        let bottom = corners.bottom().min(f64::from(frame.image.height));
        Rect::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }

    /// Fits the image to the viewport.
    pub fn fit(&mut self) {
        self.fit = true;
        self.centre = Point::new(0.5, 0.5);
    }

    /// Multiplies the zoom by `factor`, keeping the image point under `at`
    /// where it is. The zoom stays between the fit zoom and [`MAX_ZOOM`], and
    /// reaching the fit zoom fits the image.
    pub fn zoom_about(&mut self, frame: Frame, at: Point, factor: f64) {
        let zoom = self.zoom(frame) * factor;
        self.zoom_to(frame, at, zoom);
    }

    /// Sets the zoom, keeping the image point under `at` where it is, within
    /// the limits [`View::zoom_about`] describes.
    pub fn zoom_to(&mut self, frame: Frame, at: Point, zoom: f64) {
        let fit_zoom = self.fit_zoom(frame);
        let zoom = zoom.min(MAX_ZOOM);
        if zoom.is_nan() || zoom <= fit_zoom {
            self.fit();
            return;
        }
        let under = self.to_image(frame, at);
        let middle = frame.viewport_centre();
        let offset = Point::new((at.x - middle.x) / zoom, (at.y - middle.y) / zoom);
        // `offset` is in turned image units; turn it back into image space.
        let back = self.unturn(offset);
        self.fit = false;
        self.zoom = zoom;
        self.set_centre(frame, Point::new(under.x - back.x, under.y - back.y));
    }

    /// Moves the image by `delta` client pixels. A fitted image does not move.
    pub fn pan(&mut self, frame: Frame, delta: Point) {
        if self.fit {
            return;
        }
        let centre = self.centre(frame);
        let back = self.unturn(Point::new(delta.x / self.zoom, delta.y / self.zoom));
        self.set_centre(frame, Point::new(centre.x - back.x, centre.y - back.y));
    }

    /// Turns the view a quarter turn clockwise.
    pub fn rotate_clockwise(&mut self) {
        self.rotation = self.rotation.clockwise();
    }

    /// Turns the view a quarter turn anticlockwise.
    pub fn rotate_anticlockwise(&mut self) {
        self.rotation = self.rotation.anticlockwise();
    }

    fn unturn(&self, point: Point) -> Point {
        Transform::rotation(self.rotation)
            .invert()
            .expect("rotations are invertible")
            .apply(point)
    }

    fn set_centre(&mut self, frame: Frame, centre: Point) {
        let centre = self.clamp(frame, self.zoom, centre);
        let fraction = |value: f64, size: u32| {
            if size == 0 {
                0.5
            } else {
                value / f64::from(size)
            }
        };
        self.centre = Point::new(
            fraction(centre.x, frame.image.width),
            fraction(centre.y, frame.image.height),
        );
    }

    /// Keeps a centre point, in image pixels, where the image covers as much
    /// of the viewport as it can at this zoom.
    fn clamp(&self, frame: Frame, zoom: f64, centre: Point) -> Point {
        let middle = frame.image_centre();
        let turn = Transform::rotation(self.rotation);
        let offset = turn.apply(Point::new(centre.x - middle.x, centre.y - middle.y));
        let turned = frame.turned(self.rotation);
        let limit_x = (turned.width - frame.viewport.width / zoom).max(0.0) / 2.0;
        let limit_y = (turned.height - frame.viewport.height / zoom).max(0.0) / 2.0;
        let offset = Point::new(
            offset.x.clamp(-limit_x, limit_x),
            offset.y.clamp(-limit_y, limit_y),
        );
        let back = self.unturn(offset);
        Point::new(middle.x + back.x, middle.y + back.y)
    }
}

/// The rectangle with two opposite corners at `a` and `b`.
fn bounds(a: Point, b: Point) -> Rect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    Rect::new(left, top, a.x.max(b.x) - left, a.y.max(b.y) - top)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);

    fn frame(width: u32, height: u32) -> Frame {
        Frame::new(PixelSize::new(width, height), WINDOW)
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn near_point(a: Point, b: Point) -> bool {
        near(a.x, b.x) && near(a.y, b.y)
    }

    fn near_rect(a: Rect, b: Rect) -> bool {
        near(a.x, b.x) && near(a.y, b.y) && near(a.width, b.width) && near(a.height, b.height)
    }

    #[test]
    fn fit_shrinks_a_large_image_and_centres_it() {
        let view = View::default();
        let frame = frame(1600, 600);
        assert!(view.is_fit());
        assert_eq!(view.zoom(frame), 0.5);
        assert!(near_point(
            view.to_client(frame, Point::new(0.0, 0.0)),
            Point::new(0.0, 150.0)
        ));
        assert!(near_rect(
            view.visible(frame),
            Rect::new(0.0, 0.0, 1600.0, 600.0)
        ));
    }

    #[test]
    fn fit_never_enlarges_a_small_image() {
        let view = View::default();
        let frame = frame(100, 50);
        assert_eq!(view.fit_zoom(frame), 1.0);
        assert!(near_point(
            view.to_client(frame, Point::new(0.0, 0.0)),
            Point::new(350.0, 275.0)
        ));
    }

    #[test]
    fn fit_copes_with_degenerate_sizes() {
        let view = View::default();
        assert_eq!(view.fit_zoom(frame(0, 10)), 1.0);
        let empty = Frame::new(PixelSize::new(10, 10), Rect::default());
        assert_eq!(view.fit_zoom(empty), MIN_ZOOM);
    }

    #[test]
    fn fit_accounts_for_rotation() {
        let mut view = View::default();
        let frame = frame(1200, 400);
        assert!(near(view.fit_zoom(frame), 800.0 / 1200.0));
        view.rotate_clockwise();
        assert_eq!(view.rotation(), Rotation::Clockwise90);
        assert!(near(view.fit_zoom(frame), 600.0 / 1200.0));
        view.rotate_anticlockwise();
        assert_eq!(view.rotation(), Rotation::None);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        for rotation in 0..4 {
            let mut view = View::default();
            for _ in 0..rotation {
                view.rotate_clockwise();
            }
            let frame = frame(4000, 3000);
            let at = Point::new(300.0, 200.0);
            let before = view.to_image(frame, at);
            view.zoom_about(frame, at, 4.0);
            assert!(!view.is_fit());
            assert!(near_point(view.to_image(frame, at), before), "{rotation}");
            assert!(near(view.zoom(frame), 4.0 * view.fit_zoom(frame)));
        }
    }

    #[test]
    fn zoom_is_limited() {
        let mut view = View::default();
        let frame = frame(4000, 3000);
        view.zoom_about(frame, Point::new(400.0, 300.0), 1e6);
        assert_eq!(view.zoom(frame), MAX_ZOOM);
        view.zoom_to(frame, Point::new(400.0, 300.0), 1.0);
        assert_eq!(view.zoom(frame), 1.0);
        // Zooming out as far as the fit zoom fits the image.
        view.zoom_about(frame, Point::new(10.0, 10.0), 1e-6);
        assert!(view.is_fit());
        assert_eq!(view.zoom(frame), 0.2);
        // A small image cannot be zoomed out at all.
        let mut view = View::default();
        view.zoom_about(self::frame(10, 10), Point::new(0.0, 0.0), 0.5);
        assert!(view.is_fit());
    }

    #[test]
    fn zooming_near_an_edge_keeps_the_image_covering_the_viewport() {
        let mut view = View::default();
        let frame = frame(1600, 1200);
        // Zoom to 100% about the top-left corner of the window.
        view.zoom_to(frame, Point::new(0.0, 0.0), 1.0);
        assert!(near_point(view.centre(frame), Point::new(400.0, 300.0)));
        assert!(near_rect(
            view.visible(frame),
            Rect::new(0.0, 0.0, 800.0, 600.0)
        ));
    }

    #[test]
    fn panning_moves_the_image_and_stops_at_its_edges() {
        let mut view = View::default();
        let frame = frame(1600, 1200);
        view.pan(frame, Point::new(100.0, 0.0));
        assert!(view.is_fit());
        view.zoom_to(frame, Point::new(400.0, 300.0), 1.0);
        assert!(near_point(view.centre(frame), Point::new(800.0, 600.0)));
        // Dragging right shows more of the left of the image.
        view.pan(frame, Point::new(100.0, 50.0));
        assert!(near_point(view.centre(frame), Point::new(700.0, 550.0)));
        view.pan(frame, Point::new(10_000.0, -10_000.0));
        assert!(near_point(view.centre(frame), Point::new(400.0, 900.0)));
    }

    #[test]
    fn panning_follows_the_rotation() {
        let mut view = View::default();
        view.rotate_clockwise();
        let frame = frame(1600, 1200);
        view.zoom_to(frame, Point::new(400.0, 300.0), 1.0);
        // Turned clockwise, the image's bottom is on the left, so dragging right
        // brings more of the bottom into view.
        view.pan(frame, Point::new(100.0, 0.0));
        assert!(near_point(view.centre(frame), Point::new(800.0, 700.0)));
        let point = Point::new(123.0, 456.0);
        assert!(near_point(
            view.to_image(frame, view.to_client(frame, point)),
            point
        ));
    }

    #[test]
    fn an_image_smaller_than_the_viewport_stays_centred_on_that_axis() {
        let mut view = View::default();
        let frame = frame(1000, 100);
        view.zoom_to(frame, Point::new(0.0, 0.0), 2.0);
        // 200 pixels tall fits in 600, so it is centred vertically.
        assert!(near(view.centre(frame).y, 50.0));
        assert!(near(view.visible(frame).height, 100.0));
        assert!(near(view.visible(frame).width, 400.0));
    }

    #[test]
    fn the_centre_keeps_its_relative_place_when_the_image_size_changes() {
        let mut view = View::default();
        view.zoom_to(frame(1600, 1200), Point::new(0.0, 0.0), 1.0);
        assert!(near_point(
            view.centre(frame(3200, 2400)),
            Point::new(800.0, 600.0)
        ));
        let mut view = View::default();
        view.zoom_to(frame(0, 0), Point::new(0.0, 0.0), 2.0);
        assert!(near_point(view.centre(frame(0, 0)), Point::new(0.0, 0.0)));
    }

    #[test]
    fn rects_map_to_client_space() {
        let mut view = View::default();
        view.rotate_clockwise();
        view.rotate_clockwise();
        let frame = frame(800, 600);
        // Turned half way, the top-left corner is at the bottom right.
        assert!(near_rect(
            view.rect_to_client(frame, Rect::new(0.0, 0.0, 100.0, 50.0)),
            Rect::new(700.0, 550.0, 100.0, 50.0)
        ));
        view.fit();
        assert!(view.is_fit());
    }
}
