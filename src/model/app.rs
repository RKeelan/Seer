//! The viewer's state machine: [`Input`] in, state change and [`Effect`]s out.
//!
//! The window procedure translates Win32 messages into [`Input`] and carries
//! out the [`Effect`]s; the pipe server's requests and the decoder's results
//! arrive as [`Input`] too. Nothing here touches Win32, so a whole session can
//! be driven in tests.
//!
//! The user-side inputs (keys, pointer, wheel, note text, window changes)
//! serialise as JSON objects tagged by `"type"`, which is how `seer debug
//! input` feeds synthetic input to a headless viewer:
//!
//! ```json
//! {"type":"key","key":"right"}
//! {"type":"key","key":"r","shift":true}
//! {"type":"pointer_down","button":"right","at":{"x":120,"y":80}}
//! {"type":"note_text","text":"this row has the wrong title bar"}
//! ```

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::model::ImageId;
use crate::model::geometry::{PixelSize, Point, Rect};
use crate::protocol::{Request, Response};

/// Identifies a pipe client's request while the viewer answers it, so the
/// reply reaches the right client. A `marks --wait` request keeps its id until
/// it is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

/// Something that happened, for the state machine to act on.
///
/// Positions are in client space: physical pixels from the client area's
/// top-left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    /// A key was pressed (or repeated while held).
    Key {
        /// The key.
        key: Key,
        /// The modifier keys held.
        #[serde(flatten)]
        modifiers: Modifiers,
    },
    /// A mouse button went down.
    PointerDown {
        /// The button.
        button: Button,
        /// Where.
        at: Point,
        /// The modifier keys held.
        #[serde(flatten)]
        modifiers: Modifiers,
    },
    /// The pointer moved.
    PointerMove {
        /// Where to.
        at: Point,
        /// The modifier keys held.
        #[serde(flatten)]
        modifiers: Modifiers,
    },
    /// A mouse button came up.
    PointerUp {
        /// The button.
        button: Button,
        /// Where.
        at: Point,
        /// The modifier keys held.
        #[serde(flatten)]
        modifiers: Modifiers,
    },
    /// A mouse button was double-clicked. Follows the second
    /// [`Input::PointerDown`].
    DoubleClick {
        /// The button.
        button: Button,
        /// Where.
        at: Point,
        /// The modifier keys held.
        #[serde(flatten)]
        modifiers: Modifiers,
    },
    /// The mouse wheel turned, or a precision touchpad scrolled. A pinch on
    /// a touchpad arrives as a wheel with Ctrl held.
    Wheel {
        /// Where the pointer was.
        at: Point,
        /// How far, in notches; positive is away from the user. Touchpads
        /// send fractions of a notch.
        delta: f64,
        /// The modifier keys held.
        #[serde(flatten)]
        modifiers: Modifiers,
    },
    /// A touchscreen pinch changed the zoom.
    Pinch {
        /// The pinch's centre.
        at: Point,
        /// The factor the pinch scaled by since the previous [`Input::Pinch`].
        scale: f64,
    },
    /// The note field's text is now `text`. The window sends this as the user
    /// types; synthetic input uses it to type a note.
    NoteText {
        /// The field's whole text.
        text: String,
    },
    /// The client area's size changed.
    Resize {
        /// The new size, in physical pixels.
        size: PixelSize,
    },
    /// The window moved to a monitor with a different scale.
    Dpi {
        /// The new DPI; 96 is 100% scale.
        dpi: u32,
    },
    /// The system's light or dark setting changed.
    Theme {
        /// Whether apps should now be dark.
        dark: bool,
    },
    /// The user asked to close the window.
    CloseWindow,
    /// A wake-up requested by [`Effect::WakeAfter`] arrived.
    Tick,
    /// A pipe client sent a request. Answered by [`Effect::Reply`] with the
    /// same id.
    #[serde(skip)]
    Request {
        /// The request's id.
        id: RequestId,
        /// The request.
        request: Request,
    },
    /// The client that sent a request disconnected before it was answered.
    #[serde(skip)]
    Disconnected {
        /// The request's id.
        id: RequestId,
    },
    /// The decoder finished an image.
    #[serde(skip)]
    Decoded {
        /// The image.
        image: ImageId,
        /// What was decoded.
        target: DecodeTarget,
        /// The image's full size in pixels, after EXIF orientation.
        size: PixelSize,
    },
    /// The decoder could not decode an image.
    #[serde(skip)]
    DecodeFailed {
        /// The image.
        image: ImageId,
        /// Why.
        error: Error,
    },
    /// An open image's file was written to.
    #[serde(skip)]
    FileChanged {
        /// The image.
        image: ImageId,
    },
    /// An open image's file was deleted or renamed away.
    #[serde(skip)]
    FileMissing {
        /// The image.
        image: ImageId,
    },
}

/// What the window must do on the state machine's behalf.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// What is shown has changed: rebuild the scene and present it.
    Redraw,
    /// Set the title bar's text.
    SetTitle {
        /// The text.
        title: String,
    },
    /// Answer a pipe client's request.
    Reply {
        /// The request's id.
        id: RequestId,
        /// The answer.
        response: Response,
    },
    /// Write the client area to a PNG and answer the request with
    /// [`crate::protocol::Reply::Screenshot`].
    Screenshot {
        /// The request's id.
        id: RequestId,
        /// Where to write the PNG.
        path: PathBuf,
    },
    /// Answer the request with the start-up timings, which the window keeps.
    ReportTimings {
        /// The request's id.
        id: RequestId,
    },
    /// Start decoding an image; the result arrives as [`Input::Decoded`] or
    /// [`Input::DecodeFailed`].
    Decode {
        /// The image.
        image: ImageId,
        /// The image's file.
        path: PathBuf,
        /// What to decode.
        target: DecodeTarget,
    },
    /// Drop any queued or running decode of an image.
    CancelDecode {
        /// The image.
        image: ImageId,
    },
    /// Show the note field over the client area, or move it there if shown.
    ShowNoteField {
        /// Where, in client space.
        rect: Rect,
        /// The text it starts with.
        text: String,
    },
    /// Hide the note field.
    HideNoteField,
    /// Set the pointer's shape over the client area.
    SetCursor {
        /// The shape.
        cursor: Cursor,
    },
    /// Bring the window to the foreground.
    Activate,
    /// Flash the taskbar button, without taking focus.
    FlashTaskbar,
    /// Start watching an image's file for changes.
    Watch {
        /// The image.
        image: ImageId,
        /// Its file.
        path: PathBuf,
    },
    /// Stop watching an image's file.
    Unwatch {
        /// The image.
        image: ImageId,
    },
    /// Send [`Input::Tick`] after a delay, for animations, fades and timeouts.
    /// A later request replaces an earlier one still pending.
    WakeAfter {
        /// The delay in milliseconds.
        ms: u64,
    },
    /// Close the window and end the process.
    Quit,
}

/// What to decode for an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecodeTarget {
    /// A version at least this large in both dimensions, keeping the aspect
    /// ratio, for showing quickly at window size.
    Scaled {
        /// The smallest acceptable size.
        at_least: PixelSize,
    },
    /// The full-resolution image.
    Full,
    /// A thumbnail for the strip, no larger than this in either dimension.
    Thumbnail {
        /// The largest acceptable size.
        at_most: PixelSize,
    },
}

/// A pointer shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    /// The normal arrow.
    Arrow,
    /// Crosshairs, for drawing a mark.
    Crosshair,
    /// A pointing hand, over something clickable.
    Hand,
    /// Four arrows, while panning.
    Move,
    /// A left-right arrow, over the split view's divider.
    SizeWestEast,
}

/// A mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    /// The primary button.
    Left,
    /// The secondary button.
    Right,
    /// The wheel button.
    Middle,
}

/// The modifier keys held during an input. Serialised as optional `ctrl`,
/// `shift` and `alt` booleans, present only when true.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Modifiers {
    /// Ctrl.
    #[serde(skip_serializing_if = "is_false")]
    pub ctrl: bool,
    /// Shift.
    #[serde(skip_serializing_if = "is_false")]
    pub shift: bool,
    /// Alt.
    #[serde(skip_serializing_if = "is_false")]
    pub alt: bool,
}

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
    };
    /// Ctrl alone.
    pub const CTRL: Modifiers = Modifiers {
        ctrl: true,
        shift: false,
        alt: false,
    };
    /// Shift alone.
    pub const SHIFT: Modifiers = Modifiers {
        ctrl: false,
        shift: true,
        alt: false,
    };
}

/// A key on the keyboard, independent of layout where it matters.
///
/// Serialised as a lower-case name (`"left"`, `"page_up"`, `"f2"`) or, for
/// [`Key::Char`], the character itself (`"r"`, `"0"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Key {
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Delete.
    Delete,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Enter, on the main keyboard or the keypad.
    Enter,
    /// Tab.
    Tab,
    /// The space bar.
    Space,
    /// The plus key, on the main keyboard (the key shared with `=`) or the
    /// keypad.
    Plus,
    /// The minus key, on the main keyboard or the keypad.
    Minus,
    /// A function key, F1 to F24.
    Function(u8),
    /// A letter, digit or punctuation key, as the lower-case character it
    /// types without Shift: R with or without Shift is `Char('r')`.
    Char(char),
}

const NAMED_KEYS: [(Key, &str); 16] = [
    (Key::Left, "left"),
    (Key::Right, "right"),
    (Key::Up, "up"),
    (Key::Down, "down"),
    (Key::Home, "home"),
    (Key::End, "end"),
    (Key::PageUp, "page_up"),
    (Key::PageDown, "page_down"),
    (Key::Delete, "delete"),
    (Key::Backspace, "backspace"),
    (Key::Escape, "escape"),
    (Key::Enter, "enter"),
    (Key::Tab, "tab"),
    (Key::Space, "space"),
    (Key::Plus, "plus"),
    (Key::Minus, "minus"),
];

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Function(n) => write!(f, "f{n}"),
            Key::Char(c) => write!(f, "{c}"),
            key => {
                let (_, name) = NAMED_KEYS
                    .iter()
                    .find(|(named, _)| named == key)
                    .expect("every other key is named");
                f.write_str(name)
            }
        }
    }
}

impl FromStr for Key {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let lower = name.to_lowercase();
        let mut chars = lower.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Ok(Key::Char(c));
        }
        if let Some((key, _)) = NAMED_KEYS.iter().find(|(_, named)| *named == lower) {
            return Ok(*key);
        }
        if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
            && (1..=24).contains(&n)
        {
            return Ok(Key::Function(n));
        }
        Err(format!("unknown key \"{name}\""))
    }
}

impl From<Key> for String {
    fn from(key: Key) -> Self {
        key.to_string()
    }
}

impl TryFrom<String> for Key {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        name.parse()
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_their_names() {
        for (key, name) in NAMED_KEYS {
            assert_eq!(key.to_string(), name);
            assert_eq!(name.parse::<Key>(), Ok(key));
        }
        assert_eq!("F2".parse::<Key>(), Ok(Key::Function(2)));
        assert_eq!(Key::Function(24).to_string(), "f24");
        assert_eq!("R".parse::<Key>(), Ok(Key::Char('r')));
        assert_eq!(Key::Char('0').to_string(), "0");
        assert!("f25".parse::<Key>().is_err());
        assert!("hyper".parse::<Key>().is_err());
        assert_eq!(serde_json::to_string(&Key::PageUp).unwrap(), "\"page_up\"");
        assert!(serde_json::from_str::<Key>("\"nope\"").is_err());
    }

    #[test]
    fn user_inputs_serialise_compactly() {
        let input = Input::Key {
            key: Key::Char('r'),
            modifiers: Modifiers::SHIFT,
        };
        let json = serde_json::to_string(&input).unwrap();
        assert_eq!(json, r#"{"type":"key","key":"r","shift":true}"#);
        assert_eq!(serde_json::from_str::<Input>(&json).unwrap(), input);

        let input: Input =
            serde_json::from_str(r#"{"type":"wheel","at":{"x":5,"y":6},"delta":-1,"ctrl":true}"#)
                .unwrap();
        assert_eq!(
            input,
            Input::Wheel {
                at: Point::new(5.0, 6.0),
                delta: -1.0,
                modifiers: Modifiers::CTRL,
            }
        );

        for input in [
            Input::PointerMove {
                at: Point::new(1.0, 2.0),
                modifiers: Modifiers::NONE,
            },
            Input::PointerUp {
                button: Button::Left,
                at: Point::new(1.0, 2.0),
                modifiers: Modifiers::NONE,
            },
            Input::DoubleClick {
                button: Button::Middle,
                at: Point::new(1.0, 2.0),
                modifiers: Modifiers::NONE,
            },
            Input::Pinch {
                at: Point::new(1.0, 2.0),
                scale: 1.5,
            },
            Input::Resize {
                size: PixelSize::new(800, 600),
            },
            Input::Dpi { dpi: 144 },
            Input::Theme { dark: true },
            Input::CloseWindow,
            Input::Tick,
        ] {
            let json = serde_json::to_string(&input).unwrap();
            assert_eq!(serde_json::from_str::<Input>(&json).unwrap(), input);
        }
    }

    #[test]
    fn internal_inputs_cannot_be_sent_as_synthetic_input() {
        assert!(serde_json::to_string(&Input::Disconnected { id: RequestId(1) }).is_err());
        assert!(serde_json::from_str::<Input>(r#"{"type":"disconnected","id":1}"#).is_err());
    }

    #[test]
    fn decode_targets_serialise_with_their_kind() {
        let target = DecodeTarget::Scaled {
            at_least: PixelSize::new(10, 20),
        };
        let json = serde_json::to_string(&target).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"scaled","at_least":{"width":10,"height":20}}"#
        );
        assert_eq!(serde_json::from_str::<DecodeTarget>(&json).unwrap(), target);
    }
}
