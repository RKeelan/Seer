//! The viewer starts on a file, presents its first frame and reports how long
//! that took, without ever showing a window.

use std::path::Path;
use std::process::{Command, Output};

use seer::protocol::Timings;
use seer::testutil::{Format, Pixels, Rgba, TempDir, unique_instance, write_image};

fn start_headless(files: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_seerw"))
        .args(["--headless", "--exit-after-first-frame", "--instance"])
        .arg(unique_instance())
        .args(files)
        .output()
        .expect("run seerw")
}

fn phases(output: &Output) -> Vec<(String, f64)> {
    let timings: Timings =
        serde_json::from_slice(&output.stdout).expect("the timings are printed as JSON");
    timings
        .phases
        .into_iter()
        .map(|phase| (phase.name, phase.ms))
        .collect()
}

fn ms(phases: &[(String, f64)], name: &str) -> f64 {
    phases
        .iter()
        .find(|(phase, _)| phase == name)
        .unwrap_or_else(|| panic!("no {name} phase in {phases:?}"))
        .1
}

#[test]
fn the_first_frame_is_presented_and_timed() {
    let dir = TempDir::new().unwrap();
    let path = dir.join("photo.jpg");
    let image = Pixels::quadrants(1200, 900, [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE]);
    write_image(&path, &image, Format::Jpeg).unwrap();
    let output = start_headless(&[&path]);
    assert!(
        output.status.success(),
        "seerw exited with {}",
        output.status
    );
    let phases = phases(&output);
    let main = ms(&phases, "main");
    assert!(main > 0.0);
    assert!(ms(&phases, "preview_decoded") > main);
    assert!(ms(&phases, "first_present") > ms(&phases, "preview_decoded"));
    assert!(ms(&phases, "first_present") > ms(&phases, "warp_device_created"));
    assert!(phases.iter().all(|(name, _)| name != "shown"));
    assert!(phases.windows(2).all(|pair| pair[0].1 <= pair[1].1));
}

#[test]
fn a_file_that_cannot_be_shown_sets_the_exit_code() {
    let dir = TempDir::new().unwrap();
    let output = start_headless(&[&dir.join("absent.png")]);
    assert_eq!(output.status.code(), Some(4));
    ms(&phases(&output), "first_present");
}

#[test]
fn with_no_file_the_window_is_empty() {
    let output = start_headless(&[]);
    assert!(output.status.success());
    let phases = phases(&output);
    ms(&phases, "first_present");
    assert!(phases.iter().all(|(name, _)| name != "preview_decoded"));
}

#[test]
fn a_bad_instance_name_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_seerw"))
        .args(["--instance", "not/valid"])
        .output()
        .expect("run seerw");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("instance"));
}
