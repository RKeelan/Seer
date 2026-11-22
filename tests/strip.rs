//! The thumbnail strip end to end: a headless viewer driven through
//! `seer.exe`, with screenshots checked for the thumbnails and the selection,
//! and a click on a thumbnail selecting its image.

use std::path::{Path, PathBuf};
use std::time::Instant;

use seer::protocol::{OpenReply, Reply};
use seer::testutil::{
    Format, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, read_image, write_image,
};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

/// The selected thumbnail's outline.
const ACCENT: Rgba = Rgba::opaque(0x8F, 0xC4, 0xA3);
/// The strip's background in the dark theme.
const STRIP: Rgba = Rgba::opaque(0x22, 0x1C, 0x19);
/// The background above the strip in the dark theme.
const BACKGROUND: Rgba = Rgba::opaque(0x1A, 0x17, 0x14);

/// The middle of each of the first three thumbnail boxes in a 256 × 276
/// client area at 100% scale: the strip takes the bottom 84 rows, and its
/// boxes are 64 × 48, 8 apart, 24 below the strip's top.
const THUMBNAILS: [(u32, u32); 3] = [(40, 240), (112, 240), (184, 240)];

/// A 64 × 48 image of one colour.
fn solid(dir: &TempDir, name: &str, colour: Rgba) -> PathBuf {
    let path = dir.join(name);
    write_image(&path, &Pixels::solid(64, 48, colour), Format::Png).unwrap();
    path
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

fn screenshot(viewer: &HeadlessViewer, dir: &TempDir) -> Pixels {
    let path = dir.join("shot.png");
    let reply: Reply = viewer.seer_json([Path::new("screenshot"), &path]);
    assert!(matches!(reply, Reply::Screenshot { .. }), "{reply:?}");
    read_image(&path).unwrap()
}

fn near(a: Rgba, b: Rgba) -> bool {
    a.r.abs_diff(b.r) <= 8 && a.g.abs_diff(b.g) <= 8 && a.b.abs_diff(b.b) <= 8
}

/// Takes screenshots until `ready` holds for one, which it returns.
/// Thumbnails are decoded in the background, after the images themselves.
fn screenshot_when(
    viewer: &HeadlessViewer,
    dir: &TempDir,
    ready: impl Fn(&Pixels) -> bool,
) -> Pixels {
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    loop {
        let shot = screenshot(viewer, dir);
        if ready(&shot) {
            return shot;
        }
        assert!(
            Instant::now() < deadline,
            "the screenshot never became ready"
        );
        std::thread::yield_now();
    }
}

#[test]
fn the_strip_shows_each_image_and_a_click_selects_one() {
    let dir = TempDir::new().unwrap();
    let colours = [Rgba::RED, Rgba::GREEN, Rgba::BLUE];
    let paths: Vec<PathBuf> = ["red.png", "green.png", "blue.png"]
        .iter()
        .zip(colours)
        .map(|(name, colour)| solid(&dir, name, colour))
        .collect();
    let viewer =
        HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), &[]).expect("the viewer starts");
    input(
        &viewer,
        &[
            r#"{"type":"resize","size":{"width":256,"height":276}}"#,
            r#"{"type":"dpi","dpi":96}"#,
        ],
    );

    // One image: no strip, so the image is in the middle of the whole client
    // area.
    let opened: OpenReply = viewer.seer_json([Path::new("open"), &paths[0]]);
    viewer.wait_until_loaded(1);
    let shot = screenshot(&viewer, &dir);
    assert_eq!(shot.get(128, 138), Rgba::RED);
    assert_eq!(shot.get(40, 240), BACKGROUND);

    // Three: each has its thumbnail, and the selected one is outlined.
    // Thumbnails are decoded whether or not their images are, so this waits
    // only for what the screenshot shows.
    let more: OpenReply = viewer.seer_json([Path::new("open"), &paths[1], &paths[2]]);
    let shot = screenshot_when(&viewer, &dir, |shot| {
        near(shot.get(128, 96), Rgba::GREEN)
            && THUMBNAILS
                .iter()
                .zip(colours)
                .all(|(&(x, y), colour)| near(shot.get(x, y), colour))
    });
    assert_eq!(shot.get(128, 96), Rgba::GREEN);
    assert_eq!(shot.get(4, 200), STRIP);
    // The second box spans x from 80 to 144 and y from 216 to 264; its
    // outline is 2 pixels wide, 2 pixels outside it.
    assert!(near(shot.get(112, 214), ACCENT), "{:?}", shot.get(112, 214));
    assert!(!near(shot.get(40, 214), ACCENT));

    // A click on the third thumbnail selects its image.
    input(
        &viewer,
        &[
            r#"{"type":"pointer_down","button":"left","at":{"x":184,"y":240}}"#,
            r#"{"type":"pointer_up","button":"left","at":{"x":184,"y":240}}"#,
        ],
    );
    assert_eq!(viewer.list().selected, Some(more.images[1].id));
    let shot = screenshot_when(&viewer, &dir, |shot| shot.get(128, 96) == Rgba::BLUE);
    assert!(near(shot.get(184, 214), ACCENT));

    // Closing all but one hides the strip again.
    let output = viewer.seer([
        "close".to_string(),
        more.images[0].id.0.to_string(),
        more.images[1].id.0.to_string(),
    ]);
    assert!(output.status.success());
    assert_eq!(viewer.list().selected, Some(opened.images[0].id));
    assert_eq!(screenshot(&viewer, &dir).get(40, 240), BACKGROUND);

    assert!(viewer.quit().success());
}
