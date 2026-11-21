//! The window: its class and procedure, which translates Win32 messages into
//! calls on a [`Handler`].
//!
//! The window is created hidden. The viewer draws and presents the first
//! frame before showing it, so it never appears blank, and in headless mode
//! never shows it at all.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, IDC_ARROW, LoadCursorW,
    LoadIconW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, SW_SHOWNOACTIVATE,
    SW_SHOWNORMAL, SetWindowLongPtrW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
    WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_SIZE, WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::error::Result;
use crate::model::geometry::{PixelRect, PixelSize};

/// Posted to the window when the decode worker has output ready.
pub const WM_DECODED: u32 = WM_APP + 1;

/// Posted to the window when the hardware graphics device is ready.
pub const WM_DEVICE_READY: u32 = WM_APP + 2;

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
            style: CS_HREDRAW | CS_VREDRAW,
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

    /// Shows the window, and activates it if `activate` is set.
    pub fn show(&self, activate: bool) {
        let how = if activate {
            SW_SHOWNORMAL
        } else {
            SW_SHOWNOACTIVATE
        };
        // SAFETY: the window is live. The return value is the previous
        // visibility, not an error.
        let _ = unsafe { ShowWindow(self.hwnd, how) };
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

/// Runs the message loop until the window is destroyed.
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
    // SAFETY: GetDpiForSystem has no arguments, and the pointer is to a live
    // local. On failure the client rectangle is used as it is.
    unsafe {
        let _ = AdjustWindowRectExForDpi(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE::default(),
            GetDpiForSystem(),
        );
    }
    rect
}

extern "system" fn procedure(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the user data is zero until Window::create stores the address
    // of its handler slot, which lives as long as the window.
    let slot = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Slot).as_ref() };
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_DESTROY => {
            // SAFETY: PostQuitMessage takes a plain value.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
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

    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    struct Recorder(Rc<RefCell<Log>>);

    impl Handler for Recorder {
        fn paint(&mut self) {
            self.0.borrow_mut().0.push("paint".into());
        }
        fn resize(&mut self, size: PixelSize) {
            self.0
                .borrow_mut()
                .0
                .push(format!("resize {}x{}", size.width, size.height));
        }
        fn decoded(&mut self) {
            self.0.borrow_mut().0.push("decoded".into());
        }
        fn device_ready(&mut self) {
            self.0.borrow_mut().0.push("device".into());
        }
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

    #[test]
    fn messages_reach_the_handler() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "test").unwrap();
        assert_eq!(window.client_size(), PixelSize::new(320, 200));
        let log = Rc::new(RefCell::new(Log::default()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        post(window.hwnd().0 as isize, WM_DECODED);
        post(window.hwnd().0 as isize, WM_DEVICE_READY);
        post(0, WM_DECODED);
        pump();
        // SAFETY: the window is live; a size message is sent synchronously.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                window.hwnd(),
                WM_SIZE,
                Some(WPARAM(0)),
                Some(LPARAM((100 << 16) | 200)),
            );
            windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                window.hwnd(),
                WM_PAINT,
                None,
                None,
            );
        }
        assert_eq!(
            log.borrow().0,
            ["decoded", "device", "resize 200x100", "paint"]
        );
        drop(window);
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
