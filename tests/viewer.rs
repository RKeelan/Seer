//! The viewer end to end, driven only through `seer.exe` as an agent drives
//! it, on headless viewers that never touch the desktop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use seer::model::ImageId;
use seer::model::geometry::{PixelSize, Rotation};
use seer::protocol::{ListReply, LoadState, OpenReply, Reply, Timings};
use seer::testutil::{
    Format, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, assert_similar, read_image,
    unique_instance, write_image,
};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

/// The background behind the image, in the dark theme.
const BACKGROUND: Rgba = Rgba::opaque(32, 32, 32);

fn viewer(files: &[&Path]) -> HeadlessViewer {
    HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), files).expect("the viewer starts")
}

/// Writes a 64 × 48 PNG in four colours, so its placement and orientation
/// show in a screenshot.
fn quadrants(dir: &TempDir, name: &str, colours: [Rgba; 4]) -> (PathBuf, Pixels) {
    let path = dir.join(name);
    let pixels = Pixels::quadrants(64, 48, colours);
    write_image(&path, &pixels, Format::Png).unwrap();
    (path, pixels)
}

/// What a 128 × 96 area shows of a 64 × 48 image fitted to it: the image at
/// 100% in the middle, since fitting never enlarges.
fn centred(image: &Pixels) -> Pixels {
    Pixels::from_fn(128, 96, |x, y| {
        if (32..96).contains(&x) && (24..72).contains(&y) {
            image.get(x - 32, y - 24)
        } else {
            BACKGROUND
        }
    })
}

/// The client area's top-left 128 × 96 pixels: all of a client area resized
/// to 128 × 96, or the area above the thumbnail strip once
/// [`size_above_strip`] has sized the window.
fn screenshot(viewer: &HeadlessViewer, dir: &TempDir, name: &str) -> Pixels {
    let path = dir.join(name);
    let reply: Reply = viewer.seer_json([Path::new("screenshot"), &path]);
    let Reply::Screenshot { path: written, .. } = reply else {
        panic!("{reply:?}");
    };
    assert_eq!(written, path);
    let shot = read_image(&path).unwrap();
    Pixels::from_fn(128, 96, |x, y| shot.get(x, y))
}

/// Sizes the client area so that, at 100% scale, the area above the strip of
/// 84 pixels is 128 × 96.
fn size_above_strip(viewer: &HeadlessViewer) {
    debug_input(viewer, r#"{"type":"dpi","dpi":96}"#);
    debug_input(
        viewer,
        r#"{"type":"resize","size":{"width":128,"height":180}}"#,
    );
}

fn debug_input(viewer: &HeadlessViewer, event: &str) {
    let output = viewer.seer(["debug", "input", event]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn ids(list: &ListReply) -> Vec<ImageId> {
    list.images.iter().map(|image| image.id).collect()
}

#[test]
fn an_agent_opens_navigates_and_reads_back_screenshots() {
    let dir = TempDir::new().unwrap();
    let (first, first_pixels) = quadrants(
        &dir,
        "first.png",
        [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE],
    );
    let (second, second_pixels) = quadrants(
        &dir,
        "second.png",
        [Rgba::WHITE, Rgba::BLUE, Rgba::GREEN, Rgba::RED],
    );
    let viewer = viewer(&[]);
    assert!(viewer.list().images.is_empty());

    let opened: OpenReply = viewer.seer_json([Path::new("open"), &first, &second]);
    assert!(opened.failed.is_empty());
    assert_eq!(opened.images.len(), 2);
    let list = viewer.wait_until_loaded(2);
    assert_eq!(list.selected, Some(opened.images[0].id));
    assert_eq!(list.images[0].path, first);
    assert_eq!(list.images[0].size, Some(PixelSize::new(64, 48)));
    assert_eq!(list.images[0].state, LoadState::Loaded);

    size_above_strip(&viewer);
    assert_similar(
        &centred(&first_pixels),
        &screenshot(&viewer, &dir, "first-shot.png"),
        2,
    );

    debug_input(&viewer, r#"{"type":"key","key":"right"}"#);
    assert_eq!(viewer.list().selected, Some(opened.images[1].id));
    assert_similar(
        &centred(&second_pixels),
        &screenshot(&viewer, &dir, "second-shot.png"),
        2,
    );

    debug_input(&viewer, r#"{"type":"key","key":"home"}"#);
    let list = viewer.list();
    assert_eq!(list.selected, Some(opened.images[0].id));
    let view = list.view.unwrap();
    assert!(view.fit);
    assert_eq!(view.zoom, 1.0);

    assert!(viewer.quit().success());
}

#[test]
fn an_image_rewritten_on_disk_is_reloaded_and_a_deleted_one_kept() {
    let dir = TempDir::new().unwrap();
    let (path, _) = quadrants(
        &dir,
        "plot.png",
        [Rgba::RED, Rgba::GREEN, Rgba::BLUE, Rgba::WHITE],
    );
    let viewer = viewer(&[&path]);
    viewer.wait_until_loaded(1);
    debug_input(
        &viewer,
        r#"{"type":"resize","size":{"width":128,"height":96}}"#,
    );
    let image = |list: &ListReply| list.images[0].clone();

    // The program that made the image writes it again, in new colours.
    let redrawn = Pixels::quadrants(64, 48, [Rgba::WHITE, Rgba::BLUE, Rgba::GREEN, Rgba::RED]);
    write_image(&path, &redrawn, Format::Png).unwrap();
    viewer.wait_for(|_| screenshot(&viewer, &dir, "poll.png").get(40, 30) == Rgba::WHITE);
    assert_eq!(image(&viewer.list()).state, LoadState::Loaded);
    assert_similar(
        &centred(&redrawn),
        &screenshot(&viewer, &dir, "redrawn.png"),
        2,
    );

    // A rotation survives a reload, even one that changes the image's size.
    debug_input(&viewer, r#"{"type":"key","key":"r"}"#);
    let smaller = Pixels::solid(32, 24, Rgba::GREEN);
    write_image(&path, &smaller, Format::Png).unwrap();
    let list = viewer.wait_for(|list| image(list).size == Some(PixelSize::new(32, 24)));
    assert_eq!(list.view.unwrap().rotation, Rotation::Clockwise90);

    // A deleted file stays open, showing its last image.
    std::fs::remove_file(&path).unwrap();
    viewer.wait_for(|list| image(list).missing);
    assert_eq!(
        screenshot(&viewer, &dir, "deleted.png").get(64, 48),
        Rgba::GREEN
    );

    // Written again, it is no longer missing.
    write_image(&path, &redrawn, Format::Png).unwrap();
    viewer
        .wait_for(|list| !image(list).missing && image(list).size == Some(PixelSize::new(64, 48)));

    assert!(viewer.quit().success());
}

#[test]
fn opening_an_image_already_open_selects_it() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    let (first, _) = quadrants(&dir, "first.png", [Rgba::RED; 4]);
    let (second, _) = quadrants(&dir, "second.png", [Rgba::BLUE; 4]);
    let viewer = viewer(&[&first]);
    let opened: OpenReply = viewer.seer_json([Path::new("open"), &second]);
    assert_eq!(viewer.list().selected, Some(opened.images[0].id));

    // The same file by a different relative path and letter case.
    let output = Command::new(SEER)
        .current_dir(dir.join("sub"))
        .args(["--json", "--instance", viewer.instance(), "open"])
        .arg(r"..\sub\..\FIRST.PNG")
        .output()
        .unwrap();
    assert!(output.status.success());
    let again: OpenReply = serde_json::from_slice(&output.stdout).unwrap();
    assert!(again.images[0].existing);
    let list = viewer.wait_until_loaded(2);
    assert_eq!(list.selected, Some(again.images[0].id));
    assert_eq!(list.images[0].id, again.images[0].id);
    assert_eq!(list.images[0].path, first);
}

#[test]
fn a_folder_opens_its_images_in_explorers_order() {
    let dir = TempDir::new().unwrap();
    let shots = dir.join("shots");
    std::fs::create_dir(&shots).unwrap();
    std::fs::create_dir(shots.join("nested")).unwrap();
    std::fs::write(shots.join("notes.txt"), "not an image").unwrap();
    let (nested, _) = quadrants(&dir, r"shots\nested\inner.png", [Rgba::RED; 4]);
    let (ten, _) = quadrants(&dir, r"shots\shot10.png", [Rgba::RED; 4]);
    let (two, _) = quadrants(&dir, r"shots\shot2.png", [Rgba::GREEN; 4]);
    let (one, _) = quadrants(&dir, r"shots\Shot1.png", [Rgba::BLUE; 4]);

    // seerw expands a folder on its command line.
    let viewer = viewer(&[&shots]);
    let list = viewer.wait_for(|list| list.images.len() == 3);
    let paths: Vec<_> = list.images.iter().map(|image| &image.path).collect();
    assert_eq!(paths, [&one, &two, &ten]);
    assert!(!paths.contains(&&nested));
    assert_eq!(list.selected, Some(list.images[0].id));

    // So does seer open, into a group.
    let opened: OpenReply = viewer.seer_json([
        Path::new("open"),
        Path::new("--group"),
        Path::new("run"),
        &shots.join("nested"),
    ]);
    assert_eq!(opened.images.len(), 1);
    assert_eq!(opened.images[0].path, nested);
    let list = viewer.list();
    assert_eq!(list.images[3].group.as_deref(), Some("run"));
    assert_eq!(list.selected, Some(opened.images[0].id));

    // A folder with no images fails to open.
    let empty = dir.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let output = viewer.seer([Path::new("--json"), Path::new("open"), &empty]);
    assert_eq!(output.status.code(), Some(4));
    let reply: OpenReply = serde_json::from_slice(&output.stdout).unwrap();
    assert!(reply.images.is_empty());
    assert_eq!(reply.failed[0].path, empty);
    assert_eq!(reply.failed[0].error.code, seer::error::ErrorCode::NoImages);
    assert_eq!(viewer.list().images.len(), 4);
}

#[test]
fn images_are_closed_and_groups_cleared() {
    let dir = TempDir::new().unwrap();
    let paths: Vec<PathBuf> = ["a.png", "b.png", "c.png", "d.png"]
        .iter()
        .map(|name| quadrants(&dir, name, [Rgba::GREEN; 4]).0)
        .collect();
    let viewer = viewer(&[]);
    let _: OpenReply = viewer.seer_json([Path::new("open"), &paths[0], &paths[1]]);
    let grouped: OpenReply = viewer.seer_json([
        Path::new("open"),
        Path::new("--group"),
        Path::new("pair"),
        &paths[2],
        &paths[3],
    ]);
    // Each open answers once its images are in the set. Only the selection
    // and its neighbours are sure to be decoded, and nothing here needs them.
    let list = viewer.list();
    assert_eq!(list.images.len(), 4);
    let all = ids(&list);

    let closed: Reply = viewer.seer_json([Path::new("close"), &paths[0]]);
    assert_eq!(
        closed,
        Reply::Closed {
            closed: vec![all[0]],
            not_found: vec![]
        }
    );
    let closed: Reply = viewer.seer_json(["close", &all[1].0.to_string()]);
    assert!(matches!(closed, Reply::Closed { closed, .. } if closed == [all[1]]));
    assert_eq!(
        ids(&viewer.list()),
        grouped
            .images
            .iter()
            .map(|image| image.id)
            .collect::<Vec<_>>()
    );

    let cleared: Reply = viewer.seer_json(["clear", "pair"]);
    assert!(matches!(cleared, Reply::Closed { closed, .. } if closed.len() == 2));
    let list = viewer.list();
    assert!(list.images.is_empty());
    assert_eq!(list.selected, None);

    let _: OpenReply = viewer.seer_json([Path::new("open"), &paths[0]]);
    let closed: Reply = viewer.seer_json(["close", "--all"]);
    assert!(matches!(closed, Reply::Closed { closed, .. } if closed.len() == 1));
    assert!(viewer.list().images.is_empty());
}

#[test]
fn the_viewer_reports_its_start_up_timings() {
    let dir = TempDir::new().unwrap();
    let (path, _) = quadrants(&dir, "photo.png", [Rgba::BLUE; 4]);
    let viewer = viewer(&[&path]);
    viewer.wait_until_loaded(1);
    let reply: Reply = viewer.seer_json(["debug", "timings"]);
    let Reply::Timings(Timings { phases }) = reply else {
        panic!("no timings: {reply:?}");
    };
    let names: Vec<&str> = phases.iter().map(|phase| phase.name.as_str()).collect();
    for name in ["main", "window_created", "preview_decoded", "first_present"] {
        assert!(names.contains(&name), "no {name} in {names:?}");
    }
}

#[test]
fn twenty_viewers_started_at_once_make_one_window_with_twenty_images() {
    let dir = TempDir::new().unwrap();
    let paths: Vec<PathBuf> = (0..20u8)
        .map(|n| {
            let path = dir.join(format!("{n}.png"));
            let colour = Rgba::opaque(n * 12, 255 - n * 12, 128);
            write_image(
                &path,
                &Pixels::solid(16 + u32::from(n), 16, colour),
                Format::Png,
            )
            .unwrap();
            path
        })
        .collect();
    let instance = unique_instance();
    let mut processes: Vec<Child> = paths
        .iter()
        .map(|path| {
            Command::new(SEERW)
                .args(["--headless", "--instance", &instance])
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();

    // Every viewer but the one that took the instance hands its file over and
    // exits.
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    let mut owner = None;
    while owner.is_none() {
        let mut running = Vec::new();
        for (index, process) in processes.iter_mut().enumerate() {
            match process.try_wait().unwrap() {
                Some(status) => assert!(status.success(), "a viewer exited with {status}"),
                None => running.push(index),
            }
        }
        if running.len() == 1 {
            owner = Some(running[0]);
        } else {
            assert!(
                Instant::now() < deadline,
                "{} viewers still run",
                running.len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let mut owner = processes.swap_remove(owner.unwrap());

    let seer = |args: &[&str]| {
        Command::new(SEER)
            .args(["--json", "--instance", &instance])
            .args(args)
            .output()
            .unwrap()
    };
    // Each hand-over selects its image, so only the last selection and its
    // neighbours are sure to be decoded: an image whose queued decode is
    // overtaken by two more arrivals waits until the selection nears it.
    let deadline = Instant::now() + VIEWER_TIMEOUT;
    let wait_for = |done: &dyn Fn(&ListReply) -> bool| loop {
        let list: ListReply = serde_json::from_slice(&seer(&["list"]).stdout).unwrap();
        if done(&list) {
            break list;
        }
        assert!(Instant::now() < deadline, "the set is {list:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    let list = wait_for(&|list| {
        let Some(selected) = list
            .selected
            .and_then(|id| list.images.iter().position(|image| image.id == id))
        else {
            return false;
        };
        let near = selected.saturating_sub(1)..(selected + 2).min(list.images.len());
        list.images.len() == 20
            && list.images[near]
                .iter()
                .all(|image| image.state == LoadState::Loaded)
    });
    assert!(
        list.images
            .iter()
            .all(|image| image.state != LoadState::Failed),
        "the set is {list:?}"
    );
    let opened: BTreeSet<PathBuf> = list.images.into_iter().map(|image| image.path).collect();
    assert_eq!(opened, paths.into_iter().collect());

    // Any image left behind is decoded once the selection reaches it.
    assert!(
        seer(&["debug", "input", r#"{"type":"key","key":"home"}"#])
            .status
            .success()
    );
    wait_for(&|list| {
        list.images[..2]
            .iter()
            .all(|image| image.state == LoadState::Loaded)
    });

    assert!(seer(&["quit"]).status.success());
    assert!(owner.wait().unwrap().success());
}
