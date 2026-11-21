//! `seer.exe` against the pipe, as an agent runs it.

use std::path::Path;
use std::process::Command;

use seer::ipc::client::{self, LAUNCH_TIMEOUT};
use seer::ipc::instance::InstanceName;
use seer::protocol::{Reply, Request};
use seer::testutil::unique_instance;

#[test]
fn no_viewer_exits_with_code_3() {
    let output = Command::new(env!("CARGO_BIN_EXE_seer"))
        .args(["--json", "--instance", &unique_instance(), "list"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#""code":"no_viewer""#), "{stdout}");
}

#[test]
fn the_viewer_is_started_and_watched_until_ready() {
    let instance = InstanceName::new(&unique_instance()).unwrap();
    let program = Path::new(env!("CARGO_BIN_EXE_seerw"));
    let mut viewer = client::start_viewer(program, &instance, false, &["--headless"]).unwrap();
    let connection = client::connect_to_started(&instance, &mut viewer, LAUNCH_TIMEOUT).unwrap();
    assert_eq!(connection.send(&Request::Quit).unwrap(), Reply::Done);
    assert!(viewer.wait().unwrap().success());
}

#[test]
fn a_viewer_that_exits_at_once_is_noticed() {
    let instance = InstanceName::new(&unique_instance()).unwrap();
    let program = Path::new(env!("CARGO_BIN_EXE_seerw"));
    let mut viewer = client::start_viewer(program, &instance, false, &["--frobnicate"]).unwrap();
    let error = client::connect_to_started(&instance, &mut viewer, LAUNCH_TIMEOUT).unwrap_err();
    assert!(error.message.contains("exited"), "{error}");
}
