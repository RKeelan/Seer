//! Drawing with Direct2D.
//!
//! - [`device`]: Direct3D, DXGI and Direct2D set-up, with a WARP fallback, and
//!   recovery from device loss.
//! - [`scene`]: what to draw, as plain data.
//! - [`draw`]: drawing a scene to a device context.
//! - [`strip`]: the thumbnail strip.
//! - [`text`]: text layout through DirectWrite.
//! - [`offscreen`]: an offscreen target and PNG encoding, for `screenshot` and
//!   render tests.

pub mod device;
pub mod draw;
pub mod offscreen;
pub mod scene;
pub mod strip;
pub mod text;
