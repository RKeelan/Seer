//! The note field: a one-line `EDIT` control, a child of the window, in which
//! the user types a mark's note.
//!
//! The scene draws the field's box (see [`Effect::ShowNoteField`]); the
//! control sits inside it, so a screenshot, which draws only the scene, shows
//! the note too. The window's content is a DirectComposition visual with no
//! redirection surface behind it, so the control is a layered child window,
//! which Windows composes on its own surface above that visual.
//!
//! What the user types reaches the window as `EN_CHANGE` notifications, which
//! [`super::window`] turns into [`Input::NoteText`]. Enter, Ctrl+Enter and
//! Escape are passed on to the window as key presses, since they finish the
//! note rather than edit it.
//!
//! [`Effect::ShowNoteField`]: crate::model::app::Effect::ShowNoteField
//! [`Input::NoteText`]: crate::model::app::Input::NoteText

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_SETCUEBANNER, EM_SETMARGINS, EM_SETSEL};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, SetFocus, VIRTUAL_KEY, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DestroyWindow, EC_LEFTMARGIN, EC_RIGHTMARGIN, ES_AUTOHSCROLL,
    GWL_STYLE, GWLP_USERDATA, GWLP_WNDPROC, GetParent, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, HMENU, LWA_ALPHA, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SW_HIDE,
    SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SendMessageW, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CHAR, WM_KEYDOWN, WM_SETFONT, WNDPROC, WS_CHILD, WS_EX_LAYERED, WS_VISIBLE,
};
use windows::core::{HSTRING, w};

use crate::error::Result;
use crate::model::app::{NOTE_PADDING, NOTE_PROMPT};
use crate::model::geometry::Rect;

/// The control's id among the window's children.
const ID: usize = 1;

/// The note field.
pub struct NoteField {
    hwnd: HWND,
    /// The font, and the DPI it was made for.
    font: Option<(u32, HFONT)>,
}

impl NoteField {
    /// Creates the field, hidden, as a child of `parent`.
    ///
    /// Windows makes a child window layered only for a program whose
    /// manifest says it supports Windows 8 or later, as Seer's does. Without
    /// one, as in tests, the field is an ordinary child: hidden behind the
    /// window's content, but still taking what the user types, which the
    /// scene shows.
    pub fn create(parent: HWND) -> Result<Self> {
        let hwnd = create_edit(parent, true).or_else(|_| create_edit(parent, false))?;
        let field = NoteField { hwnd, font: None };
        let prompt = HSTRING::from(NOTE_PROMPT);
        // SAFETY: the control is live and the prompt outlives the call, which
        // copies it. A cue banner needs version 6 of the common controls,
        // which the manifest asks for; without it the field has no prompt.
        unsafe {
            SendMessageW(
                hwnd,
                EM_SETCUEBANNER,
                Some(WPARAM(1)),
                Some(LPARAM(prompt.as_ptr() as isize)),
            );
            SendMessageW(
                hwnd,
                EM_SETMARGINS,
                Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
                Some(LPARAM(0)),
            );
        }
        // SAFETY: the control is live and belongs to this thread. Its old
        // procedure is kept in its user data, which nothing else uses, for
        // `procedure` to pass messages on to.
        unsafe {
            let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, procedure as *const () as isize);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, previous);
        }
        Ok(field)
    }

    /// The control's handle.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Shows the field in the box at `rect`, holding `text`. The text is
    /// replaced only if it differs, so the caret stays put while the user
    /// types. Returns whether the field was hidden before.
    pub fn show(&mut self, rect: Rect, text: &str) -> bool {
        // SAFETY: GetParent and GetDpiForWindow check the handle.
        let dpi = unsafe { GetParent(self.hwnd) }
            .map(|parent| unsafe { GetDpiForWindow(parent) })
            .unwrap_or(96)
            .max(96);
        let line = self.font_for(dpi);
        let scale = f64::from(dpi) / 96.0;
        let padding = NOTE_PADDING * scale;
        let height = line.min(rect.height);
        let (x, y) = (rect.x + padding, rect.y + (rect.height - height) / 2.0);
        let width = (rect.width - 2.0 * padding).max(0.0);
        if self.text() != text {
            let end = text.encode_utf16().count();
            // SAFETY: the string outlives the call, which copies it. The
            // selection is then emptied at the end, putting the caret there.
            unsafe {
                let _ = SetWindowTextW(self.hwnd, &HSTRING::from(text));
                SendMessageW(
                    self.hwnd,
                    EM_SETSEL,
                    Some(WPARAM(end)),
                    Some(LPARAM(end as isize)),
                );
            }
        }
        let hidden = !self.is_shown();
        // SAFETY: the control is live; the position is in its parent's
        // client space.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x.round() as i32,
                y.round() as i32,
                width.round() as i32,
                height.round() as i32,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
        hidden
    }

    /// Hides the field, giving the keyboard focus back to the window if the
    /// field had it.
    pub fn hide(&self) {
        // SAFETY: the calls check their handles. Focus moves only within this
        // thread's windows.
        unsafe {
            if GetFocus() == self.hwnd
                && let Ok(parent) = GetParent(self.hwnd)
            {
                let _ = SetFocus(Some(parent));
            }
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Gives the field the keyboard focus.
    pub fn focus(&self) {
        // SAFETY: the control belongs to this thread.
        let _ = unsafe { SetFocus(Some(self.hwnd)) };
    }

    /// Whether the field is shown. It can be shown while the window is
    /// hidden, as it always is in a headless viewer.
    pub fn is_shown(&self) -> bool {
        // SAFETY: GetWindowLongPtrW checks the handle.
        let style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) };
        style as u32 & WS_VISIBLE.0 != 0
    }

    /// What the field holds.
    pub fn text(&self) -> String {
        text(self.hwnd)
    }

    /// Sets the font for `dpi`, if it is not set already, and returns the
    /// height of a line of text in it.
    fn font_for(&mut self, dpi: u32) -> f64 {
        let mut metrics = NONCLIENTMETRICSW {
            cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        // SAFETY: the structure is a live local with its size set.
        let read = unsafe {
            SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                metrics.cbSize,
                Some((&raw mut metrics).cast()),
                0,
                dpi,
            )
        };
        let size = if read.is_ok() {
            metrics.lfMessageFont.lfHeight.unsigned_abs()
        } else {
            12 * dpi / 96
        };
        if self.font.is_none_or(|(made_for, _)| made_for != dpi) && read.is_ok() {
            // SAFETY: the structure is a live local.
            let font = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
            if !font.is_invalid() {
                // SAFETY: the control is live and the font outlives its use:
                // it is deleted only once replaced or once the control is
                // destroyed.
                unsafe {
                    SendMessageW(
                        self.hwnd,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
                if let Some((_, old)) = self.font.replace((dpi, font)) {
                    // SAFETY: the control no longer uses the old font.
                    let _ = unsafe { DeleteObject(HGDIOBJ(old.0)) };
                }
            }
        }
        // A line of text is about a third taller than the font's em height.
        (f64::from(size) * 4.0 / 3.0).ceil()
    }
}

impl Drop for NoteField {
    fn drop(&mut self) {
        // SAFETY: the control belongs to this thread; once it is destroyed
        // nothing uses the font.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            if let Some((_, font)) = self.font.take() {
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
        }
    }
}

/// Creates a hidden `EDIT` control, layered if `layered` is set.
fn create_edit(parent: HWND, layered: bool) -> Result<HWND> {
    // SAFETY: a null module name means this executable.
    let instance = unsafe { GetModuleHandleW(None) }?;
    let extended = if layered {
        WS_EX_LAYERED
    } else {
        WINDOW_EX_STYLE::default()
    };
    // SAFETY: EDIT is a system class, the parent is a live window of this
    // thread, and no creation data is passed.
    let hwnd = unsafe {
        CreateWindowExW(
            extended,
            w!("EDIT"),
            None,
            WS_CHILD | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(ID as _)),
            Some(instance.into()),
            None,
        )
    }?;
    if layered {
        // SAFETY: the control is live. A layered window shows nothing until
        // it is given an opacity.
        if let Err(error) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) }
        {
            // SAFETY: the control was made above and is used nowhere else.
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(error.into());
        }
    }
    Ok(hwnd)
}

/// A window's text, such as an edit control's.
pub fn text(hwnd: HWND) -> String {
    // SAFETY: the call checks the handle.
    let length = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    // SAFETY: the buffer is live and the call writes within it.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..copied])
}

/// Whether a key press in the field belongs to the window: Enter keeps the
/// note, with Ctrl it also finishes marking, and Escape discards it.
fn finishes(vk: VIRTUAL_KEY) -> bool {
    vk == VK_RETURN || vk == VK_ESCAPE
}

/// The field's window procedure, in front of the `EDIT` class's own.
extern "system" fn procedure(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_KEYDOWN if finishes(VIRTUAL_KEY(wparam.0 as u16)) => {
            // SAFETY: GetParent and SendMessageW check their handles. The
            // window handles the key as if it had been pressed there.
            unsafe {
                if let Ok(parent) = GetParent(hwnd) {
                    SendMessageW(parent, message, Some(wparam), Some(lparam));
                }
            }
            return LRESULT(0);
        }
        // The characters those keys type: a single-line field would beep.
        WM_CHAR if matches!(wparam.0, 0x0A | 0x0D | 0x1B) => return LRESULT(0),
        _ => {}
    }
    // SAFETY: NoteField::create stored the class's procedure in the user data
    // before installing this one, and a WNDPROC is a nullable function
    // pointer, the same size as the isize it was stored as.
    unsafe {
        let previous: WNDPROC = std::mem::transmute(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
        CallWindowProcW(previous, hwnd, message, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::model::app::Input;
    use crate::model::geometry::{PixelRect, PixelSize};
    use crate::viewer::window::{Handler, Window};
    use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;

    /// Records the note text and keys the window receives.
    struct Recorder(Rc<RefCell<Vec<Input>>>);

    impl Handler for Recorder {
        fn paint(&mut self) {}
        fn resize(&mut self, _: PixelSize) {}
        fn decoded(&mut self) {}
        fn device_ready(&mut self) {}
        fn requests(&mut self) {}
        fn timer(&mut self, _: usize) {}
        fn input(&mut self, input: Input) {
            self.0.borrow_mut().push(input);
        }
    }

    fn send(hwnd: HWND, message: u32, wparam: usize) {
        // SAFETY: the window is live; the message is handled synchronously.
        unsafe { SendMessageW(hwnd, message, Some(WPARAM(wparam)), Some(LPARAM(0))) };
    }

    #[test]
    fn the_field_echoes_text_and_passes_finishing_keys_to_the_window() {
        let window = Window::create(PixelRect::new(10, 10, 320, 200), "note").unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));
        window.set_handler(Box::new(Recorder(Rc::clone(&log))));
        // A test program has no manifest, so its children cannot be layered,
        // and the field falls back to an ordinary child.
        assert!(create_edit(window.hwnd(), true).is_err());
        let mut field = NoteField::create(window.hwnd()).unwrap();
        // SAFETY: GetWindowLongPtrW checks the handle.
        let extended = unsafe { GetWindowLongPtrW(field.hwnd(), GWL_EXSTYLE) };
        assert_eq!(extended as u32 & WS_EX_LAYERED.0, 0);
        assert!(!field.is_shown());

        // Text set by the viewer is echoed into the control and reported back
        // as typed.
        assert!(field.show(Rect::new(20.0, 30.0, 240.0, 28.0), "typed"));
        assert!(field.is_shown());
        assert_eq!(field.text(), "typed");
        assert!(!field.show(Rect::new(40.0, 30.0, 240.0, 28.0), "typed"));
        field.focus();
        // Typing a character in the control goes to the control.
        send(field.hwnd(), WM_CHAR, usize::from(b'!'));
        assert_eq!(field.text(), "typed!");
        // Enter and Escape go to the window, and type nothing.
        send(field.hwnd(), WM_KEYDOWN, usize::from(VK_RETURN.0));
        send(field.hwnd(), WM_CHAR, 0x0D);
        send(field.hwnd(), WM_KEYDOWN, usize::from(VK_ESCAPE.0));
        send(field.hwnd(), WM_CHAR, 0x1B);
        assert_eq!(field.text(), "typed!");
        field.hide();
        assert!(!field.is_shown());

        let log = log.borrow();
        let notes: Vec<&str> = log
            .iter()
            .filter_map(|input| match input {
                Input::NoteText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(notes, ["typed", "typed!"]);
        let keys: Vec<String> = log
            .iter()
            .filter_map(|input| match input {
                Input::Key { key, .. } => Some(key.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, ["enter", "escape"]);
        assert_eq!(text(HWND::default()), "");
    }
}
