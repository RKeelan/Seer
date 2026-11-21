//! The viewer's log, end to end: what a viewer records, and that file names
//! stay out of it unless `SEER_LOG_PATHS` is `1`.

use std::path::{Path, PathBuf};

use seer::testutil::{Format, HeadlessViewer, Pixels, Rgba, TempDir, write_image};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

/// A PNG and a file named like one that is not an image.
fn files(dir: &TempDir) -> [PathBuf; 2] {
    let good = dir.join("good-image.png");
    write_image(&good, &Pixels::solid(32, 24, Rgba::GREEN), Format::Png).unwrap();
    let bad = dir.join("bad-image.png");
    std::fs::write(&bad, "not an image").unwrap();
    [good, bad]
}

fn start(files: &[PathBuf], env: &[(&str, &str)]) -> HeadlessViewer {
    HeadlessViewer::start_with_env(Path::new(SEERW), Path::new(SEER), files, env)
        .expect("the viewer starts")
}

#[test]
fn the_log_records_start_up_and_failures_without_file_names() {
    let dir = TempDir::new().unwrap();
    let files = files(&dir);
    let viewer = start(&files, &[]);
    viewer.wait_until_loaded(2);

    let log = viewer.log();
    let started = format!(
        "seer {} started on instance {}, headless, with 2 files",
        env!("CARGO_PKG_VERSION"),
        viewer.instance()
    );
    assert!(log.contains(&started), "{log}");
    assert!(log.contains("first frame presented"), "{log}");
    assert!(
        log.contains("image 2 could not be shown: unsupported_format\r\n"),
        "{log}"
    );
    assert!(!log.contains("image.png"), "{log}");
    assert!(viewer.quit().success());
}

#[test]
fn file_names_are_logged_when_asked_for() {
    let dir = TempDir::new().unwrap();
    let files = files(&dir);
    let viewer = start(&files, &[("SEER_LOG_PATHS", "1")]);
    viewer.wait_until_loaded(2);

    let log = viewer.log();
    assert!(log.contains("unsupported_format: "), "{log}");
    assert!(log.contains("bad-image.png"), "{log}");
    assert!(viewer.quit().success());
}
