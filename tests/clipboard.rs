//! Copying end to end: Ctrl+C sent to a headless viewer through `seer debug
//! input`. A headless viewer never touches the user's clipboard; given
//! `SEER_CLIPBOARD_DIR`, it writes what it would have put there to files in
//! that directory, which these tests read back.

use std::path::Path;
use std::time::{Duration, Instant};

use seer::testutil::{
    Format, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, assert_similar, read_image,
    write_image,
};
use seer::viewer::clipboard::{SINK_DIB, SINK_PNG};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

const CTRL_C: &str = r#"{"type":"key","key":"c","ctrl":true}"#;

/// Starts a viewer on `path` in a 128 × 96 client area at 100% scale, copying
/// to `sink` if given.
fn viewer(path: &Path, sink: Option<&TempDir>) -> HeadlessViewer {
    let sink = sink.map(|sink| sink.path().to_str().unwrap().to_owned());
    let env: Vec<(&str, &str)> = sink
        .iter()
        .map(|sink| ("SEER_CLIPBOARD_DIR", sink.as_str()))
        .collect();
    let viewer = HeadlessViewer::start_with_env(Path::new(SEERW), Path::new(SEER), [path], &env)
        .expect("the viewer starts");
    viewer.wait_until_loaded(1);
    input(
        &viewer,
        &[
            r#"{"type":"resize","size":{"width":128,"height":96}}"#,
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

/// A left click at a client point.
fn click(viewer: &HeadlessViewer, x: f64, y: f64) {
    input(
        viewer,
        &[
            &format!(r#"{{"type":"pointer_down","button":"left","at":{{"x":{x},"y":{y}}}}}"#),
            &format!(r#"{{"type":"pointer_up","button":"left","at":{{"x":{x},"y":{y}}}}}"#),
        ],
    );
}

/// Polls until `done`, panicking after [`VIEWER_TIMEOUT`].
fn wait_until(done: impl Fn() -> bool) {
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "gave up waiting");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the viewer copied, once it has, as the PNG and the length of the
/// DIB. Copies are prepared off the viewer's UI thread, so they land after
/// `seer debug input` returns; the viewer writes the PNG last.
fn copied(sink: &TempDir) -> (Pixels, usize) {
    wait_until(|| sink.join(SINK_PNG).is_file());
    let png = read_image(&sink.join(SINK_PNG)).expect("a PNG was copied");
    let dib = std::fs::read(sink.join(SINK_DIB)).expect("a DIB was copied");
    std::fs::remove_file(sink.join(SINK_PNG)).unwrap();
    std::fs::remove_file(sink.join(SINK_DIB)).unwrap();
    (png, dib.len())
}

#[test]
fn ctrl_c_copies_the_image_the_marked_region_and_the_turned_view() {
    let dir = TempDir::new().unwrap();
    let sink = TempDir::new().unwrap();
    let path = dir.join("gradient.png");
    // Every pixel differs, and the top-left one is half transparent.
    let image = Pixels::from_fn(64, 32, |x, y| Rgba {
        r: (x * 4) as u8,
        g: (y * 8) as u8,
        b: 100,
        a: if (x, y) == (0, 0) { 128 } else { 255 },
    });
    write_image(&path, &image, Format::Png).unwrap();
    // The image shows at 100%, its top-left at (32, 32).
    let viewer = viewer(&path, Some(&sink));

    input(&viewer, &[CTRL_C]);
    let (png, dib) = copied(&sink);
    assert_similar(&image, &png, 2);
    assert_eq!(dib, 124 + 64 * 32 * 4);

    // A selected mark copies only what it covers.
    input(
        &viewer,
        &[
            r#"{"type":"pointer_down","button":"right","at":{"x":40,"y":36}}"#,
            r#"{"type":"pointer_move","at":{"x":56,"y":52}}"#,
            r#"{"type":"pointer_up","button":"right","at":{"x":56,"y":52}}"#,
            r#"{"type":"key","key":"enter"}"#,
        ],
    );
    click(&viewer, 48.0, 44.0);
    input(&viewer, &[CTRL_C]);
    let (png, _) = copied(&sink);
    let marked = Pixels::from_fn(16, 16, |x, y| image.get(x + 8, y + 4));
    assert_similar(&marked, &png, 2);

    // The copy is turned as the view is.
    input(
        &viewer,
        &[
            r#"{"type":"key","key":"escape"}"#,
            r#"{"type":"key","key":"r"}"#,
            CTRL_C,
        ],
    );
    let (png, _) = copied(&sink);
    let turned = Pixels::from_fn(32, 64, |x, y| image.get(y, 31 - x));
    assert_similar(&turned, &png, 2);
    assert!(!viewer.log().contains("could not be copied"));
    assert!(viewer.quit().success());
}

#[test]
fn svg_documents_are_copied_at_their_own_size() {
    let dir = TempDir::new().unwrap();
    let sink = TempDir::new().unwrap();
    let path = dir.join("halves.svg");
    std::fs::write(
        &path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
             <rect width="20" height="20" fill="red"/>
             <rect x="20" width="20" height="20" fill="blue"/>
           </svg>"#,
    )
    .unwrap();
    let viewer = viewer(&path, Some(&sink));
    input(&viewer, &[r#"{"type":"key","key":"plus"}"#, CTRL_C]);
    let (png, _) = copied(&sink);
    let halves = Pixels::from_fn(40, 20, |x, _| if x < 20 { Rgba::RED } else { Rgba::BLUE });
    assert_similar(&halves, &png, 2);
    assert!(viewer.quit().success());
}

#[test]
fn a_headless_viewer_without_a_sink_copies_nothing() {
    let dir = TempDir::new().unwrap();
    let path = dir.join("green.png");
    write_image(&path, &Pixels::solid(8, 8, Rgba::GREEN), Format::Png).unwrap();
    let viewer = viewer(&path, None);
    input(&viewer, &[CTRL_C]);
    wait_until(|| viewer.log().contains("could not be copied"));
    assert!(viewer.quit().success());
}
