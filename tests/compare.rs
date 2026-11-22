//! Groups, captions and compare view end to end: a headless viewer driven
//! through `seer.exe`, with screenshots checked for where each image, caption
//! and divider is drawn.

use std::path::{Path, PathBuf};

use seer::protocol::{CompareLayout, ListReply, OpenReply, Reply};
use seer::testutil::{Format, HeadlessViewer, Pixels, Rgba, TempDir, read_image, write_image};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

/// The selected tile's outline.
const ACCENT: Rgba = Rgba::opaque(0x8F, 0xC4, 0xA3);

/// A rectangle of client pixels: left and top inclusive, right and bottom
/// exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

const fn region(left: u32, top: u32, right: u32, bottom: u32) -> Region {
    Region {
        left,
        top,
        right,
        bottom,
    }
}

/// A 64 × 48 image of one colour.
fn solid(dir: &TempDir, name: &str, colour: Rgba) -> PathBuf {
    let path = dir.join(name);
    write_image(&path, &Pixels::solid(64, 48, colour), Format::Png).unwrap();
    path
}

/// Starts an empty viewer at 100% scale with a client area of 256 × 192 above
/// the thumbnail strip, which is 84 pixels high.
fn viewer() -> HeadlessViewer {
    let viewer =
        HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), &[]).expect("the viewer starts");
    input(
        &viewer,
        &[
            r#"{"type":"resize","size":{"width":256,"height":276}}"#,
            r#"{"type":"dpi","dpi":96}"#,
        ],
    );
    viewer
}

fn input(viewer: &HeadlessViewer, events: &[&str]) {
    let mut args = vec!["debug", "input"];
    args.extend(events);
    let output = viewer.seer(&args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn key(viewer: &HeadlessViewer, key: &str) {
    input(viewer, &[&format!(r#"{{"type":"key","key":"{key}"}}"#)]);
}

/// A left-button drag from one client point to another.
fn drag(viewer: &HeadlessViewer, from: (f64, f64), to: (f64, f64)) {
    let at = |(x, y): (f64, f64)| format!(r#"{{"x":{x},"y":{y}}}"#);
    input(
        viewer,
        &[
            &format!(
                r#"{{"type":"pointer_down","button":"left","at":{}}}"#,
                at(from)
            ),
            &format!(r#"{{"type":"pointer_move","at":{}}}"#, at(to)),
            &format!(r#"{{"type":"pointer_up","button":"left","at":{}}}"#, at(to)),
        ],
    );
}

fn screenshot(viewer: &HeadlessViewer, dir: &TempDir, name: &str) -> Pixels {
    let path = dir.join(name);
    let reply: Reply = viewer.seer_json([Path::new("screenshot"), &path]);
    assert!(matches!(reply, Reply::Screenshot { .. }), "{reply:?}");
    read_image(&path).unwrap()
}

fn near(a: Rgba, b: Rgba) -> bool {
    a.r.abs_diff(b.r) <= 8 && a.g.abs_diff(b.g) <= 8 && a.b.abs_diff(b.b) <= 8
}

/// The smallest region holding every pixel of `colour` within `within`, if
/// there are any.
fn extent(shot: &Pixels, within: Region, colour: Rgba) -> Option<Region> {
    let mut found: Option<Region> = None;
    for y in within.top..within.bottom {
        for x in within.left..within.right {
            if near(shot.get(x, y), colour) {
                let r = found.get_or_insert(region(x, y, x + 1, y + 1));
                r.left = r.left.min(x);
                r.top = r.top.min(y);
                r.right = r.right.max(x + 1);
                r.bottom = r.bottom.max(y + 1);
            }
        }
    }
    found
}

/// Whether text is drawn within `within`: the theme's cream text on the dark
/// background leaves bright, slightly warm pixels.
fn has_text(shot: &Pixels, within: Region) -> bool {
    (within.top..within.bottom).any(|y| {
        (within.left..within.right).any(|x| {
            let Rgba { r, g, b, .. } = shot.get(x, y);
            r > 150 && r.abs_diff(g) < 24 && r.abs_diff(b) < 48
        })
    })
}

fn layout(viewer: &HeadlessViewer) -> Option<CompareLayout> {
    viewer.list().compare
}

#[test]
fn a_group_opened_with_compare_shows_two_up_tiles_with_one_shared_zoom() {
    let dir = TempDir::new().unwrap();
    let before = solid(&dir, "before.png", Rgba::RED);
    let after = solid(&dir, "after.png", Rgba::BLUE);
    let viewer = viewer();
    let opened: OpenReply = viewer.seer_json([
        Path::new("open"),
        Path::new("--group"),
        Path::new("fix"),
        Path::new("--caption"),
        Path::new("before the fix"),
        Path::new("--compare"),
        &before,
        &after,
    ]);
    assert!(opened.failed.is_empty());
    let list = viewer.wait_until_loaded(2);
    assert_eq!(list.compare, Some(CompareLayout::Tiles));
    assert_eq!(list.images[0].caption.as_deref(), Some("before the fix"));
    assert_eq!(list.images[1].caption, None);
    assert!(
        list.images
            .iter()
            .all(|image| image.group.as_deref() == Some("fix"))
    );
    let text = viewer.seer(["list"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("compare tiles"));

    // Two tiles 128 pixels wide, each a 120 × 156 viewport inset by 4 with a
    // 28-pixel caption beneath. The images fit at 100%, centred.
    let shot = screenshot(&viewer, &dir, "tiles.png");
    let left = region(0, 0, 128, 192);
    let right = region(128, 0, 256, 192);
    assert_eq!(
        extent(&shot, left, Rgba::RED),
        Some(region(32, 58, 96, 106))
    );
    assert_eq!(
        extent(&shot, right, Rgba::BLUE),
        Some(region(160, 58, 224, 106))
    );
    assert_eq!(extent(&shot, left, Rgba::BLUE), None);
    // Each caption, or the file name without one, beneath its tile.
    assert!(has_text(&shot, region(8, 162, 124, 186)));
    assert!(has_text(&shot, region(136, 162, 252, 186)));
    assert!(!has_text(&shot, region(8, 8, 124, 56)));
    // The selected tile, the first, is outlined round image and caption.
    assert!(near(shot.get(4, 100), ACCENT));
    assert!(near(shot.get(4, 186), ACCENT));
    assert!(!near(shot.get(132, 100), ACCENT));

    // Zooming in zooms both tiles alike.
    key(&viewer, "plus");
    let zoom = viewer.list().view.unwrap().zoom;
    assert!(zoom > 1.0);
    let shot = screenshot(&viewer, &dir, "tiles-zoomed.png");
    let red = extent(&shot, left, Rgba::RED).unwrap();
    let blue = extent(&shot, right, Rgba::BLUE).unwrap();
    let size = |r: Region| (r.right - r.left, r.bottom - r.top);
    assert_eq!(size(red), size(blue));
    assert!(size(red).0.abs_diff((64.0 * zoom).round() as u32) <= 1);
    assert_eq!(blue.left - red.left, 128);

    // Selecting the second tile moves the outline but keeps the zoom.
    key(&viewer, "right");
    let list = viewer.list();
    assert_eq!(list.selected, Some(opened.images[1].id));
    assert_eq!(list.view.unwrap().zoom, zoom);
    let shot = screenshot(&viewer, &dir, "tiles-second.png");
    assert!(near(shot.get(132, 100), ACCENT));
    assert!(!near(shot.get(4, 100), ACCENT));

    // Clicking a tile selects its image.
    let at = r#"{"x":64,"y":82}"#;
    input(
        &viewer,
        &[
            &format!(r#"{{"type":"pointer_down","button":"left","at":{at}}}"#),
            &format!(r#"{{"type":"pointer_up","button":"left","at":{at}}}"#),
        ],
    );
    assert_eq!(viewer.list().selected, Some(opened.images[0].id));

    // C leaves compare view and shows the selected image alone.
    key(&viewer, "c");
    assert_eq!(layout(&viewer), None);
    let shot = screenshot(&viewer, &dir, "single.png");
    assert!(extent(&shot, region(0, 0, 256, 192), Rgba::RED).is_some());
    assert_eq!(extent(&shot, region(0, 0, 256, 192), Rgba::BLUE), None);
    assert!(viewer.quit().success());
}

#[test]
fn four_up_tiles_form_a_grid_with_captions_beneath() {
    let dir = TempDir::new().unwrap();
    let colours = [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE];
    let paths: Vec<PathBuf> = colours
        .iter()
        .enumerate()
        .map(|(index, &colour)| solid(&dir, &format!("{index}.png"), colour))
        .collect();
    let viewer = viewer();
    let mut args: Vec<&Path> = vec![Path::new("open"), Path::new("--group"), Path::new("four")];
    for caption in ["one", "two", "three", "four"] {
        args.extend([Path::new("--caption"), Path::new(caption)]);
    }
    args.push(Path::new("--compare"));
    args.extend(paths.iter().map(PathBuf::as_path));
    let _: OpenReply = viewer.seer_json(args);
    viewer.wait_until_loaded(4);
    assert_eq!(layout(&viewer), Some(CompareLayout::Tiles));

    // Tiles of 128 × 96, each a 120 × 60 viewport and a caption beneath.
    let shot = screenshot(&viewer, &dir, "four.png");
    for (index, &colour) in colours.iter().enumerate() {
        let (x, y) = (128 * (index as u32 % 2), 96 * (index as u32 / 2));
        let tile = region(x, y, x + 128, y + 96);
        assert_eq!(
            extent(&shot, tile, colour),
            Some(region(x + 32, y + 10, x + 96, y + 58)),
            "tile {index}"
        );
        assert!(
            has_text(&shot, region(x + 8, y + 66, x + 124, y + 90)),
            "caption {index}"
        );
    }

    // A pair added to the group with --compare keeps compare view and joins
    // the grid, now three columns.
    let extra = solid(&dir, "4.png", Rgba::opaque(255, 255, 0));
    let _: OpenReply = viewer.seer_json([
        Path::new("open"),
        Path::new("--group"),
        Path::new("four"),
        Path::new("--compare"),
        &extra,
    ]);
    viewer.wait_until_loaded(5);
    assert_eq!(layout(&viewer), Some(CompareLayout::Tiles));
    let shot = screenshot(&viewer, &dir, "five.png");
    let yellow = extent(&shot, region(0, 0, 256, 192), Rgba::opaque(255, 255, 0)).unwrap();
    assert!(yellow.left >= 85 && yellow.right <= 171 && yellow.top >= 96);

    // Clearing the group ends compare view.
    let cleared: Reply = viewer.seer_json(["clear", "four"]);
    assert!(matches!(cleared, Reply::Closed { closed, .. } if closed.len() == 5));
    let list: ListReply = viewer.list();
    assert!(list.images.is_empty());
    assert_eq!(list.compare, None);
    assert!(viewer.quit().success());
}

#[test]
fn the_split_view_divides_two_images_with_a_line_the_user_drags() {
    let dir = TempDir::new().unwrap();
    let before = solid(&dir, "before.png", Rgba::RED);
    let after = solid(&dir, "after.png", Rgba::BLUE);
    let viewer = viewer();
    let _: OpenReply = viewer.seer_json([
        Path::new("open"),
        Path::new("--group"),
        Path::new("fix"),
        Path::new("--caption"),
        Path::new("before"),
        Path::new("--caption"),
        Path::new("after"),
        &before,
        &after,
    ]);
    viewer.wait_until_loaded(2);
    // S does nothing outside compare view.
    key(&viewer, "s");
    assert_eq!(layout(&viewer), None);
    key(&viewer, "c");
    key(&viewer, "s");
    assert_eq!(layout(&viewer), Some(CompareLayout::Split));
    let text = viewer.seer(["list"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("compare split"));

    // Both images are centred over the whole area, the first left of the
    // divider at the middle and the second right of it.
    let whole = region(0, 0, 256, 192);
    let shot = screenshot(&viewer, &dir, "split.png");
    assert_eq!(
        extent(&shot, whole, Rgba::RED),
        Some(region(96, 72, 127, 120))
    );
    assert_eq!(
        extent(&shot, whole, Rgba::BLUE),
        Some(region(129, 72, 160, 120))
    );
    assert_eq!(
        extent(&shot, region(120, 0, 136, 192), Rgba::WHITE),
        Some(region(127, 0, 129, 192))
    );
    // Each side's caption sits in its top corner.
    assert!(has_text(&shot, region(8, 8, 64, 32)));
    assert!(has_text(&shot, region(192, 8, 248, 32)));
    assert!(!has_text(&shot, region(64, 8, 120, 32)));
    assert!(!has_text(&shot, region(136, 8, 192, 32)));

    // Dragging the divider shows more of the first image.
    drag(&viewer, (128.0, 96.0), (144.0, 96.0));
    let shot = screenshot(&viewer, &dir, "split-dragged.png");
    assert_eq!(
        extent(&shot, whole, Rgba::RED),
        Some(region(96, 72, 143, 120))
    );
    assert_eq!(
        extent(&shot, whole, Rgba::BLUE),
        Some(region(145, 72, 160, 120))
    );

    // Zooming zooms both sides alike, about the middle of the area.
    key(&viewer, "plus");
    let zoom = viewer.list().view.unwrap().zoom;
    let shot = screenshot(&viewer, &dir, "split-zoomed.png");
    let red = extent(&shot, whole, Rgba::RED).unwrap();
    let blue = extent(&shot, whole, Rgba::BLUE).unwrap();
    assert_eq!(red.left, 256 - blue.right);
    assert_eq!((red.top, red.bottom), (blue.top, blue.bottom));
    assert!((blue.right - red.left).abs_diff((64.0 * zoom).round() as u32) <= 1);

    // S returns to the tiles, keeping the zoom.
    key(&viewer, "s");
    let list = viewer.list();
    assert_eq!(list.compare, Some(CompareLayout::Tiles));
    assert_eq!(list.view.unwrap().zoom, zoom);
    assert!(viewer.quit().success());
}
