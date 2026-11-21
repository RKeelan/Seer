//! Decoding images to bitmaps, off the UI thread.
//!
//! - [`wic`]: still images through the Windows Imaging Component, scaled and
//!   full, with EXIF orientation applied.
//! - [`gif`]: animated GIF frames, composed from their metadata.
//! - [`svg`]: SVG documents, drawn through Direct2D.
//! - [`cache`]: which decoded images to keep under the memory budget, and
//!   which to decode ahead.
//! - [`worker`]: the decode thread and its queue.

pub mod cache;
pub mod gif;
pub mod svg;
pub mod wic;
pub mod worker;
