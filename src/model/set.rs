//! The open set: the images opened, in order, without duplicates, with their
//! groups, captions and ids, and which one is selected.
//!
//! Two paths name the same image when their [`PathKey`]s match. The key is
//! made from the path the [`Files`] seam returns, which for the real
//! filesystem resolves links and letter case, then normalised and folded to
//! lower case, since Windows paths are case-insensitive.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, ErrorCode, Result};
use crate::model::ImageId;

/// Learns which file a path names, so the same file opened by two paths is
/// recognised. The viewer uses [`DiskFiles`]; tests can supply paths without
/// touching the disk.
pub trait Files {
    /// The canonical form of an absolute path to a file, or the reason it
    /// cannot be opened.
    fn canonical(&self, path: &Path) -> Result<PathBuf>;

    /// If the file at `path` needs a codec extension that is not installed,
    /// the error that names it.
    fn missing_codec(&self, _path: &Path) -> Option<Error> {
        None
    }
}

/// [`Files`] backed by the filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskFiles;

impl Files for DiskFiles {
    fn canonical(&self, path: &Path) -> Result<PathBuf> {
        let canonical = std::fs::canonicalize(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => Error::new(
                ErrorCode::FileNotFound,
                format!("{} does not exist", path.display()),
            ),
            _ => Error::new(
                ErrorCode::OpenFailed,
                format!("cannot open {}: {error}", path.display()),
            ),
        })?;
        if !canonical.is_file() {
            return Err(Error::new(
                ErrorCode::OpenFailed,
                format!("{} is not a file", path.display()),
            ));
        }
        Ok(canonical)
    }

    fn missing_codec(&self, path: &Path) -> Option<Error> {
        crate::decode::wic::missing_codec(path)
    }
}

/// Normalises an absolute path without touching the disk: `/` becomes `\`,
/// the `\\?\` prefix goes, and `.` and `..` components are resolved.
pub fn normalise(path: &Path) -> PathBuf {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    };
    let mut normal = PathBuf::new();
    for component in Path::new(&text).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

/// What two paths to the same file have in common: the normalised path in
/// lower case.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathKey(String);

impl PathKey {
    /// The key for a canonical path.
    pub fn new(path: &Path) -> Self {
        PathKey(normalise(path).to_string_lossy().to_lowercase())
    }
}

/// An image in the open set.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Its id.
    pub id: ImageId,
    /// The path it was first opened by.
    pub path: PathBuf,
    /// What recognises it when opened again.
    pub key: PathKey,
    /// The caption shown with it.
    pub caption: Option<String>,
    /// The group it belongs to.
    pub group: Option<String>,
}

/// The images opened, in order, and which is selected.
///
/// The members of a group are kept next to each other. Whenever the set is
/// not empty, one image is selected.
#[derive(Debug)]
pub struct OpenSet {
    entries: Vec<Entry>,
    selected: Option<ImageId>,
    next_id: u32,
}

impl Default for OpenSet {
    fn default() -> Self {
        OpenSet::new()
    }
}

impl OpenSet {
    /// An empty set. The first image added gets id 1.
    pub fn new() -> Self {
        OpenSet {
            entries: Vec::new(),
            selected: None,
            next_id: 1,
        }
    }

    /// Adds an image, or finds it if a path with the same key is already in
    /// the set. Returns its id and whether it was already there.
    ///
    /// A new image goes after the last member of its group, or at the end.
    /// An image already in the set keeps its place, takes the caption if one
    /// is given, and moves to the end of the group if a different one is
    /// given. Adding to an empty set selects the image; otherwise the
    /// selection is unchanged.
    pub fn add(
        &mut self,
        path: PathBuf,
        key: PathKey,
        caption: Option<String>,
        group: Option<String>,
    ) -> (ImageId, bool) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let entry = &mut self.entries[index];
            if caption.is_some() {
                entry.caption = caption;
            }
            if group.is_some() && entry.group != group {
                let mut entry = self.entries.remove(index);
                entry.group = group;
                let id = entry.id;
                self.insert(entry);
                return (id, true);
            }
            return (entry.id, true);
        }
        let id = ImageId(self.next_id);
        self.next_id += 1;
        self.insert(Entry {
            id,
            path,
            key,
            caption,
            group,
        });
        self.selected.get_or_insert(id);
        (id, false)
    }

    fn insert(&mut self, entry: Entry) {
        let after_group = entry.group.as_ref().and_then(|group| {
            self.entries
                .iter()
                .rposition(|other| other.group.as_ref() == Some(group))
        });
        match after_group {
            Some(index) => self.entries.insert(index + 1, entry),
            None => self.entries.push(entry),
        }
    }

    /// Removes an image. If it was selected, the image that takes its place
    /// is selected, or the one before it if it was last.
    pub fn remove(&mut self, id: ImageId) -> Option<Entry> {
        let index = self.index(id)?;
        let entry = self.entries.remove(index);
        if self.selected == Some(id) {
            self.selected = self
                .entries
                .get(index)
                .or_else(|| self.entries.last())
                .map(|entry| entry.id);
        }
        Some(entry)
    }

    /// Selects an image. Returns false, changing nothing, if it is not in
    /// the set.
    pub fn select(&mut self, id: ImageId) -> bool {
        let found = self.index(id).is_some();
        if found {
            self.selected = Some(id);
        }
        found
    }

    /// Moves the selection `by` places, stopping at either end. Returns
    /// whether it moved.
    pub fn step(&mut self, by: isize) -> bool {
        let Some(index) = self.selected.and_then(|id| self.index(id)) else {
            return false;
        };
        let last = self.entries.len() - 1;
        let target = index.saturating_add_signed(by).min(last);
        self.selected = Some(self.entries[target].id);
        target != index
    }

    /// Selects the first image.
    pub fn select_first(&mut self) {
        self.selected = self.entries.first().map(|entry| entry.id);
    }

    /// Selects the last image.
    pub fn select_last(&mut self) {
        self.selected = self.entries.last().map(|entry| entry.id);
    }

    /// The selected image's id.
    pub fn selected(&self) -> Option<ImageId> {
        self.selected
    }

    /// The selected image.
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.selected.and_then(|id| self.get(id))
    }

    /// An image by id.
    pub fn get(&self, id: ImageId) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The image with this key.
    pub fn find(&self, key: &PathKey) -> Option<ImageId> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| entry.id)
    }

    /// The images of a group, in order.
    pub fn group(&self, group: &str) -> Vec<ImageId> {
        self.entries
            .iter()
            .filter(|entry| entry.group.as_deref() == Some(group))
            .map(|entry| entry.id)
            .collect()
    }

    /// The images, in order.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// How many images there are.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn index(&self, id: ImageId) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn add(set: &mut OpenSet, path: &str, group: Option<&str>) -> (ImageId, bool) {
        set.add(
            PathBuf::from(path),
            PathKey::new(Path::new(path)),
            None,
            group.map(str::to_string),
        )
    }

    fn order(set: &OpenSet) -> Vec<u32> {
        set.iter().map(|entry| entry.id.0).collect()
    }

    #[test]
    fn normalise_resolves_dots_separators_and_verbatim_prefixes() {
        assert_eq!(
            normalise(Path::new(r"C:\a\.\b\..\c.png")),
            PathBuf::from(r"C:\a\c.png")
        );
        assert_eq!(
            normalise(Path::new("C:/a/b.png")),
            PathBuf::from(r"C:\a\b.png")
        );
        assert_eq!(
            normalise(Path::new(r"\\?\C:\a\b.png")),
            PathBuf::from(r"C:\a\b.png")
        );
        assert_eq!(
            normalise(Path::new(r"\\?\UNC\server\share\b.png")),
            PathBuf::from(r"\\server\share\b.png")
        );
        assert_eq!(normalise(Path::new(r"C:\..\a")), PathBuf::from(r"C:\a"));
    }

    #[test]
    fn keys_ignore_case_and_route() {
        assert_eq!(
            PathKey::new(Path::new(r"C:\Shots\After.PNG")),
            PathKey::new(Path::new(r"c:\shots\run\..\after.png"))
        );
        assert_ne!(
            PathKey::new(Path::new(r"C:\shots\a.png")),
            PathKey::new(Path::new(r"C:\shots\b.png"))
        );
    }

    #[test]
    fn disk_files_find_the_same_file_by_different_paths() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("Shot.png"), b"x").unwrap();
        let direct = DiskFiles.canonical(&dir.join("Shot.png")).unwrap();
        let roundabout = DiskFiles
            .canonical(&dir.join("sub").join("..").join("SHOT.PNG"))
            .unwrap();
        assert_eq!(PathKey::new(&direct), PathKey::new(&roundabout));

        let missing = DiskFiles.canonical(&dir.join("missing.png")).unwrap_err();
        assert_eq!(missing.code, ErrorCode::FileNotFound);
        let directory = DiskFiles.canonical(&dir.join("sub")).unwrap_err();
        assert_eq!(directory.code, ErrorCode::OpenFailed);
        let invalid = DiskFiles.canonical(Path::new("C:\\bad\0name")).unwrap_err();
        assert_eq!(invalid.code, ErrorCode::OpenFailed);
    }

    #[test]
    fn adding_keeps_order_and_finds_duplicates() {
        let mut set = OpenSet::default();
        assert!(set.is_empty());
        assert_eq!(set.selected(), None);
        assert_eq!(add(&mut set, r"C:\a.png", None), (ImageId(1), false));
        assert_eq!(add(&mut set, r"C:\b.png", None), (ImageId(2), false));
        assert_eq!(add(&mut set, r"C:\A.PNG", None), (ImageId(1), true));
        assert_eq!(order(&set), [1, 2]);
        assert_eq!(set.len(), 2);
        assert_eq!(set.selected(), Some(ImageId(1)));
        assert_eq!(
            set.get(ImageId(1)).unwrap().path,
            PathBuf::from(r"C:\a.png")
        );
        assert_eq!(
            set.find(&PathKey::new(Path::new(r"c:\B.png"))),
            Some(ImageId(2))
        );
        assert_eq!(set.find(&PathKey::new(Path::new(r"c:\z.png"))), None);
    }

    #[test]
    fn adding_again_updates_the_caption() {
        let mut set = OpenSet::new();
        set.add(
            "C:\\a.png".into(),
            PathKey::new(Path::new("C:\\a.png")),
            Some("before".into()),
            None,
        );
        set.add(
            "C:\\a.png".into(),
            PathKey::new(Path::new("C:\\a.png")),
            None,
            None,
        );
        assert_eq!(
            set.iter().next().unwrap().caption.as_deref(),
            Some("before")
        );
        set.add(
            "C:\\a.png".into(),
            PathKey::new(Path::new("C:\\a.png")),
            Some("after".into()),
            None,
        );
        assert_eq!(set.iter().next().unwrap().caption.as_deref(), Some("after"));
    }

    #[test]
    fn groups_stay_adjacent() {
        let mut set = OpenSet::new();
        add(&mut set, r"C:\1.png", Some("g"));
        add(&mut set, r"C:\2.png", None);
        add(&mut set, r"C:\3.png", Some("g"));
        add(&mut set, r"C:\4.png", Some("h"));
        assert_eq!(order(&set), [1, 3, 2, 4]);
        assert_eq!(set.group("g"), [ImageId(1), ImageId(3)]);

        // Joining a group moves the image to the group's end.
        assert_eq!(add(&mut set, r"C:\2.png", Some("h")), (ImageId(2), true));
        assert_eq!(order(&set), [1, 3, 4, 2]);
        // The same group again changes nothing.
        add(&mut set, r"C:\1.png", Some("g"));
        assert_eq!(order(&set), [1, 3, 4, 2]);
    }

    #[test]
    fn removing_moves_the_selection_to_the_next_then_the_previous() {
        let mut set = OpenSet::new();
        for path in [r"C:\1.png", r"C:\2.png", r"C:\3.png"] {
            add(&mut set, path, None);
        }
        assert!(set.select(ImageId(2)));
        assert!(!set.select(ImageId(9)));
        assert_eq!(set.remove(ImageId(2)).unwrap().id, ImageId(2));
        assert_eq!(set.selected(), Some(ImageId(3)));
        set.remove(ImageId(3));
        assert_eq!(set.selected(), Some(ImageId(1)));
        // Removing an unselected image leaves the selection alone.
        add(&mut set, r"C:\4.png", None);
        set.remove(ImageId(4));
        assert_eq!(set.selected(), Some(ImageId(1)));
        assert!(set.remove(ImageId(4)).is_none());
        set.remove(ImageId(1));
        assert_eq!(set.selected(), None);
        assert_eq!(set.selected_entry(), None);
        // Ids are never reused.
        assert_eq!(add(&mut set, r"C:\1.png", None), (ImageId(5), false));
    }

    #[test]
    fn stepping_stops_at_the_ends() {
        let mut set = OpenSet::new();
        assert!(!set.step(1));
        for path in [r"C:\1.png", r"C:\2.png", r"C:\3.png"] {
            add(&mut set, path, None);
        }
        assert!(!set.step(-1));
        assert!(set.step(1));
        assert_eq!(set.selected(), Some(ImageId(2)));
        assert!(set.step(5));
        assert_eq!(set.selected(), Some(ImageId(3)));
        assert!(!set.step(1));
        set.select_first();
        assert_eq!(set.selected_entry().unwrap().id, ImageId(1));
        set.select_last();
        assert_eq!(set.selected(), Some(ImageId(3)));
    }
}
