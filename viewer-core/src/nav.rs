// SPDX-License-Identifier: GPL-3.0-only

use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
};
use tokio::task::spawn_blocking;
use viewer_config::{SortMode, SortOrder};

pub const EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif", "ico", "avif", "ppm", "pgm", "pbm",
    "pnm", "qoi", "ff", "farbfeld", "hdr", "jxl", "svg", "heif", "heic",
];

#[derive(Debug, Clone, Default)]
pub struct NavState {
    dir: Option<PathBuf>,
    images: Vec<PathBuf>,
    cur_idx: Option<usize>,
}

impl NavState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn current(&self) -> Option<&PathBuf> {
        self.cur_idx.and_then(|idx| self.images.get(idx))
    }

    #[must_use]
    pub const fn index(&self) -> Option<usize> {
        self.cur_idx
    }

    #[must_use]
    pub const fn is_selected(&self) -> bool {
        self.cur_idx.is_some()
    }

    #[must_use]
    pub const fn total(&self) -> usize {
        self.images.len()
    }

    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    #[must_use]
    pub fn images(&self) -> &[PathBuf] {
        &self.images
    }

    pub fn set_images(&mut self, dir: PathBuf, images: Vec<PathBuf>, select: Option<&Path>) {
        self.dir = Some(dir);
        self.images = images;
        self.cur_idx = select.and_then(|path| self.images.iter().position(|pos| pos == path));
    }

    pub fn select(&mut self, idx: usize) -> Option<&PathBuf> {
        if idx < self.images.len() {
            self.cur_idx = Some(idx);
            self.current()
        } else {
            None
        }
    }

    pub const fn deselect(&mut self) {
        self.cur_idx = None;
    }

    pub fn go_next(&mut self) -> Option<&PathBuf> {
        if self.images.is_empty() {
            return None;
        }

        let next = (self.cur_idx.unwrap_or(0) + 1) % self.images.len();
        self.cur_idx = Some(next);
        self.current()
    }

    pub fn go_prev(&mut self) -> Option<&PathBuf> {
        if self.images.is_empty() {
            return None;
        }

        let prev = match self.cur_idx {
            Some(0) | None => self.images.len() - 1,
            Some(idx) => idx - 1,
        };
        self.cur_idx = Some(prev);
        self.current()
    }

    pub fn first(&mut self) -> Option<&PathBuf> {
        if self.images.is_empty() {
            return None;
        }

        self.cur_idx = Some(0);
        self.current()
    }

    pub fn last(&mut self) -> Option<&PathBuf> {
        if self.images.is_empty() {
            return None;
        }

        self.cur_idx = Some(self.images.len() - 1);
        self.current()
    }
}

#[must_use]
pub fn get_image_dir(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        path.parent().map(std::path::Path::to_path_buf)
    } else if path.is_dir() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

pub async fn scan_dir(
    dir: &Path,
    include_hidden: bool,
    sort_mode: SortMode,
    sort_order: SortOrder,
) -> Vec<PathBuf> {
    let dir = dir.to_path_buf();
    spawn_blocking(move || scan_dir_sync(&dir, include_hidden, sort_mode, sort_order))
        .await
        .unwrap_or_default()
}

fn scan_dir_sync(
    dir: &Path,
    include_hidden: bool,
    sort_mode: SortMode,
    sort_order: SortOrder,
) -> Vec<PathBuf> {
    let mut images: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            if !include_hidden
                && let Some(name) = path.file_name().and_then(|name| name.to_str())
                && name.starts_with('.')
            {
                return false;
            }
            is_supported_image(path)
        })
        .collect();

    match sort_mode {
        SortMode::Name => {
            // Cache normalized names before natural sorting.
            let mut keyed: Vec<(String, PathBuf)> = images
                .into_iter()
                .map(|path| (natural_key(&path), path))
                .collect();
            keyed.sort_by(|(a, _), (b, _)| natural_cmp(a, b));
            images = keyed.into_iter().map(|(_, path)| path).collect();
        }
        // Cache metadata keys to avoid repeated filesystem lookups.
        SortMode::Date => images.sort_by_cached_key(|path| modified_time(path)),
        SortMode::Size => images.sort_by_cached_key(|path| file_size(path)),
    }

    if sort_order == SortOrder::Descending {
        images.reverse();
    }

    images
}

fn modified_time(path: &Path) -> std::time::SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

#[must_use]
pub fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        })
}

/// Case-folded file name used as the cached key for [`natural_cmp`].
fn natural_key(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a_chars = a.chars().peekable();
    let mut b_chars = b.chars().peekable();

    loop {
        match (a_chars.peek().copied(), b_chars.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ac), Some(bc)) if ac.is_ascii_digit() && bc.is_ascii_digit() => {
                match collect_number(&mut a_chars).cmp(&collect_number(&mut b_chars)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(ac), Some(bc)) => match ac.cmp(&bc) {
                Ordering::Equal => {
                    a_chars.next();
                    b_chars.next();
                }
                other => return other,
            },
        }
    }
}

fn collect_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> u64 {
    let mut num: u64 = 0;

    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            num = num.saturating_mul(10).saturating_add(c as u64 - '0' as u64);
            chars.next();
        } else {
            break;
        }
    }

    num
}
