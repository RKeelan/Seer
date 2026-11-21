//! Decoding ahead of need, end to end: the images next to the shown one are
//! decoded before the user moves to them, so moving presents at once.

use std::path::{Path, PathBuf};

use seer::protocol::{ListReply, LoadState, Reply, Timings};
use seer::testutil::{Format, HeadlessViewer, Pixels, Rgba, TempDir, read_image, write_image};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

fn solid(dir: &TempDir, name: &str, colour: Rgba) -> PathBuf {
    let path = dir.join(name);
    write_image(&path, &Pixels::solid(64, 48, colour), Format::Png).unwrap();
    path
}

fn input(viewer: &HeadlessViewer, event: &str) {
    let output = viewer.seer(["debug", "input", event]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The milliseconds of each phase named, or `None` for one not reached.
fn phases<const N: usize>(viewer: &HeadlessViewer, names: [&str; N]) -> [Option<f64>; N] {
    let reply: Reply = viewer.seer_json(["debug", "timings"]);
    let Reply::Timings(Timings { phases }) = reply else {
        panic!("no timings: {reply:?}");
    };
    names.map(|name| {
        phases
            .iter()
            .find(|phase| phase.name == name)
            .map(|phase| phase.ms)
    })
}

fn states(list: &ListReply) -> Vec<LoadState> {
    list.images.iter().map(|image| image.state).collect()
}

#[test]
fn moving_to_a_neighbour_decoded_ahead_presents_without_a_decode() {
    let dir = TempDir::new().unwrap();
    let colours = [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE];
    let names = ["a.png", "b.png", "c.png", "d.png"];
    let paths: Vec<PathBuf> = names
        .iter()
        .zip(colours)
        .map(|(name, colour)| solid(&dir, name, colour))
        .collect();
    let files: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let viewer = HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), &files).unwrap();
    // At 100% scale the thumbnail strip takes the bottom 84 rows, leaving
    // 128 × 96 above it for the image.
    input(&viewer, r#"{"type":"dpi","dpi":96}"#);
    input(
        &viewer,
        r#"{"type":"resize","size":{"width":128,"height":180}}"#,
    );

    // The first image and the one after it are decoded; the others wait.
    let list = viewer.wait_for(|list| {
        list.images.len() == 4
            && list.images[..2]
                .iter()
                .all(|image| image.state == LoadState::Loaded)
    });
    assert_eq!(states(&list)[2..], [LoadState::Loading; 2]);

    let timed = ["navigated", "navigation_decoded", "navigation_presented"];
    input(&viewer, r#"{"type":"key","key":"right"}"#);
    let [navigated, decoded, presented] = phases(&viewer, timed);
    let (navigated, presented) = (navigated.unwrap(), presented.unwrap());
    assert!(decoded.is_none(), "the move waited for a decode");
    assert!(presented >= navigated);
    let shot = dir.join("shot.png");
    let _: Reply = viewer.seer_json([Path::new("screenshot"), &shot]);
    assert_eq!(read_image(&shot).unwrap().get(64, 48), Rgba::GREEN);

    // The last image was not next to the second, so moving there waits for
    // its decode.
    input(&viewer, r#"{"type":"key","key":"end"}"#);
    viewer.wait_for(|list| list.images[3].state == LoadState::Loaded);
    let [navigated, decoded, presented] = phases(&viewer, timed);
    let (navigated, decoded) = (navigated.unwrap(), decoded.unwrap());
    assert!(decoded >= navigated);
    assert!(presented.unwrap() >= decoded);

    assert!(viewer.quit().success());
}
