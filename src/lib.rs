//! Seer, a single-window image viewer for Windows.

/// The program's name and version, as shown by `seer --version`.
pub fn version_line() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_program_and_version() {
        assert_eq!(
            version_line(),
            format!("seer {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
