//! Instance names, and the names of the kernel objects derived from them.
//!
//! Every named kernel object Seer creates is namespaced by an instance name,
//! so viewers on different instances never meet. The name comes from
//! `--instance <name>`, else the `SEER_INSTANCE` environment variable, else
//! `default`. Tests use a unique instance each.

use std::fmt;

use crate::error::{Error, Result};

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
