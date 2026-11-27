//! Pasting end to end: Ctrl+V sent to a headless viewer through `seer debug
//! input`, and `seer paste`. A headless viewer never reads the user's
//! clipboard; given `SEER_CLIPBOARD_DIR`, it reads what these tests write to
//! that directory, and saves pasted images in a `pasted` folder inside it.

use std::path::Path;
use std::time::{Duration, Instant};

use seer::protocol::OpenReply;
use seer::testutil::{
    Format, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, read_image, retry, write_image,
};
use seer::viewer::clipboard::{SINK_DIB, SINK_PNG};
use seer::viewer::paste::{SINK_FILES, SINK_PASTED};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

const CTRL_V: &str = r#"{"type":"key","key":"v","ctrl":true}"#;

/// Starts a viewer on `files` whose clipboard is `sink`.
fn viewer(files: &[&Path], sink: &TempDir) -> HeadlessViewer {
    let sink = sink.path().to_str().unwrap();
    HeadlessViewer::start_with_env(
        Path::new(SEERW),
        Path::new(SEER),
        files,
        &[("SEER_CLIPBOARD_DIR", sink)],
    )
    .expect("the viewer starts")
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

/// A picture whose quadrants differ, with one half-transparent pixel.
fn picture() -> Pixels {
    let mut pixels = Pixels::quadrants(32, 16, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
    pixels.set(
        0,
        0,
        Rgba {
            a: 128,
            ..Rgba::RED
        },
    );
    pixels
}

#[test]
fn ctrl_v_saves_the_clipboard_image_and_opens_it() {
    let dir = TempDir::new().unwrap();
    let sink = TempDir::new().unwrap();
    let first = dir.join("first.png");
    write_image(&first, &Pixels::solid(8, 8, Rgba::GREEN), Format::Png).unwrap();
    write_image(&sink.join(SINK_PNG), &picture(), Format::Png).unwrap();
    let viewer = viewer(&[&first], &sink);
    viewer.wait_until_loaded(1);

    input(&viewer, &[CTRL_V]);
    // The image is saved off the UI thread, so it opens a little later.
    let list = viewer.wait_until_loaded(2);
    let pasted = &list.images[1];
    assert_eq!(list.selected, Some(pasted.id));
    let folder = sink.join(SINK_PASTED).join(viewer.instance());
    assert_eq!(pasted.path.parent(), Some(folder.as_path()), "{pasted:?}");
    let name = pasted.path.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("pasted-") && name.ends_with(".png"));
    assert_eq!(retry(|| read_image(&pasted.path)).unwrap(), picture());

    // Closing the pasted image deletes its file; the viewer's own files stay.
    input(&viewer, &[CTRL_V]);
    let list = viewer.wait_until_loaded(3);
    let second = list.images[2].path.clone();
    input(&viewer, &[r#"{"type":"key","key":"delete"}"#]);
    assert!(!second.exists());
    assert!(pasted.path.exists());
    let _: serde_json::Value = viewer.seer_json(["close", "--all"]);
    assert!(!pasted.path.exists());
    assert!(first.exists());

    // Exiting deletes the pasted folder.
    input(&viewer, &[CTRL_V]);
    viewer.wait_until_loaded(1);
    assert!(viewer.quit().success());
    assert!(!folder.exists());
}

#[test]
fn a_viewer_starting_deletes_only_pastes_no_running_viewer_has() {
    let sink = TempDir::new().unwrap();
    write_image(&sink.join(SINK_PNG), &picture(), Format::Png).unwrap();
    let crashed = sink.join(SINK_PASTED).join("crashed-viewer");
    std::fs::create_dir_all(&crashed).unwrap();
    std::fs::write(crashed.join("pasted-1.png"), b"").unwrap();

    let first = viewer(&[], &sink);
    assert!(!crashed.exists());
    let opened: OpenReply = first.seer_json(["paste"]);
    let pasted = opened.images[0].path.clone();
    // A second viewer, on another instance, leaves the first's pastes.
    let second = viewer(&[], &sink);
    assert!(pasted.exists());
    assert!(second.quit().success());
    assert!(pasted.exists());
    assert!(first.quit().success());
    assert!(!pasted.exists());
}

#[test]
fn seer_paste_converts_a_bitmap_and_answers_with_the_file() {
    let sink = TempDir::new().unwrap();
    let viewer = viewer(&[], &sink);
    // A bitmap Seer copied itself: copy an image, then keep only the DIB.
    let dir = TempDir::new().unwrap();
    let source = dir.join("source.png");
    write_image(&source, &picture(), Format::Png).unwrap();
    let _: OpenReply = viewer.seer_json(["open", source.to_str().unwrap()]);
    viewer.wait_until_loaded(1);
    input(&viewer, &[r#"{"type":"key","key":"c","ctrl":true}"#]);
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    while !sink.join(SINK_PNG).is_file() {
        assert!(Instant::now() < deadline, "nothing was copied");
        std::thread::sleep(Duration::from_millis(10));
    }
    retry(|| std::fs::remove_file(sink.join(SINK_PNG))).unwrap();
    assert!(sink.join(SINK_DIB).is_file());

    let opened: OpenReply = viewer.seer_json(["paste", "--group", "bugs", "--caption", "the menu"]);
    assert!(opened.failed.is_empty());
    let image = &opened.images[0];
    assert!(image.path.starts_with(sink.join(SINK_PASTED)));
    assert_eq!(retry(|| read_image(&image.path)).unwrap(), picture());
    let list = viewer.wait_until_loaded(2);
    assert_eq!(list.images[1].group.as_deref(), Some("bugs"));
    assert_eq!(list.images[1].caption.as_deref(), Some("the menu"));
    assert!(viewer.quit().success());
}

#[test]
fn copied_files_open_as_copies() {
    let dir = TempDir::new().unwrap();
    let sink = TempDir::new().unwrap();
    let a = dir.join("a.png");
    let b = dir.join("b.png");
    write_image(&a, &Pixels::solid(8, 8, Rgba::RED), Format::Png).unwrap();
    write_image(&b, &Pixels::solid(8, 8, Rgba::BLUE), Format::Png).unwrap();
    std::fs::write(
        sink.join(SINK_FILES),
        format!("{}\n{}\n", a.display(), b.display()),
    )
    .unwrap();
    let viewer = viewer(&[], &sink);
    let opened: OpenReply = viewer.seer_json(["paste"]);
    let folder = sink.join(SINK_PASTED).join(viewer.instance());
    let paths: Vec<_> = opened.images.iter().map(|image| &image.path).collect();
    assert_eq!(paths, [&folder.join("a.png"), &folder.join("b.png")]);
    assert_eq!(
        retry(|| read_image(paths[0])).unwrap(),
        Pixels::solid(8, 8, Rgba::RED)
    );
    // Pasted again, the copies get names of their own.
    let again: OpenReply = viewer.seer_json(["paste"]);
    assert_eq!(again.images[0].path, folder.join("a-2.png"));
    let _: serde_json::Value = viewer.seer_json(["close", "--all"]);
    assert!(a.exists() && b.exists(), "the originals are never deleted");
    assert!(!folder.join("a.png").exists());
    assert!(viewer.quit().success());
}

#[test]
fn an_empty_clipboard_pastes_nothing() {
    let sink = TempDir::new().unwrap();
    let viewer = viewer(&[], &sink);
    let output = viewer.seer(["--json", "paste"]);
    assert_eq!(output.status.code(), Some(4));
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(printed.contains(r#""code":"no_images""#), "{printed}");
    input(&viewer, &[CTRL_V]);
    assert!(viewer.list().images.is_empty());
    assert!(!viewer.log().contains("could not be pasted"));
    assert!(viewer.quit().success());
}
