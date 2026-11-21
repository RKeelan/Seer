//! A small owning wrapper over the Win32 registry, for the few keys and
//! values Seer reads and writes under `HKEY_CURRENT_USER`.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_NONE, REG_OPTION_NON_VOLATILE,
    REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW,
    RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::{HSTRING, PWSTR};

use crate::error::{Error, Result};

/// A value's data, for the kinds Seer uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// `REG_SZ`.
    String(String),
    /// `REG_EXPAND_SZ`: a string whose `%VARIABLES%` are expanded by readers.
    ExpandString(String),
    /// `REG_NONE` with no data, the form `OpenWithProgids` entries take.
    None,
    /// Any other type, as its type number and raw bytes.
    Other(u32, Vec<u8>),
}

/// An open registry key, closed when dropped.
#[derive(Debug)]
pub struct Key(HKEY);

impl Key {
    /// Opens `path` under `HKEY_CURRENT_USER` for reading and writing,
    /// creating it and any missing parents.
    pub fn create(path: &str) -> Result<Key> {
        let mut key = HKEY::default();
        // SAFETY: the path outlives the call, and the result pointer is to a
        // live local.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut key,
                None,
            )
        };
        check(status, path)?;
        Ok(Key(key))
    }

    /// Opens `path` under `HKEY_CURRENT_USER`, or `None` if it does not exist.
    pub fn open(path: &str, writable: bool) -> Result<Option<Key>> {
        let access: REG_SAM_FLAGS = if writable {
            KEY_READ | KEY_WRITE
        } else {
            KEY_READ
        };
        let mut key = HKEY::default();
        // SAFETY: the path outlives the call, and the result pointer is to a
        // live local.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                None,
                access,
                &mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(status, path)?;
        Ok(Some(Key(key)))
    }

    /// Sets the value `name` (empty for the key's default value).
    pub fn set(&self, name: &str, value: &Value) -> Result<()> {
        let (kind, data) = match value {
            Value::String(text) => (REG_SZ, wide_bytes(text)),
            Value::ExpandString(text) => (REG_EXPAND_SZ, wide_bytes(text)),
            Value::None => (REG_NONE, Vec::new()),
            Value::Other(kind, data) => (REG_VALUE_TYPE(*kind), data.clone()),
        };
        // SAFETY: the key is open, and the name and data outlive the call.
        let status =
            unsafe { RegSetValueExW(self.0, &HSTRING::from(name), None, kind, Some(&data)) };
        check(status, name)
    }

    /// Reads the value `name`, or `None` if it does not exist.
    pub fn get(&self, name: &str) -> Result<Option<Value>> {
        let name_wide = HSTRING::from(name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        // SAFETY: the key is open, and the pointers are to live locals; with
        // no buffer the call reports only the type and size.
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                &name_wide,
                None,
                Some(&mut kind),
                None,
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(status, name)?;
        let mut data = vec![0u8; size as usize];
        // SAFETY: as above, with a buffer of the size just reported.
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                &name_wide,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut size),
            )
        };
        check(status, name)?;
        data.truncate(size as usize);
        Ok(Some(value_from(kind, data)))
    }

    /// Deletes the value `name`. Returns whether it existed.
    pub fn delete_value(&self, name: &str) -> Result<bool> {
        // SAFETY: the key is open and the name outlives the call.
        let status = unsafe { RegDeleteValueW(self.0, &HSTRING::from(name)) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        check(status, name)?;
        Ok(true)
    }

    /// The names of the key's subkeys.
    pub fn subkeys(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for index in 0.. {
            let mut buffer = [0u16; 256];
            let mut length = buffer.len() as u32;
            // SAFETY: the key is open, and the buffer and length are live
            // locals with the length giving the buffer's size.
            let status = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    Some(PWSTR(buffer.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }
            names.push(String::from_utf16_lossy(&buffer[..length as usize]));
        }
        Ok(names)
    }

    /// The key's values, by name.
    pub fn values(&self) -> Result<Vec<(String, Value)>> {
        let mut values = Vec::new();
        for index in 0.. {
            let mut name = [0u16; 16384];
            let mut length = name.len() as u32;
            // SAFETY: the key is open, and the buffer and length are live
            // locals with the length giving the buffer's size. Only the name
            // is read here.
            let status = unsafe {
                RegEnumValueW(
                    self.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }
            let name = String::from_utf16_lossy(&name[..length as usize]);
            if let Some(value) = self.get(&name)? {
                values.push((name, value));
            }
        }
        Ok(values)
    }

    /// Whether the key has neither subkeys nor values.
    pub fn is_empty(&self) -> Result<bool> {
        let (mut subkeys, mut values) = (0u32, 0u32);
        // SAFETY: the key is open and the counts are live locals.
        let status = unsafe {
            RegQueryInfoKeyW(
                self.0,
                None,
                None,
                None,
                Some(&mut subkeys),
                None,
                None,
                Some(&mut values),
                None,
                None,
                None,
                None,
            )
        };
        check(status, "key")?;
        Ok(subkeys == 0 && values == 0)
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by this wrapper and is closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Deletes `path` under `HKEY_CURRENT_USER` with everything beneath it.
/// Returns whether it existed.
///
/// As a guard against deleting a whole branch of the user's registry by
/// mistake, `path` must name a key at least two levels down, such as
/// `Software\Seer`.
pub fn delete_tree(path: &str) -> Result<bool> {
    if path.split('\\').filter(|part| !part.is_empty()).count() < 2 {
        return Err(Error::unexpected(format!(
            "refusing to delete the registry key {path:?}"
        )));
    }
    // SAFETY: the path outlives the call. Naming a subkey deletes that key
    // as well as everything beneath it.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(path)) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(status, path)?;
    Ok(true)
}

/// Deletes `path` under `HKEY_CURRENT_USER` if it exists and is empty, then
/// does the same for each parent up to, but not including, `stop`, which
/// must be a prefix of `path`.
pub fn delete_empty(path: &str, stop: &str) -> Result<()> {
    let mut path = path.trim_end_matches('\\');
    while path.len() > stop.len() && path.starts_with(stop) {
        match Key::open(path, false)? {
            Some(key) if key.is_empty()? => {
                drop(key);
                // SAFETY: the path outlives the call.
                let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(path)) };
                check(status, path)?;
            }
            Some(_) => return Ok(()),
            None => {}
        }
        path = match path.rfind('\\') {
            Some(end) => &path[..end],
            None => return Ok(()),
        };
    }
    Ok(())
}

fn check(status: WIN32_ERROR, what: &str) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        let error = windows::core::Error::from(status.to_hresult());
        Err(Error::unexpected(format!(
            "registry: {what}: {} ({})",
            error.message(),
            error.code()
        )))
    }
}

/// A string as UTF-16 bytes with a terminating null, as `REG_SZ` stores it.
fn wide_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn value_from(kind: REG_VALUE_TYPE, data: Vec<u8>) -> Value {
    let text = || {
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        let end = units
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    };
    match kind {
        REG_SZ => Value::String(text()),
        REG_EXPAND_SZ => Value::ExpandString(text()),
        REG_NONE if data.is_empty() => Value::None,
        other => Value::Other(other.0, data),
    }
}

/// A throwaway key under `HKEY_CURRENT_USER\Software` for tests, deleted with
/// everything beneath it when dropped, even if the test fails.
#[cfg(any(test, feature = "testutil"))]
#[derive(Debug)]
pub struct TestRoot {
    path: String,
}

#[cfg(any(test, feature = "testutil"))]
impl TestRoot {
    /// Creates `HKEY_CURRENT_USER\Software\Seer-Test-<unique>`.
    pub fn new() -> Result<Self> {
        let path = format!(r"Software\Seer-Test-{}", crate::testutil::unique_instance());
        Key::create(&path)?;
        Ok(TestRoot { path })
    }

    /// The key's path under `HKEY_CURRENT_USER`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Every key and value beneath the root, as sorted lines of
    /// `key\path` and `key\path : name = value`, with paths relative to the
    /// root.
    pub fn dump(&self) -> Vec<String> {
        let mut lines = Vec::new();
        dump_into(&self.path, "", &mut lines);
        lines.sort();
        lines
    }
}

#[cfg(any(test, feature = "testutil"))]
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = delete_tree(&self.path);
    }
}

#[cfg(any(test, feature = "testutil"))]
fn dump_into(root: &str, relative: &str, lines: &mut Vec<String>) {
    let full = if relative.is_empty() {
        root.to_owned()
    } else {
        format!(r"{root}\{relative}")
    };
    let Ok(Some(key)) = Key::open(&full, false) else {
        return;
    };
    if !relative.is_empty() {
        lines.push(relative.to_owned());
    }
    for (name, value) in key.values().unwrap_or_default() {
        let shown = match value {
            Value::String(text) => format!("{text:?}"),
            Value::ExpandString(text) => format!("expand {text:?}"),
            Value::None => "none".to_owned(),
            Value::Other(kind, data) => format!("type {kind} {data:?}"),
        };
        lines.push(format!("{relative} : {name} = {shown}"));
    }
    for subkey in key.subkeys().unwrap_or_default() {
        let child = if relative.is_empty() {
            subkey
        } else {
            format!(r"{relative}\{subkey}")
        };
        dump_into(root, &child, lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
        let root = TestRoot::new().unwrap();
        let key = Key::create(&format!(r"{}\A\B", root.path())).unwrap();
        let values = [
            ("", Value::String("default".into())),
            ("text", Value::String("héllo".into())),
            ("empty", Value::String(String::new())),
            ("expand", Value::ExpandString(r"%TEMP%\x".into())),
            ("none", Value::None),
            ("dword", Value::Other(4, vec![1, 0, 0, 0])),
        ];
        for (name, value) in &values {
            key.set(name, value).unwrap();
        }
        for (name, value) in &values {
            assert_eq!(key.get(name).unwrap().as_ref(), Some(value), "{name}");
        }
        assert_eq!(key.get("missing").unwrap(), None);
        assert_eq!(key.values().unwrap().len(), values.len());
        assert!(key.delete_value("text").unwrap());
        assert!(!key.delete_value("text").unwrap());
        assert!(!key.is_empty().unwrap());
    }

    #[test]
    fn keys_are_listed_and_deleted() {
        let root = TestRoot::new().unwrap();
        let base = root.path();
        Key::create(&format!(r"{base}\Tree\One\Deep")).unwrap();
        Key::create(&format!(r"{base}\Tree\Two")).unwrap();
        let tree = Key::open(&format!(r"{base}\Tree"), false).unwrap().unwrap();
        let mut subkeys = tree.subkeys().unwrap();
        subkeys.sort();
        assert_eq!(subkeys, ["One", "Two"]);
        drop(tree);

        assert!(delete_tree(&format!(r"{base}\Tree\One")).unwrap());
        assert!(!delete_tree(&format!(r"{base}\Tree\One")).unwrap());
        assert!(delete_tree("Software").is_err());
        assert!(delete_tree(r"\Software\").is_err());
        assert!(
            Key::open(&format!(r"{base}\Missing"), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(root.dump(), ["Tree", r"Tree\Two"]);
    }

    #[test]
    fn empty_keys_are_pruned_up_to_the_stop() {
        let root = TestRoot::new().unwrap();
        let base = root.path();
        Key::create(&format!(r"{base}\Kept\Empty\Emptier")).unwrap();
        Key::create(&format!(r"{base}\Kept"))
            .unwrap()
            .set("value", &Value::String("x".into()))
            .unwrap();
        delete_empty(&format!(r"{base}\Kept\Empty\Emptier"), base).unwrap();
        assert_eq!(root.dump(), ["Kept", r#"Kept : value = "x""#]);

        // Nothing above the stop is touched, and a missing key is passed over.
        Key::create(&format!(r"{base}\Lone")).unwrap();
        delete_empty(&format!(r"{base}\Lone\Missing"), base).unwrap();
        assert!(Key::open(base, false).unwrap().is_some());
        assert_eq!(root.dump(), ["Kept", r#"Kept : value = "x""#]);
    }

    #[test]
    fn errors_name_what_failed() {
        let error = check(ERROR_FILE_NOT_FOUND, "Somewhere").unwrap_err();
        assert!(error.message.contains("Somewhere"), "{error}");
    }

    #[test]
    fn the_test_root_is_removed_when_dropped() {
        let root = TestRoot::new().unwrap();
        let path = root.path().to_owned();
        Key::create(&format!(r"{path}\Child")).unwrap();
        drop(root);
        assert!(Key::open(&path, false).unwrap().is_none());
    }
}
