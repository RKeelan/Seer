//! `seer install` and `seer uninstall`: copying the executables into the
//! user's programs directory, adding it to the user's `PATH`, and registering
//! the file associations, all per user and without administrator rights.
//!
//! Installing registers, under `HKEY_CURRENT_USER\Software\Classes`:
//!
//! - A ProgID per format family, such as `Seer.JPEG`, whose open verb runs
//!   `"…\seerw.exe" "%1"` with `MultiSelectModel=Player`, so the verb is
//!   offered for selections of any size. The verb's `DelegateExecute` names
//!   [`CLSID`], so Explorer hands a whole selection, in order, to one
//!   `seerw.exe` (see [`crate::viewer::execute`]); the command line is for
//!   programs that read it rather than invoke the verb.
//! - `CLSID\{…}`, whose `LocalServer32` is `"…\seerw.exe"`.
//! - The ProgID under each of the family's extensions' `OpenWithProgids`, so
//!   Seer appears in Open with.
//! - `Applications\seerw.exe`, with its `SupportedTypes`.
//!
//! and, so Seer appears in Default Apps, `Software\Seer\Capabilities` with a
//! `Software\RegisteredApplications` entry pointing to it. It never makes
//! Seer the default for any type: Windows asks the user to choose.
//!
//! It also writes `Seer.lnk`, a shortcut to `seerw.exe`, to the user's Start
//! menu programs folder, so that Start search finds Seer.
//!
//! Last, it adds an uninstall entry,
//! `Software\Microsoft\Windows\CurrentVersion\Uninstall\RKeelan.Seer`, so Seer
//! appears in Settings > Apps > Installed apps, which runs `seer uninstall` to
//! remove it. The entry is also how winget, which installs Seer by running
//! `seer install` from the release zip, recognises it as installed and reads
//! its version.
//!
//! Uninstalling removes all of it, leaving other programs' entries under the
//! same extensions alone.
//!
//! Every location is a field of [`Target`], so tests install into a
//! throwaway registry key and directories.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IPersistFile,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
    SHChangeNotify, SHGetKnownFolderPath, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows::core::{HSTRING, Interface, w};

use crate::error::{Error, Result};
use crate::registry::{self, Key, Value};
use crate::viewer::execute::{CLSID, registry_form};

/// The executables that are installed.
pub const EXECUTABLES: [&str; 2] = ["seer.exe", "seerw.exe"];

/// The executable file associations open.
pub const VIEWER: &str = "seerw.exe";

/// The name Seer registers itself under.
const APPLICATION: &str = "Seer";

/// The name of Seer's uninstall entry, which is also its winget package
/// identifier and the `ProductCode` its winget manifest names.
pub const PACKAGE: &str = "RKeelan.Seer";

/// The publisher the uninstall entry names, as the winget manifest does.
const PUBLISHER: &str = "RKeelan";

/// The Start menu shortcut's description, which Start shows as its tooltip.
const SHORTCUT_DESCRIPTION: &str = "Image viewer";

/// A family of formats that shares one ProgID.
#[derive(Clone, Copy, Debug)]
pub struct Family {
    /// The ProgID, such as `Seer.JPEG`.
    pub prog_id: &'static str,
    /// What Explorer calls a file of this type.
    pub name: &'static str,
    /// The extensions, with their dots.
    pub extensions: &'static [&'static str],
}

/// The formats Seer registers for: those the Windows Imaging Component
/// decodes, including the ones that need a codec extension from the Store.
pub const FAMILIES: [Family; 11] = [
    Family {
        prog_id: "Seer.JPEG",
        name: "JPEG image",
        extensions: &[".jpg", ".jpeg", ".jpe", ".jfif"],
    },
    Family {
        prog_id: "Seer.PNG",
        name: "PNG image",
        extensions: &[".png"],
    },
    Family {
        prog_id: "Seer.BMP",
        name: "Bitmap image",
        extensions: &[".bmp", ".dib"],
    },
    Family {
        prog_id: "Seer.GIF",
        name: "GIF image",
        extensions: &[".gif"],
    },
    Family {
        prog_id: "Seer.TIFF",
        name: "TIFF image",
        extensions: &[".tif", ".tiff"],
    },
    Family {
        prog_id: "Seer.ICO",
        name: "Icon",
        extensions: &[".ico"],
    },
    Family {
        prog_id: "Seer.JXR",
        name: "JPEG XR image",
        extensions: &[".jxr", ".wdp", ".hdp"],
    },
    Family {
        prog_id: "Seer.WebP",
        name: "WebP image",
        extensions: &[".webp"],
    },
    Family {
        prog_id: "Seer.HEIF",
        name: "HEIF image",
        extensions: &[".heic", ".heif"],
    },
    Family {
        prog_id: "Seer.AVIF",
        name: "AVIF image",
        extensions: &[".avif"],
    },
    Family {
        prog_id: "Seer.JXL",
        name: "JPEG XL image",
        extensions: &[".jxl"],
    },
];

/// Where an installation goes.
#[derive(Clone, Debug)]
pub struct Target {
    /// The key under `HKEY_CURRENT_USER` that stands for `Software`: it holds
    /// `Classes`, `RegisteredApplications` and `Seer`.
    pub software: String,
    /// The key under `HKEY_CURRENT_USER` whose `Path` value is the user's
    /// `PATH`.
    pub environment: String,
    /// The directory the executables are copied to.
    pub directory: PathBuf,
    /// Where an executable that cannot be deleted because it is running is
    /// moved to instead. It must be on the same volume as `directory`.
    pub trash: PathBuf,
    /// The Start menu directory the shortcut to the viewer goes in.
    pub shortcuts: PathBuf,
}

impl Target {
    /// The real locations for the current user: `HKEY_CURRENT_USER\Software`,
    /// `HKEY_CURRENT_USER\Environment`, `%LOCALAPPDATA%\Programs\Seer` and the
    /// user's Start menu programs folder.
    pub fn for_user() -> Result<Target> {
        let local = std::env::var_os("LOCALAPPDATA")
            .filter(|local| !local.is_empty())
            .ok_or_else(|| Error::unexpected("LOCALAPPDATA is not set"))?;
        Ok(Target {
            software: "Software".to_owned(),
            environment: "Environment".to_owned(),
            directory: PathBuf::from(local).join("Programs").join(APPLICATION),
            trash: std::env::temp_dir(),
            shortcuts: start_menu_programs()?,
        })
    }

    /// The Start menu shortcut to the viewer.
    pub fn shortcut(&self) -> PathBuf {
        self.shortcuts.join(format!("{APPLICATION}.lnk"))
    }

    fn classes(&self) -> String {
        format!(r"{}\Classes", self.software)
    }

    fn application(&self) -> String {
        format!(r"{}\Applications\{VIEWER}", self.classes())
    }

    fn capabilities(&self) -> String {
        format!(r"{}\{APPLICATION}\Capabilities", self.software)
    }

    fn class(&self) -> String {
        format!(r"{}\CLSID\{}", self.classes(), registry_form(&CLSID))
    }

    fn registered_applications(&self) -> String {
        format!(r"{}\RegisteredApplications", self.software)
    }

    fn uninstall_entry(&self) -> String {
        format!(
            r"{}\Microsoft\Windows\CurrentVersion\Uninstall\{PACKAGE}",
            self.software
        )
    }

    fn viewer(&self) -> PathBuf {
        self.directory.join(VIEWER)
    }
}

/// What [`install`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Installed {
    /// Where the executables are.
    pub directory: PathBuf,
    /// Whether the directory was added to `PATH`; false if it was there
    /// already.
    pub path_added: bool,
}

/// What [`uninstall`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Uninstalled {
    /// Where the executables were.
    pub directory: PathBuf,
    /// Whether the directory was removed from `PATH`; false if it was not
    /// there.
    pub path_removed: bool,
}

/// Copies the executables from `source` into the target directory, adds it
/// to `PATH`, registers the file associations, and adds the Start menu
/// shortcut. Installing again replaces the executables, including running
/// ones, and the shortcut, and leaves the rest as it was.
pub fn install(target: &Target, source: &Path) -> Result<Installed> {
    copy_executables(source, target)?;
    register(target)?;
    let path_added = edit_path(target, |path| path_with(path, &target.directory))?;
    create_shortcut(target)?;
    add_uninstall_entry(target)?;
    Ok(Installed {
        directory: target.directory.clone(),
        path_added,
    })
}

/// Removes what [`install`] added.
pub fn uninstall(target: &Target) -> Result<Uninstalled> {
    unregister(target)?;
    let path_removed = edit_path(target, |path| path_without(path, &target.directory))?;
    remove_shortcut(target)?;
    remove_executables(target)?;
    Ok(Uninstalled {
        directory: target.directory.clone(),
        path_removed,
    })
}

/// Tells Explorer that file associations changed, and running programs that
/// the environment did, so both take effect without signing out.
pub fn announce() {
    // SAFETY: SHCNE_ASSOCCHANGED takes no items.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    // SAFETY: the string is a static, null-terminated literal, read by each
    // window during the call. Windows that do not answer are skipped.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        )
    };
}

fn copy_executables(source: &Path, target: &Target) -> Result<()> {
    if let Some(missing) = EXECUTABLES
        .iter()
        .map(|name| source.join(name))
        .find(|from| !from.is_file())
    {
        return Err(Error::unexpected(format!(
            "{} is missing; seer install copies it from beside seer.exe",
            missing.display()
        )));
    }
    fs::create_dir_all(&target.directory)
        .map_err(|error| file_error("could not create", &target.directory, &error))?;
    for name in EXECUTABLES {
        let from = source.join(name);
        let to = target.directory.join(name);
        if same_file(&from, &to) {
            continue;
        }
        discard(&to, &target.trash)?;
        fs::copy(&from, &to).map_err(|error| file_error("could not copy to", &to, &error))?;
    }
    Ok(())
}

fn remove_executables(target: &Target) -> Result<()> {
    for name in EXECUTABLES {
        discard(&target.directory.join(name), &target.trash)?;
    }
    // Anything else in the directory is not Seer's to delete.
    let _ = fs::remove_dir(&target.directory);
    Ok(())
}

/// Deletes a file if it exists. A running executable cannot be deleted but
/// can be renamed, so one that will not delete is moved into `trash` instead,
/// for the system's temporary-file clean-up to remove.
fn discard(path: &Path, trash: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    let mut name = OsString::from(path.file_name().unwrap_or_default());
    name.push(format!(".{}.old", unique_suffix()));
    fs::rename(path, trash.join(name))
        .map_err(|error| file_error("could not remove or replace", path, &error))
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The user's Start menu programs folder.
fn start_menu_programs() -> Result<PathBuf> {
    // SAFETY: the folder id is a constant and no access token is given.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None) }?;
    // SAFETY: the shell returned a valid, null-terminated string.
    let copied = unsafe { path.to_hstring() };
    // SAFETY: the shell allocated the string with the COM allocator, and
    // nothing uses it after this.
    unsafe { CoTaskMemFree(Some(path.as_ptr().cast_const().cast())) };
    Ok(PathBuf::from(copied.to_os_string()))
}

/// Initialises COM on this thread, if it is not already, and creates a shell
/// link object.
fn shell_link() -> Result<IShellLinkW> {
    // SAFETY: initialising COM has no preconditions. It reports S_FALSE or
    // RPC_E_CHANGED_MODE when COM is already initialised on this thread, and
    // COM is usable either way, so the result is ignored.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: the class id is a constant and no outer object is given.
    Ok(unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }?)
}

/// Writes the Start menu shortcut to the viewer, replacing any already there.
fn create_shortcut(target: &Target) -> Result<()> {
    fs::create_dir_all(&target.shortcuts)
        .map_err(|error| file_error("could not create", &target.shortcuts, &error))?;
    let viewer = HSTRING::from(target.viewer().as_os_str());
    let directory = HSTRING::from(target.directory.as_os_str());
    let shortcut = target.shortcut();
    let link = shell_link()?;
    // SAFETY: each string outlives the call that reads it.
    unsafe {
        link.SetPath(&viewer)?;
        link.SetWorkingDirectory(&directory)?;
        link.SetIconLocation(&viewer, 0)?;
        link.SetDescription(&HSTRING::from(SHORTCUT_DESCRIPTION))?;
    }
    let file: IPersistFile = link.cast()?;
    // SAFETY: the path outlives the call.
    unsafe { file.Save(&HSTRING::from(shortcut.as_os_str()), true) }.map_err(|error| {
        Error::unexpected(format!("could not write {}: {error}", shortcut.display()))
    })
}

fn remove_shortcut(target: &Target) -> Result<()> {
    let shortcut = target.shortcut();
    match fs::remove_file(&shortcut) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(file_error("could not remove", &shortcut, &error))
        }
        _ => Ok(()),
    }
}

fn file_error(what: &str, path: &Path, error: &io::Error) -> Error {
    Error::unexpected(format!("{what} {}: {error}", path.display()))
}

fn register(target: &Target) -> Result<()> {
    let viewer = target.viewer();
    let icon = Value::String(format!("{},0", viewer.display()));
    let command = Value::String(format!("\"{}\" \"%1\"", viewer.display()));
    let delegate = Value::String(registry_form(&CLSID));
    let classes = target.classes();

    let class = target.class();
    Key::create(&class)?.set("", &Value::String(format!("{APPLICATION} open verb")))?;
    Key::create(&format!(r"{class}\LocalServer32"))?
        .set("", &Value::String(format!("\"{}\"", viewer.display())))?;

    // The open verb, as both the ProgIDs and the application key carry it.
    let verb = |key: &str| -> Result<()> {
        Key::create(&format!(r"{key}\DefaultIcon"))?.set("", &icon)?;
        Key::create(&format!(r"{key}\shell\open"))?
            .set("MultiSelectModel", &Value::String("Player".into()))?;
        let verb_command = Key::create(&format!(r"{key}\shell\open\command"))?;
        verb_command.set("", &command)?;
        verb_command.set("DelegateExecute", &delegate)?;
        Ok(())
    };

    let application = target.application();
    Key::create(&application)?.set("FriendlyAppName", &Value::String(APPLICATION.into()))?;
    verb(&application)?;
    let supported = Key::create(&format!(r"{application}\SupportedTypes"))?;

    let capabilities = target.capabilities();
    let capabilities_key = Key::create(&capabilities)?;
    capabilities_key.set("ApplicationName", &Value::String(APPLICATION.into()))?;
    capabilities_key.set(
        "ApplicationDescription",
        &Value::String(env!("CARGO_PKG_DESCRIPTION").into()),
    )?;
    capabilities_key.set("ApplicationIcon", &icon)?;
    let associations = Key::create(&format!(r"{capabilities}\FileAssociations"))?;

    for family in FAMILIES {
        let prog_id = format!(r"{classes}\{}", family.prog_id);
        Key::create(&prog_id)?.set("", &Value::String(family.name.into()))?;
        verb(&prog_id)?;
        for extension in family.extensions {
            Key::create(&format!(r"{classes}\{extension}\OpenWithProgids"))?
                .set(family.prog_id, &Value::None)?;
            supported.set(extension, &Value::String(String::new()))?;
            associations.set(extension, &Value::String(family.prog_id.into()))?;
        }
    }

    Key::create(&target.registered_applications())?
        .set(APPLICATION, &Value::String(capabilities))?;
    Ok(())
}

/// Adds the entry Settings > Apps and winget read. It is added last, so that
/// an installation that failed part way does not look finished.
fn add_uninstall_entry(target: &Target) -> Result<()> {
    let seer = target.directory.join(EXECUTABLES[0]);
    let uninstall = Value::String(format!("\"{}\" uninstall", seer.display()));
    let entry = Key::create(&target.uninstall_entry())?;
    entry.set("DisplayName", &Value::String(APPLICATION.into()))?;
    entry.set(
        "DisplayVersion",
        &Value::String(env!("CARGO_PKG_VERSION").into()),
    )?;
    entry.set("Publisher", &Value::String(PUBLISHER.into()))?;
    entry.set(
        "DisplayIcon",
        &Value::String(format!("{},0", target.viewer().display())),
    )?;
    entry.set(
        "InstallLocation",
        &Value::String(target.directory.display().to_string()),
    )?;
    entry.set(
        "URLInfoAbout",
        &Value::String(env!("CARGO_PKG_REPOSITORY").into()),
    )?;
    entry.set("UninstallString", &uninstall)?;
    entry.set("QuietUninstallString", &uninstall)?;
    Ok(())
}

fn unregister(target: &Target) -> Result<()> {
    let software = &target.software;
    let entry = target.uninstall_entry();
    registry::delete_tree(&entry)?;
    registry::delete_empty(&entry, software)?;
    let classes = target.classes();
    for family in FAMILIES {
        registry::delete_tree(&format!(r"{classes}\{}", family.prog_id))?;
        for extension in family.extensions {
            let with = format!(r"{classes}\{extension}\OpenWithProgids");
            if let Some(key) = Key::open(&with, true)? {
                key.delete_value(family.prog_id)?;
            }
            registry::delete_empty(&with, software)?;
        }
    }
    registry::delete_tree(&target.class())?;
    registry::delete_empty(&format!(r"{classes}\CLSID"), software)?;
    registry::delete_tree(&target.application())?;
    registry::delete_empty(&format!(r"{classes}\Applications"), software)?;
    registry::delete_tree(&target.capabilities())?;
    registry::delete_empty(&format!(r"{software}\{APPLICATION}"), software)?;
    let registered = target.registered_applications();
    if let Some(key) = Key::open(&registered, true)? {
        key.delete_value(APPLICATION)?;
    }
    registry::delete_empty(&registered, software)?;
    Ok(())
}

/// Rewrites the user's `PATH` with `edit`, which returns the new value or
/// `None` to leave it alone. Returns whether it changed. The value keeps its
/// type; a new one is `REG_EXPAND_SZ`, as Windows makes it, and one that
/// becomes empty is deleted.
fn edit_path(target: &Target, edit: impl Fn(&str) -> Option<String>) -> Result<bool> {
    let key = Key::create(&target.environment)?;
    let (current, expand) = match key.get("Path")? {
        None => (String::new(), true),
        Some(Value::ExpandString(path)) => (path, true),
        Some(Value::String(path)) => (path, false),
        Some(_) => {
            return Err(Error::unexpected(
                "the user's PATH is not a string, so Seer leaves it alone",
            ));
        }
    };
    let Some(path) = edit(&current) else {
        return Ok(false);
    };
    if path.is_empty() {
        key.delete_value("Path")?;
    } else if expand {
        key.set("Path", &Value::ExpandString(path))?;
    } else {
        key.set("Path", &Value::String(path))?;
    }
    Ok(true)
}

/// `path` with `directory` appended, or `None` if it is there already.
pub fn path_with(path: &str, directory: &Path) -> Option<String> {
    let directory = directory.display().to_string();
    if path.split(';').any(|entry| same_entry(entry, &directory)) {
        return None;
    }
    let path = path.trim_end_matches(';');
    Some(if path.is_empty() {
        directory
    } else {
        format!("{path};{directory}")
    })
}

/// `path` without any entry naming `directory`, or `None` if it has none.
pub fn path_without(path: &str, directory: &Path) -> Option<String> {
    let directory = directory.display().to_string();
    let entries: Vec<&str> = path.split(';').collect();
    let kept: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| !same_entry(entry, &directory))
        .collect();
    (kept.len() != entries.len()).then(|| kept.join(";"))
}

/// Whether a `PATH` entry names the directory, ignoring case, quotes and a
/// trailing separator.
fn same_entry(entry: &str, directory: &str) -> bool {
    let normal = |text: &str| {
        text.trim()
            .trim_matches('"')
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    !entry.trim().is_empty() && normal(entry) == normal(directory)
}

/// A short report of an installation, for people.
pub fn describe_installed(installed: &Installed) -> String {
    let directory = installed.directory.display();
    let mut text = format!(
        "Installed {} to {directory}.\nSeer is now in the Start menu, and in Open with for its \
         image types; \
         to make it the default, choose it in Settings > Apps > Default apps.\n",
        EXECUTABLES.join(" and ")
    );
    if installed.path_added {
        text.push_str(&format!(
            "Added {directory} to your PATH; terminals opened from now on will find seer.\n"
        ));
    }
    text
}

/// A short report of an uninstallation, for people.
pub fn describe_uninstalled(uninstalled: &Uninstalled) -> String {
    let directory = uninstalled.directory.display();
    let mut text =
        format!("Removed Seer from {directory}, the Start menu and its file associations.\n");
    if uninstalled.path_removed {
        text.push_str(&format!("Removed {directory} from your PATH.\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::MAX_PATH;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::UI::Shell::SLGP_RAWPATH;

    use crate::registry::TestRoot;
    use crate::testutil::TempDir;

    /// A target in a throwaway registry key and directory, with a `PATH`
    /// already holding two entries.
    struct Sandbox {
        root: TestRoot,
        files: TempDir,
        target: Target,
        source: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = TestRoot::new().unwrap();
            let files = TempDir::new().unwrap();
            let source = files.join("build");
            fs::create_dir(&source).unwrap();
            for name in EXECUTABLES {
                fs::write(source.join(name), name).unwrap();
            }
            let trash = files.join("trash");
            fs::create_dir(&trash).unwrap();
            let target = Target {
                software: format!(r"{}\Software", root.path()),
                environment: format!(r"{}\Environment", root.path()),
                directory: files.join(r"Programs\Seer"),
                trash,
                shortcuts: files.join(r"Start Menu\Programs"),
            };
            // The real `Software` key always exists, and uninstalling leaves it.
            Key::create(&target.software).unwrap();
            Key::create(&target.environment)
                .unwrap()
                .set(
                    "Path",
                    &Value::ExpandString(r"C:\Tools;%USERPROFILE%\bin".into()),
                )
                .unwrap();
            Sandbox {
                root,
                files,
                target,
                source,
            }
        }
    }

    /// What a shortcut says: its target, icon file and index, and description.
    /// The shell stores the target's long name even when it was given a short
    /// one (such as `RUNNER~1` in a CI runner's `TEMP`), so both paths are
    /// canonicalised to compare them.
    fn read_shortcut(path: &Path) -> (PathBuf, PathBuf, i32, String) {
        let link = shell_link().unwrap();
        let file: IPersistFile = link.cast().unwrap();
        // SAFETY: the path outlives the call.
        unsafe { file.Load(&HSTRING::from(path.as_os_str()), STGM_READ) }.unwrap();
        let text = |buffer: &[u16]| {
            let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            String::from_utf16_lossy(&buffer[..end])
        };
        let mut target = [0u16; MAX_PATH as usize];
        let mut icon = [0u16; MAX_PATH as usize];
        let mut index = 0;
        let mut description = [0u16; 256];
        // SAFETY: each buffer outlives the call that fills it, and no find
        // data is asked for.
        unsafe {
            link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
                .unwrap();
            link.GetIconLocation(&mut icon, &mut index).unwrap();
            link.GetDescription(&mut description).unwrap();
        }
        (
            fs::canonicalize(text(&target)).unwrap(),
            fs::canonicalize(text(&icon)).unwrap(),
            index,
            text(&description),
        )
    }

    /// Every key and value an installation into `directory` makes, as
    /// [`TestRoot::dump`] lists them.
    fn expected_registry(root: &str, directory: &Path) -> Vec<String> {
        let viewer = directory.join("seerw.exe").display().to_string();
        let icon = format!("{:?}", format!("{viewer},0"));
        let command = format!("{:?}", format!("\"{viewer}\" \"%1\""));
        let delegate = r#""{1F3C3843-28F6-4436-93EE-284B751C1107}""#;
        let mut lines = Vec::new();
        let verb = |key: &str, lines: &mut Vec<String>| {
            lines.push(format!(r"{key}\DefaultIcon"));
            lines.push(format!(r"{key}\DefaultIcon :  = {icon}"));
            lines.push(format!(r"{key}\shell"));
            lines.push(format!(r"{key}\shell\open"));
            lines.push(format!(r#"{key}\shell\open : MultiSelectModel = "Player""#));
            lines.push(format!(r"{key}\shell\open\command"));
            lines.push(format!(r"{key}\shell\open\command :  = {command}"));
            lines.push(format!(
                r"{key}\shell\open\command : DelegateExecute = {delegate}"
            ));
        };
        lines.push("Software".to_owned());
        lines.push(r"Software\Classes".to_owned());
        let class = r"Software\Classes\CLSID\{1F3C3843-28F6-4436-93EE-284B751C1107}";
        lines.push(r"Software\Classes\CLSID".to_owned());
        lines.push(class.to_owned());
        lines.push(format!(r#"{class} :  = "Seer open verb""#));
        lines.push(format!(r"{class}\LocalServer32"));
        lines.push(format!(
            r"{class}\LocalServer32 :  = {:?}",
            format!("\"{viewer}\"")
        ));
        lines.push(r"Software\Classes\Applications".to_owned());
        let application = r"Software\Classes\Applications\seerw.exe";
        lines.push(application.to_owned());
        lines.push(format!(r#"{application} : FriendlyAppName = "Seer""#));
        verb(application, &mut lines);
        lines.push(format!(r"{application}\SupportedTypes"));
        let capabilities = r"Software\Seer\Capabilities";
        lines.push(r"Software\Seer".to_owned());
        lines.push(capabilities.to_owned());
        lines.push(format!(r#"{capabilities} : ApplicationName = "Seer""#));
        lines.push(format!(
            "{capabilities} : ApplicationDescription = {:?}",
            env!("CARGO_PKG_DESCRIPTION")
        ));
        lines.push(format!("{capabilities} : ApplicationIcon = {icon}"));
        lines.push(format!(r"{capabilities}\FileAssociations"));
        for family in FAMILIES {
            let prog_id = format!(r"Software\Classes\{}", family.prog_id);
            lines.push(prog_id.clone());
            lines.push(format!("{prog_id} :  = {:?}", family.name));
            verb(&prog_id, &mut lines);
            for extension in family.extensions {
                let key = format!(r"Software\Classes\{extension}");
                lines.push(key.clone());
                lines.push(format!(r"{key}\OpenWithProgids"));
                lines.push(format!(
                    r"{key}\OpenWithProgids : {} = none",
                    family.prog_id
                ));
                lines.push(format!(
                    r#"{application}\SupportedTypes : {extension} = """#
                ));
                lines.push(format!(
                    r"{capabilities}\FileAssociations : {extension} = {:?}",
                    family.prog_id
                ));
            }
        }
        lines.push(r"Software\RegisteredApplications".to_owned());
        lines.push(format!(
            r"Software\RegisteredApplications : Seer = {:?}",
            format!(r"{root}\Software\Seer\Capabilities")
        ));
        let mut key = "Software".to_owned();
        for part in ["Microsoft", "Windows", "CurrentVersion", "Uninstall"] {
            key = format!(r"{key}\{part}");
            lines.push(key.clone());
        }
        let entry = format!(r"{key}\RKeelan.Seer");
        lines.push(entry.clone());
        let seer = directory.join("seer.exe").display().to_string();
        let uninstall = format!("{:?}", format!("\"{seer}\" uninstall"));
        for (name, value) in [
            ("DisplayName", r#""Seer""#.to_owned()),
            ("DisplayVersion", format!("{:?}", env!("CARGO_PKG_VERSION"))),
            ("Publisher", r#""RKeelan""#.to_owned()),
            ("DisplayIcon", icon.clone()),
            (
                "InstallLocation",
                format!("{:?}", directory.display().to_string()),
            ),
            (
                "URLInfoAbout",
                r#""https://github.com/RKeelan/Seer""#.to_owned(),
            ),
            ("UninstallString", uninstall.clone()),
            ("QuietUninstallString", uninstall),
        ] {
            lines.push(format!("{entry} : {name} = {value}"));
        }
        lines.sort();
        lines
    }

    #[test]
    fn install_registers_everything_and_uninstall_removes_it() {
        let sandbox = Sandbox::new();
        let target = &sandbox.target;
        let before = sandbox.root.dump();

        let installed = install(target, &sandbox.source).unwrap();
        assert_eq!(
            installed,
            Installed {
                directory: target.directory.clone(),
                path_added: true
            }
        );
        for name in EXECUTABLES {
            assert_eq!(
                fs::read_to_string(target.directory.join(name)).unwrap(),
                name
            );
        }
        let directory = target.directory.display().to_string();
        let mut expected = expected_registry(sandbox.root.path(), &target.directory);
        expected.push("Environment".to_owned());
        expected.push(format!(
            r"Environment : Path = expand {:?}",
            format!(r"C:\Tools;%USERPROFILE%\bin;{directory}")
        ));
        expected.sort();
        assert_eq!(sandbox.root.dump(), expected);
        let viewer = fs::canonicalize(target.directory.join(VIEWER)).unwrap();
        let shortcut = target.shortcuts.join("Seer.lnk");
        assert_eq!(target.shortcut(), shortcut);
        let expected_shortcut = (viewer.clone(), viewer, 0, "Image viewer".to_owned());
        assert_eq!(read_shortcut(&shortcut), expected_shortcut);

        // Installing again changes nothing, and doesn't add to PATH twice. It
        // overwrites a shortcut that was changed.
        fs::write(&shortcut, "not a shortcut").unwrap();
        let again = install(target, &sandbox.source).unwrap();
        assert!(!again.path_added);
        assert_eq!(sandbox.root.dump(), expected);
        assert_eq!(read_shortcut(&shortcut), expected_shortcut);

        let uninstalled = uninstall(target).unwrap();
        assert_eq!(
            uninstalled,
            Uninstalled {
                directory: target.directory.clone(),
                path_removed: true
            }
        );
        assert_eq!(sandbox.root.dump(), before);
        assert!(!target.directory.exists());
        assert!(target.directory.parent().unwrap().exists());
        assert!(!shortcut.exists());
        assert!(target.shortcuts.exists());

        // Uninstalling again finds nothing to do.
        let again = uninstall(target).unwrap();
        assert!(!again.path_removed);
        assert_eq!(sandbox.root.dump(), before);
    }

    #[test]
    fn uninstall_leaves_other_programs_entries() {
        let sandbox = Sandbox::new();
        let target = &sandbox.target;
        let classes = target.classes();
        let other = Key::create(&format!(r"{classes}\.png\OpenWithProgids")).unwrap();
        other.set("Other.PNG", &Value::None).unwrap();
        Key::create(&format!(r"{}\RegisteredApplications", target.software))
            .unwrap()
            .set(
                "Other",
                &Value::String(r"Software\Other\Capabilities".into()),
            )
            .unwrap();
        fs::create_dir_all(&target.directory).unwrap();
        fs::write(target.directory.join("notes.txt"), "mine").unwrap();
        let before = sandbox.root.dump();

        install(target, &sandbox.source).unwrap();
        uninstall(target).unwrap();
        assert_eq!(sandbox.root.dump(), before);
        assert!(target.directory.join("notes.txt").exists());
        drop(sandbox.files);
    }

    #[test]
    fn a_path_of_plain_type_or_none_at_all_is_handled() {
        let sandbox = Sandbox::new();
        let target = &sandbox.target;
        let environment = Key::create(&target.environment).unwrap();
        environment
            .set("Path", &Value::String(r"C:\Tools;".into()))
            .unwrap();
        install(target, &sandbox.source).unwrap();
        let directory = target.directory.display().to_string();
        assert_eq!(
            environment.get("Path").unwrap(),
            Some(Value::String(format!(r"C:\Tools;{directory}")))
        );
        uninstall(target).unwrap();
        assert_eq!(
            environment.get("Path").unwrap(),
            Some(Value::String(r"C:\Tools".into()))
        );

        environment.delete_value("Path").unwrap();
        drop(environment);
        let before = sandbox.root.dump();
        install(target, &sandbox.source).unwrap();
        assert_eq!(
            Key::create(&target.environment)
                .unwrap()
                .get("Path")
                .unwrap(),
            Some(Value::ExpandString(directory))
        );
        uninstall(target).unwrap();
        // The emptied value goes, and with it the key, which held nothing else.
        assert_eq!(sandbox.root.dump(), before);
    }

    #[test]
    fn a_path_that_is_not_a_string_is_left_alone() {
        let sandbox = Sandbox::new();
        let target = &sandbox.target;
        let odd = Value::Other(4, vec![1, 0, 0, 0]);
        Key::create(&target.environment)
            .unwrap()
            .set("Path", &odd)
            .unwrap();
        let error = install(target, &sandbox.source).unwrap_err();
        assert!(error.message.contains("PATH"), "{error}");
        assert_eq!(
            Key::create(&target.environment)
                .unwrap()
                .get("Path")
                .unwrap(),
            Some(odd)
        );
        uninstall(target).unwrap_err();
    }

    #[test]
    fn a_missing_executable_stops_the_install() {
        let sandbox = Sandbox::new();
        fs::remove_file(sandbox.source.join("seerw.exe")).unwrap();
        let before = sandbox.root.dump();
        let error = install(&sandbox.target, &sandbox.source).unwrap_err();
        assert!(error.message.contains("seerw.exe"), "{error}");
        assert_eq!(sandbox.root.dump(), before);
    }

    #[test]
    fn installing_from_the_install_directory_keeps_the_files() {
        let sandbox = Sandbox::new();
        let target = &sandbox.target;
        install(target, &sandbox.source).unwrap();
        install(target, &target.directory).unwrap();
        for name in EXECUTABLES {
            assert_eq!(
                fs::read_to_string(target.directory.join(name)).unwrap(),
                name
            );
        }
        uninstall(target).unwrap();
    }

    #[test]
    fn an_uncreatable_directory_is_reported() {
        let sandbox = Sandbox::new();
        let blocker = sandbox.files.join("file");
        fs::write(&blocker, "").unwrap();
        let target = Target {
            directory: blocker.join("Seer"),
            ..sandbox.target.clone()
        };
        let error = install(&target, &sandbox.source).unwrap_err();
        assert!(error.message.contains("could not create"), "{error}");
    }

    #[test]
    fn a_shortcut_that_cannot_be_written_or_removed_is_reported() {
        let sandbox = Sandbox::new();
        let blocker = sandbox.files.join("file");
        fs::write(&blocker, "").unwrap();
        let target = Target {
            shortcuts: blocker.join("Programs"),
            ..sandbox.target.clone()
        };
        let error = install(&target, &sandbox.source).unwrap_err();
        assert!(error.message.contains("could not create"), "{error}");

        // A directory where the shortcut goes can be neither written over
        // nor removed as a file.
        let target = &sandbox.target;
        fs::create_dir_all(target.shortcut()).unwrap();
        let error = install(target, &sandbox.source).unwrap_err();
        assert!(error.message.contains("could not write"), "{error}");
        let error = uninstall(target).unwrap_err();
        assert!(error.message.contains("could not remove"), "{error}");
    }

    #[test]
    fn a_file_that_will_not_go_is_reported() {
        let dir = TempDir::new().unwrap();
        // A directory cannot be removed as a file, nor moved into a trash
        // that does not exist.
        let stuck = dir.join("stuck.exe");
        fs::create_dir(&stuck).unwrap();
        let error = discard(&stuck, &dir.join("absent")).unwrap_err();
        assert!(error.message.contains("stuck.exe"), "{error}");
        discard(&dir.join("absent.exe"), dir.path()).unwrap();
    }

    #[test]
    fn path_entries_are_added_and_removed() {
        let dir = Path::new(r"C:\Users\Someone\AppData\Local\Programs\Seer");
        let plain = r"C:\Users\Someone\AppData\Local\Programs\Seer";
        assert_eq!(path_with("", dir).unwrap(), plain);
        assert_eq!(path_with(r"C:\A", dir).unwrap(), format!(r"C:\A;{plain}"));
        assert_eq!(path_with(r"C:\A;;", dir).unwrap(), format!(r"C:\A;{plain}"));
        for present in [
            format!(r"C:\A;{plain}"),
            format!(r"{}\;C:\A", plain.to_uppercase()),
            format!(r#""{plain}""#),
        ] {
            assert_eq!(path_with(&present, dir), None, "{present}");
        }
        assert_eq!(
            path_without(&format!(r"C:\A;{plain}\;C:\B;{plain}"), dir).unwrap(),
            r"C:\A;C:\B"
        );
        assert_eq!(path_without(plain, dir).unwrap(), "");
        assert_eq!(path_without(r"C:\A;;C:\B", dir), None);
    }

    #[test]
    fn the_user_target_is_under_local_app_data() {
        let target = Target::for_user().unwrap();
        assert_eq!(target.software, "Software");
        assert_eq!(target.environment, "Environment");
        assert!(target.directory.ends_with(r"Programs\Seer"));
        assert!(target.shortcuts.ends_with(r"Start Menu\Programs"));
        assert!(target.shortcuts.is_absolute());
    }

    #[test]
    fn reports_describe_what_happened() {
        let directory = PathBuf::from(r"C:\Programs\Seer");
        let installed = Installed {
            directory: directory.clone(),
            path_added: true,
        };
        let text = describe_installed(&installed);
        assert!(text.contains(r"Installed seer.exe and seerw.exe to C:\Programs\Seer."));
        assert!(text.contains("PATH"));
        let quiet = describe_installed(&Installed {
            path_added: false,
            ..installed
        });
        assert!(!quiet.contains("PATH"));

        let uninstalled = Uninstalled {
            directory,
            path_removed: true,
        };
        assert!(describe_uninstalled(&uninstalled).contains("PATH"));
        assert!(
            !describe_uninstalled(&Uninstalled {
                path_removed: false,
                ..uninstalled
            })
            .contains("PATH")
        );
        assert_eq!(
            serde_json::to_string(&Uninstalled {
                directory: PathBuf::from("D"),
                path_removed: false
            })
            .unwrap(),
            r#"{"directory":"D","path_removed":false}"#
        );
    }
}
