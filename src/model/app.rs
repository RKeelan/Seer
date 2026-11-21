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

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode};
use crate::model::geometry::{PixelRect, PixelSize, Point, Rect};
use crate::model::marks::{Marks, rect_between};
use crate::model::set::{Files, OpenSet, PathKey, normalise};
use crate::model::view::{Frame, View, ZOOM_STEP};
use crate::model::{ImageId, MarkId};
use crate::protocol::{
    CompareLayout, ImageInfo, ImageRef, ListReply, LoadState, MarkInfo, OpenFailure, OpenFile,
    OpenReply, OpenedImage, Reply, Request, Response, ViewInfo,
};
use crate::render::scene::{Align, Colour, Interpolation, Item, Scene};

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
    /// A version large enough to show the whole image fitted within this
    /// size without enlarging it, keeping the aspect ratio. Unlike `Scaled`,
    /// it needs no knowledge of the image's shape, so it suits the first
    /// decode of an image at window size.
    Fit {
        /// The area the image is fitted within.
        within: PixelSize,
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

/// The title bar's text, ahead of the usual title, while an agent waits on
/// `marks --wait`. The banner across the top of the client area says the same.
pub const WAITING_TITLE: &str = "Agent waiting for your marks (Ctrl+Enter when done)";

/// How long the zoom readout takes to fade in.
const READOUT_FADE_IN: Duration = Duration::from_millis(150);
/// How long after the last zoom change the readout starts to fade out.
const READOUT_HOLD: Duration = Duration::from_millis(1000);
/// How long the readout takes to fade out.
const READOUT_FADE_OUT: Duration = Duration::from_millis(300);
/// The interval between frames of an animation.
const FRAME: Duration = Duration::from_millis(16);
/// How far, in pixels at 100% scale, the pointer must move with a button
/// down before a press becomes a drag.
const DRAG_THRESHOLD: f64 = 4.0;
/// The zoom from which pixels are drawn as squares so they can be inspected.
const NEAREST_FROM: f64 = 4.0;

const DARK_BACKGROUND: Colour = Colour::rgb(32, 32, 32);
const DARK_TEXT: Colour = Colour::rgb(230, 230, 230);
const LIGHT_BACKGROUND: Colour = Colour::rgb(243, 243, 243);
const LIGHT_TEXT: Colour = Colour::rgb(28, 28, 28);
const ACCENT: Colour = Colour::rgb(0, 103, 192);
const WHITE: Colour = Colour::rgb(255, 255, 255);
const BLACK: Colour = Colour::rgb(0, 0, 0);
const MARK: Colour = Colour::rgb(255, 196, 0);
const SELECTED_MARK: Colour = Colour::rgb(255, 64, 129);

/// How the viewer starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// Whether the viewer was started with `--headless`, which allows
    /// [`Request::DebugInput`].
    pub headless: bool,
    /// The client area's size.
    pub size: PixelSize,
    /// The window's DPI; 96 is 100% scale.
    pub dpi: u32,
    /// Whether apps are dark.
    pub dark: bool,
}

/// The viewer's state: the open set, the views, the marks, and what the user
/// is in the middle of doing.
///
/// [`App::handle`] takes each [`Input`] with the time it arrived, changes the
/// state, and returns the [`Effect`]s for the window to carry out.
/// [`App::scene`] describes what the window should show. `App` has no clock of
/// its own: the time passed to `handle` drives the zoom readout's fade and the
/// timeouts of `marks --wait`, and `App` asks for an [`Input::Tick`] with
/// [`Effect::WakeAfter`] when it next needs to look at the time.
pub struct App {
    files: Box<dyn Files>,
    headless: bool,
    size: PixelSize,
    dpi: u32,
    dark: bool,
    now: Instant,
    set: OpenSet,
    images: HashMap<ImageId, ImageState>,
    marks: Marks,
    compare: Option<Compare>,
    mark_mode: bool,
    selected_mark: Option<MarkId>,
    note: Option<Note>,
    gesture: Gesture,
    pointer: Option<Point>,
    waiters: Vec<Waiter>,
    zoom_changed: Option<Instant>,
    shown: Shown,
}

/// What the model knows about an image beyond its place in the set.
struct ImageState {
    size: Option<PixelSize>,
    state: LoadState,
    error: Option<Error>,
    missing: bool,
    full_requested: bool,
    view: View,
}

impl ImageState {
    fn new(full_requested: bool) -> Self {
        ImageState {
            size: None,
            state: LoadState::Loading,
            error: None,
            missing: false,
            full_requested,
            view: View::default(),
        }
    }
}

/// Compare view: a group's images shown together with one shared view.
struct Compare {
    group: String,
    layout: CompareLayout,
    view: View,
    /// The split layout's divider, as a fraction of the width.
    split: f64,
}

/// The note field, open on a mark being made or on an existing mark.
struct Note {
    target: NoteTarget,
    text: String,
}

enum NoteTarget {
    New { image: ImageId, rect: PixelRect },
    Existing(MarkId),
}

/// What a held mouse button is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gesture {
    Idle,
    /// Panning, once the pointer has moved far enough to count as a drag;
    /// a click until then.
    Pan {
        button: Button,
        image: ImageId,
        start: Point,
        last: Point,
        moved: bool,
    },
    /// Drawing a mark.
    Draw {
        button: Button,
        image: ImageId,
        start: Point,
        current: Point,
    },
    /// Moving the split layout's divider.
    Divider {
        button: Button,
    },
}

impl Gesture {
    fn button(&self) -> Option<Button> {
        match *self {
            Gesture::Idle => None,
            Gesture::Pan { button, .. }
            | Gesture::Draw { button, .. }
            | Gesture::Divider { button } => Some(button),
        }
    }

    fn image(&self) -> Option<ImageId> {
        match *self {
            Gesture::Pan { image, .. } | Gesture::Draw { image, .. } => Some(image),
            Gesture::Idle | Gesture::Divider { .. } => None,
        }
    }
}

/// A `marks --wait` request not yet answered.
struct Waiter {
    id: RequestId,
    deadline: Option<Instant>,
    timeout_ms: u64,
    clear: bool,
}

/// What the window has been told, so effects are sent only on a change.
struct Shown {
    title: Option<String>,
    cursor: Cursor,
    note_field: Option<Rect>,
    wake_at: Option<Instant>,
}

/// Where an image is drawn.
#[derive(Clone, Debug, PartialEq)]
struct Pane {
    image: ImageId,
    /// The area the image is placed in.
    viewport: Rect,
    /// The part of the viewport the image may cover.
    clip: Rect,
    /// Where its caption goes, if it shows one.
    label: Option<(Rect, Align)>,
}

impl App {
    /// A viewer with nothing open, recognising files through `files`.
    pub fn new(config: Config, files: Box<dyn Files>, now: Instant) -> Self {
        App {
            files,
            headless: config.headless,
            size: config.size,
            dpi: config.dpi,
            dark: config.dark,
            now,
            set: OpenSet::new(),
            images: HashMap::new(),
            marks: Marks::new(),
            compare: None,
            mark_mode: false,
            selected_mark: None,
            note: None,
            gesture: Gesture::Idle,
            pointer: None,
            waiters: Vec::new(),
            zoom_changed: None,
            shown: Shown {
                title: None,
                cursor: Cursor::Arrow,
                note_field: None,
                wake_at: None,
            },
        }
    }

    /// Acts on an input that arrived at `now`, and returns what the window
    /// must do. `now` must come from a monotonic clock; an earlier time than
    /// the last is treated as the last.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Effect> {
        let before = self.scene();
        self.now = self.now.max(now);
        let mut effects = Vec::new();
        self.expire_waiters(&mut effects);
        self.apply(input, &mut effects);
        self.settle(&before, &mut effects);
        effects
    }

    /// The open set.
    pub fn set(&self) -> &OpenSet {
        &self.set
    }

    /// The marks.
    pub fn marks(&self) -> &Marks {
        &self.marks
    }

    /// Whether a plain left drag draws a mark rather than panning.
    pub fn mark_mode(&self) -> bool {
        self.mark_mode
    }

    /// Whether any client is waiting on `marks --wait`.
    pub fn waiting(&self) -> bool {
        !self.waiters.is_empty()
    }

    fn apply(&mut self, input: Input, effects: &mut Vec<Effect>) {
        match input {
            Input::Key { key, modifiers } => self.key(key, modifiers, effects),
            Input::PointerDown { button, at, .. } => self.pointer_down(button, at, effects),
            Input::PointerMove { at, .. } => self.pointer_move(at),
            Input::PointerUp { button, at, .. } => self.pointer_up(button, at),
            Input::DoubleClick { button, at, .. } => self.double_click(button, at),
            Input::Wheel { at, delta, .. } => self.zoom_at(at, ZOOM_STEP.powf(delta)),
            Input::Pinch { at, scale } => self.zoom_at(at, scale),
            Input::NoteText { text } => {
                if let Some(note) = &mut self.note {
                    note.text = text;
                }
            }
            Input::Resize { size } => self.size = size,
            Input::Dpi { dpi } => self.dpi = dpi,
            Input::Theme { dark } => self.dark = dark,
            Input::CloseWindow => self.quit(effects),
            Input::Tick => self.shown.wake_at = None,
            Input::Request { id, request } => self.request(id, request, effects),
            Input::Disconnected { id } => self.waiters.retain(|waiter| waiter.id != id),
            Input::Decoded {
                image,
                target,
                size,
            } => self.decoded(image, target, size),
            Input::DecodeFailed { image, error } => {
                if let Some(state) = self.images.get_mut(&image) {
                    state.state = LoadState::Failed;
                    state.error = Some(error);
                }
            }
            Input::FileChanged { image } => {
                if let (Some(entry), Some(state)) =
                    (self.set.get(image), self.images.get_mut(&image))
                {
                    state.missing = false;
                    state.full_requested = true;
                    effects.push(Effect::Decode {
                        image,
                        path: entry.path.clone(),
                        target: DecodeTarget::Full,
                    });
                }
            }
            Input::FileMissing { image } => {
                if let Some(state) = self.images.get_mut(&image) {
                    state.missing = true;
                }
            }
        }
    }

    // Keys.

    fn key(&mut self, key: Key, modifiers: Modifiers, effects: &mut Vec<Effect>) {
        if self.note.is_some() {
            // The note field has the focus; only Enter and Escape reach here.
            match key {
                Key::Enter => {
                    self.commit_note();
                    if modifiers.ctrl {
                        self.finish_marking(effects);
                    }
                }
                Key::Escape => self.note = None,
                _ => {}
            }
            return;
        }
        if modifiers.alt {
            return;
        }
        if modifiers.ctrl {
            match key {
                Key::Enter => self.finish_marking(effects),
                Key::Char('w') => self.close_selected(effects),
                Key::Plus | Key::Minus | Key::Char('0') | Key::Char('1') => self.zoom_key(key),
                _ => {}
            }
            return;
        }
        match key {
            Key::Left | Key::Up | Key::PageUp => self.navigate(|set| {
                set.step(-1);
            }),
            Key::Right | Key::Down | Key::PageDown => self.navigate(|set| {
                set.step(1);
            }),
            Key::Home => self.navigate(OpenSet::select_first),
            Key::End => self.navigate(OpenSet::select_last),
            Key::Delete => match self.selected_mark.take() {
                Some(mark) => {
                    self.marks.remove(mark);
                }
                None => self.close_selected(effects),
            },
            Key::Escape => self.escape(),
            Key::Plus | Key::Minus | Key::Char('0') | Key::Char('1') => self.zoom_key(key),
            Key::Char('r') => {
                if let Some(image) = self.set.selected() {
                    let view = self.view_mut(image);
                    if modifiers.shift {
                        view.rotate_anticlockwise();
                    } else {
                        view.rotate_clockwise();
                    }
                }
            }
            Key::Char('m') => {
                self.mark_mode = !self.mark_mode;
                if let Gesture::Draw { .. } = self.gesture {
                    self.gesture = Gesture::Idle;
                }
            }
            Key::Char('c') => self.toggle_compare(),
            Key::Char('s') => {
                if let Some(compare) = &mut self.compare {
                    compare.layout = match compare.layout {
                        CompareLayout::Tiles => CompareLayout::Split,
                        CompareLayout::Split => CompareLayout::Tiles,
                    };
                }
            }
            Key::Function(2) => {
                if let Some(mark) = self.selected_mark {
                    self.edit_mark(mark);
                }
            }
            _ => {}
        }
    }

    fn navigate(&mut self, step: impl FnOnce(&mut OpenSet)) {
        let before = self.set.selected();
        step(&mut self.set);
        if self.set.selected() != before {
            self.selected_mark = None;
        }
    }

    fn escape(&mut self) {
        if let Gesture::Draw { .. } = self.gesture {
            self.gesture = Gesture::Idle;
        } else if self.mark_mode {
            self.mark_mode = false;
        } else {
            self.selected_mark = None;
        }
    }

    fn zoom_key(&mut self, key: Key) {
        self.change_zoom(None, |view, frame, at| match key {
            Key::Plus => view.zoom_about(frame, at, ZOOM_STEP),
            Key::Minus => view.zoom_about(frame, at, 1.0 / ZOOM_STEP),
            Key::Char('0') => view.fit(),
            _ => view.zoom_to(frame, at, 1.0),
        });
    }

    fn zoom_at(&mut self, at: Point, factor: f64) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        self.change_zoom(Some(at), |view, frame, at| {
            view.zoom_about(frame, at, factor)
        });
    }

    /// Changes the view of the pane under `at`, or of the selected image about
    /// its middle, and shows the zoom readout if the zoom changed.
    fn change_zoom(&mut self, at: Option<Point>, change: impl FnOnce(&mut View, Frame, Point)) {
        let pane = match at {
            Some(at) => self.pane_at(at),
            None => self.selected_pane(),
        };
        let Some((pane, frame)) = pane.and_then(|pane| self.frame(&pane).map(|f| (pane, f))) else {
            return;
        };
        let at = at.unwrap_or_else(|| middle(pane.viewport));
        let before = self.view(pane.image).zoom(frame);
        change(self.view_mut(pane.image), frame, at);
        if self.view(pane.image).zoom(frame) != before {
            self.zoom_changed = Some(self.now);
        }
    }

    fn toggle_compare(&mut self) {
        match self.compare.take() {
            Some(compare) => {
                if let Some(image) = self.set.selected() {
                    *self.view_mut(image) = compare.view;
                }
            }
            None => self.start_compare(),
        }
    }

    fn start_compare(&mut self) {
        let Some(entry) = self.set.selected_entry() else {
            return;
        };
        let Some(group) = entry.group.clone() else {
            return;
        };
        self.compare = Some(Compare {
            group,
            layout: CompareLayout::Tiles,
            view: self.images[&entry.id].view,
            split: 0.5,
        });
    }

    // The pointer.

    fn pointer_down(&mut self, button: Button, at: Point, effects: &mut Vec<Effect>) {
        self.pointer = Some(at);
        if self.gesture != Gesture::Idle {
            return;
        }
        self.commit_note();
        if button == Button::Left {
            if self.done_button().is_some_and(|done| done.contains(at)) {
                self.finish_marking(effects);
                return;
            }
            if self.banner().is_some_and(|banner| banner.contains(at)) {
                return;
            }
            if self.near_divider(at) {
                self.gesture = Gesture::Divider { button };
                return;
            }
        }
        let Some(pane) = self.pane_at(at) else {
            return;
        };
        let draw = button == Button::Right || (button == Button::Left && self.mark_mode);
        if !draw {
            self.gesture = Gesture::Pan {
                button,
                image: pane.image,
                start: at,
                last: at,
                moved: false,
            };
        } else if self.frame(&pane).is_some() {
            self.gesture = Gesture::Draw {
                button,
                image: pane.image,
                start: at,
                current: at,
            };
        }
    }

    fn pointer_move(&mut self, at: Point) {
        self.pointer = Some(at);
        match self.gesture {
            Gesture::Idle => {}
            Gesture::Pan {
                button,
                image,
                start,
                last,
                moved,
            } => {
                let moved = moved || distance(start, at) >= self.drag_threshold();
                if moved
                    && let Some(frame) = self.pane_for(image).and_then(|pane| self.frame(&pane))
                {
                    let delta = Point::new(at.x - last.x, at.y - last.y);
                    self.view_mut(image).pan(frame, delta);
                }
                self.gesture = Gesture::Pan {
                    button,
                    image,
                    start,
                    last: if moved { at } else { last },
                    moved,
                };
            }
            Gesture::Draw {
                button,
                image,
                start,
                ..
            } => {
                self.gesture = Gesture::Draw {
                    button,
                    image,
                    start,
                    current: at,
                };
            }
            Gesture::Divider { .. } => {
                let area = self.area();
                if let Some(compare) = &mut self.compare
                    && area.width > 0.0
                {
                    compare.split = ((at.x - area.x) / area.width).clamp(0.05, 0.95);
                }
            }
        }
    }

    fn pointer_up(&mut self, button: Button, at: Point) {
        self.pointer = Some(at);
        let gesture = self.gesture;
        if gesture.button() != Some(button) {
            return;
        }
        self.gesture = Gesture::Idle;
        match gesture {
            Gesture::Pan {
                image,
                moved: false,
                ..
            } => self.click(image, at),
            Gesture::Draw { image, start, .. } => {
                if distance(start, at) < self.drag_threshold() {
                    self.click(image, at);
                    return;
                }
                let Some(frame) = self.pane_for(image).and_then(|pane| self.frame(&pane)) else {
                    return;
                };
                let view = self.view(image);
                let rect = rect_between(
                    view.to_image(frame, start),
                    view.to_image(frame, at),
                    frame.image,
                );
                if let Some(rect) = rect {
                    self.selected_mark = None;
                    self.note = Some(Note {
                        target: NoteTarget::New { image, rect },
                        text: String::new(),
                    });
                }
            }
            _ => {}
        }
    }

    fn double_click(&mut self, button: Button, at: Point) {
        if button != Button::Left {
            return;
        }
        let mark = self
            .pane_at(at)
            .and_then(|pane| self.mark_at(pane.image, at));
        if let Some(mark) = mark {
            self.gesture = Gesture::Idle;
            self.edit_mark(mark);
        }
    }

    fn click(&mut self, image: ImageId, at: Point) {
        self.selected_mark = self.mark_at(image, at);
    }

    fn mark_at(&self, image: ImageId, at: Point) -> Option<MarkId> {
        let frame = self.pane_for(image).and_then(|pane| self.frame(&pane))?;
        let view = self.view(image);
        let tolerance = self.drag_threshold() / view.zoom(frame);
        self.marks.hit(image, view.to_image(frame, at), tolerance)
    }

    fn drag_threshold(&self) -> f64 {
        DRAG_THRESHOLD * self.scale()
    }

    // Marking.

    fn edit_mark(&mut self, mark: MarkId) {
        if let Some(existing) = self.marks.get(mark) {
            self.selected_mark = Some(mark);
            self.note = Some(Note {
                target: NoteTarget::Existing(mark),
                text: existing.note.clone(),
            });
        }
    }

    fn commit_note(&mut self) {
        match self.note.take() {
            Some(Note {
                target: NoteTarget::New { image, rect },
                text,
            }) => {
                self.marks.add(image, rect, text);
            }
            Some(Note {
                target: NoteTarget::Existing(mark),
                text,
            }) => {
                self.marks.set_note(mark, text);
            }
            None => {}
        }
    }

    /// Answers every `marks --wait` with the marks, and clears them if any
    /// waiter asked to.
    fn finish_marking(&mut self, effects: &mut Vec<Effect>) {
        self.commit_note();
        if self.waiters.is_empty() {
            return;
        }
        let reply = Response::Ok(Reply::Marks {
            marks: self.mark_infos(),
        });
        let mut clear = false;
        for waiter in self.waiters.drain(..) {
            clear |= waiter.clear;
            effects.push(Effect::Reply {
                id: waiter.id,
                response: reply.clone(),
            });
        }
        if clear {
            self.marks.clear();
        }
    }

    fn expire_waiters(&mut self, effects: &mut Vec<Effect>) {
        let now = self.now;
        self.waiters.retain(|waiter| {
            let expired = waiter.deadline.is_some_and(|deadline| deadline <= now);
            if expired {
                effects.push(Effect::Reply {
                    id: waiter.id,
                    response: Response::Error(Error::new(
                        ErrorCode::Timeout,
                        format!(
                            "marking was not finished within {} seconds",
                            waiter.timeout_ms as f64 / 1000.0
                        ),
                    )),
                });
            }
            !expired
        });
    }

    fn mark_infos(&self) -> Vec<MarkInfo> {
        self.marks
            .iter()
            .filter_map(|mark| {
                Some(MarkInfo {
                    id: mark.id,
                    image: mark.image,
                    path: self.set.get(mark.image)?.path.clone(),
                    rect: mark.rect,
                    note: mark.note.clone(),
                })
            })
            .collect()
    }

    fn quit(&mut self, effects: &mut Vec<Effect>) {
        self.finish_marking(effects);
        effects.push(Effect::Quit);
    }

    // Requests from pipe clients.

    fn request(&mut self, id: RequestId, request: Request, effects: &mut Vec<Effect>) {
        let reply = match request {
            Request::Open {
                files,
                group,
                compare,
                activate,
            } => Ok(self.open(files, group, compare, activate, effects)),
            Request::List => Ok(Reply::List(self.list())),
            Request::Close { images } => {
                let mut ids = Vec::new();
                let mut not_found = Vec::new();
                for image in images {
                    match self.resolve(&image) {
                        Some(found) if !ids.contains(&found) => ids.push(found),
                        Some(_) => {}
                        None => not_found.push(image),
                    }
                }
                Ok(self.close(ids, not_found, effects))
            }
            Request::CloseAll => {
                let ids = self.set.iter().map(|entry| entry.id).collect();
                Ok(self.close(ids, Vec::new(), effects))
            }
            Request::Clear { group } => {
                let ids = self.set.group(&group);
                Ok(self.close(ids, Vec::new(), effects))
            }
            Request::Marks {
                wait,
                timeout_ms,
                clear,
            } => {
                if wait {
                    self.waiters.push(Waiter {
                        id,
                        deadline: timeout_ms.map(|ms| self.now + Duration::from_millis(ms)),
                        timeout_ms: timeout_ms.unwrap_or_default(),
                        clear,
                    });
                    return;
                }
                let marks = self.mark_infos();
                if clear {
                    self.marks.clear();
                }
                Ok(Reply::Marks { marks })
            }
            Request::Screenshot { path } => {
                effects.push(Effect::Screenshot { id, path });
                return;
            }
            Request::Quit => {
                effects.push(Effect::Reply {
                    id,
                    response: Response::Ok(Reply::Done),
                });
                self.quit(effects);
                return;
            }
            Request::DebugInput { events } => {
                if self.headless {
                    for event in events {
                        self.apply(event, effects);
                        self.tidy(effects);
                    }
                    Ok(Reply::Done)
                } else {
                    Err(Error::new(
                        ErrorCode::NotHeadless,
                        "debug input needs a viewer started with --headless",
                    ))
                }
            }
            Request::DebugTimings => {
                effects.push(Effect::ReportTimings { id });
                return;
            }
        };
        effects.push(Effect::Reply {
            id,
            response: reply.into(),
        });
    }

    fn open(
        &mut self,
        files: Vec<OpenFile>,
        group: Option<String>,
        compare: bool,
        activate: bool,
        effects: &mut Vec<Effect>,
    ) -> Reply {
        let mut images = Vec::new();
        let mut failed = Vec::new();
        for file in files {
            let canonical = match self.files.canonical(&file.path) {
                Ok(canonical) => canonical,
                Err(error) => {
                    failed.push(OpenFailure {
                        path: file.path,
                        error,
                    });
                    continue;
                }
            };
            let (id, existing) = self.set.add(
                file.path.clone(),
                PathKey::new(&canonical),
                file.caption,
                group.clone(),
            );
            let retry = existing && self.images[&id].state == LoadState::Failed;
            if !existing || retry {
                let target = if self.size.width == 0 || self.size.height == 0 {
                    DecodeTarget::Full
                } else {
                    DecodeTarget::Scaled {
                        at_least: self.size,
                    }
                };
                let state = ImageState::new(target == DecodeTarget::Full);
                if retry {
                    let old = self.images.get_mut(&id).expect("in the set");
                    old.state = state.state;
                    old.error = None;
                    old.full_requested = state.full_requested;
                } else {
                    self.images.insert(id, state);
                    effects.push(Effect::Watch {
                        image: id,
                        path: file.path.clone(),
                    });
                }
                effects.push(Effect::Decode {
                    image: id,
                    path: file.path.clone(),
                    target,
                });
            }
            images.push(OpenedImage {
                id,
                path: file.path,
                existing,
            });
        }
        if let Some(first) = images.first() {
            self.navigate(|set| {
                set.select(first.id);
            });
            if compare
                && group.is_some()
                && self.compare.as_ref().map(|compare| &compare.group) != group.as_ref()
            {
                self.start_compare();
            }
            effects.push(if activate {
                Effect::Activate
            } else {
                Effect::FlashTaskbar
            });
        }
        Reply::Opened(OpenReply { images, failed })
    }

    fn list(&self) -> ListReply {
        let images = self
            .set
            .iter()
            .map(|entry| {
                let state = &self.images[&entry.id];
                ImageInfo {
                    id: entry.id,
                    path: entry.path.clone(),
                    caption: entry.caption.clone(),
                    group: entry.group.clone(),
                    size: state.size,
                    state: state.state,
                    error: state.error.clone(),
                    missing: state.missing,
                }
            })
            .collect();
        let view = self.selected_pane().and_then(|pane| {
            let frame = self.frame(&pane)?;
            let view = self.view(pane.image);
            Some(ViewInfo {
                zoom: view.zoom(frame),
                fit: view.is_fit(),
                rotation: view.rotation(),
                visible: view.visible(frame),
            })
        });
        ListReply {
            images,
            selected: self.set.selected(),
            view,
            compare: self.compare.as_ref().map(|compare| compare.layout),
        }
    }

    fn resolve(&self, image: &ImageRef) -> Option<ImageId> {
        match image {
            ImageRef::Id(id) => self.set.get(*id).map(|entry| entry.id),
            ImageRef::Path(path) => {
                // A file deleted since it was opened can still be closed by
                // the path it was opened with.
                let canonical = self
                    .files
                    .canonical(path)
                    .unwrap_or_else(|_| normalise(path));
                self.set.find(&PathKey::new(&canonical))
            }
        }
    }

    fn close(
        &mut self,
        ids: Vec<ImageId>,
        not_found: Vec<ImageRef>,
        effects: &mut Vec<Effect>,
    ) -> Reply {
        for &id in &ids {
            self.remove(id, effects);
        }
        Reply::Closed {
            closed: ids,
            not_found,
        }
    }

    fn close_selected(&mut self, effects: &mut Vec<Effect>) {
        if let Some(id) = self.set.selected() {
            self.remove(id, effects);
        }
    }

    fn remove(&mut self, id: ImageId, effects: &mut Vec<Effect>) {
        if self.set.remove(id).is_some() {
            self.images.remove(&id);
            self.marks.remove_image(id);
            effects.push(Effect::CancelDecode { image: id });
            effects.push(Effect::Unwatch { image: id });
        }
    }

    fn decoded(&mut self, image: ImageId, target: DecodeTarget, size: PixelSize) {
        let Some(state) = self.images.get_mut(&image) else {
            return;
        };
        state.size = Some(size);
        state.error = None;
        match target {
            DecodeTarget::Full => state.state = LoadState::Loaded,
            DecodeTarget::Scaled { .. } | DecodeTarget::Fit { .. } => {
                if state.state != LoadState::Loaded {
                    state.state = LoadState::Preview;
                }
            }
            DecodeTarget::Thumbnail { .. } => {}
        }
    }

    // Keeping the state consistent and telling the window what changed.

    /// Drops state that refers to something no longer shown, and asks for the
    /// full decode of each shown image that has only a preview.
    fn tidy(&mut self, effects: &mut Vec<Effect>) {
        if let Some(compare) = &self.compare {
            let group = self.set.selected_entry().and_then(|e| e.group.as_ref());
            if group != Some(&compare.group) {
                self.compare = None;
            }
        }
        let panes = self.panes();
        let shown = |image: ImageId| panes.iter().any(|pane| pane.image == image);
        let note_image = self.note.as_ref().and_then(|note| match note.target {
            NoteTarget::New { image, .. } => Some(image),
            NoteTarget::Existing(mark) => self.marks.get(mark).map(|mark| mark.image),
        });
        if !note_image.is_some_and(shown) {
            self.note = None;
        }
        let mark_image = self
            .selected_mark
            .and_then(|mark| self.marks.get(mark))
            .map(|mark| mark.image);
        if !mark_image.is_some_and(shown) {
            self.selected_mark = None;
        }
        if self.gesture.image().is_some_and(|image| !shown(image)) {
            self.gesture = Gesture::Idle;
        }
        if let Gesture::Divider { .. } = self.gesture
            && !self.split_shown()
        {
            self.gesture = Gesture::Idle;
        }
        if self
            .zoom_changed
            .is_some_and(|changed| self.now - changed >= READOUT_HOLD + READOUT_FADE_OUT)
        {
            self.zoom_changed = None;
        }
        for pane in &panes {
            let state = self.images.get_mut(&pane.image).expect("in the set");
            if state.state == LoadState::Preview && !state.full_requested {
                state.full_requested = true;
                effects.push(Effect::Decode {
                    image: pane.image,
                    path: self.set.get(pane.image).expect("shown").path.clone(),
                    target: DecodeTarget::Full,
                });
            }
        }
    }

    fn settle(&mut self, before: &Scene, effects: &mut Vec<Effect>) {
        self.tidy(effects);
        if self.scene() != *before {
            effects.push(Effect::Redraw);
        }

        let title = self.title();
        if self.shown.title.as_ref() != Some(&title) {
            self.shown.title = Some(title.clone());
            effects.push(Effect::SetTitle { title });
        }

        let cursor = self.cursor();
        if self.shown.cursor != cursor {
            self.shown.cursor = cursor;
            effects.push(Effect::SetCursor { cursor });
        }

        let field = self.note_field();
        if field != self.shown.note_field {
            match (field, &self.note) {
                (Some(rect), Some(note)) => effects.push(Effect::ShowNoteField {
                    rect,
                    text: note.text.clone(),
                }),
                _ => effects.push(Effect::HideNoteField),
            }
            self.shown.note_field = field;
        }

        if let Some(wake) = self.next_wake()
            && self.shown.wake_at.is_none_or(|pending| wake < pending)
        {
            self.shown.wake_at = Some(wake);
            let nanos = wake.saturating_duration_since(self.now).as_nanos();
            effects.push(Effect::WakeAfter {
                ms: u64::try_from(nanos.div_ceil(1_000_000)).unwrap_or(u64::MAX),
            });
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        let readout = self.zoom_changed.map(|changed| {
            let age = self.now - changed;
            if age < READOUT_FADE_IN || age >= READOUT_HOLD {
                self.now + FRAME
            } else {
                changed + READOUT_HOLD
            }
        });
        let waiters = self.waiters.iter().filter_map(|waiter| waiter.deadline);
        readout.into_iter().chain(waiters).min()
    }

    /// The window title: the image's name, size and caption, or the group in
    /// compare view, after the waiting notice while an agent waits.
    pub fn title(&self) -> String {
        let mut parts = Vec::new();
        if let Some(compare) = &self.compare {
            parts.push(compare.group.clone());
            parts.push("compare".to_string());
        } else if let Some(entry) = self.set.selected_entry() {
            parts.push(file_name(&entry.path));
            if let Some(size) = self.images[&entry.id].size {
                parts.push(format!("{} × {}", size.width, size.height));
            }
            if let Some(caption) = &entry.caption {
                parts.push(caption.clone());
            }
        }
        parts.push("Seer".to_string());
        if self.waiting() {
            parts.insert(0, WAITING_TITLE.to_string());
        }
        parts.join(" — ")
    }

    fn cursor(&self) -> Cursor {
        match self.gesture {
            Gesture::Pan { moved: true, .. } => return Cursor::Move,
            Gesture::Divider { .. } => return Cursor::SizeWestEast,
            Gesture::Draw { .. } => return Cursor::Crosshair,
            _ => {}
        }
        if let Some(at) = self.pointer {
            if self.done_button().is_some_and(|done| done.contains(at)) {
                return Cursor::Hand;
            }
            if self.near_divider(at) {
                return Cursor::SizeWestEast;
            }
        }
        if self.mark_mode {
            Cursor::Crosshair
        } else {
            Cursor::Arrow
        }
    }

    /// Where the note field goes: beside its mark, inside the client area.
    fn note_field(&self) -> Option<Rect> {
        let note = self.note.as_ref()?;
        let (image, rect) = match note.target {
            NoteTarget::New { image, rect } => (image, rect),
            NoteTarget::Existing(mark) => {
                let mark = self.marks.get(mark)?;
                (mark.image, mark.rect)
            }
        };
        let frame = self.pane_for(image).and_then(|pane| self.frame(&pane))?;
        let mark = self.view(image).rect_to_client(frame, rect.into());
        let scale = self.scale();
        let (width, height, gap) = (240.0 * scale, 28.0 * scale, 8.0 * scale);
        let client = self.area();
        let mut x = mark.right() + gap;
        if x + width > client.right() {
            x = mark.x - gap - width;
        }
        let x = x.clamp(0.0, (client.width - width).max(0.0));
        let y = mark.y.clamp(0.0, (client.height - height).max(0.0));
        Some(Rect::new(x, y, width, height))
    }

    // Layout.

    fn scale(&self) -> f64 {
        if self.dpi == 0 {
            1.0
        } else {
            f64::from(self.dpi) / 96.0
        }
    }

    fn area(&self) -> Rect {
        Rect::new(
            0.0,
            0.0,
            f64::from(self.size.width),
            f64::from(self.size.height),
        )
    }

    fn panes(&self) -> Vec<Pane> {
        let area = self.area();
        let Some(selected) = self.set.selected() else {
            return Vec::new();
        };
        let Some(compare) = &self.compare else {
            return vec![Pane {
                image: selected,
                viewport: area,
                clip: area,
                label: None,
            }];
        };
        let members = self.set.group(&compare.group);
        let scale = self.scale();
        match compare.layout {
            CompareLayout::Tiles => {
                let count = members.len();
                let columns = if count <= 3 {
                    count
                } else {
                    count.isqrt() + usize::from(count.isqrt().pow(2) < count)
                };
                let rows = count.div_ceil(columns);
                let width = area.width / columns as f64;
                let height = area.height / rows as f64;
                let (gap, caption) = (4.0 * scale, 28.0 * scale);
                members
                    .iter()
                    .enumerate()
                    .map(|(index, &image)| {
                        let x = area.x + (index % columns) as f64 * width;
                        let y = area.y + (index / columns) as f64 * height;
                        let inner = (width - 2.0 * gap).max(0.0);
                        let viewport = Rect::new(
                            x + gap,
                            y + gap,
                            inner,
                            (height - 2.0 * gap - caption).max(0.0),
                        );
                        let label = Rect::new(x + gap, viewport.bottom(), inner, caption);
                        Pane {
                            image,
                            viewport,
                            clip: viewport,
                            label: Some((label, Align::Centre)),
                        }
                    })
                    .collect()
            }
            CompareLayout::Split => {
                let index = members.iter().position(|&id| id == selected).unwrap_or(0);
                let other = members[(index + 1) % members.len()];
                let divider = self.divider_x(compare);
                let left = Rect::new(area.x, area.y, divider - area.x, area.height);
                let right = Rect::new(divider, area.y, area.right() - divider, area.height);
                let inset = 8.0 * scale;
                let label = |side: Rect| {
                    Rect::new(
                        side.x + inset,
                        side.y + inset,
                        (side.width - 2.0 * inset).max(0.0),
                        24.0 * scale,
                    )
                };
                vec![
                    Pane {
                        image: selected,
                        viewport: area,
                        clip: left,
                        label: Some((label(left), Align::Start)),
                    },
                    Pane {
                        image: other,
                        viewport: area,
                        clip: right,
                        label: Some((label(right), Align::End)),
                    },
                ]
            }
        }
    }

    fn divider_x(&self, compare: &Compare) -> f64 {
        let area = self.area();
        area.x + area.width * compare.split
    }

    fn split_shown(&self) -> bool {
        self.compare
            .as_ref()
            .is_some_and(|compare| compare.layout == CompareLayout::Split)
    }

    fn near_divider(&self, at: Point) -> bool {
        match &self.compare {
            Some(compare) if compare.layout == CompareLayout::Split => {
                (at.x - self.divider_x(compare)).abs() <= 6.0 * self.scale()
                    && self.area().contains(at)
            }
            _ => false,
        }
    }

    fn pane_at(&self, at: Point) -> Option<Pane> {
        self.panes().into_iter().find(|pane| pane.clip.contains(at))
    }

    fn pane_for(&self, image: ImageId) -> Option<Pane> {
        self.panes().into_iter().find(|pane| pane.image == image)
    }

    fn selected_pane(&self) -> Option<Pane> {
        self.set.selected().and_then(|image| self.pane_for(image))
    }

    fn frame(&self, pane: &Pane) -> Option<Frame> {
        let size = self.images[&pane.image].size?;
        Some(Frame::new(size, pane.viewport))
    }

    /// The view an image is shown with: compare view's shared view, or its
    /// own.
    fn view(&self, image: ImageId) -> &View {
        match &self.compare {
            Some(compare) => &compare.view,
            None => &self.images[&image].view,
        }
    }

    fn view_mut(&mut self, image: ImageId) -> &mut View {
        match &mut self.compare {
            Some(compare) => &mut compare.view,
            None => &mut self.images.get_mut(&image).expect("in the set").view,
        }
    }

    fn banner(&self) -> Option<Rect> {
        self.waiting()
            .then(|| Rect::new(0.0, 0.0, f64::from(self.size.width), 40.0 * self.scale()))
    }

    fn done_button(&self) -> Option<Rect> {
        let banner = self.banner()?;
        let scale = self.scale();
        Some(Rect::new(
            banner.right() - 88.0 * scale,
            8.0 * scale,
            80.0 * scale,
            24.0 * scale,
        ))
    }

    fn readout_opacity(&self) -> Option<f32> {
        let age = self.now - self.zoom_changed?;
        let opacity = if age < READOUT_FADE_IN {
            age.as_secs_f64() / READOUT_FADE_IN.as_secs_f64()
        } else if age < READOUT_HOLD {
            1.0
        } else if age < READOUT_HOLD + READOUT_FADE_OUT {
            1.0 - (age - READOUT_HOLD).as_secs_f64() / READOUT_FADE_OUT.as_secs_f64()
        } else {
            return None;
        };
        Some(opacity as f32)
    }

    // The scene.

    /// What to draw for the current state, as of the last input.
    pub fn scene(&self) -> Scene {
        let scale = self.scale();
        let (background, text) = if self.dark {
            (DARK_BACKGROUND, DARK_TEXT)
        } else {
            (LIGHT_BACKGROUND, LIGHT_TEXT)
        };
        let mut items = Vec::new();
        let area = self.area();

        if self.set.is_empty() {
            items.push(Item::Text {
                text: "No images open".to_string(),
                rect: area,
                colour: text.with_alpha(0.6),
                size: 16.0 * scale,
                align: Align::Centre,
                bold: false,
            });
        }

        let tiles = self
            .compare
            .as_ref()
            .is_some_and(|compare| compare.layout == CompareLayout::Tiles);
        for pane in self.panes() {
            if tiles && Some(pane.image) == self.set.selected() {
                let tile = match pane.label {
                    Some((label, _)) => Rect::new(
                        pane.viewport.x,
                        pane.viewport.y,
                        pane.viewport.width,
                        label.bottom() - pane.viewport.y,
                    ),
                    None => pane.viewport,
                };
                items.push(Item::StrokeRect {
                    rect: tile,
                    colour: ACCENT,
                    width: 2.0 * scale,
                    dashed: false,
                });
            }
            self.draw_pane(&pane, text, background, &mut items);
        }

        if let Some(compare) = &self.compare
            && compare.layout == CompareLayout::Split
        {
            let x = self.divider_x(compare);
            items.push(Item::Line {
                from: Point::new(x, area.y),
                to: Point::new(x, area.bottom()),
                colour: WHITE,
                width: 2.0 * scale,
            });
        }

        if let (Some(opacity), Some(pane)) = (self.readout_opacity(), self.selected_pane())
            && let Some(frame) = self.frame(&pane)
        {
            let zoom = self.view(pane.image).zoom(frame);
            let (width, height) = (80.0 * scale, 28.0 * scale);
            let rect = Rect::new(
                pane.viewport.x + (pane.viewport.width - width) / 2.0,
                pane.viewport.bottom() - height - 16.0 * scale,
                width,
                height,
            );
            items.push(Item::FillRect {
                rect,
                colour: BLACK.with_alpha(0.6 * opacity),
            });
            items.push(Item::Text {
                text: format!("{:.0}%", zoom * 100.0),
                rect,
                colour: WHITE.with_alpha(opacity),
                size: 14.0 * scale,
                align: Align::Centre,
                bold: false,
            });
        }

        if let (Some(banner), Some(done)) = (self.banner(), self.done_button()) {
            items.push(Item::FillRect {
                rect: banner,
                colour: ACCENT,
            });
            items.push(Item::Text {
                text: WAITING_TITLE.to_string(),
                rect: Rect::new(
                    12.0 * scale,
                    0.0,
                    (done.x - 24.0 * scale).max(0.0),
                    banner.height,
                ),
                colour: WHITE,
                size: 14.0 * scale,
                align: Align::Start,
                bold: false,
            });
            items.push(Item::FillRect {
                rect: done,
                colour: WHITE,
            });
            items.push(Item::Text {
                text: "Done".to_string(),
                rect: done,
                colour: ACCENT,
                size: 14.0 * scale,
                align: Align::Centre,
                bold: true,
            });
        }

        Scene {
            size: self.size,
            background,
            items,
        }
    }

    fn draw_pane(&self, pane: &Pane, text: Colour, background: Colour, items: &mut Vec<Item>) {
        let scale = self.scale();
        let state = &self.images[&pane.image];
        if let Some(frame) = self.frame(pane) {
            let view = self.view(pane.image);
            let zoom = view.zoom(frame);
            items.push(Item::Image {
                image: pane.image,
                size: frame.image,
                transform: view.transform(frame),
                clip: pane.clip,
                interpolation: if zoom >= NEAREST_FROM {
                    Interpolation::Nearest
                } else {
                    Interpolation::Smooth
                },
                opacity: 1.0,
            });
            for mark in self.marks.for_image(pane.image) {
                let rect = view.rect_to_client(frame, mark.rect.into());
                let Some(rect) = intersect(rect, pane.clip) else {
                    continue;
                };
                let selected = self.selected_mark == Some(mark.id);
                let colour = if selected { SELECTED_MARK } else { MARK };
                items.push(Item::StrokeRect {
                    rect,
                    colour,
                    width: if selected { 3.0 } else { 2.0 } * scale,
                    dashed: false,
                });
                if !mark.note.is_empty() {
                    let label = Rect::new(
                        rect.x,
                        rect.y - 22.0 * scale,
                        rect.width.max(200.0 * scale),
                        20.0 * scale,
                    );
                    items.push(Item::FillRect {
                        rect: label,
                        colour: background.with_alpha(0.75),
                    });
                    items.push(Item::Text {
                        text: mark.note.clone(),
                        rect: label,
                        colour: text,
                        size: 12.0 * scale,
                        align: Align::Start,
                        bold: false,
                    });
                }
            }
            let pending = match (&self.note, self.gesture) {
                (
                    Some(Note {
                        target: NoteTarget::New { image, rect },
                        ..
                    }),
                    _,
                ) if *image == pane.image => Some(view.rect_to_client(frame, (*rect).into())),
                (
                    _,
                    Gesture::Draw {
                        image,
                        start,
                        current,
                        ..
                    },
                ) if image == pane.image => Some(between(start, current)),
                _ => None,
            };
            if let Some(rect) = pending.and_then(|rect| intersect(rect, pane.clip)) {
                items.push(Item::StrokeRect {
                    rect,
                    colour: MARK,
                    width: 2.0 * scale,
                    dashed: true,
                });
            }
        } else if let Some(error) = &state.error {
            items.push(Item::Text {
                text: error.message.clone(),
                rect: pane.viewport,
                colour: text,
                size: 14.0 * scale,
                align: Align::Centre,
                bold: false,
            });
        }
        if let Some((rect, align)) = pane.label
            && let Some(entry) = self.set.get(pane.image)
        {
            items.push(Item::Text {
                text: entry
                    .caption
                    .clone()
                    .unwrap_or_else(|| file_name(&entry.path)),
                rect,
                colour: text,
                size: 14.0 * scale,
                align,
                bold: false,
            });
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn middle(rect: Rect) -> Point {
    Point::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0)
}

fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn between(a: Point, b: Point) -> Rect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    Rect::new(left, top, a.x.max(b.x) - left, a.y.max(b.y) - top)
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
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

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::model::geometry::Rotation;

    /// Recognises paths without touching the disk; any path containing
    /// "missing" does not exist.
    struct FakeFiles;

    impl Files for FakeFiles {
        fn canonical(&self, path: &Path) -> crate::Result<PathBuf> {
            if path.to_string_lossy().contains("missing") {
                Err(Error::new(ErrorCode::FileNotFound, "no such file"))
            } else {
                Ok(normalise(path))
            }
        }
    }

    const WINDOW: PixelSize = PixelSize::new(800, 600);

    struct Session {
        app: App,
        now: Instant,
        next_request: u64,
    }

    impl Session {
        fn new() -> Self {
            Session::with(Config {
                headless: true,
                size: WINDOW,
                dpi: 96,
                dark: true,
            })
        }

        fn with(config: Config) -> Self {
            let now = Instant::now();
            Session {
                app: App::new(config, Box::new(FakeFiles), now),
                now,
                next_request: 1,
            }
        }

        fn send(&mut self, input: Input) -> Vec<Effect> {
            self.app.handle(input, self.now)
        }

        fn wait(&mut self, ms: u64) {
            self.now += Duration::from_millis(ms);
        }

        fn request(&mut self, request: Request) -> (RequestId, Vec<Effect>) {
            let id = RequestId(self.next_request);
            self.next_request += 1;
            (id, self.send(Input::Request { id, request }))
        }

        fn ask(&mut self, request: Request) -> crate::Result<Reply> {
            let (id, effects) = self.request(request);
            reply(&effects, id).expect("a reply")
        }

        fn open_with(
            &mut self,
            paths: &[&str],
            group: Option<&str>,
            compare: bool,
        ) -> (OpenReply, Vec<Effect>) {
            let (id, effects) = self.request(Request::Open {
                files: paths
                    .iter()
                    .map(|path| OpenFile {
                        path: PathBuf::from(path),
                        caption: None,
                    })
                    .collect(),
                group: group.map(str::to_string),
                compare,
                activate: false,
            });
            match reply(&effects, id) {
                Some(Ok(Reply::Opened(opened))) => (opened, effects),
                other => panic!("{other:?}"),
            }
        }

        fn open(&mut self, paths: &[&str]) -> OpenReply {
            self.open_with(paths, None, false).0
        }

        fn decode(&mut self, image: u32, width: u32, height: u32) -> Vec<Effect> {
            self.send(Input::Decoded {
                image: ImageId(image),
                target: DecodeTarget::Full,
                size: PixelSize::new(width, height),
            })
        }

        fn list(&mut self) -> ListReply {
            match self.ask(Request::List) {
                Ok(Reply::List(list)) => list,
                other => panic!("{other:?}"),
            }
        }

        fn marks(&mut self) -> Vec<MarkInfo> {
            match self.ask(Request::Marks {
                wait: false,
                timeout_ms: None,
                clear: false,
            }) {
                Ok(Reply::Marks { marks }) => marks,
                other => panic!("{other:?}"),
            }
        }

        fn key(&mut self, key: Key) -> Vec<Effect> {
            self.key_with(key, Modifiers::NONE)
        }

        fn key_with(&mut self, key: Key, modifiers: Modifiers) -> Vec<Effect> {
            self.send(Input::Key { key, modifiers })
        }

        fn down(&mut self, button: Button, x: f64, y: f64) -> Vec<Effect> {
            self.send(Input::PointerDown {
                button,
                at: Point::new(x, y),
                modifiers: Modifiers::NONE,
            })
        }

        fn move_to(&mut self, x: f64, y: f64) -> Vec<Effect> {
            self.send(Input::PointerMove {
                at: Point::new(x, y),
                modifiers: Modifiers::NONE,
            })
        }

        fn up(&mut self, button: Button, x: f64, y: f64) -> Vec<Effect> {
            self.send(Input::PointerUp {
                button,
                at: Point::new(x, y),
                modifiers: Modifiers::NONE,
            })
        }

        fn drag(&mut self, button: Button, from: (f64, f64), to: (f64, f64)) -> Vec<Effect> {
            self.down(button, from.0, from.1);
            self.move_to(to.0, to.1);
            self.up(button, to.0, to.1)
        }

        fn mark(&mut self, from: (f64, f64), to: (f64, f64), note: &str) {
            self.drag(Button::Right, from, to);
            self.send(Input::NoteText { text: note.into() });
            self.key(Key::Enter);
        }

        fn selected(&mut self) -> Option<u32> {
            self.list().selected.map(|id| id.0)
        }

        fn view(&mut self) -> ViewInfo {
            self.list().view.expect("a view")
        }
    }

    fn reply(effects: &[Effect], id: RequestId) -> Option<crate::Result<Reply>> {
        effects.iter().find_map(|effect| match effect {
            Effect::Reply {
                id: replied,
                response,
            } if *replied == id => Some(response.clone().into()),
            _ => None,
        })
    }

    fn title(effects: &[Effect]) -> Option<&str> {
        effects.iter().find_map(|effect| match effect {
            Effect::SetTitle { title } => Some(title.as_str()),
            _ => None,
        })
    }

    fn texts(scene: &Scene) -> Vec<String> {
        scene
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn strokes(scene: &Scene) -> Vec<(Rect, bool)> {
        scene
            .items
            .iter()
            .filter_map(|item| match item {
                Item::StrokeRect { rect, dashed, .. } => Some((*rect, *dashed)),
                _ => None,
            })
            .collect()
    }

    fn images(scene: &Scene) -> Vec<(u32, Rect)> {
        scene
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Image { image, clip, .. } => Some((image.0, *clip)),
                _ => None,
            })
            .collect()
    }

    fn three_images() -> Session {
        let mut session = Session::new();
        session.open(&[
            r"C:\shots\one.png",
            r"C:\shots\two.png",
            r"C:\shots\three.png",
        ]);
        session.decode(1, 1600, 1200);
        session.decode(2, 400, 300);
        session.decode(3, 1600, 1200);
        session
    }

    #[test]
    fn a_whole_session() {
        let mut session = Session::new();
        let effects = session.send(Input::Resize { size: WINDOW });
        assert_eq!(title(&effects), Some("Seer"));
        assert_eq!(texts(&session.app.scene()), ["No images open"]);

        // Open three images.
        let opened = session.open(&[
            r"C:\shots\one.png",
            r"C:\shots\two.png",
            r"C:\shots\three.png",
        ]);
        let ids: Vec<u32> = opened.images.iter().map(|image| image.id.0).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert!(opened.failed.is_empty());
        for (id, (width, height)) in [(1, (1600, 1200)), (2, (400, 300)), (3, (1600, 1200))] {
            session.decode(id, width, height);
        }
        let list = session.list();
        assert_eq!(list.images.len(), 3);
        assert!(
            list.images
                .iter()
                .all(|image| image.state == LoadState::Loaded)
        );
        assert_eq!(list.selected, Some(ImageId(1)));
        assert_eq!(
            images(&session.app.scene()),
            [(1, Rect::new(0.0, 0.0, 800.0, 600.0))]
        );

        // Navigate.
        let effects = session.key(Key::Right);
        assert!(effects.contains(&Effect::Redraw));
        assert_eq!(title(&effects), Some("two.png — 400 × 300 — Seer"));
        assert_eq!(session.selected(), Some(2));
        session.key(Key::End);
        assert_eq!(session.selected(), Some(3));
        session.key(Key::Right);
        assert_eq!(session.selected(), Some(3));
        session.key(Key::Home);
        assert_eq!(session.selected(), Some(1));

        // Zoom: fitted at 50%, then 100%, then in a step about the pointer.
        let view = session.view();
        assert!(view.fit);
        assert_eq!(view.zoom, 0.5);
        assert_eq!(view.visible, Rect::new(0.0, 0.0, 1600.0, 1200.0));
        session.key(Key::Char('1'));
        let view = session.view();
        assert_eq!((view.zoom, view.fit), (1.0, false));
        assert_eq!(view.visible, Rect::new(400.0, 300.0, 800.0, 600.0));
        session.send(Input::Wheel {
            at: Point::new(400.0, 300.0),
            delta: 1.0,
            modifiers: Modifiers::CTRL,
        });
        assert_eq!(session.view().zoom, ZOOM_STEP);
        session.key(Key::Char('0'));
        assert!(session.view().fit);

        // Mark: drag, type a note, keep it.
        session.mark((100.0, 100.0), (300.0, 200.0), "the wrong title bar");
        let marks = session.marks();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].image, ImageId(1));
        assert_eq!(marks[0].path, PathBuf::from(r"C:\shots\one.png"));
        assert_eq!(marks[0].rect, PixelRect::new(200, 200, 400, 200));
        assert_eq!(marks[0].note, "the wrong title bar");

        // Close: the selected image with Ctrl+W, another by request.
        let effects = session.key_with(Key::Char('w'), Modifiers::CTRL);
        assert!(effects.contains(&Effect::CancelDecode { image: ImageId(1) }));
        assert!(effects.contains(&Effect::Unwatch { image: ImageId(1) }));
        assert_eq!(session.selected(), Some(2));
        assert!(session.marks().is_empty());
        let closed = session.ask(Request::Close {
            images: vec![ImageRef::Id(ImageId(3))],
        });
        assert_eq!(
            closed.unwrap(),
            Reply::Closed {
                closed: vec![ImageId(3)],
                not_found: vec![]
            }
        );
        session.key(Key::Delete);
        assert!(session.list().images.is_empty());
        assert_eq!(session.selected(), None);
        assert_eq!(texts(&session.app.scene()), ["No images open"]);

        // Close the window.
        let effects = session.send(Input::CloseWindow);
        assert_eq!(effects.last(), Some(&Effect::Quit));
    }

    #[test]
    fn opening_watches_decodes_and_flashes() {
        let mut session = Session::new();
        let (opened, effects) = session.open_with(&[r"C:\a.png"], None, false);
        let path = PathBuf::from(r"C:\a.png");
        assert!(!opened.images[0].existing);
        assert!(effects.contains(&Effect::Watch {
            image: ImageId(1),
            path: path.clone()
        }));
        assert!(effects.contains(&Effect::Decode {
            image: ImageId(1),
            path: path.clone(),
            target: DecodeTarget::Scaled { at_least: WINDOW },
        }));
        assert!(effects.contains(&Effect::FlashTaskbar));
        assert_eq!(session.list().images[0].state, LoadState::Loading);
        assert!(session.list().view.is_none());

        let (id, effects) = session.request(Request::Open {
            files: vec![OpenFile {
                path: r"C:\b.png".into(),
                caption: Some("after".into()),
            }],
            group: None,
            compare: false,
            activate: true,
        });
        assert!(effects.contains(&Effect::Activate));
        assert!(reply(&effects, id).unwrap().is_ok());
        assert_eq!(session.list().images[1].caption.as_deref(), Some("after"));
    }

    #[test]
    fn opening_an_image_again_selects_it() {
        let mut session = three_images();
        let (opened, effects) = session.open_with(
            &[r"c:\SHOTS\other\..\Two.PNG", r"C:\shots\missing.png"],
            None,
            false,
        );
        assert_eq!(opened.images.len(), 1);
        assert_eq!(opened.images[0].id, ImageId(2));
        assert!(opened.images[0].existing);
        assert_eq!(
            opened.failed[0].path,
            PathBuf::from(r"C:\shots\missing.png")
        );
        assert_eq!(opened.failed[0].error.code, ErrorCode::FileNotFound);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Decode { .. }))
        );
        assert_eq!(session.selected(), Some(2));
        assert_eq!(session.list().images.len(), 3);

        // Nothing opened: no focus change.
        let (_, effects) = session.open_with(&[r"C:\missing.png"], None, false);
        assert!(!effects.contains(&Effect::FlashTaskbar));
    }

    #[test]
    fn a_failed_image_is_retried_when_opened_again() {
        let mut session = Session::new();
        session.open(&[r"C:\bad.png"]);
        session.send(Input::DecodeFailed {
            image: ImageId(1),
            error: Error::new(ErrorCode::DecodeFailed, "not an image"),
        });
        let list = session.list();
        assert_eq!(list.images[0].state, LoadState::Failed);
        assert_eq!(
            list.images[0].error.as_ref().unwrap().message,
            "not an image"
        );
        assert!(texts(&session.app.scene()).contains(&"not an image".to_string()));

        let (_, effects) = session.open_with(&[r"C:\bad.png"], None, false);
        assert!(effects.contains(&Effect::Decode {
            image: ImageId(1),
            path: r"C:\bad.png".into(),
            target: DecodeTarget::Scaled { at_least: WINDOW },
        }));
        let list = session.list();
        assert_eq!(list.images[0].state, LoadState::Loading);
        assert!(list.images[0].error.is_none());
    }

    #[test]
    fn previews_are_followed_by_full_decodes_of_shown_images() {
        let mut session = Session::new();
        session.open(&[r"C:\a.png", r"C:\b.png"]);
        let scaled = DecodeTarget::Scaled { at_least: WINDOW };
        let full = |image| Effect::Decode {
            image: ImageId(image),
            path: if image == 1 { r"C:\a.png" } else { r"C:\b.png" }.into(),
            target: DecodeTarget::Full,
        };
        let effects = session.send(Input::Decoded {
            image: ImageId(1),
            target: scaled,
            size: PixelSize::new(4000, 3000),
        });
        assert!(effects.contains(&full(1)));
        assert!(effects.contains(&Effect::Redraw));
        assert_eq!(session.list().images[0].state, LoadState::Preview);
        // The second is not shown, so it waits for its full decode.
        let effects = session.send(Input::Decoded {
            image: ImageId(2),
            target: scaled,
            size: PixelSize::new(10, 10),
        });
        assert!(!effects.contains(&full(2)));
        let effects = session.key(Key::Right);
        assert!(effects.contains(&full(2)));
        assert!(!session.key(Key::Left).contains(&full(1)));

        session.decode(1, 4000, 3000);
        assert_eq!(session.list().images[0].state, LoadState::Loaded);
        // A late preview does not undo a full decode, and a thumbnail changes
        // nothing.
        session.send(Input::Decoded {
            image: ImageId(1),
            target: scaled,
            size: PixelSize::new(4000, 3000),
        });
        session.send(Input::Decoded {
            image: ImageId(1),
            target: DecodeTarget::Thumbnail {
                at_most: PixelSize::new(64, 64),
            },
            size: PixelSize::new(4000, 3000),
        });
        assert_eq!(session.list().images[0].state, LoadState::Loaded);
        // Results for closed images are ignored.
        session.decode(9, 1, 1);
        session.send(Input::DecodeFailed {
            image: ImageId(9),
            error: Error::unexpected("gone"),
        });
        session.send(Input::FileChanged { image: ImageId(9) });
        session.send(Input::FileMissing { image: ImageId(9) });
    }

    #[test]
    fn an_empty_window_decodes_in_full() {
        let mut session = Session::with(Config {
            headless: false,
            size: PixelSize::new(0, 0),
            dpi: 0,
            dark: false,
        });
        let (_, effects) = session.open_with(&[r"C:\a.png"], None, false);
        assert!(effects.contains(&Effect::Decode {
            image: ImageId(1),
            path: r"C:\a.png".into(),
            target: DecodeTarget::Full,
        }));
        let effects = session.send(Input::Decoded {
            image: ImageId(1),
            target: DecodeTarget::Scaled {
                at_least: PixelSize::new(1, 1),
            },
            size: PixelSize::new(10, 10),
        });
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Decode { .. }))
        );
        assert_eq!(session.app.scene().background, LIGHT_BACKGROUND);
    }

    #[test]
    fn file_changes_reload_and_report_missing_files() {
        let mut session = three_images();
        session.send(Input::FileMissing { image: ImageId(2) });
        assert!(session.list().images[1].missing);
        let effects = session.send(Input::FileChanged { image: ImageId(2) });
        assert!(effects.contains(&Effect::Decode {
            image: ImageId(2),
            path: r"C:\shots\two.png".into(),
            target: DecodeTarget::Full,
        }));
        assert!(!session.list().images[1].missing);
    }

    #[test]
    fn the_title_shows_name_size_and_caption_and_changes_only_when_needed() {
        let mut session = Session::new();
        session.request(Request::Open {
            files: vec![OpenFile {
                path: r"C:\shots\after.png".into(),
                caption: Some("after the fix".into()),
            }],
            group: None,
            compare: false,
            activate: false,
        });
        let effects = session.decode(1, 640, 480);
        assert_eq!(
            title(&effects),
            Some("after.png — 640 × 480 — after the fix — Seer")
        );
        assert_eq!(
            session.app.title(),
            "after.png — 640 × 480 — after the fix — Seer"
        );
        assert_eq!(title(&session.key(Key::Right)), None);
    }

    #[test]
    fn closing_by_path_id_group_and_all() {
        let mut session = Session::new();
        session.open_with(&[r"C:\a.png", r"C:\b.png"], Some("pair"), false);
        session.open(&[r"C:\c.png", r"C:\d.png"]);
        let closed = session.ask(Request::Close {
            images: vec![
                ImageRef::Path(r"c:\A.PNG".into()),
                ImageRef::Id(ImageId(1)),
                ImageRef::Id(ImageId(42)),
                ImageRef::Path(r"C:\missing.png".into()),
            ],
        });
        assert_eq!(
            closed.unwrap(),
            Reply::Closed {
                closed: vec![ImageId(1)],
                not_found: vec![
                    ImageRef::Id(ImageId(42)),
                    ImageRef::Path(r"C:\missing.png".into())
                ],
            }
        );
        let cleared = session.ask(Request::Clear {
            group: "pair".into(),
        });
        assert_eq!(
            cleared.unwrap(),
            Reply::Closed {
                closed: vec![ImageId(2)],
                not_found: vec![]
            }
        );
        let all = session.ask(Request::CloseAll);
        assert_eq!(
            all.unwrap(),
            Reply::Closed {
                closed: vec![ImageId(3), ImageId(4)],
                not_found: vec![]
            }
        );
        // Nothing to close.
        session.key(Key::Delete);
        session.key_with(Key::Char('w'), Modifiers::CTRL);
        assert!(session.list().images.is_empty());
    }

    #[test]
    fn a_mark_is_made_in_image_pixels_at_any_zoom_and_rotation() {
        let mut session = three_images();
        // Fitted at 50%.
        let effects = session.drag(Button::Right, (300.0, 200.0), (100.0, 100.0));
        assert!(effects.contains(&Effect::ShowNoteField {
            rect: Rect::new(308.0, 100.0, 240.0, 28.0),
            text: String::new(),
        }));
        assert!(
            strokes(&session.app.scene()).contains(&(Rect::new(100.0, 100.0, 200.0, 100.0), true))
        );
        session.send(Input::NoteText { text: "a".into() });
        let effects = session.key(Key::Enter);
        assert!(effects.contains(&Effect::HideNoteField));
        assert!(
            strokes(&session.app.scene()).contains(&(Rect::new(100.0, 100.0, 200.0, 100.0), false))
        );
        assert!(texts(&session.app.scene()).contains(&"a".to_string()));

        // At 100%, the window's top-left is the image's (400, 300).
        session.key(Key::Char('1'));
        session.mark((0.0, 0.0), (100.0, 50.0), "b");
        // Turned clockwise, a drag right and down runs down and left in the
        // image.
        session.key(Key::Char('r'));
        assert_eq!(session.view().rotation, Rotation::Clockwise90);
        session.mark((400.0, 300.0), (500.0, 350.0), "c");
        let rects: Vec<PixelRect> = session.marks().iter().map(|mark| mark.rect).collect();
        assert_eq!(
            rects,
            [
                PixelRect::new(200, 200, 400, 200),
                PixelRect::new(400, 300, 100, 50),
                PixelRect::new(800, 500, 50, 100),
            ]
        );
        session.key_with(Key::Char('r'), Modifiers::SHIFT);
        assert_eq!(session.view().rotation, Rotation::None);
    }

    #[test]
    fn the_note_field_moves_with_the_view_and_stays_in_the_window() {
        let mut session = three_images();
        let effects = session.drag(Button::Right, (700.0, 500.0), (790.0, 590.0));
        assert!(effects.contains(&Effect::ShowNoteField {
            rect: Rect::new(452.0, 500.0, 240.0, 28.0),
            text: String::new(),
        }));
        session.send(Input::NoteText { text: "x".into() });
        let effects = session.send(Input::Wheel {
            at: Point::new(400.0, 300.0),
            delta: 1.0,
            modifiers: Modifiers::NONE,
        });
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::ShowNoteField { text, .. } if text == "x"))
        );
        // Keys other than Enter and Escape go to the field, not the viewer.
        session.key(Key::Right);
        assert_eq!(session.selected(), Some(1));
        // Escape discards the new mark.
        let effects = session.key(Key::Escape);
        assert!(effects.contains(&Effect::HideNoteField));
        assert!(session.marks().is_empty());
    }

    #[test]
    fn a_short_or_empty_drag_makes_no_mark() {
        let mut session = three_images();
        session.drag(Button::Right, (100.0, 100.0), (101.0, 101.0));
        assert!(session.app.note.is_none());
        // Entirely beside the image (fitted 400 × 300 is centred).
        session.key(Key::Right);
        session.drag(Button::Right, (10.0, 10.0), (100.0, 100.0));
        assert!(session.app.note.is_none());
        // On an image that has not been decoded, nothing is drawn.
        session.open(&[r"C:\new.png"]);
        session.drag(Button::Right, (10.0, 10.0), (100.0, 100.0));
        assert!(session.app.note.is_none());
        assert!(session.marks().is_empty());
    }

    #[test]
    fn mark_mode_draws_with_the_left_button() {
        let mut session = three_images();
        let effects = session.key(Key::Char('m'));
        assert!(effects.contains(&Effect::SetCursor {
            cursor: Cursor::Crosshair
        }));
        assert!(session.app.mark_mode());
        session.mark((100.0, 100.0), (300.0, 200.0), "left");
        assert_eq!(session.marks().len(), 1);
        // Escape abandons a drag in progress, then leaves mark mode.
        session.down(Button::Left, 10.0, 10.0);
        session.move_to(50.0, 50.0);
        session.key(Key::Escape);
        session.up(Button::Left, 50.0, 50.0);
        assert!(session.app.mark_mode());
        let effects = session.key(Key::Escape);
        assert!(effects.contains(&Effect::SetCursor {
            cursor: Cursor::Arrow
        }));
        assert!(!session.app.mark_mode());
        // M also leaves it, abandoning a drag.
        session.key(Key::Char('m'));
        session.down(Button::Left, 10.0, 10.0);
        session.key(Key::Char('m'));
        assert_eq!(session.app.gesture, Gesture::Idle);
    }

    #[test]
    fn marks_are_selected_deleted_and_their_notes_edited() {
        let mut session = three_images();
        session.mark((100.0, 100.0), (300.0, 200.0), "first");
        session.mark((400.0, 300.0), (500.0, 400.0), "second");
        // A click selects the mark under it.
        session.down(Button::Left, 150.0, 150.0);
        session.up(Button::Left, 150.0, 150.0);
        assert_eq!(session.app.selected_mark, Some(MarkId(1)));
        assert!(
            strokes(&session.app.scene())
                .iter()
                .any(|(rect, _)| *rect == Rect::new(100.0, 100.0, 200.0, 100.0))
        );
        // F2 reopens its note; Escape leaves it as it was.
        let effects = session.key(Key::Function(2));
        assert!(effects.contains(&Effect::ShowNoteField {
            rect: Rect::new(308.0, 100.0, 240.0, 28.0),
            text: "first".into(),
        }));
        session.send(Input::NoteText {
            text: "changed".into(),
        });
        session.key(Key::Escape);
        assert_eq!(session.marks()[0].note, "first");
        // A double-click reopens it; Enter keeps the change.
        session.down(Button::Left, 450.0, 350.0);
        session.send(Input::DoubleClick {
            button: Button::Left,
            at: Point::new(450.0, 350.0),
            modifiers: Modifiers::NONE,
        });
        session.up(Button::Left, 450.0, 350.0);
        session.send(Input::NoteText {
            text: "edited".into(),
        });
        session.key(Key::Enter);
        assert_eq!(session.marks()[1].note, "edited");
        assert_eq!(session.app.selected_mark, Some(MarkId(2)));
        // Delete removes the selected mark, not the image.
        session.key(Key::Delete);
        assert_eq!(session.marks().len(), 1);
        assert_eq!(session.list().images.len(), 3);
        // A right click away from the marks selects nothing; Escape clears a
        // selection.
        session.drag(Button::Right, (150.0, 150.0), (150.0, 150.0));
        assert_eq!(session.app.selected_mark, Some(MarkId(1)));
        session.key(Key::Escape);
        assert_eq!(session.app.selected_mark, None);
        // Double-clicks elsewhere, or with another button, do nothing.
        session.send(Input::DoubleClick {
            button: Button::Left,
            at: Point::new(700.0, 50.0),
            modifiers: Modifiers::NONE,
        });
        session.send(Input::DoubleClick {
            button: Button::Right,
            at: Point::new(150.0, 150.0),
            modifiers: Modifiers::NONE,
        });
        assert!(session.app.note.is_none());
        // F2 without a selected mark does nothing.
        session.key(Key::Function(2));
        assert!(session.app.note.is_none());
        // Moving to another image drops the selection.
        session.down(Button::Left, 150.0, 150.0);
        session.up(Button::Left, 150.0, 150.0);
        session.key(Key::Right);
        assert_eq!(session.app.selected_mark, None);
    }

    #[test]
    fn pressing_elsewhere_keeps_an_open_note() {
        let mut session = three_images();
        session.drag(Button::Right, (100.0, 100.0), (300.0, 200.0));
        session.send(Input::NoteText {
            text: "kept".into(),
        });
        session.down(Button::Left, 700.0, 500.0);
        session.up(Button::Left, 700.0, 500.0);
        assert_eq!(session.marks()[0].note, "kept");
    }

    #[test]
    fn dragging_pans_and_a_second_button_is_ignored() {
        let mut session = three_images();
        session.key(Key::Char('1'));
        session.down(Button::Left, 400.0, 300.0);
        // A small movement is not yet a drag.
        assert!(session.move_to(402.0, 300.0).is_empty());
        assert_eq!(session.view().visible.x, 400.0);
        let effects = session.move_to(450.0, 350.0);
        assert!(effects.contains(&Effect::SetCursor {
            cursor: Cursor::Move
        }));
        assert!(effects.contains(&Effect::Redraw));
        session.down(Button::Right, 450.0, 350.0);
        session.up(Button::Right, 450.0, 350.0);
        session.up(Button::Left, 450.0, 350.0);
        assert_eq!(
            session.view().visible,
            Rect::new(350.0, 250.0, 800.0, 600.0)
        );
        assert!(session.app.note.is_none());
        // The middle button pans too, and a press outside any image does
        // nothing.
        session.drag(Button::Middle, (450.0, 350.0), (400.0, 300.0));
        assert_eq!(
            session.view().visible,
            Rect::new(400.0, 300.0, 800.0, 600.0)
        );
        session.down(Button::Left, 900.0, 900.0);
        assert_eq!(session.app.gesture, Gesture::Idle);
        // Closing the image being dragged ends the drag.
        session.down(Button::Left, 400.0, 300.0);
        let closed = session.ask(Request::Close {
            images: vec![ImageRef::Id(ImageId(1))],
        });
        assert!(closed.is_ok());
        assert_eq!(session.app.gesture, Gesture::Idle);
        assert_eq!(session.app.set().len(), 2);
        assert!(session.app.marks().is_empty());
    }

    #[test]
    fn zoom_keys_and_pinch() {
        let mut session = three_images();
        session.key(Key::Plus);
        assert_eq!(session.view().zoom, 0.5 * ZOOM_STEP);
        session.key_with(Key::Minus, Modifiers::CTRL);
        assert!(session.view().fit);
        session.key_with(Key::Char('1'), Modifiers::CTRL);
        assert_eq!(session.view().zoom, 1.0);
        session.key_with(Key::Char('0'), Modifiers::CTRL);
        assert!(session.view().fit);
        session.send(Input::Pinch {
            at: Point::new(0.0, 0.0),
            scale: 2.0,
        });
        assert_eq!(session.view().zoom, 1.0);
        assert_eq!(session.view().visible.x, 0.0);
        // Nonsense factors are ignored.
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            session.send(Input::Pinch {
                at: Point::new(0.0, 0.0),
                scale,
            });
        }
        assert_eq!(session.view().zoom, 1.0);
        // Alt combinations and unbound keys do nothing.
        session.key_with(
            Key::Plus,
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        );
        session.key_with(Key::Char('q'), Modifiers::CTRL);
        session.key(Key::Char('q'));
        session.key(Key::Enter);
        assert_eq!(session.view().zoom, 1.0);
        // Far in, pixels are drawn as squares.
        session.send(Input::Wheel {
            at: Point::new(400.0, 300.0),
            delta: 20.0,
            modifiers: Modifiers::NONE,
        });
        assert!(session.app.scene().items.iter().any(|item| matches!(
            item,
            Item::Image {
                interpolation: Interpolation::Nearest,
                ..
            }
        )));
    }

    #[test]
    fn the_zoom_readout_fades_in_and_out() {
        let mut session = three_images();
        let readout = |session: &Session| {
            session
                .app
                .scene()
                .items
                .iter()
                .find_map(|item| match item {
                    Item::Text { text, colour, .. } if text.ends_with('%') => {
                        Some((text.clone(), colour.a))
                    }
                    _ => None,
                })
        };
        assert_eq!(readout(&session), None);
        let effects = session.key(Key::Char('1'));
        assert!(effects.contains(&Effect::WakeAfter { ms: 16 }));
        assert_eq!(readout(&session), Some(("100%".to_string(), 0.0)));

        session.wait(500);
        let effects = session.send(Input::Tick);
        assert!(effects.contains(&Effect::Redraw));
        assert!(effects.contains(&Effect::WakeAfter { ms: 500 }));
        assert_eq!(readout(&session), Some(("100%".to_string(), 1.0)));
        // Nothing needs a wake-up sooner than the one pending.
        session.wait(100);
        let effects = session.send(Input::Theme { dark: false });
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::WakeAfter { .. }))
        );
        // A second zoom starts the readout again.
        let effects = session.key(Key::Plus);
        assert!(effects.contains(&Effect::WakeAfter { ms: 16 }));
        assert_eq!(readout(&session), Some(("125%".to_string(), 0.0)));

        session.wait(1150);
        let effects = session.send(Input::Tick);
        assert!(effects.contains(&Effect::Redraw));
        assert!(effects.contains(&Effect::WakeAfter { ms: 16 }));
        let (_, alpha) = readout(&session).unwrap();
        assert!((alpha - 0.5).abs() < 1e-3, "{alpha}");

        session.wait(200);
        let effects = session.send(Input::Tick);
        assert!(effects.contains(&Effect::Redraw));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::WakeAfter { .. }))
        );
        assert_eq!(readout(&session), None);
        assert!(session.app.zoom_changed.is_none());

        // Fitting an image already fitted does not show the readout again.
        session.key(Key::Char('0'));
        let changed = session.app.zoom_changed;
        assert!(changed.is_some());
        session.wait(10);
        session.key(Key::Char('0'));
        assert_eq!(session.app.zoom_changed, changed);
    }

    #[test]
    fn waiting_for_marks_until_ctrl_enter() {
        let mut session = three_images();
        let (waiter, effects) = session.request(Request::Marks {
            wait: true,
            timeout_ms: None,
            clear: true,
        });
        assert!(reply(&effects, waiter).is_none());
        assert!(session.app.waiting());
        assert_eq!(
            title(&effects),
            Some(
                "Agent waiting for your marks (Ctrl+Enter when done) — one.png — 1600 × 1200 — Seer"
            )
        );
        assert!(texts(&session.app.scene()).contains(&WAITING_TITLE.to_string()));
        // A second waiter that goes away is forgotten.
        let (gone, _) = session.request(Request::Marks {
            wait: true,
            timeout_ms: None,
            clear: false,
        });
        session.send(Input::Disconnected { id: gone });

        session.mark((100.0, 100.0), (300.0, 200.0), "here");
        // Ctrl+Enter in the note field keeps the note and finishes.
        session.drag(Button::Right, (400.0, 300.0), (500.0, 400.0));
        session.send(Input::NoteText {
            text: "and here".into(),
        });
        let effects = session.key_with(Key::Enter, Modifiers::CTRL);
        let Some(Ok(Reply::Marks { marks })) = reply(&effects, waiter) else {
            panic!("{effects:?}");
        };
        assert_eq!(marks.len(), 2);
        assert!(reply(&effects, gone).is_none());
        assert!(!session.app.waiting());
        assert_eq!(title(&effects), Some("one.png — 1600 × 1200 — Seer"));
        // The waiter asked for the marks to be cleared.
        assert!(session.marks().is_empty());
        // Ctrl+Enter with nobody waiting does nothing.
        assert!(session.key_with(Key::Enter, Modifiers::CTRL).is_empty());
    }

    #[test]
    fn the_banner_done_button_finishes_marking() {
        let mut session = three_images();
        let (waiter, _) = session.request(Request::Marks {
            wait: true,
            timeout_ms: Some(60_000),
            clear: false,
        });
        session.mark((100.0, 100.0), (300.0, 200.0), "kept");
        // The banner swallows clicks, and the button shows a hand.
        session.down(Button::Left, 100.0, 20.0);
        assert_eq!(session.app.gesture, Gesture::Idle);
        let effects = session.move_to(760.0, 20.0);
        assert!(effects.contains(&Effect::SetCursor {
            cursor: Cursor::Hand
        }));
        let effects = session.down(Button::Left, 760.0, 20.0);
        assert!(matches!(
            reply(&effects, waiter),
            Some(Ok(Reply::Marks { .. }))
        ));
        assert_eq!(session.marks().len(), 1);
    }

    #[test]
    fn waiting_for_marks_times_out() {
        let mut session = three_images();
        let (waiter, effects) = session.request(Request::Marks {
            wait: true,
            timeout_ms: Some(5_000),
            clear: false,
        });
        assert!(effects.contains(&Effect::WakeAfter { ms: 5_000 }));
        session.wait(4_000);
        assert!(reply(&session.send(Input::Tick), waiter).is_none());
        session.wait(1_000);
        let effects = session.send(Input::Tick);
        let Some(Err(error)) = reply(&effects, waiter) else {
            panic!("{effects:?}");
        };
        assert_eq!(error.code, ErrorCode::Timeout);
        assert_eq!(error.message, "marking was not finished within 5 seconds");
        assert!(!session.app.waiting());
    }

    #[test]
    fn marks_can_be_read_and_cleared_without_waiting() {
        let mut session = three_images();
        session.mark((100.0, 100.0), (300.0, 200.0), "one");
        let reply = session.ask(Request::Marks {
            wait: false,
            timeout_ms: None,
            clear: true,
        });
        let Ok(Reply::Marks { marks }) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(marks.len(), 1);
        assert!(session.marks().is_empty());
    }

    #[test]
    fn quitting_answers_waiters() {
        let mut session = three_images();
        let (waiter, _) = session.request(Request::Marks {
            wait: true,
            timeout_ms: None,
            clear: false,
        });
        let (quit, effects) = session.request(Request::Quit);
        assert_eq!(reply(&effects, quit), Some(Ok(Reply::Done)));
        assert!(matches!(
            reply(&effects, waiter),
            Some(Ok(Reply::Marks { .. }))
        ));
        assert!(effects.contains(&Effect::Quit));
    }

    #[test]
    fn compare_tiles_a_group_with_one_shared_view() {
        let mut session = Session::new();
        session.open(&[r"C:\solo.png"]);
        session.open_with(&[r"C:\before.png", r"C:\after.png"], Some("fix"), false);
        session.decode(1, 800, 600);
        session.decode(2, 1600, 1200);
        session.decode(3, 1600, 1200);
        assert_eq!(session.selected(), Some(2));
        let effects = session.key(Key::Char('c'));
        assert_eq!(title(&effects), Some("fix — compare — Seer"));
        assert_eq!(session.list().compare, Some(CompareLayout::Tiles));
        let scene = session.app.scene();
        assert_eq!(
            images(&scene),
            [
                (2, Rect::new(4.0, 4.0, 392.0, 564.0)),
                (3, Rect::new(404.0, 4.0, 392.0, 564.0))
            ]
        );
        assert!(texts(&scene).contains(&"after.png".to_string()));
        assert!(strokes(&scene).contains(&(Rect::new(4.0, 4.0, 392.0, 592.0), false)));

        // Zooming either tile zooms both.
        session.send(Input::Wheel {
            at: Point::new(600.0, 300.0),
            delta: 3.0,
            modifiers: Modifiers::NONE,
        });
        let view = session.view();
        assert!(!view.fit);
        let transforms: Vec<f64> = session
            .app
            .scene()
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Image { transform, .. } => Some(transform.m11),
                _ => None,
            })
            .collect();
        assert_eq!(transforms, [view.zoom, view.zoom]);

        // Moving within the group keeps compare view; leaving it ends it.
        session.key(Key::Right);
        assert_eq!(session.list().compare, Some(CompareLayout::Tiles));
        session.key(Key::Home);
        assert_eq!(session.list().compare, None);
        // C on an image with no group does nothing.
        session.key(Key::Char('c'));
        assert_eq!(session.list().compare, None);
        // Leaving compare view with C keeps the shared view.
        session.key(Key::End);
        session.key(Key::Char('c'));
        session.key(Key::Char('1'));
        session.key(Key::Char('c'));
        assert_eq!(session.list().compare, None);
        assert_eq!(session.view().zoom, 1.0);
    }

    #[test]
    fn compare_tiles_four_up_in_a_grid() {
        let mut session = Session::new();
        let paths = [r"C:\1.png", r"C:\2.png", r"C:\3.png", r"C:\4.png"];
        session.open_with(&paths, Some("many"), true);
        let effects = session.decode(1, 10, 10);
        assert!(effects.contains(&Effect::Redraw));
        assert_eq!(session.list().compare, Some(CompareLayout::Tiles));
        let panes = session.app.panes();
        let corners: Vec<(f64, f64)> = panes
            .iter()
            .map(|pane| (pane.viewport.x, pane.viewport.y))
            .collect();
        assert_eq!(
            corners,
            [(4.0, 4.0), (404.0, 4.0), (4.0, 304.0), (404.0, 304.0)]
        );
        // Opening more of the group with --compare keeps compare view, and
        // five tiles take three columns.
        session.open_with(&[r"C:\5.png"], Some("many"), true);
        assert_eq!(session.list().compare, Some(CompareLayout::Tiles));
        let columns: Vec<f64> = session
            .app
            .panes()
            .iter()
            .map(|pane| pane.viewport.x.round())
            .collect();
        assert_eq!(columns, [4.0, 271.0, 537.0, 4.0, 271.0]);
    }

    #[test]
    fn compare_split_divides_two_images_with_a_draggable_line() {
        let mut session = Session::new();
        session.request(Request::Open {
            files: vec![
                OpenFile {
                    path: r"C:\before.png".into(),
                    caption: Some("before".into()),
                },
                OpenFile {
                    path: r"C:\after.png".into(),
                    caption: Some("after".into()),
                },
            ],
            group: Some("fix".into()),
            compare: true,
            activate: false,
        });
        session.decode(1, 800, 600);
        session.decode(2, 800, 600);
        // S does nothing outside compare view, so open it first.
        session.key(Key::Char('s'));
        assert_eq!(session.list().compare, Some(CompareLayout::Split));
        let scene = session.app.scene();
        assert_eq!(
            images(&scene),
            [
                (1, Rect::new(0.0, 0.0, 400.0, 600.0)),
                (2, Rect::new(400.0, 0.0, 400.0, 600.0))
            ]
        );
        assert!(texts(&scene).contains(&"before".to_string()));
        assert!(
            scene
                .items
                .iter()
                .any(|item| matches!(item, Item::Line { .. }))
        );

        // Hovering over the divider shows the resize cursor; dragging moves it.
        let effects = session.move_to(402.0, 300.0);
        assert!(effects.contains(&Effect::SetCursor {
            cursor: Cursor::SizeWestEast
        }));
        session.drag(Button::Left, (402.0, 300.0), (600.0, 300.0));
        assert_eq!(
            images(&session.app.scene())[0],
            (1, Rect::new(0.0, 0.0, 600.0, 600.0))
        );
        session.drag(Button::Left, (600.0, 300.0), (-100.0, 300.0));
        assert_eq!(images(&session.app.scene())[0].1.width, 40.0);

        // A mark drawn on the right side belongs to the right image.
        session.mark((500.0, 100.0), (600.0, 200.0), "right");
        assert_eq!(session.marks()[0].image, ImageId(2));

        // The second image's partner wraps round to the first.
        session.key(Key::Right);
        assert_eq!(images(&session.app.scene())[1].0, 1);

        // Leaving the split while dragging the divider ends the drag.
        session.down(Button::Left, 40.0, 300.0);
        session.key(Key::Char('s'));
        assert_eq!(session.app.gesture, Gesture::Idle);
        assert_eq!(session.list().compare, Some(CompareLayout::Tiles));
    }

    #[test]
    fn debug_input_needs_headless() {
        let mut session = three_images();
        let events = vec![
            Input::Key {
                key: Key::Right,
                modifiers: Modifiers::NONE,
            },
            Input::Key {
                key: Key::Delete,
                modifiers: Modifiers::NONE,
            },
            Input::PointerDown {
                button: Button::Right,
                at: Point::new(100.0, 100.0),
                modifiers: Modifiers::NONE,
            },
            Input::PointerUp {
                button: Button::Right,
                at: Point::new(300.0, 200.0),
                modifiers: Modifiers::NONE,
            },
            Input::NoteText {
                text: "typed".into(),
            },
            Input::Key {
                key: Key::Enter,
                modifiers: Modifiers::NONE,
            },
        ];
        let reply = session.ask(Request::DebugInput {
            events: events.clone(),
        });
        assert_eq!(reply, Ok(Reply::Done));
        assert_eq!(session.selected(), Some(3));
        assert_eq!(session.marks()[0].note, "typed");

        let mut session = Session::with(Config {
            headless: false,
            size: WINDOW,
            dpi: 96,
            dark: true,
        });
        let error = session.ask(Request::DebugInput { events }).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotHeadless);
    }

    #[test]
    fn screenshots_and_timings_go_to_the_window() {
        let mut session = Session::new();
        let (id, effects) = session.request(Request::Screenshot {
            path: r"C:\out.png".into(),
        });
        assert!(effects.contains(&Effect::Screenshot {
            id,
            path: r"C:\out.png".into()
        }));
        let (id, effects) = session.request(Request::DebugTimings);
        assert!(effects.contains(&Effect::ReportTimings { id }));
    }

    #[test]
    fn window_changes_redraw() {
        let mut session = three_images();
        assert!(
            session
                .send(Input::Theme { dark: false })
                .contains(&Effect::Redraw)
        );
        assert_eq!(session.app.scene().background, LIGHT_BACKGROUND);
        assert!(session.send(Input::Dpi { dpi: 192 }).is_empty());
        session.mark((100.0, 100.0), (300.0, 200.0), "scaled");
        let effects = session.send(Input::Resize {
            size: PixelSize::new(1600, 1200),
        });
        assert!(effects.contains(&Effect::Redraw));
        assert_eq!(session.view().zoom, 1.0);
        assert_eq!(session.app.scene().size, PixelSize::new(1600, 1200));
        // Earlier times than the last are treated as the last.
        let earlier = session.now - Duration::from_secs(1);
        session.app.handle(Input::Tick, earlier);
        assert_eq!(session.app.now, session.now);
    }
}
