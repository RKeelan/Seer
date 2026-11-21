//! The window: its class and procedure, which translates Win32 messages into
//! [`Input`] and other calls on a [`Handler`].
//!
//! The window is created hidden. The viewer draws and presents the first
//! frame before showing it, so it never appears blank, and in headless mode
//! never shows it at all.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HBRUSH, HDC, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    ScreenToClient, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_ADD, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_LBUTTON, VK_LEFT, VK_MBUTTON, VK_MENU,
    VK_NEXT, VK_OEM_MINUS, VK_OEM_PLUS, VK_PRIOR, VK_RBUTTON, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_SPACE, VK_SUBTRACT, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, EN_CHANGE, FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx,
    GWLP_USERDATA, GetClientRect, GetForegroundWindow, GetMessageW, GetWindowLongPtrW,
    GetWindowTextW, HTCLIENT, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_SIZEALL, IDC_SIZEWE, IsIconic,
    KillTimer, LoadCursorW, LoadIconW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    SW_RESTORE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY,
    WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT,
    WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SETTINGCHANGE,
    WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::error::Result;
use crate::model::app::{Button, Cursor, Input, Key, Modifiers};
use crate::model::geometry::{PixelRect, PixelSize, Point};
use crate::viewer::{note, theme};

/// Posted to the window when the decode worker has output ready.
pub const WM_DECODED: u32 = WM_APP + 1;

/// Posted to the window when the hardware graphics device is ready.
pub const WM_DEVICE_READY: u32 = WM_APP + 2;

/// Posted to the window when the pipe server has requests waiting.
pub const WM_REQUEST: u32 = WM_APP + 3;

/// Posted to the window when watched files have changed.
pub const WM_FILES_CHANGED: u32 = WM_APP + 4;

/// The wheel delta of one notch.
const WHEEL_NOTCH: f64 = 120.0;

const CLASS: PCWSTR = w!("Seer");

/// What the window asks of the viewer.
pub trait Handler {
    /// Draw and present the current frame.
    fn paint(&mut self);
    /// The client area changed size.
    fn resize(&mut self, size: PixelSize);
    /// The decode worker has output ready.
    fn decoded(&mut self);
    /// The hardware graphics device is ready.
    fn device_ready(&mut self);
    /// The pipe server has requests waiting.
    fn requests(&mut self);
    /// The timer set with [`set_timer`] and this id has fired. It fires once.
    fn timer(&mut self, id: usize);
    /// The user did something.
    fn input(&mut self, input: Input);
    /// The pointer's shape over the client area.
    fn cursor(&mut self) -> Cursor {
        Cursor::Arrow
    }
    /// The window has the keyboard focus, and may pass it to a child.
    fn focus(&mut self) {}
    /// The system's light or dark setting may have changed.
    fn theme_changed(&mut self) {}
    /// Watched files have changed.
    fn files_changed(&mut self) {}
    /// A child edit control, such as the note field, is about to paint with
    /// `hdc`. Returns the brush for its background, having set its colours
    /// in `hdc`, or `None` for the system's colours.
    fn control_colours(&mut self, _hdc: HDC, _control: HWND) -> Option<HBRUSH> {
        None
    }
}

type Slot = RefCell<Option<Box<dyn Handler>>>;

/// A top-level window, destroyed when dropped.
pub struct Window {
    hwnd: HWND,
    /// Reached from the window procedure through `GWLP_USERDATA`.
    handler: Box<Slot>,
}

impl Window {
    /// Creates a hidden window placed at `placement`, a rectangle of the
    /// client area in screen pixels.
    pub fn create(placement: PixelRect, title: &str) -> Result<Self> {
        // SAFETY: a null module name means this executable.
        let instance = unsafe { GetModuleHandleW(None) }?;
        // SAFETY: resource 1 is the icon build.rs embeds; a missing icon or
        // cursor gives a null handle, which the class tolerates.
        let (icon, cursor) = unsafe {
            (
                LoadIconW(Some(instance.into()), PCWSTR(1 as _)).unwrap_or_default(),
                LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            )
        };
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            hIcon: icon,
            hCursor: cursor,
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: the class structure is live. Registering a second time
        // fails harmlessly, leaving the first registration.
        unsafe { RegisterClassExW(&class) };
        let frame = frame_for(placement);
        // SAFETY: the class is registered, the strings outlive the call, and
        // no creation data is passed.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                CLASS,
                &HSTRING::from(title),
                WS_OVERLAPPEDWINDOW,
                frame.left,
                frame.top,
                frame.right - frame.left,
                frame.bottom - frame.top,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }?;
        let handler: Box<Slot> = Box::new(RefCell::new(None));
        // SAFETY: the slot is boxed, so its address is stable, and it lives
        // as long as the window: Drop destroys the window before the box.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, &raw const *handler as isize) };
        Ok(Window { hwnd, handler })
    }

    /// The window's handle.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Gives the window the handler its messages go to.
    pub fn set_handler(&self, handler: Box<dyn Handler>) {
        *self.handler.borrow_mut() = Some(handler);
    }

    /// Takes the handler back, so it is dropped before the window.
    pub fn take_handler(&self) -> Option<Box<dyn Handler>> {
        self.handler.borrow_mut().take()
    }

    /// The client area's size.
    pub fn client_size(&self) -> PixelSize {
        let mut rect = RECT::default();
        // SAFETY: the pointer is to a live local.
        let _ = unsafe { GetClientRect(self.hwnd, &mut rect) };
        PixelSize::new(
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the window is live; destroying it ends its messages, so the
        // handler slot is not reached again.
        let _ = unsafe { DestroyWindow(self.hwnd) };
        self.handler.borrow_mut().take();
    }
}

/// Shows the window, and activates it if `activate` is set.
pub fn show(hwnd: HWND, activate: bool) {
    let how = if activate {
        SW_SHOWNORMAL
    } else {
        SW_SHOWNOACTIVATE
    };
    // SAFETY: ShowWindow checks the handle. The return value is the previous
    // visibility, not an error.
    let _ = unsafe { ShowWindow(hwnd, how) };
}

/// Brings the window to the foreground, restoring it if minimised. Windows
/// allows this only when the foreground process has passed on its permission
/// (see [`crate::ipc::client::Connection::allow_foreground`]); otherwise it
/// flashes the taskbar button instead.
pub fn activate(hwnd: HWND) {
    // SAFETY: both calls check the handle.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Flashes the window's taskbar button until it is next activated, unless it
/// is the foreground window already.
pub fn flash(hwnd: HWND) {
    // SAFETY: GetForegroundWindow takes no arguments.
    if unsafe { GetForegroundWindow() } == hwnd {
        return;
    }
    let info = FLASHWINFO {
        cbSize: size_of::<FLASHWINFO>() as u32,
        hwnd,
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: the structure is a live local with its size set. The return
    // value is the previous state, not an error.
    let _ = unsafe { FlashWindowEx(&info) };
}

/// Sets the title bar's text.
pub fn set_title(hwnd: HWND, title: &str) {
    // SAFETY: the string outlives the call, which copies it.
    let _ = unsafe { SetWindowTextW(hwnd, &HSTRING::from(title)) };
}

/// The title bar's text.
pub fn title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer is a live local, and the call writes within it.
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

/// Calls [`Handler::timer`] with `id` once, after `ms` milliseconds,
/// replacing a timer with the same id that has not fired yet.
pub fn set_timer(hwnd: HWND, id: usize, ms: u64) {
    let ms = u32::try_from(ms).unwrap_or(u32::MAX);
    // SAFETY: SetTimer checks the handle; no callback is given, so the timer
    // arrives as WM_TIMER.
    unsafe { SetTimer(Some(hwnd), id, ms, None) };
}

/// Ends the message loop.
pub fn quit() {
    // SAFETY: PostQuitMessage takes a plain value.
    unsafe { PostQuitMessage(0) };
}

/// Posts `message` (such as [`WM_DECODED`]) to the window whose handle is
/// `hwnd`, given as an integer so it can cross threads. Posting to no window
/// (zero) does nothing, and to a window that no longer exists fails
/// harmlessly.
pub fn post(hwnd: isize, message: u32) {
    if hwnd != 0 {
        // SAFETY: PostMessageW checks the handle.
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as _)), message, WPARAM(0), LPARAM(0)) };
    }
}

/// Runs the message loop until [`quit`] is called or the window is destroyed.
pub fn run_message_loop() {
    let mut message = MSG::default();
    // SAFETY: the pointer is to a live local. GetMessageW returns 0 for
    // WM_QUIT and -1 on error; both end the loop.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        // SAFETY: the message came from GetMessageW.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Where to put the client area of a new window: centred in the primary
/// monitor's work area, four fifths of its width and height.
pub fn default_placement() -> PixelRect {
    // SAFETY: plain values.
    let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: the monitor handle came from MonitorFromPoint and the pointer is
    // to a live local with its size set.
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 800,
        }
    };
    placement_in(work)
}

/// The system's DPI; 96 is 100% scale.
pub fn system_dpi() -> u32 {
    // SAFETY: GetDpiForSystem has no arguments.
    unsafe { GetDpiForSystem() }
}

fn placement_in(work: RECT) -> PixelRect {
    let (width, height) = (work.right - work.left, work.bottom - work.top);
    let (client_width, client_height) = (width * 4 / 5, height * 4 / 5);
    PixelRect::new(
        (work.left + (width - client_width) / 2).max(0) as u32,
        (work.top + (height - client_height) / 2).max(0) as u32,
        client_width.max(1) as u32,
        client_height.max(1) as u32,
    )
}

/// The window rectangle whose client area is `placement`.
fn frame_for(placement: PixelRect) -> RECT {
    let mut rect = RECT {
        left: placement.x as i32,
        top: placement.y as i32,
        right: (placement.x + placement.width) as i32,
        bottom: (placement.y + placement.height) as i32,
    };
    // SAFETY: the pointer is to a live local. On failure the client rectangle
    // is used as it is.
    unsafe {
        let _ = AdjustWindowRectExForDpi(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE::default(),
            system_dpi(),
        );
    }
    rect
}

/// The key a virtual-key code stands for, if Seer has a use for it.
pub fn key_for(vk: VIRTUAL_KEY) -> Option<Key> {
    let key = match vk {
        VK_LEFT => Key::Left,
        VK_RIGHT => Key::Right,
        VK_UP => Key::Up,
        VK_DOWN => Key::Down,
        VK_HOME => Key::Home,
        VK_END => Key::End,
        VK_PRIOR => Key::PageUp,
        VK_NEXT => Key::PageDown,
        VK_DELETE => Key::Delete,
        VK_BACK => Key::Backspace,
        VK_ESCAPE => Key::Escape,
        VK_RETURN => Key::Enter,
        VK_TAB => Key::Tab,
        VK_SPACE => Key::Space,
        VK_OEM_PLUS | VK_ADD => Key::Plus,
        VK_OEM_MINUS | VK_SUBTRACT => Key::Minus,
        VIRTUAL_KEY(code) if (VK_F1.0..=VK_F24.0).contains(&code) => {
            Key::Function((code - VK_F1.0 + 1) as u8)
        }
        // Letters and digits have their ASCII codes.
        VIRTUAL_KEY(code @ (0x30..=0x39 | 0x41..=0x5A)) => {
            Key::Char(char::from(code as u8).to_ascii_lowercase())
        }
        _ => return None,
    };
    Some(key)
}

/// The point packed into a mouse message's `lparam`, as signed 16-bit `x`
/// and `y`.
fn point_from(lparam: LPARAM) -> Point {
    let x = (lparam.0 & 0xFFFF) as u16 as i16;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16;
    Point::new(f64::from(x), f64::from(y))
}

/// The wheel's movement in a `WM_MOUSEWHEEL`'s `wparam`, in notches.
fn wheel_notches(wparam: WPARAM) -> f64 {
    let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
    f64::from(delta) / WHEEL_NOTCH
}

/// The modifier keys held now.
fn modifiers() -> Modifiers {
    // SAFETY: GetKeyState takes a plain value; the high bit means held.
    let held = |vk: VIRTUAL_KEY| unsafe { GetKeyState(i32::from(vk.0)) } < 0;
    Modifiers {
        ctrl: held(VK_CONTROL),
        shift: held(VK_SHIFT),
        alt: held(VK_MENU),
    }
}

/// Whether any mouse button is down.
fn any_button_down() -> bool {
    // SAFETY: GetKeyState takes a plain value; the high bit means held.
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
        .iter()
        .any(|vk| unsafe { GetKeyState(i32::from(vk.0)) } < 0)
}

/// The system cursor for a shape.
fn cursor_id(cursor: Cursor) -> PCWSTR {
    match cursor {
        Cursor::Arrow => IDC_ARROW,
        Cursor::Crosshair => IDC_CROSS,
        Cursor::Hand => IDC_HAND,
        Cursor::Move => IDC_SIZEALL,
        Cursor::SizeWestEast => IDC_SIZEWE,
    }
}

/// The input a mouse or keyboard message means, if any.
fn input_for(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> Vec<Input> {
    let button = match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK => Some(Button::Left),
        WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK => Some(Button::Right),
        WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MBUTTONDBLCLK => Some(Button::Middle),
        _ => None,
    };
    let at = point_from(lparam);
    let modifiers = modifiers();
    match (message, button) {
        (WM_KEYDOWN, _) => key_for(VIRTUAL_KEY(wparam.0 as u16))
            .map(|key| Input::Key { key, modifiers })
            .into_iter()
            .collect(),
        (WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN, Some(button)) => {
            // SAFETY: SetCapture checks the handle.
            unsafe { SetCapture(hwnd) };
            vec![Input::PointerDown {
                button,
                at,
                modifiers,
            }]
        }
        // A double click replaces the second press.
        (WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK, Some(button)) => {
            // SAFETY: SetCapture checks the handle.
            unsafe { SetCapture(hwnd) };
            vec![
                Input::PointerDown {
                    button,
                    at,
                    modifiers,
                },
                Input::DoubleClick {
                    button,
                    at,
                    modifiers,
                },
            ]
        }
        (WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP, Some(button)) => {
            if !any_button_down() {
                // SAFETY: ReleaseCapture has no arguments; failing means the
                // window had no capture.
                let _ = unsafe { ReleaseCapture() };
            }
            vec![Input::PointerUp {
                button,
                at,
                modifiers,
            }]
        }
        (WM_MOUSEMOVE, _) => vec![Input::PointerMove { at, modifiers }],
        (WM_MOUSEWHEEL, _) => {
            // The wheel's position is in screen coordinates.
            let mut point = POINT {
                x: at.x as i32,
                y: at.y as i32,
            };
            // SAFETY: the pointer is to a live local.
            let _ = unsafe { ScreenToClient(hwnd, &mut point) };
            vec![Input::Wheel {
                at: Point::new(f64::from(point.x), f64::from(point.y)),
                delta: wheel_notches(wparam),
                modifiers,
            }]
        }
        _ => Vec::new(),
    }
}

extern "system" fn procedure(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the user data is zero until Window::create stores the address
    // of its handler slot, which lives as long as the window.
    let slot = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Slot).as_ref() };
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_DESTROY => {
            quit();
            return LRESULT(0);
        }
        // Moved to a monitor with a different scale: take the size Windows
        // suggests for it, which keeps the window the same size to the eye.
        // This happens before the handler is reached, so that the resize it
        // causes reaches the handler too.
        WM_DPICHANGED if lparam.0 != 0 => {
            // SAFETY: WM_DPICHANGED's lParam points to the suggested window
            // rectangle, valid for the message.
            let r = unsafe { *(lparam.0 as *const RECT) };
            // SAFETY: the window is live.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        _ => {}
    }
    // A message that arrives while the handler is busy, such as one sent by a
    // call it makes, gets the default handling.
    let handled = slot
        .and_then(|slot| slot.try_borrow_mut().ok())
        .and_then(|mut handler| {
            let handler = handler.as_mut()?;
            match message {
                WM_PAINT => {
                    handler.paint();
                    // SAFETY: the window is live.
                    let _ = unsafe { ValidateRect(Some(hwnd), None) };
                }
                WM_SIZE => {
                    let width = (lparam.0 & 0xFFFF) as u32;
                    let height = ((lparam.0 >> 16) & 0xFFFF) as u32;
                    handler.resize(PixelSize::new(width, height));
                }
                WM_DECODED => handler.decoded(),
                WM_DEVICE_READY => handler.device_ready(),
                WM_REQUEST => handler.requests(),
                WM_FILES_CHANGED => handler.files_changed(),
                WM_TIMER => {
                    // SAFETY: KillTimer checks the handle and id.
                    let _ = unsafe { KillTimer(Some(hwnd), wparam.0) };
                    handler.timer(wparam.0);
                }
                WM_CLOSE => handler.input(Input::CloseWindow),
                WM_SETTINGCHANGE if theme::is_theme_change(lparam) => handler.theme_changed(),
                WM_SETFOCUS => handler.focus(),
                WM_DPICHANGED => handler.input(Input::Dpi {
                    dpi: (wparam.0 & 0xFFFF) as u32,
                }),
                WM_CTLCOLOREDIT => {
                    let brush = handler.control_colours(HDC(wparam.0 as _), HWND(lparam.0 as _))?;
                    return Some(LRESULT(brush.0 as isize));
                }
                WM_COMMAND if ((wparam.0 >> 16) & 0xFFFF) as u32 == EN_CHANGE => {
                    let text = note::text(HWND(lparam.0 as _));
                    handler.input(Input::NoteText { text });
                }
                // Over a child, such as the note field, the child's cursor
                // shows.
                WM_SETCURSOR
                    if (lparam.0 & 0xFFFF) as u32 == HTCLIENT && wparam.0 == hwnd.0 as usize =>
                {
                    let cursor = cursor_id(handler.cursor());
                    // SAFETY: a system cursor needs no module; a missing one
                    // gives a null handle, which hides the pointer.
                    unsafe { SetCursor(LoadCursorW(None, cursor).ok()) };
                    return Some(LRESULT(1));
                }
                WM_KEYDOWN | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN
                | WM_RBUTTONUP | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONUP
                | WM_MBUTTONDBLCLK | WM_MOUSEMOVE | WM_MOUSEWHEEL => {
                    let inputs = input_for(hwnd, message, wparam, lparam);
                    if inputs.is_empty() {
                        return None;
                    }
                    for input in inputs {
                        handler.input(input);
                    }
                }
                _ => return None,
            }
            Some(LRESULT(0))
        });
    // SAFETY: the arguments are the ones this procedure was called with.
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, message, wparam, lparam) })
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use windows::Win32::UI::Input::KeyboardAndMouse::VK_OEM_1;
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    struct Recorder(Rc<RefCell<Log>>);

    impl Recorder {
        fn push(&self, entry: String) {
            self.0.borrow_mut().0.push(entry);
        }
    }

    impl Handler for Recorder {
        fn paint(&mut self) {
            self.push("paint".into());
        }
        fn resize(&mut self, size: PixelSize) {
            self.push(format!("resize {}x{}", size.width, size.height));
        }
        fn decoded(&mut self) {
            self.push("decoded".into());
        }
        fn device_ready(&mut self) {
            self.push("device".into());
        }
        fn requests(&mut self) {
            self.push("requests".into());
        }
        fn timer(&mut self, id: usize) {
            self.push(format!("timer {id}"));
        }
        fn input(&mut self, input: Input) {
            self.push(serde_json::to_string(&input).unwrap());
        }
        fn theme_changed(&mut self) {
            self.push("theme".into());
        }
        fn control_colours(&mut self, _: HDC, _: HWND) -> Option<HBRUSH> {
            self.push("colours".into());
            Some(HBRUSH(7 as _))
        }
    }

    /// A handler that keeps every default.
    struct Defaults;

    impl Handler for Defaults {
        fn paint(&mut self) {}
        fn resize(&mut self, _: PixelSize) {}
        fn decoded(&mut self) {}
        fn device_ready(&mut self) {}
        fn requests(&mut self) {}
        fn timer(&mut self, _: usize) {}
        fn input(&mut self, _: Input) {}
    }

    fn pump() {
        use windows::Win32::UI::WindowsAndMessaging::{PM_REMOVE, PeekMessageW};
        let mut message = MSG::default();
        // SAFETY: the pointer is to a live local and the messages came from
        // PeekMessageW.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn send(window: &Window, message: u32, wparam: usize, lparam: isize) {
        // SAFETY: the window is live; the message is handled synchronously.
        unsafe {
            SendMessageW(
                window.hwnd(),
                message,
                Some(WPARAM(wparam)),
                Some(LPARAM(lparam)),
            );
        }
    }

    #[test]
    fn messages_reach_the_handler() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "test").unwrap();
        assert_eq!(window.client_size(), PixelSize::new(320, 200));
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        post(window.hwnd().0 as isize, WM_DECODED);
        post(window.hwnd().0 as isize, WM_DEVICE_READY);
        post(window.hwnd().0 as isize, WM_REQUEST);
        post(window.hwnd().0 as isize, WM_FILES_CHANGED);
        post(0, WM_DECODED);
        pump();
        send(&window, WM_SIZE, 0, (100 << 16) | 200);
        send(&window, WM_PAINT, 0, 0);
        send(&window, WM_TIMER, 7, 0);
        send(&window, WM_CLOSE, 0, 0);
        assert_eq!(
            log.borrow().0,
            [
                "decoded",
                "device",
                "requests",
                "resize 200x100",
                "paint",
                "timer 7",
                r#"{"type":"close_window"}"#
            ]
        );
        assert!(window.take_handler().is_some());
        assert!(window.take_handler().is_none());
    }

    #[test]
    fn a_change_to_the_colour_setting_reaches_the_handler() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "test").unwrap();
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        let colour = HSTRING::from("ImmersiveColorSet");
        let environment = HSTRING::from("Environment");
        send(&window, WM_SETTINGCHANGE, 0, colour.as_ptr() as isize);
        send(&window, WM_SETTINGCHANGE, 0, environment.as_ptr() as isize);
        send(&window, WM_SETTINGCHANGE, 0, 0);
        assert_eq!(log.borrow().0, ["theme"]);

        window.set_handler(Box::new(Defaults));
        send(&window, WM_SETTINGCHANGE, 0, colour.as_ptr() as isize);
        theme::set_title_bar(window.hwnd(), true);
        theme::set_title_bar(window.hwnd(), false);
    }

    #[test]
    fn a_change_of_scale_resizes_the_window_and_reaches_the_handler() {
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        let window = Window::create(PixelRect::new(10, 10, 320, 200), "test").unwrap();
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        let suggested = RECT {
            left: 20,
            top: 20,
            right: 520,
            bottom: 420,
        };
        let pointer = &raw const suggested as isize;
        send(&window, WM_DPICHANGED, (144 << 16) | 144, pointer);
        let log = log.borrow();
        assert!(
            log.0.iter().any(|entry| entry.starts_with("resize ")),
            "{:?}",
            log.0
        );
        assert_eq!(log.0.last().unwrap(), r#"{"type":"dpi","dpi":144}"#);
        let mut frame = RECT::default();
        // SAFETY: the pointer is to a live local.
        unsafe { GetWindowRect(window.hwnd(), &mut frame) }.unwrap();
        assert_eq!(frame, suggested);
    }

    #[test]
    fn edit_controls_paint_in_the_handler_s_colours() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "test").unwrap();
        let sent = |window: &Window| {
            // SAFETY: the window is live; the message is handled
            // synchronously.
            unsafe {
                SendMessageW(
                    window.hwnd(),
                    WM_CTLCOLOREDIT,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                )
            }
        };
        // Without colours from the handler the system's apply.
        window.set_handler(Box::new(Defaults));
        assert_ne!(sent(&window), LRESULT(7));
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        assert_eq!(sent(&window), LRESULT(7));
        assert_eq!(log.borrow().0, ["colours"]);
    }

    #[test]
    fn keys_and_the_mouse_become_input() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "test").unwrap();
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        send(&window, WM_KEYDOWN, usize::from(VK_RIGHT.0), 0);
        send(&window, WM_KEYDOWN, usize::from(VK_CONTROL.0), 0);
        let at = (20 << 16) | 30;
        send(&window, WM_RBUTTONDOWN, 0, at);
        send(&window, WM_MOUSEMOVE, 0, at);
        send(&window, WM_RBUTTONUP, 0, at);
        send(&window, WM_LBUTTONDBLCLK, 0, at);
        send(&window, WM_LBUTTONUP, 0, at);
        // One notch away from the user, at the screen's origin.
        send(&window, WM_MOUSEWHEEL, 120 << 16, 0);
        let log = log.borrow();
        let expected = [
            r#"{"type":"key","key":"right"}"#,
            r#"{"type":"pointer_down","button":"right","at":{"x":30.0,"y":20.0}}"#,
            r#"{"type":"pointer_move","at":{"x":30.0,"y":20.0}}"#,
            r#"{"type":"pointer_up","button":"right","at":{"x":30.0,"y":20.0}}"#,
            r#"{"type":"pointer_down","button":"left","at":{"x":30.0,"y":20.0}}"#,
            r#"{"type":"double_click","button":"left","at":{"x":30.0,"y":20.0}}"#,
            r#"{"type":"pointer_up","button":"left","at":{"x":30.0,"y":20.0}}"#,
        ];
        // Modifier keys may be held by whoever runs the tests, so compare
        // only what does not depend on them.
        let strip = |entry: &String| {
            entry
                .replace(r#","ctrl":true"#, "")
                .replace(r#","shift":true"#, "")
                .replace(r#","alt":true"#, "")
        };
        let entries: Vec<String> = log.0.iter().map(strip).collect();
        assert_eq!(entries[..expected.len()], expected);
        assert!(entries[expected.len()].starts_with(r#"{"type":"wheel""#));
        assert!(entries[expected.len()].contains(r#""delta":1.0"#));
        assert_eq!(entries.len(), expected.len() + 1);
    }

    #[test]
    fn windows_show_title_flash_and_time() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "first").unwrap();
        let hwnd = window.hwnd();
        assert_eq!(title(hwnd), "first");
        set_title(hwnd, "cat.png — 640 × 480 — Seer");
        assert_eq!(title(hwnd), "cat.png — 640 × 480 — Seer");
        // The window is hidden, so none of these show anything.
        flash(hwnd);
        set_timer(hwnd, 3, 1);
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while log.borrow().0.is_empty() && std::time::Instant::now() < deadline {
            pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(log.borrow().0, ["timer 3"]);
        assert!(system_dpi() >= 96);
    }

    #[test]
    fn virtual_keys_map_to_keys() {
        assert_eq!(key_for(VK_LEFT), Some(Key::Left));
        assert_eq!(key_for(VK_NEXT), Some(Key::PageDown));
        assert_eq!(key_for(VK_ADD), Some(Key::Plus));
        assert_eq!(key_for(VK_OEM_MINUS), Some(Key::Minus));
        assert_eq!(key_for(VK_F1), Some(Key::Function(1)));
        assert_eq!(key_for(VK_F24), Some(Key::Function(24)));
        assert_eq!(key_for(VIRTUAL_KEY(u16::from(b'R'))), Some(Key::Char('r')));
        assert_eq!(key_for(VIRTUAL_KEY(u16::from(b'0'))), Some(Key::Char('0')));
        assert_eq!(key_for(VK_SHIFT), None);
        assert_eq!(key_for(VK_OEM_1), None);
        for vk in [
            VK_RIGHT,
            VK_UP,
            VK_DOWN,
            VK_HOME,
            VK_END,
            VK_PRIOR,
            VK_DELETE,
            VK_BACK,
            VK_ESCAPE,
            VK_RETURN,
            VK_TAB,
            VK_SPACE,
            VK_OEM_PLUS,
            VK_SUBTRACT,
        ] {
            assert!(key_for(vk).is_some());
        }
    }

    #[test]
    fn mouse_positions_and_wheel_deltas_are_signed() {
        assert_eq!(
            point_from(LPARAM(((-5i16 as u16 as isize) << 16) | 0xFFFE)),
            Point::new(-2.0, -5.0)
        );
        assert_eq!(wheel_notches(WPARAM((-240i16 as u16 as usize) << 16)), -2.0);
        for cursor in [
            Cursor::Arrow,
            Cursor::Crosshair,
            Cursor::Hand,
            Cursor::Move,
            Cursor::SizeWestEast,
        ] {
            assert!(!cursor_id(cursor).is_null());
        }
    }

    #[test]
    fn placements_centre_four_fifths_of_the_work_area() {
        let placement = placement_in(RECT {
            left: 100,
            top: 0,
            right: 1100,
            bottom: 500,
        });
        assert_eq!(placement, PixelRect::new(200, 50, 800, 400));
        let placement = default_placement();
        assert!(placement.width > 0 && placement.height > 0);
    }
}
