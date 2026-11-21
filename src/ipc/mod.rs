//! The single-instance machinery: the named mutex that makes one viewer per
//! instance, and the named pipe its clients talk to it over.
//!
//! - [`instance`]: instance names and the kernel object names derived from them.
//! - [`server`]: the viewer's pipe server.
//! - [`client`]: the client `seer.exe` and a second `seerw.exe` use.

pub mod client;
pub mod instance;
pub mod server;
