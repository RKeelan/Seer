//! Both executables print the version.

use std::process::Command;

fn version_of(program: &str) -> String {
    let output = Command::new(program)
        .arg("--version")
        .output()
        .expect("run the program");
    assert!(
        output.status.success(),
        "{program} exited with {}",
        output.status
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

#[test]
fn seer_prints_the_version() {
    assert_eq!(
        version_of(env!("CARGO_BIN_EXE_seer")),
        format!("{}\n", seer::version_line())
    );
}

#[test]
fn seerw_prints_the_version() {
    assert_eq!(
        version_of(env!("CARGO_BIN_EXE_seerw")),
        format!("{}\n", seer::version_line())
    );
}

#[test]
fn testutil_is_available_to_integration_tests() {
    let dir = seer::testutil::TempDir::new().unwrap();
    assert!(dir.path().is_dir());
}
