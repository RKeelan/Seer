//! Makes the screenshots in the README from generated images, on a headless
//! viewer, with `seer screenshot`.
//!
//! ```text
//! cargo build
//! cargo run --example screenshots -- [<output directory>]
//! ```
//!
//! The images are drawn here, so the screenshots hold nothing but what this
//! file makes. They are written to `docs` unless another directory is given.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use seer::protocol::{ListReply, LoadState, OpenReply, Reply};
use seer::testutil::{Format, HeadlessViewer, Pixels, Rgba, TempDir, write_image};

const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("screenshots: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "docs".into()));
    std::fs::create_dir_all(&out).map_err(|error| error.to_string())?;
    let (seerw, seer) = programs()?;

    let dir = TempDir::new().map_err(|error| error.to_string())?;
    let images: [(&str, &str, Pixels); 4] = [
        ("dusk.png", "dusk", dusk()),
        ("rings.png", "rings", rings()),
        ("tiles.png", "tiles", tiles()),
        ("bands.png", "bands", bands()),
    ];
    let mut args: Vec<String> = vec!["open".into(), "--group".into(), "variants".into()];
    for (_, caption, _) in &images {
        args.extend(["--caption".into(), (*caption).into()]);
    }
    for (name, _, pixels) in &images {
        let path = dir.join(name);
        write_image(&path, pixels, Format::Png).map_err(|error| error.to_string())?;
        args.push(path.display().to_string());
    }

    let viewer =
        HeadlessViewer::start(&seerw, &seer, &[] as &[&Path]).map_err(|error| error.to_string())?;
    input(
        &viewer,
        &[
            r#"{"type":"resize","size":{"width":720,"height":480}}"#,
            r#"{"type":"dpi","dpi":96}"#,
        ],
    );
    let opened: OpenReply = viewer.seer_json(&args);
    let first = opened.images[0].id;
    input(&viewer, &[r#"{"type":"key","key":"home"}"#]);
    viewer.wait_for(|list| loaded(list, |image| image == first));

    // A mark with its note, over the single view and the strip.
    input(
        &viewer,
        &[
            r#"{"type":"pointer_down","button":"right","at":{"x":330,"y":250}}"#,
            r#"{"type":"pointer_move","at":{"x":480,"y":310}}"#,
            r#"{"type":"pointer_up","button":"right","at":{"x":480,"y":310}}"#,
            r#"{"type":"note_text","text":"raise the horizon here"}"#,
            r#"{"type":"key","key":"enter"}"#,
        ],
    );
    screenshot(&viewer, &out.join("viewer.png"))?;

    // The group side by side.
    input(&viewer, &[r#"{"type":"key","key":"c"}"#]);
    viewer.wait_for(|list| loaded(list, |_| true));
    screenshot(&viewer, &out.join("compare.png"))?;

    if viewer.quit().success() {
        Ok(())
    } else {
        Err("the viewer did not exit cleanly".into())
    }
}

/// `seerw.exe` and `seer.exe`, beside the directory this example was built
/// into.
fn programs() -> Result<(PathBuf, PathBuf), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let dir = exe
        .parent()
        .and_then(|examples| examples.parent())
        .ok_or("cannot find the build directory")?;
    let found = |name: &str| {
        let path = dir.join(name);
        if path.exists() {
            Ok(path)
        } else {
            Err(format!(
                "{} does not exist; run `cargo build` first",
                path.display()
            ))
        }
    };
    Ok((found("seerw.exe")?, found("seer.exe")?))
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

fn loaded(list: &ListReply, wanted: impl Fn(seer::model::ImageId) -> bool) -> bool {
    list.images
        .iter()
        .filter(|image| wanted(image.id))
        .all(|image| image.state == LoadState::Loaded)
}

/// Takes screenshots until two in a row match, so thumbnails decoded in the
/// background are in it, and writes the last.
fn screenshot(viewer: &HeadlessViewer, path: &Path) -> Result<(), String> {
    let mut last = Vec::new();
    for _ in 0..50 {
        let reply: Reply = viewer.seer_json([Path::new("screenshot"), path]);
        if !matches!(reply, Reply::Screenshot { .. }) {
            return Err(format!("unexpected reply {reply:?}"));
        }
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        if bytes == last {
            println!("wrote {}", path.display());
            return Ok(());
        }
        last = bytes;
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{} never settled", path.display()))
}

/// A sky fading from deep blue to orange over a dark horizon.
fn dusk() -> Pixels {
    Pixels::from_fn(WIDTH, HEIGHT, |x, y| {
        let horizon = HEIGHT * 3 / 4 + (x * 7 % 23) / 8;
        if y > horizon {
            return Rgba::opaque(24, 20, 30);
        }
        let t = y * 255 / horizon;
        Rgba::opaque(
            (40 + t * 200 / 255) as u8,
            (30 + t * 110 / 255) as u8,
            (110 - t * 80 / 255) as u8,
        )
    })
}

/// Concentric rings in alternating colours.
fn rings() -> Pixels {
    Pixels::from_fn(WIDTH, HEIGHT, |x, y| {
        let dx = f64::from(x) - f64::from(WIDTH) / 2.0;
        let dy = f64::from(y) - f64::from(HEIGHT) / 2.0;
        let ring = ((dx * dx + dy * dy).sqrt() / 24.0) as u32;
        [
            Rgba::opaque(230, 190, 60),
            Rgba::opaque(40, 110, 150),
            Rgba::opaque(240, 240, 230),
        ][(ring % 3) as usize]
    })
}

/// A grid of tiles shading from green to blue.
fn tiles() -> Pixels {
    Pixels::from_fn(WIDTH, HEIGHT, |x, y| {
        let (column, row) = (x / 40, y / 40);
        if x % 40 < 3 || y % 40 < 3 {
            return Rgba::opaque(20, 20, 20);
        }
        Rgba::opaque(
            (30 + row * 20) as u8,
            (200 - column * 10) as u8,
            (90 + column * 12) as u8,
        )
    })
}

/// Diagonal bands in warm colours.
fn bands() -> Pixels {
    Pixels::from_fn(WIDTH, HEIGHT, |x, y| {
        [
            Rgba::opaque(200, 60, 50),
            Rgba::opaque(240, 150, 60),
            Rgba::opaque(250, 220, 120),
            Rgba::opaque(120, 40, 60),
        ][((x + y) / 48 % 4) as usize]
    })
}
