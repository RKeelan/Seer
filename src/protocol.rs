//! The messages `seer.exe` and the viewer exchange over the pipe.
//!
//! Each message is one JSON object on one line, UTF-8, carrying the protocol
//! version as `"v"`. A client sends one [`Request`] and reads one
//! [`Response`]. Requests are tagged by `"cmd"`:
//!
//! ```json
//! {"v":1,"cmd":"open","files":[{"path":"C:\\shots\\after.png","caption":"after"}],"group":"run 3"}
//! {"v":1,"cmd":"marks","wait":true,"timeout_ms":60000}
//! ```
//!
//! A response holds either `"ok"`, a [`Reply`] tagged by `"type"`, or
//! `"error"`, an [`Error`]:
//!
//! ```json
//! {"v":1,"ok":{"type":"opened","images":[{"id":4,"path":"C:\\shots\\after.png","existing":false}],"failed":[]}}
//! {"v":1,"error":{"code":"timeout","message":"no marks were finished within 60 seconds"}}
//! ```
//!
//! Paths are absolute: the client resolves them before sending, since the
//! viewer's working directory is not the client's.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode, Result};
use crate::model::app::Input;
use crate::model::geometry::{PixelRect, PixelSize, Rect, Rotation};
use crate::model::{ImageId, MarkId};

/// The protocol version this build speaks, sent as `"v"` on every message.
pub const VERSION: u32 = 1;

/// A request from a client to the viewer, one per command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Add images to the set (`seer open`). An image already in the set is
    /// selected rather than added again.
    Open {
        /// The files, in order, each with its optional caption.
        files: Vec<OpenFile>,
        /// The group every file joins, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// Show the group side by side once opened.
        #[serde(default, skip_serializing_if = "is_false")]
        compare: bool,
        /// Bring the window to the foreground. Without it an existing window
        /// flashes its taskbar button, and a new one shows without activating.
        #[serde(default, skip_serializing_if = "is_false")]
        activate: bool,
        /// The index in `files` of the file to select. Without it, or if that
        /// file cannot be opened, the first file opened is selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        select: Option<usize>,
    },
    /// Report the set and the view (`seer list`).
    List,
    /// Remove the given images (`seer close <id or path>…`).
    Close {
        /// The images to remove.
        images: Vec<ImageRef>,
    },
    /// Remove every image (`seer close --all`).
    CloseAll,
    /// Remove a group's images (`seer clear <group>`).
    Clear {
        /// The group's name.
        group: String,
    },
    /// Report the marks (`seer marks`).
    Marks {
        /// Answer only once the user has finished marking (Ctrl+Enter or the
        /// banner's Done button).
        #[serde(default, skip_serializing_if = "is_false")]
        wait: bool,
        /// With `wait`, how long to wait before answering with an
        /// [`ErrorCode::Timeout`] error. No limit when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        /// Remove the marks once reported.
        #[serde(default, skip_serializing_if = "is_false")]
        clear: bool,
    },
    /// Write the client area, as the user would see it, to a PNG file
    /// (`seer screenshot`).
    Screenshot {
        /// Where to write the PNG.
        path: PathBuf,
    },
    /// Close the viewer (`seer quit`). The viewer replies before it exits.
    Quit,
    /// Feed synthetic input to the viewer (`seer debug input`). Accepted only
    /// by a viewer started with `--headless`; others answer
    /// [`ErrorCode::NotHeadless`].
    DebugInput {
        /// The events, applied in order.
        events: Vec<Input>,
    },
    /// Report start-up timings (`seer debug timings`).
    DebugTimings,
}

/// One file to open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFile {
    /// The file's absolute path.
    pub path: PathBuf,
    /// The caption shown with the image, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

/// An image named by id or by path. Serialised as a bare number or a bare
/// string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageRef {
    /// The image with this id.
    Id(ImageId),
    /// The image opened from this absolute path.
    Path(PathBuf),
}

/// The viewer's answer to a request: a [`Reply`] or an [`Error`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// The request succeeded.
    Ok(Reply),
    /// The request failed.
    Error(Error),
}

impl From<Result<Reply>> for Response {
    fn from(result: Result<Reply>) -> Self {
        match result {
            Ok(reply) => Response::Ok(reply),
            Err(error) => Response::Error(error),
        }
    }
}

impl From<Response> for Result<Reply> {
    fn from(response: Response) -> Self {
        match response {
            Response::Ok(reply) => Ok(reply),
            Response::Error(error) => Err(error),
        }
    }
}

/// What a successful request returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// The answer to [`Request::Open`].
    Opened(OpenReply),
    /// The answer to [`Request::List`].
    List(ListReply),
    /// The answer to [`Request::Close`], [`Request::CloseAll`] and
    /// [`Request::Clear`].
    Closed {
        /// The images removed, in set order.
        closed: Vec<ImageId>,
        /// The requested images that were not in the set.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        not_found: Vec<ImageRef>,
    },
    /// The answer to [`Request::Marks`].
    Marks {
        /// Every mark, in the order the images are in the set and, within an
        /// image, the order the marks were made.
        marks: Vec<MarkInfo>,
    },
    /// The answer to [`Request::Screenshot`].
    Screenshot {
        /// Where the PNG was written.
        path: PathBuf,
        /// The PNG's size, which is the client area's.
        size: PixelSize,
    },
    /// The answer to [`Request::DebugTimings`].
    Timings(Timings),
    /// The answer to a request that returns nothing: [`Request::Quit`] and
    /// [`Request::DebugInput`].
    Done,
}

/// The answer to [`Request::Open`].
///
/// When `failed` is not empty `seer.exe` exits with code 4; the other files
/// are still opened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    /// The files that are now in the set, in request order.
    pub images: Vec<OpenedImage>,
    /// The files that could not be added, such as missing files or ones in a
    /// format Seer does not show, and those added that need a codec
    /// extension that is not installed. The latter are in `images` too, and
    /// the window shows which extension they need.
    pub failed: Vec<OpenFailure>,
}

/// A file [`Request::Open`] put in the set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenedImage {
    /// The image's id.
    pub id: ImageId,
    /// The image's absolute path.
    pub path: PathBuf,
    /// Whether the image was already in the set, and so was selected rather
    /// than added.
    pub existing: bool,
}

/// A file [`Request::Open`] could not add.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFailure {
    /// The file's absolute path.
    pub path: PathBuf,
    /// Why it could not be added.
    pub error: Error,
}

/// The answer to [`Request::List`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListReply {
    /// The set, in order.
    pub images: Vec<ImageInfo>,
    /// The selected image, if the set is not empty.
    pub selected: Option<ImageId>,
    /// How the selected image is shown, if the set is not empty.
    pub view: Option<ViewInfo>,
    /// The compare view's layout, if compare view is on.
    pub compare: Option<CompareLayout>,
}

/// One image in [`ListReply`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// The image's id.
    pub id: ImageId,
    /// The image's absolute path.
    pub path: PathBuf,
    /// The image's caption, if any.
    pub caption: Option<String>,
    /// The image's group, if any.
    pub group: Option<String>,
    /// The image's size in pixels, once known. For an SVG, its viewport.
    pub size: Option<PixelSize>,
    /// How far decoding has got.
    pub state: LoadState,
    /// Why decoding failed, when `state` is [`LoadState::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    /// Whether the file has been deleted since it was opened. The viewer keeps
    /// showing its last image.
    pub missing: bool,
}

/// How far an image's decoding has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    /// Nothing is decoded: the image is not decoded yet, or was dropped from
    /// memory. It is decoded when it is shown or next to the image shown.
    Loading,
    /// A version scaled to the window, or the image as it was before its file
    /// changed, is shown; the full-resolution decode is still to come.
    Preview,
    /// Decoded at full resolution.
    Loaded,
    /// Decoding failed.
    Failed,
}

/// How the selected image is shown, in [`ListReply`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewInfo {
    /// The zoom factor: 1.0 shows one image pixel per screen pixel.
    pub zoom: f64,
    /// Whether the zoom follows the window, fitting the image to it.
    pub fit: bool,
    /// The view rotation.
    pub rotation: Rotation,
    /// The part of the image in view, in image pixels before rotation.
    pub visible: Rect,
}

/// The compare view's layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareLayout {
    /// The group's images side by side, each captioned.
    Tiles,
    /// The selected image and the next in its group over the same area,
    /// divided by a line the user drags.
    Split,
}

/// One mark in [`Reply::Marks`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkInfo {
    /// The mark's id.
    pub id: MarkId,
    /// The image the mark is on.
    pub image: ImageId,
    /// That image's absolute path.
    pub path: PathBuf,
    /// The marked region, in image pixels before any view rotation.
    pub rect: PixelRect,
    /// The user's note; empty if they left none.
    pub note: String,
}

/// The answer to [`Request::DebugTimings`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Points in start-up, in the order they were reached.
    pub phases: Vec<Phase>,
}

/// A point in start-up, in [`Timings`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Phase {
    /// What was reached, such as `first_present`.
    pub name: String,
    /// Milliseconds from the process's creation.
    pub ms: f64,
}

/// Serialises a request as one line, without the newline that ends it.
pub fn encode_request(request: &Request) -> Result<String> {
    encode(request)
}

/// Parses a line holding a request.
pub fn decode_request(line: &str) -> Result<Request> {
    decode(line)
}

/// Serialises a response as one line, without the newline that ends it.
pub fn encode_response(response: &Response) -> Result<String> {
    encode(response)
}

/// Parses a line holding a response.
pub fn decode_response(line: &str) -> Result<Response> {
    decode(line)
}

#[derive(Serialize)]
struct Outgoing<'a, T> {
    v: u32,
    #[serde(flatten)]
    body: &'a T,
}

#[derive(Deserialize)]
struct Incoming<T> {
    #[serde(rename = "v")]
    _version: u32,
    #[serde(flatten)]
    body: T,
}

#[derive(Deserialize)]
struct VersionOnly {
    v: Option<u32>,
}

fn encode<T: Serialize>(body: &T) -> Result<String> {
    Ok(serde_json::to_string(&Outgoing { v: VERSION, body })?)
}

fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T> {
    let line = line.trim_end_matches(['\r', '\n']);
    match serde_json::from_str::<VersionOnly>(line)?.v {
        Some(VERSION) => Ok(serde_json::from_str::<Incoming<T>>(line)?.body),
        Some(other) => Err(Error::new(
            ErrorCode::ProtocolVersion,
            format!("the peer speaks protocol version {other}, not {VERSION}"),
        )),
        None => Err(Error::new(
            ErrorCode::Protocol,
            "the message has no protocol version",
        )),
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::app::{Button, Key, Modifiers};
    use crate::model::geometry::Point;

    fn round_trip_request(request: Request) {
        let line = encode_request(&request).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(decode_request(&line).unwrap(), request, "{line}");
    }

    fn round_trip_response(response: Response) {
        let line = encode_response(&response).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(decode_response(&line).unwrap(), response, "{line}");
    }

    #[test]
    fn every_request_round_trips() {
        round_trip_request(Request::Open {
            files: vec![
                OpenFile {
                    path: PathBuf::from(r"C:\shots\before.png"),
                    caption: Some("before".into()),
                },
                OpenFile {
                    path: PathBuf::from(r"C:\shots\after.png"),
                    caption: None,
                },
            ],
            group: Some("run 3".into()),
            compare: true,
            activate: true,
            select: Some(1),
        });
        round_trip_request(Request::List);
        round_trip_request(Request::Close {
            images: vec![
                ImageRef::Id(ImageId(2)),
                ImageRef::Path(PathBuf::from(r"C:\a.png")),
            ],
        });
        round_trip_request(Request::CloseAll);
        round_trip_request(Request::Clear {
            group: "run 3".into(),
        });
        round_trip_request(Request::Marks {
            wait: true,
            timeout_ms: Some(5000),
            clear: true,
        });
        round_trip_request(Request::Screenshot {
            path: PathBuf::from(r"C:\out.png"),
        });
        round_trip_request(Request::Quit);
        round_trip_request(Request::DebugInput {
            events: vec![
                Input::Key {
                    key: Key::Right,
                    modifiers: Modifiers::default(),
                },
                Input::PointerDown {
                    button: Button::Right,
                    at: Point::new(10.0, 20.0),
                    modifiers: Modifiers {
                        ctrl: true,
                        ..Modifiers::default()
                    },
                },
                Input::NoteText {
                    text: "wrong title bar".into(),
                },
            ],
        });
        round_trip_request(Request::DebugTimings);
    }

    #[test]
    fn every_reply_round_trips() {
        round_trip_response(Response::Ok(Reply::Opened(OpenReply {
            images: vec![OpenedImage {
                id: ImageId(1),
                path: PathBuf::from(r"C:\a.png"),
                existing: false,
            }],
            failed: vec![OpenFailure {
                path: PathBuf::from(r"C:\gone.png"),
                error: Error::new(ErrorCode::FileNotFound, "no such file"),
            }],
        })));
        round_trip_response(Response::Ok(Reply::List(ListReply {
            images: vec![ImageInfo {
                id: ImageId(1),
                path: PathBuf::from(r"C:\a.png"),
                caption: Some("before".into()),
                group: Some("g".into()),
                size: Some(PixelSize::new(640, 480)),
                state: LoadState::Failed,
                error: Some(Error::new(ErrorCode::DecodeFailed, "truncated")),
                missing: false,
            }],
            selected: Some(ImageId(1)),
            view: Some(ViewInfo {
                zoom: 0.5,
                fit: true,
                rotation: Rotation::Clockwise90,
                visible: Rect::new(0.0, 0.0, 640.0, 480.0),
            }),
            compare: Some(CompareLayout::Split),
        })));
        round_trip_response(Response::Ok(Reply::List(ListReply {
            images: vec![],
            selected: None,
            view: None,
            compare: None,
        })));
        round_trip_response(Response::Ok(Reply::Closed {
            closed: vec![ImageId(1)],
            not_found: vec![ImageRef::Id(ImageId(9))],
        }));
        round_trip_response(Response::Ok(Reply::Marks {
            marks: vec![MarkInfo {
                id: MarkId(1),
                image: ImageId(1),
                path: PathBuf::from(r"C:\a.png"),
                rect: PixelRect::new(1, 2, 3, 4),
                note: "here".into(),
            }],
        }));
        round_trip_response(Response::Ok(Reply::Screenshot {
            path: PathBuf::from(r"C:\out.png"),
            size: PixelSize::new(800, 600),
        }));
        round_trip_response(Response::Ok(Reply::Timings(Timings {
            phases: vec![Phase {
                name: "first_present".into(),
                ms: 92.5,
            }],
        })));
        round_trip_response(Response::Ok(Reply::Done));
        round_trip_response(Response::Error(Error::new(ErrorCode::Timeout, "timed out")));
    }

    #[test]
    fn wire_format_is_as_documented() {
        let line = encode_request(&Request::Marks {
            wait: true,
            timeout_ms: Some(60000),
            clear: false,
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"cmd":"marks","wait":true,"timeout_ms":60000}"#
        );

        let line = encode_response(&Response::Ok(Reply::Done)).unwrap();
        assert_eq!(line, r#"{"v":1,"ok":{"type":"done"}}"#);

        let line =
            encode_response(&Response::Error(Error::new(ErrorCode::NoViewer, "none"))).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"error":{"code":"no_viewer","message":"none"}}"#
        );

        // Optional fields may be left out, and a trailing newline is accepted.
        assert_eq!(
            decode_request("{\"v\":1,\"cmd\":\"open\",\"files\":[{\"path\":\"C:\\\\a.png\"}]}\r\n")
                .unwrap(),
            Request::Open {
                files: vec![OpenFile {
                    path: PathBuf::from(r"C:\a.png"),
                    caption: None
                }],
                group: None,
                compare: false,
                activate: false,
                select: None,
            }
        );
        assert_eq!(
            decode_request(r#"{"v":1,"cmd":"marks"}"#).unwrap(),
            Request::Marks {
                wait: false,
                timeout_ms: None,
                clear: false
            }
        );
    }

    #[test]
    fn versions_and_malformed_lines_are_rejected() {
        let error = decode_request(r#"{"v":2,"cmd":"list"}"#).unwrap_err();
        assert_eq!(error.code, ErrorCode::ProtocolVersion);

        let error = decode_request(r#"{"cmd":"list"}"#).unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);

        let error = decode_request(r#"{"v":1,"cmd":"dance"}"#).unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);

        let error = decode_response("not json").unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
    }

    #[test]
    fn response_converts_to_and_from_result() {
        let response = Response::from(Ok(Reply::Done));
        assert_eq!(Result::<Reply>::from(response), Ok(Reply::Done));
        let error = Error::usage("bad");
        let response = Response::from(Err(error.clone()));
        assert_eq!(Result::<Reply>::from(response), Err(error));
    }
}
