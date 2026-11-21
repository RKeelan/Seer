//! Instance names, and the names of the kernel objects derived from them.
//!
//! Every named kernel object Seer creates is namespaced by an instance name,
//! so viewers on different instances never meet. The name comes from
//! `--instance <name>`, else the `SEER_INSTANCE` environment variable, else
//! `default`. Tests use a unique instance each.
//!
//! - The mutex is `Local\Seer-<instance>`. The viewer that creates it owns the
//!   instance, and holds it until it exits.
//! - The pipe is `\\.\pipe\seer-<session id>-<instance>`. Pipe names are
//!   machine-wide, so the session id keeps two signed-in users apart.

use std::fmt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcessId, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::core::HSTRING;

use crate::error::{Error, ErrorCode, Result};

/// The instance used when none is given.
pub const DEFAULT_INSTANCE: &str = "default";

/// The environment variable that names the instance when `--instance` does
/// not.
pub const INSTANCE_VARIABLE: &str = "SEER_INSTANCE";

/// The longest instance name accepted, in characters.
pub const MAX_INSTANCE_LENGTH: usize = 64;

/// A valid instance name: 1 to 64 ASCII letters, digits, `-`, `_` or `.`, so
/// it can go into a mutex or pipe name unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceName(String);

impl InstanceName {
    /// Checks a name.
    pub fn new(name: &str) -> Result<Self> {
        let valid_characters = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if name.is_empty() || name.len() > MAX_INSTANCE_LENGTH || !valid_characters {
            return Err(Error::usage(format!(
                "instance name \"{name}\" must be 1 to {MAX_INSTANCE_LENGTH} letters, digits, '-', '_' or '.'"
            )));
        }
        Ok(InstanceName(name.to_owned()))
    }

    /// The instance named by `--instance`, else by the environment variable's
    /// value, else the default. An empty variable counts as unset.
    pub fn resolve(flag: Option<&str>, variable: Option<&str>) -> Result<Self> {
        match (flag, variable) {
            (Some(name), _) => InstanceName::new(name),
            (None, Some(name)) if !name.is_empty() => InstanceName::new(name),
            _ => InstanceName::new(DEFAULT_INSTANCE),
        }
    }

    /// [`InstanceName::resolve`] with the variable read from this process's
    /// environment.
    pub fn from_environment(flag: Option<&str>) -> Result<Self> {
        let variable = std::env::var(INSTANCE_VARIABLE).ok();
        InstanceName::resolve(flag, variable.as_deref())
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name of the instance's mutex.
    pub fn mutex_name(&self) -> String {
        format!(r"Local\Seer-{}", self.0)
    }

    /// The name of the instance's pipe in this process's session.
    pub fn pipe_name(&self) -> String {
        self.pipe_name_in_session(current_session_id())
    }

    /// The name of the instance's pipe in the given session.
    pub fn pipe_name_in_session(&self, session: u32) -> String {
        format!(r"\\.\pipe\seer-{session}-{}", self.0)
    }

    /// Whether a viewer holds the instance's mutex, which it does from just
    /// after it starts until it exits.
    pub fn is_taken(&self) -> bool {
        let name = HSTRING::from(self.mutex_name());
        // SAFETY: OpenMutexW reads the name, which outlives the call.
        let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &name) };
        match handle {
            Ok(handle) => {
                // SAFETY: the handle is valid and nothing else owns it, so
                // owning it closes it exactly once.
                drop(unsafe { OwnedHandle::from_raw_handle(handle.0) });
                true
            }
            Err(_) => false,
        }
    }
}

/// The session this process runs in, or 0 if it cannot be read.
fn current_session_id() -> u32 {
    let mut session = 0;
    // SAFETY: both calls take plain values, and `session` outlives the call.
    let read = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    if read.is_ok() { session } else { 0 }
}

/// Ownership of an instance: its named mutex, held for as long as this value
/// lives.
#[derive(Debug)]
pub struct InstanceMutex {
    _handle: OwnedHandle,
}

impl InstanceMutex {
    /// Takes the instance, or returns `None` if another viewer has it.
    pub fn acquire(instance: &InstanceName) -> Result<Option<InstanceMutex>> {
        let name = HSTRING::from(instance.mutex_name());
        // SAFETY: CreateMutexW reads the name, which outlives the call, and
        // takes no security attributes.
        let handle = unsafe { CreateMutexW(None, false, &name) }?;
        // SAFETY: GetLastError only reads this thread's last-error value, which
        // CreateMutexW sets to ERROR_ALREADY_EXISTS when the mutex existed.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: the handle is valid and nothing else owns it.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok((!existed).then_some(InstanceMutex { _handle: handle }))
    }

    /// Takes the instance, or fails with [`ErrorCode::AlreadyRunning`].
    pub fn acquire_or_fail(instance: &InstanceName) -> Result<InstanceMutex> {
        InstanceMutex::acquire(instance)?.ok_or_else(|| {
            Error::new(
                ErrorCode::AlreadyRunning,
                format!("a viewer is already running on instance {instance}"),
            )
        })
    }
}

impl fmt::Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn accepts_simple_names() {
        for name in [
            "default",
            "test-42",
            "a.b_c",
            &"x".repeat(MAX_INSTANCE_LENGTH),
        ] {
            assert_eq!(InstanceName::new(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn rejects_names_unfit_for_kernel_objects() {
        for name in [
            "",
            "a\\b",
            "a b",
            "naïve",
            &"x".repeat(MAX_INSTANCE_LENGTH + 1),
        ] {
            let error = InstanceName::new(name).unwrap_err();
            assert_eq!(error.code, ErrorCode::Usage, "{name}");
        }
    }

    #[test]
    fn flag_beats_variable_beats_default() {
        let resolve = |flag, variable| InstanceName::resolve(flag, variable).unwrap().to_string();
        assert_eq!(resolve(Some("flag"), Some("variable")), "flag");
        assert_eq!(resolve(None, Some("variable")), "variable");
        assert_eq!(resolve(None, Some("")), DEFAULT_INSTANCE);
        assert_eq!(resolve(None, None), DEFAULT_INSTANCE);
        assert!(InstanceName::resolve(Some("bad name"), None).is_err());
    }

    #[test]
    fn kernel_object_names_carry_the_instance() {
        let name = InstanceName::new("t-1").unwrap();
        assert_eq!(name.mutex_name(), r"Local\Seer-t-1");
        assert_eq!(name.pipe_name_in_session(3), r"\\.\pipe\seer-3-t-1");
        assert!(name.pipe_name().starts_with(r"\\.\pipe\seer-"));
        assert!(name.pipe_name().ends_with("-t-1"));
    }

    #[test]
    fn one_holder_per_instance() {
        let name = InstanceName::new(&crate::testutil::unique_instance()).unwrap();
        assert!(!name.is_taken());
        let held = InstanceMutex::acquire(&name).unwrap().expect("first taker");
        assert!(name.is_taken());
        assert!(InstanceMutex::acquire(&name).unwrap().is_none());
        let error = InstanceMutex::acquire_or_fail(&name).unwrap_err();
        assert_eq!(error.code, ErrorCode::AlreadyRunning);
        drop(held);
        assert!(!name.is_taken());
        assert!(InstanceMutex::acquire_or_fail(&name).is_ok());
    }

    #[test]
    fn reads_the_environment() {
        // The variable may or may not be set where the tests run; a flag wins
        // either way.
        assert_eq!(
            InstanceName::from_environment(Some("flag"))
                .unwrap()
                .as_str(),
            "flag"
        );
        assert!(InstanceName::from_environment(None).is_ok());
    }
}
