//! Following the system's light or dark setting for apps, including the
//! title bar.
//!
//! The setting is the `AppsUseLightTheme` value under the user's
//! `Personalize` key, read once before the first frame (it is the only
//! registry read start-up makes) and again whenever Windows broadcasts
//! `WM_SETTINGCHANGE` with `ImmersiveColorSet`, which it does when the
//! setting changes.

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

/// Where Windows keeps the light or dark setting, under `HKEY_CURRENT_USER`.
pub const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// The `lParam` string of the `WM_SETTINGCHANGE` that announces a change to
/// the light or dark setting.
const IMMERSIVE_COLOR_SET: &str = "ImmersiveColorSet";

/// Whether apps should be dark, per the user's setting. Windows treats a
/// missing setting as light.
pub fn apps_dark() -> bool {
    apps_dark_in(PERSONALIZE)
}

/// Whether apps should be dark, per the `AppsUseLightTheme` value under
/// `key`, a path under `HKEY_CURRENT_USER`.
pub fn apps_dark_in(key: &str) -> bool {
    let mut light = 1u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the strings outlive the call, and the data pointer and size are
    // for a live local `u32`, the only type RRF_RT_REG_DWORD accepts.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut light).cast()),
            Some(&mut size),
        )
    };
    status.is_ok() && light == 0
}

/// Gives the window a dark or light title bar and frame.
pub fn set_title_bar(hwnd: HWND, dark: bool) {
    let value = BOOL::from(dark);
    // SAFETY: the attribute's data is a live local BOOL of the size given.
    // Failure, as on Windows versions without the attribute, leaves the
    // default title bar.
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const value).cast(),
            size_of::<BOOL>() as u32,
        )
    };
}

/// Whether a `WM_SETTINGCHANGE` with this `lParam` announces a change to the
/// light or dark setting.
pub fn is_theme_change(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return false;
    }
    // SAFETY: a non-zero lParam of WM_SETTINGCHANGE points to a
    // null-terminated string naming the changed area, valid for the call.
    let name = unsafe { PCWSTR(lparam.0 as *const u16).to_string() };
    name.is_ok_and(|name| name == IMMERSIVE_COLOR_SET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Key, TestRoot, Value};

    #[test]
    fn the_setting_is_read_from_the_personalize_key() {
        let root = TestRoot::new().unwrap();
        let key = Key::create(root.path()).unwrap();
        // Missing means light.
        assert!(!apps_dark_in(root.path()));
        key.set(
            "AppsUseLightTheme",
            &Value::Other(4, 0u32.to_le_bytes().to_vec()),
        )
        .unwrap();
        assert!(apps_dark_in(root.path()));
        key.set(
            "AppsUseLightTheme",
            &Value::Other(4, 1u32.to_le_bytes().to_vec()),
        )
        .unwrap();
        assert!(!apps_dark_in(root.path()));
        // A value of the wrong type is ignored.
        key.set("AppsUseLightTheme", &Value::String("0".into()))
            .unwrap();
        assert!(!apps_dark_in(root.path()));
        // The real setting can be read, whatever it is.
        let _ = apps_dark();
    }

    #[test]
    fn only_the_colour_set_change_is_a_theme_change() {
        let colour = HSTRING::from("ImmersiveColorSet");
        let other = HSTRING::from("Environment");
        assert!(is_theme_change(LPARAM(colour.as_ptr() as isize)));
        assert!(!is_theme_change(LPARAM(other.as_ptr() as isize)));
        assert!(!is_theme_change(LPARAM(0)));
    }
}
