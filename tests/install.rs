//! Installing over, and uninstalling, a viewer that is running from the
//! install directory. Everything goes into a throwaway registry key and
//! directory; nothing touches the user's real associations or `PATH`.

use std::fs;
use std::path::Path;

use seer::install::{self, EXECUTABLES, Target};
use seer::ipc::client::{self, LAUNCH_TIMEOUT};
use seer::ipc::instance::InstanceName;
use seer::protocol::{Reply, Request};
use seer::registry::TestRoot;
use seer::testutil::{TempDir, unique_instance};

/// Starts the installed viewer headless, runs `during` while it runs, then
/// quits it.
fn while_running(target: &Target, during: impl FnOnce()) {
    let instance = InstanceName::new(&unique_instance()).unwrap();
    let program = target.directory.join(install::VIEWER);
    let mut viewer = client::start_viewer(&program, &instance, false, &["--headless"]).unwrap();
    client::connect_to_started(&instance, &mut viewer, LAUNCH_TIMEOUT).unwrap();
    during();
    let reply = client::connect(&instance)
        .unwrap()
        .send(&Request::Quit)
        .unwrap();
    assert_eq!(reply, Reply::Done);
    assert!(viewer.wait().unwrap().success());
}

#[test]
fn a_running_viewer_is_replaced_and_removed() {
    let root = TestRoot::new().unwrap();
    let files = TempDir::new().unwrap();
    let trash = files.join("trash");
    fs::create_dir(&trash).unwrap();
    let target = Target {
        software: format!(r"{}\Software", root.path()),
        environment: format!(r"{}\Environment", root.path()),
        directory: files.join(r"Programs\Seer"),
        trash: trash.clone(),
        shortcuts: files.join(r"Start Menu\Programs"),
    };
    let source = Path::new(env!("CARGO_BIN_EXE_seerw")).parent().unwrap();
    assert!(source.join(EXECUTABLES[0]).is_file());

    install::install(&target, source).unwrap();
    assert!(target.shortcut().is_file());
    // The running viewer can't be overwritten, so it is moved aside.
    while_running(&target, || {
        install::install(&target, source).unwrap();
    });
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 1);
    while_running(&target, || {
        install::uninstall(&target).unwrap();
    });
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 2);
    assert!(!target.directory.exists());
    assert!(!target.shortcut().exists());
}
