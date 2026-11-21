//! The decoded-image cache: what to keep under the memory budget, what to
//! evict first, and which neighbours to decode ahead.
//!
//! This is policy only, as plain data: [`crate::model::app::App`] consults it
//! to decide which decodes to ask for and which bitmaps to drop, and the
//! window holds the bitmaps themselves.
//!
//! - The budget is a quarter of physical memory, capped at [`MAX_BUDGET`].
//! - Each decoded image costs four bytes a pixel of its bitmap.
//! - Over budget, the image shown least recently goes first. An image that
//!   was decoded ahead and never shown goes before any that was shown. The
//!   images shown now and their neighbours are never evicted.

use std::collections::HashMap;

use crate::decode::wic::target_size;
use crate::model::ImageId;
use crate::model::app::DecodeTarget;
use crate::model::geometry::PixelSize;

/// The most the cache holds, whatever the machine's memory: 2 GiB.
pub const MAX_BUDGET: u64 = 2 << 30;

/// The budget on a machine with `physical` bytes of memory.
pub fn budget(physical: u64) -> u64 {
    (physical / 4).min(MAX_BUDGET)
}

/// The bytes a decode of an image of `size` for `target` takes: four a pixel.
pub fn bitmap_bytes(size: PixelSize, target: DecodeTarget) -> u64 {
    let bitmap = target_size(size, target);
    u64::from(bitmap.width) * u64::from(bitmap.height) * 4
}

/// The images either side of `selected` in `order`: the ones to decode ahead
/// of the user moving to them.
pub fn neighbours(order: &[ImageId], selected: ImageId) -> Vec<ImageId> {
    let Some(index) = order.iter().position(|&id| id == selected) else {
        return Vec::new();
    };
    let before = index.checked_sub(1).map(|index| order[index]);
    let after = order.get(index + 1).copied();
    before.into_iter().chain(after).collect()
}

/// The decoded images held, and when each was last shown.
#[derive(Clone, Debug)]
pub struct Cache {
    budget: u64,
    held: HashMap<ImageId, Held>,
    /// Counts calls to [`Cache::show`], so a larger stamp is more recent.
    clock: u64,
}

#[derive(Clone, Copy, Debug)]
struct Held {
    bytes: u64,
    /// The clock when the image was last shown; 0 if it never was.
    shown: u64,
}

impl Cache {
    /// An empty cache that holds up to `budget` bytes.
    pub fn new(budget: u64) -> Self {
        Cache {
            budget,
            held: HashMap::new(),
            clock: 0,
        }
    }

    /// The most the cache holds, in bytes.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Changes the budget. Evictions it calls for happen at the next
    /// [`Cache::evict`].
    pub fn set_budget(&mut self, budget: u64) {
        self.budget = budget;
    }

    /// The bytes held.
    pub fn used(&self) -> u64 {
        self.held.values().map(|held| held.bytes).sum()
    }

    /// Whether anything is held for `image`.
    pub fn holds(&self, image: ImageId) -> bool {
        self.held.contains_key(&image)
    }

    /// Records that `image`'s bitmap now takes `bytes`. The window keeps one
    /// bitmap an image, the best it has, so this replaces what was held.
    pub fn hold(&mut self, image: ImageId, bytes: u64) {
        self.held
            .entry(image)
            .and_modify(|held| held.bytes = bytes)
            .or_insert(Held { bytes, shown: 0 });
    }

    /// Records that `images` are shown now.
    pub fn show(&mut self, images: &[ImageId]) {
        self.clock += 1;
        for image in images {
            if let Some(held) = self.held.get_mut(image) {
                held.shown = self.clock;
            }
        }
    }

    /// Forgets `image`, whose bitmap is gone.
    pub fn forget(&mut self, image: ImageId) {
        self.held.remove(&image);
    }

    /// Chooses images to drop until the cache is within its budget, least
    /// recently shown first, never one in `keep`, and forgets them. Returns
    /// them in the order chosen.
    pub fn evict(&mut self, keep: &[ImageId]) -> Vec<ImageId> {
        let mut used = self.used();
        if used <= self.budget {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, ImageId, u64)> = self
            .held
            .iter()
            .filter(|(image, _)| !keep.contains(image))
            .map(|(&image, held)| (held.shown, image, held.bytes))
            .collect();
        candidates.sort_unstable();
        let mut evicted = Vec::new();
        for (_, image, bytes) in candidates {
            if used <= self.budget {
                break;
            }
            used -= bytes;
            self.held.remove(&image);
            evicted.push(image);
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[u32]) -> Vec<ImageId> {
        ids.iter().map(|&id| ImageId(id)).collect()
    }

    #[test]
    fn the_budget_is_a_quarter_of_memory_up_to_two_gibibytes() {
        assert_eq!(budget(4 << 30), 1 << 30);
        assert_eq!(budget(8 << 30), 2 << 30);
        assert_eq!(budget(64 << 30), MAX_BUDGET);
    }

    #[test]
    fn bitmaps_cost_four_bytes_a_pixel_at_their_decoded_size() {
        let size = PixelSize::new(4000, 3000);
        assert_eq!(bitmap_bytes(size, DecodeTarget::Full), 48_000_000);
        let fit = DecodeTarget::Fit {
            within: PixelSize::new(400, 400),
        };
        assert_eq!(bitmap_bytes(size, fit), 400 * 300 * 4);
    }

    #[test]
    fn neighbours_are_either_side_and_stop_at_the_ends() {
        let order = ids(&[1, 2, 3]);
        assert_eq!(neighbours(&order, ImageId(2)), ids(&[1, 3]));
        assert_eq!(neighbours(&order, ImageId(1)), ids(&[2]));
        assert_eq!(neighbours(&order, ImageId(3)), ids(&[2]));
        assert_eq!(neighbours(&ids(&[1]), ImageId(1)), ids(&[]));
        assert_eq!(neighbours(&order, ImageId(9)), ids(&[]));
    }

    #[test]
    fn nothing_is_evicted_within_the_budget() {
        let mut cache = Cache::new(100);
        cache.hold(ImageId(1), 60);
        cache.hold(ImageId(2), 40);
        assert_eq!(cache.used(), 100);
        assert!(cache.evict(&[]).is_empty());
        // A better decode replaces the one held.
        cache.hold(ImageId(2), 30);
        assert_eq!(cache.used(), 90);
        assert!(cache.holds(ImageId(2)));
        cache.forget(ImageId(2));
        assert!(!cache.holds(ImageId(2)));
        assert_eq!(cache.used(), 60);
    }

    #[test]
    fn the_least_recently_shown_goes_first_and_never_one_kept() {
        let mut cache = Cache::new(100);
        for image in 1..=4 {
            cache.hold(ImageId(image), 30);
        }
        // 4 was decoded ahead and never shown; 1, 2 and 3 were shown in turn.
        for image in 1..=3 {
            cache.show(&[ImageId(image)]);
        }
        assert_eq!(cache.used(), 120);
        assert_eq!(cache.evict(&ids(&[3])), ids(&[4]));
        assert_eq!(cache.used(), 90);

        cache.set_budget(40);
        assert_eq!(cache.budget(), 40);
        assert_eq!(cache.evict(&ids(&[1])), ids(&[2, 3]));
        assert_eq!(cache.used(), 30);

        // What is kept stays, even over budget.
        cache.set_budget(10);
        assert!(cache.evict(&ids(&[1])).is_empty());
        assert!(cache.holds(ImageId(1)));
    }

    #[test]
    fn showing_an_image_not_held_changes_nothing() {
        let mut cache = Cache::new(10);
        cache.show(&[ImageId(1)]);
        cache.hold(ImageId(1), 20);
        cache.hold(ImageId(2), 20);
        cache.show(&[ImageId(2)]);
        // 1 was shown before it was held, so it counts as never shown.
        assert_eq!(cache.evict(&[]), ids(&[1, 2]));
    }
}
