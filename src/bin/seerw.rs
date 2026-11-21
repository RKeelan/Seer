//! `seerw.exe`, the viewer. See [`seer::viewer`].

#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    seer::viewer::main()
}
