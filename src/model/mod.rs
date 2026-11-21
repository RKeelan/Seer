//! The viewer's state and behaviour as pure Rust, with no Win32 types, so all
//! of it can be tested without a window.
//!
//! - [`set`]: the open set, its order, groups and selection.
//! - [`view`]: zoom, pan, fit and rotation, and the conversions between client
//!   and image coordinates.
//! - [`marks`]: marks in image pixels.
//! - [`app`]: the state machine that turns [`app::Input`] into state changes
//!   and [`app::Effect`]s.
//! - [`geometry`]: points, sizes, rectangles, rotations and transforms.

pub mod app;
pub mod geometry;
pub mod marks;
pub mod set;
pub mod view;

use serde::{Deserialize, Serialize};

/// Identifies an image in the open set.
///
/// Ids are small integers, unique for the life of the viewer process and
/// never reused, so an id an agent holds can never come to mean a different
/// image. Serialised as a bare number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ImageId(pub u32);

/// Identifies a mark.
///
/// Unique for the life of the viewer process and never reused, like
/// [`ImageId`]. Serialised as a bare number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarkId(pub u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialise_as_numbers() {
        assert_eq!(serde_json::to_string(&ImageId(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<MarkId>("3").unwrap(), MarkId(3));
    }
}
