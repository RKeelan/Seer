//! Marking end to end: a headless viewer driven through `seer debug input` as
//! a user would draw marks and type notes, read back through `seer marks`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use seer::model::geometry::{PixelRect, PixelSize};
use seer::protocol::{ListReply, Reply};
use seer::testutil::{
    Format, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, read_image, write_image,
};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

/// The banner shown while an agent waits for marks.
const BANNER: Rgba = Rgba::opaque(0x1E, 0x4D, 0x36);
/// A mark's outline.
const MARK: Rgba = Rgba::opaque(255, 0, 0);
/// A selected mark's outline.
const SELECTED_MARK: Rgba = Rgba::opaque(0, 120, 255);

/// Starts a viewer on a 256 × 192 green image, in a 128 × 96 client area at
/// 100% scale, where the image fits at 50%.
fn viewer(dir: &TempDir) -> (HeadlessViewer, PathBuf) {
    let path = dir.join("green.png");
    write_image(&path, &Pixels::solid(256, 192, Rgba::GREEN), Format::Png).unwrap();
    let viewer = HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), &[&path])
        .expect("the viewer starts");
    viewer.wait_until_loaded(1);
    input(
        &viewer,
        &[
            r#"{"type":"resize","size":{"width":128,"height":96}}"#,
            r#"{"type":"dpi","dpi":96}"#,
        ],
    );
    (viewer, path)
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

/// A right-button drag from one client point to another.
fn drag(viewer: &HeadlessViewer, from: (f64, f64), to: (f64, f64)) {
    let at = |(x, y): (f64, f64)| format!(r#"{{"x":{x},"y":{y}}}"#);
    input(
        viewer,
        &[
            &format!(
                r#"{{"type":"pointer_down","button":"right","at":{}}}"#,
                at(from)
            ),
            &format!(r#"{{"type":"pointer_move","at":{}}}"#, at(to)),
            &format!(
                r#"{{"type":"pointer_up","button":"right","at":{}}}"#,
                at(to)
            ),
        ],
    );
}

/// Draws a mark, types its note and keeps it with Enter.
fn mark(viewer: &HeadlessViewer, from: (f64, f64), to: (f64, f64), note: &str) {
    drag(viewer, from, to);
    input(
        viewer,
        &[
            &format!(r#"{{"type":"note_text","text":"{note}"}}"#),
            r#"{"type":"key","key":"enter"}"#,
        ],
    );
}

fn marks(viewer: &HeadlessViewer, args: &[&str]) -> Vec<(PixelRect, String)> {
    let mut all = vec!["marks"];
    all.extend(args);
    let reply: Reply = viewer.seer_json(&all);
    let Reply::Marks { marks } = reply else {
        panic!("not marks: {reply:?}");
    };
    marks
        .into_iter()
        .map(|mark| (mark.rect, mark.note))
        .collect()
}

fn screenshot(viewer: &HeadlessViewer, dir: &TempDir) -> Pixels {
    let path = dir.join("shot.png");
    let reply: Reply = viewer.seer_json([Path::new("screenshot"), &path]);
    assert!(matches!(reply, Reply::Screenshot { size, .. } if size == PixelSize::new(128, 96)));
    read_image(&path).unwrap()
}

fn near(actual: Rgba, expected: Rgba) -> bool {
    [
        (actual.r, expected.r),
        (actual.g, expected.g),
        (actual.b, expected.b),
    ]
    .iter()
    .all(|(a, e)| a.abs_diff(*e) <= 8)
}

/// Starts `seer marks --wait` with `args`, and waits until the viewer shows
/// that it is waiting.
fn wait_for_marks(viewer: &HeadlessViewer, dir: &TempDir, args: &[&str]) -> Child {
    let child = Command::new(SEER)
        .args(["--json", "--instance", viewer.instance(), "marks", "--wait"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    while !near(screenshot(viewer, dir).get(2, 2), BANNER) {
        assert!(Instant::now() < deadline, "the banner never showed");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

/// Waits for `seer marks --wait` to answer, and returns its exit code and
/// what it printed.
fn answer(mut child: Child) -> (i32, String) {
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "marks --wait never answered");
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    (status.code().unwrap(), out)
}

#[test]
fn marks_are_reported_in_image_pixels_at_any_zoom_and_rotation() {
    let dir = TempDir::new().unwrap();
    let (viewer, path) = viewer(&dir);

    // Fitted at 50%, the image covers the client area.
    drag(&viewer, (16.0, 12.0), (48.0, 36.0));
    // The note field shows over the image, beside the box, in the dark
    // theme a headless viewer always has.
    let shot = screenshot(&viewer, &dir);
    assert!(
        near(shot.get(100, 14), Rgba::opaque(0x26, 0x20, 0x1C)),
        "{:?}",
        shot.get(100, 14)
    );
    input(
        &viewer,
        &[
            r#"{"type":"note_text","text":"fitted"}"#,
            r#"{"type":"key","key":"enter"}"#,
        ],
    );
    // The mark is drawn, and the field has gone.
    let shot = screenshot(&viewer, &dir);
    assert!(near(shot.get(30, 12), MARK), "{:?}", shot.get(30, 12));
    assert!(near(shot.get(100, 50), Rgba::GREEN));

    // At 100%, the client area's top-left is the image's (64, 48).
    input(&viewer, &[r#"{"type":"key","key":"1"}"#]);
    mark(&viewer, (10.0, 10.0), (30.0, 20.0), "actual size");
    // Turned clockwise, a drag right and down runs down and left in the
    // image, from the middle, which stays put.
    input(&viewer, &[r#"{"type":"key","key":"r"}"#]);
    mark(&viewer, (64.0, 48.0), (84.0, 58.0), "turned");
    // At 125% and turned half round, a drag runs up and left, and partly
    // covered pixels are included.
    input(
        &viewer,
        &[
            r#"{"type":"key","key":"plus"}"#,
            r#"{"type":"key","key":"r"}"#,
        ],
    );
    mark(&viewer, (75.0, 54.0), (95.0, 69.0), "zoomed");
    // Escape discards a mark being made.
    drag(&viewer, (10.0, 10.0), (50.0, 50.0));
    input(&viewer, &[r#"{"type":"key","key":"escape"}"#]);

    assert_eq!(
        marks(&viewer, &[]),
        [
            (PixelRect::new(32, 24, 64, 48), "fitted".to_string()),
            (PixelRect::new(74, 58, 20, 10), "actual size".to_string()),
            (PixelRect::new(128, 76, 10, 20), "turned".to_string()),
            (PixelRect::new(103, 79, 17, 13), "zoomed".to_string()),
        ]
    );
    let reply: Reply = viewer.seer_json(["marks"]);
    let Reply::Marks { marks: infos } = reply else {
        panic!("{reply:?}");
    };
    assert!(infos.iter().all(|info| info.path == path));

    // --clear removes the marks once reported.
    assert_eq!(marks(&viewer, &["--clear"]).len(), 4);
    assert!(marks(&viewer, &[]).is_empty());
    assert!(viewer.quit().success());
}

#[test]
fn a_mark_is_selected_and_edited_through_its_note() {
    let dir = TempDir::new().unwrap();
    let (viewer, _) = viewer(&dir);
    // The note is drawn in a strip from 22 to 2 pixels above the box, at
    // least 200 pixels wide, so (80, 28) is on the note but not the box.
    mark(&viewer, (16.0, 40.0), (48.0, 60.0), "box");
    assert!(near(screenshot(&viewer, &dir).get(30, 40), MARK));

    // Dragging from the note selects the mark without panning the image.
    input(
        &viewer,
        &[
            r#"{"type":"pointer_down","button":"left","at":{"x":80,"y":28}}"#,
            r#"{"type":"pointer_move","at":{"x":120,"y":90}}"#,
            r#"{"type":"pointer_up","button":"left","at":{"x":120,"y":90}}"#,
        ],
    );
    let shot = screenshot(&viewer, &dir);
    assert!(
        near(shot.get(30, 40), SELECTED_MARK),
        "{:?}",
        shot.get(30, 40)
    );

    // Double-clicking the note opens it for editing.
    input(
        &viewer,
        &[
            r#"{"type":"pointer_down","button":"left","at":{"x":80,"y":28}}"#,
            r#"{"type":"double_click","button":"left","at":{"x":80,"y":28}}"#,
            r#"{"type":"pointer_up","button":"left","at":{"x":80,"y":28}}"#,
            r#"{"type":"note_text","text":"edited"}"#,
            r#"{"type":"key","key":"enter"}"#,
        ],
    );
    assert_eq!(
        marks(&viewer, &[]),
        [(PixelRect::new(32, 80, 64, 40), "edited".to_string())]
    );
    assert!(viewer.quit().success());
}

#[test]
fn marks_wait_answers_on_ctrl_enter_or_the_done_button() {
    let dir = TempDir::new().unwrap();
    let (viewer, _) = viewer(&dir);

    let mut waiting = wait_for_marks(&viewer, &dir, &["--clear"]);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        waiting.try_wait().unwrap().is_none(),
        "answered before Ctrl+Enter"
    );
    mark(&viewer, (16.0, 60.0), (48.0, 80.0), "first");
    // Ctrl+Enter in the note field keeps the note and finishes marking.
    drag(&viewer, (60.0, 60.0), (100.0, 80.0));
    input(
        &viewer,
        &[
            r#"{"type":"note_text","text":"second"}"#,
            r#"{"type":"key","key":"enter","ctrl":true}"#,
        ],
    );
    let (code, out) = answer(waiting);
    assert_eq!(code, 0);
    let Reply::Marks { marks: reported } = serde_json::from_str(&out).unwrap() else {
        panic!("{out}");
    };
    let notes: Vec<&str> = reported.iter().map(|mark| mark.note.as_str()).collect();
    assert_eq!(notes, ["first", "second"]);
    // The waiter asked for the marks to be cleared, and the banner is gone.
    assert!(marks(&viewer, &[]).is_empty());
    assert!(near(screenshot(&viewer, &dir).get(2, 2), Rgba::GREEN));

    // The banner's Done button, at its right end, does the same.
    let waiting = wait_for_marks(&viewer, &dir, &[]);
    mark(&viewer, (16.0, 60.0), (48.0, 80.0), "third");
    input(
        &viewer,
        &[
            r#"{"type":"pointer_down","button":"left","at":{"x":80,"y":20}}"#,
            r#"{"type":"pointer_up","button":"left","at":{"x":80,"y":20}}"#,
        ],
    );
    let (code, out) = answer(waiting);
    assert_eq!(code, 0);
    let Reply::Marks { marks: reported } = serde_json::from_str(&out).unwrap() else {
        panic!("{out}");
    };
    assert_eq!(reported.len(), 1);
    assert_eq!(marks(&viewer, &[]).len(), 1);
    assert!(viewer.quit().success());
}

#[test]
fn marks_wait_times_out_with_exit_code_5() {
    let dir = TempDir::new().unwrap();
    let (viewer, _) = viewer(&dir);
    let started = Instant::now();
    let output = viewer.seer(["--json", "marks", "--wait", "--timeout", "0.5"]);
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(output.status.code(), Some(5));
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed["error"]["code"], "timeout");
    // The viewer no longer waits.
    assert!(near(screenshot(&viewer, &dir).get(2, 2), Rgba::GREEN));
    assert!(viewer.quit().success());
}

#[test]
fn a_viewer_started_after_one_closes_has_no_images_or_marks() {
    let dir = TempDir::new().unwrap();
    let (viewer, _) = viewer(&dir);
    mark(&viewer, (40.0, 30.0), (80.0, 60.0), "gone");
    assert_eq!(marks(&viewer, &[]).len(), 1);
    let instance = viewer.instance().to_owned();
    assert!(viewer.quit().success());

    let mut next = Command::new(SEERW)
        .args(["--headless", "--instance", &instance])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let seer = |args: &[&str]| {
        Command::new(SEER)
            .args(["--json", "--instance", &instance])
            .args(args)
            .output()
            .unwrap()
    };
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    let list = loop {
        let output = seer(&["list"]);
        if output.status.success() {
            break output;
        }
        assert!(
            Instant::now() < deadline,
            "the viewer never served its pipe"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let list: ListReply = serde_json::from_slice(&list.stdout).unwrap();
    assert!(list.images.is_empty(), "{list:?}");
    let reply: Reply = serde_json::from_slice(&seer(&["marks"]).stdout).unwrap();
    assert!(matches!(reply, Reply::Marks { marks } if marks.is_empty()));
    assert!(seer(&["quit"]).status.success());
    assert!(next.wait().unwrap().success());
}
