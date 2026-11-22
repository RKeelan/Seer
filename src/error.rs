//! The library's one error type.
//!
//! Every error carries an [`ErrorCode`], which names the kind of failure on the
//! wire (`{"code": "no_viewer", "message": "..."}`) and decides the exit code
//! of `seer.exe`. [`Error`] serialises as exactly that object, so the pipe
//! protocol and the command line's JSON output use it directly.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The library's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The kind of a failure.
///
/// Serialised in `snake_case` (`NoViewer` is `"no_viewer"`). Codes are part of
/// the command line's interface for agents, so existing ones are not renamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A failure with no more specific code, such as an unexpected Win32 or
    /// I/O error.
    Unexpected,
    /// The command line was malformed.
    Usage,
    /// No viewer is running on the instance.
    NoViewer,
    /// One or more files could not be opened.
    OpenFailed,
    /// The file does not exist.
    FileNotFound,
    /// A folder given to open has no images in it that Seer reads.
    NoImages,
    /// The file is not an image format Seer can show.
    UnsupportedFormat,
    /// The file's format needs a Windows codec extension that is not
    /// installed, such as the WebP, HEIF, AV1 or JPEG XL extension.
    CodecMissing,
    /// The file's format is supported but the file could not be decoded.
    DecodeFailed,
    /// `marks --wait` reached its timeout.
    Timeout,
    /// The viewer does not serve this request yet.
    NotImplemented,
    /// The request is only accepted by a viewer started with `--headless`.
    NotHeadless,
    /// The peer speaks a different protocol version.
    ProtocolVersion,
    /// A message on the pipe could not be parsed.
    Protocol,
    /// A viewer already owns the instance.
    AlreadyRunning,
}

impl ErrorCode {
    /// The exit code `seer.exe` returns for an error of this kind.
    ///
    /// - 1: unexpected failure.
    /// - 2: usage error.
    /// - 3: no viewer is running.
    /// - 4: one or more files could not be opened.
    /// - 5: `marks --wait` timed out.
    pub fn exit_code(self) -> u8 {
        match self {
            ErrorCode::Unexpected
            | ErrorCode::NotImplemented
            | ErrorCode::ProtocolVersion
            | ErrorCode::Protocol
            | ErrorCode::AlreadyRunning => 1,
            ErrorCode::Usage | ErrorCode::NotHeadless => 2,
            ErrorCode::NoViewer => 3,
            ErrorCode::OpenFailed
            | ErrorCode::FileNotFound
            | ErrorCode::NoImages
            | ErrorCode::UnsupportedFormat
            | ErrorCode::CodecMissing
            | ErrorCode::DecodeFailed => 4,
            ErrorCode::Timeout => 5,
        }
    }
}

/// An error: a code saying what kind of failure it is, and a message for
/// people.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    /// The kind of failure.
    pub code: ErrorCode,
    /// What went wrong, in a sentence fit to show the user.
    pub message: String,
}

impl Error {
    /// An error of the given kind.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
        }
    }

    /// An [`ErrorCode::Unexpected`] error.
    pub fn unexpected(message: impl Into<String>) -> Self {
        Error::new(ErrorCode::Unexpected, message)
    }

    /// An [`ErrorCode::Usage`] error.
    pub fn usage(message: impl Into<String>) -> Self {
        Error::new(ErrorCode::Usage, message)
    }

    /// The exit code `seer.exe` returns for this error.
    pub fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(error: windows::core::Error) -> Self {
        Error::unexpected(format!("{} ({})", error.message(), error.code()))
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::unexpected(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::new(ErrorCode::Protocol, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_command_line_contract() {
        assert_eq!(ErrorCode::Unexpected.exit_code(), 1);
        assert_eq!(ErrorCode::NotImplemented.exit_code(), 1);
        assert_eq!(ErrorCode::ProtocolVersion.exit_code(), 1);
        assert_eq!(ErrorCode::Protocol.exit_code(), 1);
        assert_eq!(ErrorCode::AlreadyRunning.exit_code(), 1);
        assert_eq!(ErrorCode::Usage.exit_code(), 2);
        assert_eq!(ErrorCode::NotHeadless.exit_code(), 2);
        assert_eq!(ErrorCode::NoViewer.exit_code(), 3);
        assert_eq!(ErrorCode::OpenFailed.exit_code(), 4);
        assert_eq!(ErrorCode::FileNotFound.exit_code(), 4);
        assert_eq!(ErrorCode::NoImages.exit_code(), 4);
        assert_eq!(ErrorCode::UnsupportedFormat.exit_code(), 4);
        assert_eq!(ErrorCode::CodecMissing.exit_code(), 4);
        assert_eq!(ErrorCode::DecodeFailed.exit_code(), 4);
        assert_eq!(ErrorCode::Timeout.exit_code(), 5);
    }

    #[test]
    fn serialises_as_code_and_message() {
        let error = Error::new(ErrorCode::NoViewer, "no viewer is running");
        let json = serde_json::to_string(&error).unwrap();
        assert_eq!(
            json,
            r#"{"code":"no_viewer","message":"no viewer is running"}"#
        );
        assert_eq!(serde_json::from_str::<Error>(&json).unwrap(), error);
        assert_eq!(error.to_string(), "no viewer is running");
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn conversions_choose_a_code() {
        let windows = Error::from(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ));
        assert_eq!(windows.code, ErrorCode::Unexpected);
        assert!(
            windows.message.contains("0x80004005"),
            "{}",
            windows.message
        );

        let io = Error::from(std::io::Error::other("disk on fire"));
        assert_eq!(io, Error::unexpected("disk on fire"));

        let json = Error::from(serde_json::from_str::<u32>("x").unwrap_err());
        assert_eq!(json.code, ErrorCode::Protocol);

        assert_eq!(Error::usage("bad").code, ErrorCode::Usage);
    }
}
