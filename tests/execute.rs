//! Explorer's side of the open verb's `DelegateExecute`, played by the test:
//! it creates the command object in a `seerw -Embedding` through COM, hands
//! it a selection, and executes it, as Explorer does. Each viewer serves a
//! class id of its own, so Explorer never reaches it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use seer::protocol::ListReply;
use seer::testutil::{
    Format, HeadlessViewer, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, init_com, shell_items,
    unique_instance, write_image,
};
use seer::viewer::execute::registry_form;
use windows::Win32::Foundation::REGDB_E_CLASSNOTREG;
use windows::Win32::System::Com::{CLSCTX_LOCAL_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{IExecuteCommand, IObjectWithSelection};
use windows::core::{GUID, Interface};

const SEERW: &str = env!("CARGO_BIN_EXE_seerw");
const SEER: &str = env!("CARGO_BIN_EXE_seer");

fn images(dir: &TempDir, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            write_image(
                &path,
                &Pixels::solid(8, 8, Rgba::opaque(9, 99, 199)),
                Format::Png,
            )
            .unwrap();
            path
        })
        .collect()
}

/// Plays Explorer on another thread: waits for `clsid`'s server to register,
/// then gives it `files` and executes the command.
fn explorer(clsid: GUID, files: Vec<PathBuf>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        init_com();
        let deadline = Instant::now() + VIEWER_TIMEOUT;
        let command: IExecuteCommand = loop {
            // SAFETY: the class id outlives the call, and no outer object is
            // given.
            match unsafe { CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) } {
                Ok(command) => break command,
                Err(error) if error.code() == REGDB_E_CLASSNOTREG => {
                    assert!(Instant::now() < deadline, "the server never registered");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("creating the command failed: {error}"),
            }
        };
        let with_selection: IObjectWithSelection = command.cast().unwrap();
        // SAFETY: the array is a live object, and Execute has no arguments.
        unsafe {
            with_selection
                .SetSelection(&shell_items(&files).unwrap())
                .unwrap();
            command.Execute().unwrap();
        }
    })
}

fn paths(list: &ListReply) -> Vec<PathBuf> {
    canonical(list.images.iter().map(|image| &image.path))
}

/// The shell gives each path in its long form, where a temporary directory
/// may be named in its short (8.3) form.
fn canonical<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|path| std::fs::canonicalize(path).unwrap())
        .collect()
}

#[test]
fn a_viewer_started_by_com_opens_the_selection_in_order() {
    let dir = TempDir::new().unwrap();
    let files = images(&dir, &["12.png", "14.png", "01.png"]);
    let clsid = GUID::new().unwrap();
    let explorer = explorer(clsid, files.clone());
    let viewer = HeadlessViewer::start_with(
        Path::new(SEERW),
        Path::new(SEER),
        ["--clsid", &registry_form(&clsid), "-Embedding"],
    )
    .expect("the viewer starts");
    explorer.join().unwrap();

    let list = viewer.wait_for(|list| list.images.len() == 3);
    assert_eq!(paths(&list), canonical(&files));
    assert_eq!(list.selected, Some(list.images[0].id));
    assert!(viewer.quit().success());
}

#[test]
fn a_selection_executed_in_a_second_process_goes_to_the_running_viewer() {
    let dir = TempDir::new().unwrap();
    let files = images(&dir, &["a.png", "c.png", "b.png"]);
    let viewer = HeadlessViewer::start(Path::new(SEERW), Path::new(SEER), &[&files[0]])
        .expect("the viewer starts");
    viewer.wait_until_loaded(1);

    let clsid = GUID::new().unwrap();
    let explorer = explorer(clsid, files[1..].to_vec());
    let status = Command::new(SEERW)
        .args(["--headless", "--instance", viewer.instance()])
        .args(["--clsid", &registry_form(&clsid), "-Embedding"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    explorer.join().unwrap();
    assert!(status.success(), "{status}");

    let list = viewer.wait_until_loaded(3);
    assert_eq!(paths(&list), canonical(&files));
    assert_eq!(list.selected, Some(list.images[1].id));
    assert!(viewer.quit().success());
}

#[test]
fn a_bad_class_id_is_a_usage_error() {
    let status = Command::new(SEERW)
        .args(["--instance", &unique_instance()])
        .args(["--clsid", "{nonsense}", "-Embedding"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}
