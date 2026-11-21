//! Taking a selection from Explorer in one piece, through the open verb's
//! `DelegateExecute`.
//!
//! An open verb registered only as a command line makes Explorer start one
//! `seerw.exe` per selected file, which race to the viewer and arrive in no
//! useful order. `seer install` also registers the verb's `DelegateExecute`
//! as [`CLSID`], whose local server is `seerw.exe`. Explorer then starts one
//! `seerw -Embedding` through COM, creates a command object in it, hands it
//! the whole selection as a shell item array, in Explorer's order, and calls
//! `Execute`.
//!
//! [`serve`] registers the class and waits for that, on a thread of its own
//! in the multithreaded apartment, so the viewer's own thread never joins an
//! apartment it would not otherwise be in. The viewer then carries on as if
//! the selection's files had been its command line. A plain `seerw <file>`
//! never comes here and never initialises COM for it.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_FAIL, E_POINTER, POINT};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoRegisterClassObject,
    CoRevokeClassObject, CoTaskMemFree, CoUninitialize, IClassFactory, IClassFactory_Impl,
    REGCLS_SINGLEUSE,
};
use windows::Win32::UI::Shell::{
    IExecuteCommand, IExecuteCommand_Impl, IObjectWithSelection, IObjectWithSelection_Impl,
    IShellItemArray, SIGDN_FILESYSPATH,
};
use windows::core::{BOOL, GUID, IUnknown, Interface, PCWSTR, Ref, implement};

use crate::error::{Error, Result};

/// The class Explorer creates to open a selection with Seer.
pub const CLSID: GUID = GUID::from_u128(0x1F3C3843_28F6_4436_93EE_284B751C1107);

/// How long [`serve`] waits for Explorer to execute the command, and then for
/// Explorer to let go of it.
pub const SERVE_TIMEOUT: Duration = Duration::from_secs(30);

/// A class id written as the registry writes them, with braces.
pub fn registry_form(clsid: &GUID) -> String {
    format!("{{{clsid:?}}}")
}

/// Parses a class id given with or without braces.
pub fn parse_clsid(text: &str) -> Result<GUID> {
    let bare = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(text);
    GUID::try_from(bare).map_err(|_| Error::usage(format!("{text} is not a class id")))
}

/// The files Explorer handed over. Dropping it waits, for at most
/// [`SERVE_TIMEOUT`] from when the class was registered, until Explorer has
/// let go of the command object, so a process that exits soon after does not
/// leave Explorer calling a server that is gone.
#[derive(Debug)]
pub struct Selection {
    /// The selection's files, in Explorer's order. Items with no file system
    /// path, such as This PC, are left out.
    pub files: Vec<PathBuf>,
    server: Option<JoinHandle<()>>,
}

impl Drop for Selection {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Registers `clsid`'s class object for one use, and waits up to `timeout`
/// for the command it creates to be executed.
pub fn serve(clsid: GUID, timeout: Duration) -> Result<Selection> {
    let (files_sent, files) = mpsc::channel();
    let (registered_sent, registered) = mpsc::channel();
    let server = std::thread::Builder::new()
        .name("com-server".into())
        .spawn(move || run_server(clsid, timeout, files_sent, registered_sent))?;
    // The thread reports once, before anything else it does.
    if let Ok(Err(error)) = registered.recv() {
        let _ = server.join();
        return Err(error);
    }
    let mut selection = Selection {
        files: Vec::new(),
        server: Some(server),
    };
    match files.recv_timeout(timeout) {
        Ok(files) => {
            selection.files = files;
            Ok(selection)
        }
        Err(_) => Err(Error::unexpected(
            "COM started seerw, but no selection arrived to open",
        )),
    }
}

fn run_server(
    clsid: GUID,
    timeout: Duration,
    files: Sender<Vec<PathBuf>>,
    registered: Sender<Result<()>>,
) {
    // SAFETY: initialising COM on a new thread has no preconditions; it is
    // undone below, on the same thread.
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        let _ = registered.send(Err(error.into()));
        return;
    }
    let (released_sent, released) = mpsc::channel();
    let factory: IClassFactory = Factory {
        files,
        released: released_sent,
    }
    .into();
    // SAFETY: the class id and the factory outlive the call; COM holds its
    // own reference to the factory until the class is revoked.
    let registration =
        unsafe { CoRegisterClassObject(&clsid, &factory, CLSCTX_LOCAL_SERVER, REGCLS_SINGLEUSE) };
    match registration {
        Ok(cookie) => {
            let _ = registered.send(Ok(()));
            let _ = released.recv_timeout(timeout);
            // SAFETY: the cookie is the registration made above.
            let _ = unsafe { CoRevokeClassObject(cookie) };
        }
        Err(error) => {
            let _ = registered.send(Err(error.into()));
        }
    }
    drop(factory);
    // SAFETY: balances the successful CoInitializeEx above, on this thread.
    unsafe { CoUninitialize() };
}

/// The class object, which makes one [`Command`] for each request.
#[implement(IClassFactory)]
struct Factory {
    files: Sender<Vec<PathBuf>>,
    released: Sender<()>,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if object.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `object` was checked above, and COM passes it writable.
        unsafe { *object = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let command: IUnknown = Command {
            selection: Mutex::new(None),
            files: self.files.clone(),
            released: self.released.clone(),
        }
        .into();
        // SAFETY: `iid` and `object` were checked above; QueryInterface
        // writes the interface, with a reference of its own, or null.
        unsafe { command.query(iid, object) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// The command object Explorer gives the selection to and executes.
#[implement(IExecuteCommand, IObjectWithSelection)]
struct Command {
    selection: Mutex<Option<IShellItemArray>>,
    files: Sender<Vec<PathBuf>>,
    released: Sender<()>,
}

impl Drop for Command {
    fn drop(&mut self) {
        let _ = self.released.send(());
    }
}

impl Command {
    fn selection(&self) -> Option<IShellItemArray> {
        self.selection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl IObjectWithSelection_Impl for Command_Impl {
    fn SetSelection(&self, items: Ref<IShellItemArray>) -> windows::core::Result<()> {
        *self
            .selection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = items.cloned();
        Ok(())
    }

    fn GetSelection(
        &self,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if object.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: `object` was checked above, and COM passes it writable.
        unsafe { *object = std::ptr::null_mut() };
        let items = self
            .selection()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        // SAFETY: `iid` and `object` were checked above.
        unsafe { items.query(iid, object) }.ok()
    }
}

impl IExecuteCommand_Impl for Command_Impl {
    fn SetKeyState(&self, _keys: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetParameters(&self, _parameters: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetPosition(&self, _point: &POINT) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetShowWindow(&self, _show: i32) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetNoShowUI(&self, _no_ui: BOOL) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetDirectory(&self, _directory: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn Execute(&self) -> windows::core::Result<()> {
        let items = self
            .selection()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        let files = paths(&items)?;
        // The viewer may have given up waiting; there is nothing more to do.
        let _ = self.files.send(files);
        Ok(())
    }
}

/// The file system paths of the items, in order, leaving out items that have
/// none.
pub fn paths(items: &IShellItemArray) -> windows::core::Result<Vec<PathBuf>> {
    // SAFETY: GetCount has no arguments.
    let count = unsafe { items.GetCount() }?;
    let mut paths = Vec::new();
    for index in 0..count {
        // SAFETY: the index is within the count.
        let item = unsafe { items.GetItemAt(index) }?;
        // SAFETY: GetDisplayName has no arguments that outlive the call.
        let Ok(name) = (unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }) else {
            continue;
        };
        // SAFETY: GetDisplayName returned a null-terminated string, which this
        // copies before freeing it with the allocator COM made it with.
        let path = unsafe {
            let path = name.to_string();
            CoTaskMemFree(Some(name.0 as *const c_void));
            path
        };
        if let Ok(path) = path {
            paths.push(PathBuf::from(path));
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;

    use super::*;
    use crate::testutil::{TempDir, shell_items};

    /// A command object as the factory makes it, with the ends of its
    /// channels.
    fn command() -> (IExecuteCommand, Receiver<Vec<PathBuf>>, Receiver<()>) {
        crate::testutil::init_com();
        let (files_sent, files) = mpsc::channel();
        let (released_sent, released) = mpsc::channel();
        let factory: IClassFactory = Factory {
            files: files_sent,
            released: released_sent,
        }
        .into();
        // SAFETY: no outer object is given.
        let command = unsafe { factory.CreateInstance(None) }.unwrap();
        (command, files, released)
    }

    fn files(dir: &TempDir, names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|name| {
                let path = dir.join(name);
                std::fs::write(&path, name).unwrap();
                path
            })
            .collect()
    }

    /// The shell gives each path in its long form, where a temporary
    /// directory may be named in its short (8.3) form.
    fn canonical(paths: &[PathBuf]) -> Vec<PathBuf> {
        paths
            .iter()
            .map(|path| std::fs::canonicalize(path).unwrap())
            .collect()
    }

    #[test]
    fn executing_sends_the_selection_in_its_order() {
        let dir = TempDir::new().unwrap();
        let files = files(&dir, &["12.png", "14.png", "01.png", "b.jpg"]);
        let (command, sent, released) = command();
        let with_selection: IObjectWithSelection = command.cast().unwrap();
        // SAFETY: the array is a live object.
        unsafe { with_selection.SetSelection(&shell_items(&files).unwrap()) }.unwrap();
        // SAFETY: the setters take plain values or null-terminated literals.
        unsafe {
            command.SetKeyState(0).unwrap();
            command.SetParameters(windows::core::w!("")).unwrap();
            command.SetPosition(POINT { x: 1, y: 2 }).unwrap();
            command.SetShowWindow(1).unwrap();
            command.SetNoShowUI(false).unwrap();
            command.SetDirectory(windows::core::w!(r"C:\")).unwrap();
            command.Execute().unwrap();
        }
        assert_eq!(canonical(&sent.try_recv().unwrap()), canonical(&files));
        // SAFETY: the selection is asked for as the interface it is.
        let back: IShellItemArray = unsafe { with_selection.GetSelection() }.unwrap();
        // SAFETY: GetCount has no arguments.
        assert_eq!(unsafe { back.GetCount() }.unwrap(), 4);

        assert!(released.try_recv().is_err());
        drop((command, with_selection));
        released.try_recv().unwrap();
    }

    #[test]
    fn items_without_a_path_are_left_out() {
        let dir = TempDir::new().unwrap();
        let file = files(&dir, &["a.png"]).remove(0);
        // This PC has no file system path.
        let this_pc = PathBuf::from("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}");
        let items = shell_items(&[this_pc, file.clone()]).unwrap();
        assert_eq!(canonical(&paths(&items).unwrap()), canonical(&[file]));
    }

    #[test]
    fn executing_without_a_selection_fails() {
        let (command, sent, _released) = command();
        // SAFETY: Execute has no arguments.
        assert!(unsafe { command.Execute() }.is_err());
        let with_selection: IObjectWithSelection = command.cast().unwrap();
        // SAFETY: GetSelection writes only the interface it returns.
        let none: windows::core::Result<IShellItemArray> = unsafe { with_selection.GetSelection() };
        assert!(none.is_err());
        assert!(sent.try_recv().is_err());
    }

    #[test]
    fn the_factory_refuses_aggregation() {
        crate::testutil::init_com();
        let (files, _) = mpsc::channel();
        let (released, _) = mpsc::channel();
        let factory: IClassFactory = Factory { files, released }.into();
        let outer: IUnknown = factory.cast().unwrap();
        // SAFETY: the outer object is a live object.
        let refused: windows::core::Result<IExecuteCommand> =
            unsafe { factory.CreateInstance(&outer) };
        assert_eq!(refused.unwrap_err().code(), CLASS_E_NOAGGREGATION);
        // SAFETY: LockServer takes a plain value.
        unsafe { factory.LockServer(true) }.unwrap();
    }

    #[test]
    fn class_ids_are_written_with_braces_and_read_either_way() {
        let written = registry_form(&CLSID);
        assert_eq!(written, "{1F3C3843-28F6-4436-93EE-284B751C1107}");
        assert_eq!(parse_clsid(&written).unwrap(), CLSID);
        assert_eq!(parse_clsid(&written[1..37]).unwrap(), CLSID);
        assert!(parse_clsid("{not a guid}").is_err());
    }

    #[test]
    fn a_server_that_is_never_called_gives_up() {
        let clsid = GUID::new().unwrap();
        let error = serve(clsid, Duration::from_millis(50)).unwrap_err();
        assert!(error.message.contains("no selection"), "{error}");
    }
}
