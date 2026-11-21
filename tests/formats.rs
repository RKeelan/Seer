//! Animated GIF, SVG and the formats whose codecs are Store extensions, end
//! to end where it matters: decoding, and what a headless viewer shows.

use std::path::Path;
use std::time::{Duration, Instant};

use seer::decode::wic::{self, Content, Decoded};
use seer::error::ErrorCode;
use seer::model::app::DecodeTarget;
use seer::model::geometry::PixelSize;
use seer::protocol::{LoadState, OpenReply, Reply};
use seer::testutil::{
    GifFrame, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, read_image,
    write_animated_gif, write_heif,
};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

/// The background behind the image, in the dark theme.
const BACKGROUND: Rgba = Rgba::opaque(32, 32, 32);

fn viewer(files: &[&Path]) -> HeadlessViewer {
    HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), files).expect("the viewer starts")
}

fn debug_input(viewer: &HeadlessViewer, event: &str) {
    let output = viewer.seer(["debug", "input", event]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn screenshot(viewer: &HeadlessViewer, dir: &TempDir, name: &str) -> Pixels {
    let path = dir.join(name);
    let _: Reply = viewer.seer_json([Path::new("screenshot"), &path]);
    read_image(&path).unwrap()
}

fn near(actual: Rgba, expected: Rgba, tolerance: u8) -> bool {
    [
        (actual.r, expected.r),
        (actual.g, expected.g),
        (actual.b, expected.b),
    ]
    .iter()
    .all(|(a, e)| a.abs_diff(*e) <= tolerance)
}

/// Takes screenshots until `done` holds of the colour at the centre, and
/// returns the colours seen in order, without repeats.
fn watch_centre(
    viewer: &HeadlessViewer,
    dir: &TempDir,
    done: impl Fn(&[Rgba]) -> bool,
) -> Vec<Rgba> {
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    let mut seen: Vec<Rgba> = Vec::new();
    for shot in 0.. {
        let centre = screenshot(viewer, dir, &format!("shot-{shot}.png")).get(64, 48);
        if seen.last() != Some(&centre) {
            seen.push(centre);
        }
        if done(&seen) {
            return seen;
        }
        assert!(Instant::now() < deadline, "the centre showed only {seen:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    unreachable!()
}

#[test]
fn an_animated_gif_plays_in_the_window() {
    let dir = TempDir::new().unwrap();
    let path = dir.join("blink.gif");
    let frames = [
        GifFrame::whole(Pixels::solid(32, 24, Rgba::RED), 15),
        GifFrame::whole(Pixels::solid(32, 24, Rgba::BLUE), 15),
    ];
    write_animated_gif(&path, 32, 24, &frames, Some(0)).unwrap();
    let viewer = viewer(&[&path]);
    debug_input(
        &viewer,
        r#"{"type":"resize","size":{"width":128,"height":96}}"#,
    );
    let list = viewer.wait_until_loaded(1);
    assert_eq!(list.images[0].size, Some(PixelSize::new(32, 24)));
    assert_eq!(list.images[0].state, LoadState::Loaded);
    // It loops forever, so whichever frame shows first comes round again.
    let seen = watch_centre(&viewer, &dir, |seen| seen.len() >= 3);
    assert!(
        seen[..3] == [Rgba::RED, Rgba::BLUE, Rgba::RED]
            || seen[..3] == [Rgba::BLUE, Rgba::RED, Rgba::BLUE],
        "{seen:?}"
    );
    assert!(viewer.quit().success());
}

#[test]
fn an_animation_that_plays_once_stops_on_its_last_frame() {
    let dir = TempDir::new().unwrap();
    let path = dir.join("once.gif");
    let frames = [
        GifFrame::whole(Pixels::solid(32, 24, Rgba::RED), 5),
        GifFrame::whole(Pixels::solid(32, 24, Rgba::BLUE), 5),
        GifFrame::whole(Pixels::solid(32, 24, Rgba::GREEN), 5),
    ];
    write_animated_gif(&path, 32, 24, &frames, None).unwrap();
    let viewer = viewer(&[&path]);
    debug_input(
        &viewer,
        r#"{"type":"resize","size":{"width":128,"height":96}}"#,
    );
    viewer.wait_until_loaded(1);
    watch_centre(&viewer, &dir, |seen| seen.last() == Some(&Rgba::GREEN));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        screenshot(&viewer, &dir, "after.png").get(64, 48),
        Rgba::GREEN
    );
    assert!(viewer.quit().success());
}

/// A 20 × 20 SVG, red to the left of x = 10.5 and blue to the right.
const SPLIT: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
    <rect width="10.5" height="20" fill="red"/>
    <rect x="10.5" width="9.5" height="20" fill="blue"/>
</svg>"#;

#[test]
fn an_svg_stays_sharp_when_zoomed_in() {
    let dir = TempDir::new().unwrap();
    let path = dir.join("split.svg");
    std::fs::write(&path, SPLIT).unwrap();
    let viewer = viewer(&[&path]);
    debug_input(
        &viewer,
        r#"{"type":"resize","size":{"width":128,"height":96}}"#,
    );
    let list = viewer.wait_until_loaded(1);
    assert_eq!(list.images[0].size, Some(PixelSize::new(20, 20)));
    assert_eq!(list.images[0].state, LoadState::Loaded);

    // At 100% the image sits in the middle, from x = 54 to 74.
    let shot = screenshot(&viewer, &dir, "fit.png");
    assert_eq!(shot.get(40, 48), BACKGROUND);
    assert!(
        near(shot.get(58, 48), Rgba::RED, 2),
        "{:?}",
        shot.get(58, 48)
    );
    assert!(
        near(shot.get(70, 48), Rgba::BLUE, 2),
        "{:?}",
        shot.get(70, 48)
    );

    // At 800% about the centre, the image spans x = -16 to 144, and the
    // edge at 10.5 falls at x = 68. A bitmap of 20 pixels would show a
    // blended pixel 8 wide there; the document is drawn sharp.
    debug_input(
        &viewer,
        r#"{"type":"pinch","at":{"x":64,"y":48},"scale":8}"#,
    );
    assert_eq!(viewer.list().view.unwrap().zoom, 8.0);
    let shot = screenshot(&viewer, &dir, "zoomed.png");
    // The zoom readout covers the middle of so small a window, so look
    // above it.
    assert!(
        near(shot.get(67, 10), Rgba::RED, 2),
        "{:?}",
        shot.get(67, 10)
    );
    assert!(
        near(shot.get(68, 10), Rgba::BLUE, 2),
        "{:?}",
        shot.get(68, 10)
    );
    assert!(viewer.quit().success());
}

/// Small images, 8 × 8, red on the left half and blue on the right, in the
/// formats WIC has no encoder for. The WebP and AVIF were written by Pillow
/// (lossless, and quality 100 at 4:4:4) and the JPEG XL by FFmpeg's libjxl
/// (lossless), from an image made for these tests.
const WEBP: [u8; 38] = [
    0x52, 0x49, 0x46, 0x46, 0x1e, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4c,
    0x12, 0x00, 0x00, 0x00, 0x2f, 0x07, 0xc0, 0x01, 0x00, 0x0f, 0x10, 0xf3, 0xbf, 0xff, 0xf3, 0x1f,
    0x0e, 0xfa, 0x10, 0xd1, 0xff, 0x00,
];

const JXL: [u8; 36] = [
    0xff, 0x0a, 0x41, 0x40, 0x24, 0x08, 0x04, 0x01, 0x00, 0x64, 0x00, 0x4b, 0x18, 0x8b, 0x15, 0xc2,
    0x49, 0x82, 0xe4, 0x05, 0x42, 0x00, 0xd8, 0xff, 0xfe, 0xb7, 0xd9, 0x1f, 0x02, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

const AVIF: [u8; 316] = [
    0x00, 0x00, 0x00, 0x20, 0x66, 0x74, 0x79, 0x70, 0x61, 0x76, 0x69, 0x66, 0x00, 0x00, 0x00, 0x00,
    0x61, 0x76, 0x69, 0x66, 0x6d, 0x69, 0x66, 0x31, 0x6d, 0x69, 0x61, 0x66, 0x4d, 0x41, 0x31, 0x41,
    0x00, 0x00, 0x00, 0xeb, 0x6d, 0x65, 0x74, 0x61, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21,
    0x68, 0x64, 0x6c, 0x72, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x70, 0x69, 0x63, 0x74,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x0e, 0x70, 0x69, 0x74, 0x6d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1e, 0x69,
    0x6c, 0x6f, 0x63, 0x00, 0x00, 0x00, 0x00, 0x44, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x01, 0x13, 0x00, 0x00, 0x00, 0x29, 0x00, 0x00, 0x00, 0x28, 0x69, 0x69, 0x6e,
    0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1a, 0x69, 0x6e, 0x66, 0x65, 0x02,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x61, 0x76, 0x30, 0x31, 0x43, 0x6f, 0x6c, 0x6f, 0x72,
    0x00, 0x00, 0x00, 0x00, 0x6a, 0x69, 0x70, 0x72, 0x70, 0x00, 0x00, 0x00, 0x4b, 0x69, 0x70, 0x63,
    0x6f, 0x00, 0x00, 0x00, 0x14, 0x69, 0x73, 0x70, 0x65, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x10, 0x70, 0x69, 0x78, 0x69, 0x00, 0x00, 0x00,
    0x00, 0x03, 0x08, 0x08, 0x08, 0x00, 0x00, 0x00, 0x0c, 0x61, 0x76, 0x31, 0x43, 0x81, 0x20, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x13, 0x63, 0x6f, 0x6c, 0x72, 0x6e, 0x63, 0x6c, 0x78, 0x00, 0x01, 0x00,
    0x0d, 0x00, 0x06, 0x80, 0x00, 0x00, 0x00, 0x17, 0x69, 0x70, 0x6d, 0x61, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x04, 0x01, 0x02, 0x83, 0x04, 0x00, 0x00, 0x00, 0x31, 0x6d,
    0x64, 0x61, 0x74, 0x12, 0x00, 0x0a, 0x05, 0x38, 0x08, 0xbf, 0x60, 0x90, 0x32, 0x1e, 0x10, 0x00,
    0x00, 0x8b, 0xa3, 0xd3, 0x4b, 0xe8, 0xfa, 0x87, 0x85, 0x15, 0xa1, 0x83, 0x93, 0x58, 0x85, 0x50,
    0xa3, 0x5f, 0xd0, 0xa7, 0xb6, 0xba, 0xc3, 0xe2, 0x55, 0x8e, 0x86, 0x80,
];

fn halves(decoded: &Decoded) -> (Rgba, Rgba) {
    let pixels = Pixels {
        width: decoded.bitmap_size.width,
        height: decoded.bitmap_size.height,
        data: decoded.pixels.clone(),
    };
    let (w, h) = (pixels.width, pixels.height);
    (pixels.get(w / 4, h / 2), pixels.get(w * 3 / 4, h / 2))
}

/// Decodes `path`, written in a format whose codec may be missing, and
/// checks it shows red and blue halves. Where the codec is missing, checks
/// the error names it instead, and says the test was skipped.
fn decode_or_name_the_codec(path: &Path, codec: &str, tolerance: u8) {
    if let Some(missing) = wic::missing_codec(path) {
        eprintln!("skipped decoding {}: {}", path.display(), missing.message);
        assert!(missing.message.contains(codec), "{}", missing.message);
        let error = wic::decode(path, DecodeTarget::Full).unwrap_err();
        assert_eq!(error.code, ErrorCode::CodecMissing, "{}", error.message);
        assert!(error.message.contains(codec), "{}", error.message);
        return;
    }
    let decoded = match wic::decode(path, DecodeTarget::Full) {
        Ok(decoded) => decoded,
        // A decoder that claims the extension may still lean on another
        // extension, as AVIF's does on the AV1 codec.
        Err(error) if error.code == ErrorCode::CodecMissing => {
            eprintln!("skipped decoding {}: {}", path.display(), error.message);
            return;
        }
        Err(error) => panic!("{}: {}", path.display(), error.message),
    };
    assert_eq!(decoded.size, PixelSize::new(8, 8));
    assert_eq!(decoded.content, Content::Still);
    let (left, right) = halves(&decoded);
    assert!(
        near(left, Rgba::RED, tolerance),
        "{}: {left:?}",
        path.display()
    );
    assert!(
        near(right, Rgba::BLUE, tolerance),
        "{}: {right:?}",
        path.display()
    );
}

#[test]
fn extension_codecs_decode_where_installed() {
    let dir = TempDir::new().unwrap();
    for (name, bytes, codec, tolerance) in [
        ("halves.webp", &WEBP[..], "WebP", 0),
        ("halves.jxl", &JXL[..], "JPEG XL", 0),
        ("halves.avif", &AVIF[..], "AV1", 40),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        decode_or_name_the_codec(&path, codec, tolerance);
    }
    let heic = dir.join("halves.heic");
    let image = Pixels::from_fn(8, 8, |x, _| if x < 4 { Rgba::RED } else { Rgba::BLUE });
    match write_heif(&heic, &image) {
        Ok(()) => decode_or_name_the_codec(&heic, "HEIF", 24),
        Err(error) => eprintln!("skipped HEIF: no encoder ({})", error.message),
    }
}

#[test]
fn open_names_a_missing_codec() {
    let dir = TempDir::new().unwrap();
    let candidates = [
        ("a.webp", &WEBP[..]),
        ("a.jxl", &JXL[..]),
        ("a.avif", &AVIF[..]),
        ("a.heic", &b"not really"[..]),
    ];
    let Some((name, bytes)) = candidates
        .into_iter()
        .find(|(name, _)| wic::missing_codec(Path::new(name)).is_some())
    else {
        eprintln!("skipped: every extension codec is installed");
        return;
    };
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    let viewer = viewer(&[]);
    let output = viewer.seer([Path::new("--json"), Path::new("open"), &path]);
    assert_eq!(output.status.code(), Some(4));
    let opened: OpenReply = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(opened.images.len(), 1);
    assert_eq!(opened.failed[0].error.code, ErrorCode::CodecMissing);
    assert!(
        opened.failed[0].error.message.contains("Microsoft Store"),
        "{}",
        opened.failed[0].error.message
    );
    let list = viewer.wait_until_loaded(1);
    assert_eq!(list.images[0].state, LoadState::Failed);
    assert_eq!(
        list.images[0].error.as_ref().unwrap().code,
        ErrorCode::CodecMissing
    );
    assert!(viewer.quit().success());
}
