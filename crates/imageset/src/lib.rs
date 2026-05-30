use std::cmp::Ordering;
use std::path::{Path, PathBuf};

const SUPPORTED: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];

/// True if the path has a supported image extension (case-insensitive).
pub fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let lower = ext.to_ascii_lowercase();
            SUPPORTED.contains(&lower.as_str())
        }
        None => false,
    }
}

/// Natural, case-insensitive comparison: "img2" < "img10", "A" == "a".
pub fn natural_cmp_ci(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let mut na = String::new();
                    while let Some(&c) = ai.peek() {
                        if c.is_ascii_digit() {
                            na.push(c);
                            ai.next();
                        } else {
                            break;
                        }
                    }
                    let mut nb = String::new();
                    while let Some(&c) = bi.peek() {
                        if c.is_ascii_digit() {
                            nb.push(c);
                            bi.next();
                        } else {
                            break;
                        }
                    }
                    let ta = na.trim_start_matches('0');
                    let tb = nb.trim_start_matches('0');
                    let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = ca.to_ascii_lowercase().cmp(&cb.to_ascii_lowercase());
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// All supported image files directly in `dir`, natural-sorted by file name.
/// Returns an empty `Vec` if the directory can't be read — errors are intentionally
/// swallowed; callers treat "empty" as "nothing to show".
pub fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_supported(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort_by(|a, b| {
        let an = a
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bn = b
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        natural_cmp_ci(&an, &bn)
    });
    files
}

/// A navigable, wrap-around set of image paths with a current cursor.
pub struct ImageSet {
    files: Vec<PathBuf>,
    idx: usize,
}

impl ImageSet {
    pub fn new(files: Vec<PathBuf>, start: usize) -> Self {
        let idx = if files.is_empty() {
            0
        } else {
            start.min(files.len() - 1)
        };
        ImageSet { files, idx }
    }

    pub fn empty() -> Self {
        ImageSet {
            files: Vec::new(),
            idx: 0,
        }
    }

    /// Scan the file's directory and position the cursor on that file.
    /// Falls back to a single-element set if the directory can't be scanned.
    /// Path matching is byte-exact (no symlink / `..` canonicalization).
    pub fn from_file(path: &Path) -> Self {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let files = scan_dir(dir);
        if files.is_empty() {
            return ImageSet {
                files: vec![path.to_path_buf()],
                idx: 0,
            };
        }
        let idx = files.iter().position(|p| p == path).unwrap_or(0);
        ImageSet { files, idx }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 0-based index of the current cursor.
    pub fn position(&self) -> usize {
        self.idx
    }

    #[must_use]
    pub fn current(&self) -> Option<PathBuf> {
        self.files.get(self.idx).cloned()
    }

    /// The path `offset` steps from the cursor, with wrap-around, WITHOUT moving the
    /// cursor. `peek(0)` == `current()`. Returns `None` if the set is empty. Used to
    /// compute the prefetch keep-set (current ±1) without disturbing navigation.
    #[must_use]
    pub fn peek(&self, offset: isize) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        let len = self.files.len() as isize;
        let i = (self.idx as isize + offset).rem_euclid(len) as usize;
        self.files.get(i).cloned()
    }

    /// Advance the cursor with wrap-around; returns the new current path.
    /// Named `advance` (not `next`) to avoid shadowing `Iterator::next`.
    pub fn advance(&mut self) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        self.idx = (self.idx + 1) % self.files.len();
        self.current()
    }

    /// Step the cursor back with wrap-around; returns the new current path.
    pub fn retreat(&mut self) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        self.idx = (self.idx + self.files.len() - 1) % self.files.len();
        self.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn supported_extensions_are_case_insensitive() {
        assert!(is_supported(&PathBuf::from("a.JPG")));
        assert!(is_supported(&PathBuf::from("a.jpeg")));
        assert!(is_supported(&PathBuf::from("a.png")));
        assert!(is_supported(&PathBuf::from("a.WebP")));
        assert!(is_supported(&PathBuf::from("a.gif")));
        assert!(!is_supported(&PathBuf::from("a.txt")));
        assert!(!is_supported(&PathBuf::from("a")));
    }

    #[test]
    fn natural_sort_orders_numbers_numerically_and_ignores_case() {
        let mut v = ["img10.jpg", "img2.jpg", "IMG1.jpg"];
        v.sort_by(|a, b| natural_cmp_ci(a, b));
        assert_eq!(v, ["IMG1.jpg", "img2.jpg", "img10.jpg"]);
    }

    #[test]
    fn imageset_next_prev_wrap() {
        let files = vec![
            PathBuf::from("/d/a.jpg"),
            PathBuf::from("/d/b.jpg"),
            PathBuf::from("/d/c.jpg"),
        ];
        let mut set = ImageSet::new(files, 0);
        assert_eq!(set.len(), 3);
        assert_eq!(set.current(), Some(PathBuf::from("/d/a.jpg")));
        assert_eq!(set.advance(), Some(PathBuf::from("/d/b.jpg")));
        assert_eq!(set.advance(), Some(PathBuf::from("/d/c.jpg")));
        assert_eq!(set.advance(), Some(PathBuf::from("/d/a.jpg")));
        assert_eq!(set.retreat(), Some(PathBuf::from("/d/c.jpg")));
    }

    #[test]
    fn empty_set_is_safe() {
        let mut set = ImageSet::empty();
        assert!(set.is_empty());
        assert_eq!(set.current(), None);
        assert_eq!(set.advance(), None);
        assert_eq!(set.retreat(), None);
    }

    #[test]
    fn scan_dir_filters_and_sorts_and_from_file_positions() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b2.jpg", "b10.jpg", "a.png", "notes.txt"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let files = scan_dir(dir.path());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.png", "b2.jpg", "b10.jpg"]);

        let set = ImageSet::from_file(&dir.path().join("b2.jpg"));
        assert_eq!(set.len(), 3);
        assert_eq!(set.position(), 1);
    }

    #[test]
    fn peek_offsets_wrap_without_moving_the_cursor() {
        let files = vec![
            PathBuf::from("/d/a.jpg"),
            PathBuf::from("/d/b.jpg"),
            PathBuf::from("/d/c.jpg"),
        ];
        let set = ImageSet::new(files, 0);
        // peek(0) is the current item.
        assert_eq!(set.peek(0), set.current());
        assert_eq!(set.peek(0), Some(PathBuf::from("/d/a.jpg")));
        // +1 / -1 wrap at both ends, and the cursor does NOT move.
        assert_eq!(set.peek(1), Some(PathBuf::from("/d/b.jpg")));
        assert_eq!(set.peek(-1), Some(PathBuf::from("/d/c.jpg"))); // wraps to the end
        assert_eq!(set.position(), 0, "peek must not move the cursor");

        // From the last item, +1 wraps to the first.
        let set_last = ImageSet::new(
            vec![PathBuf::from("/d/a.jpg"), PathBuf::from("/d/b.jpg")],
            1,
        );
        assert_eq!(set_last.peek(1), Some(PathBuf::from("/d/a.jpg")));
        assert_eq!(set_last.peek(-1), Some(PathBuf::from("/d/a.jpg")));

        // Empty set yields None for any offset.
        let empty = ImageSet::empty();
        assert_eq!(empty.peek(0), None);
        assert_eq!(empty.peek(1), None);
        assert_eq!(empty.peek(-1), None);
    }

    #[test]
    fn single_image_nav_stays_put() {
        let mut set = ImageSet::new(vec![PathBuf::from("/d/only.jpg")], 0);
        assert_eq!(set.advance(), Some(PathBuf::from("/d/only.jpg"))); // wraps to itself
        assert_eq!(set.retreat(), Some(PathBuf::from("/d/only.jpg")));
        assert_eq!(set.position(), 0);
    }
}
