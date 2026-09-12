// SPDX-License-Identifier: GPL-3.0-only

use cosmic::widget::image::Handle;
use image::DynamicImage;
use lru::LruCache;
use std::{
    collections::HashSet,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::animation::Animation;

/// Cached image with optional full-resolution pixels.
#[derive(Clone)]
pub struct CachedImage {
    pub handle: Handle,
    pub image: Option<Arc<DynamicImage>>,
    pub width: u32,
    pub height: u32,
    /// Lazy frame source for animated GIFs; `None` for static images.
    pub animation: Option<Arc<Animation>>,
}

impl std::fmt::Debug for CachedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("full_pixels", &self.image.is_some())
            .field("animation", &self.animation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ImageCache {
    full_images: Arc<Mutex<LruCache<PathBuf, CachedImage>>>,
    thumbnails: Arc<Mutex<ThumbnailStore>>,
    pending: Arc<Mutex<HashSet<PathBuf>>>,
    pending_thumbnails: Arc<Mutex<HashSet<PathBuf>>>,
    pending_previews: Arc<Mutex<HashSet<PathBuf>>>,
}

/// Memory budget for navigation thumbnails.
const THUMBNAIL_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// LRU thumbnail store bounded by pixel bytes.
#[derive(Debug)]
struct ThumbnailStore {
    entries: LruCache<PathBuf, Handle>,
    bytes: usize,
}

impl ThumbnailStore {
    fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            entries: LruCache::new(capacity),
            bytes: 0,
        }
    }

    fn get(&mut self, path: &Path) -> Option<Handle> {
        self.entries.get(path).cloned()
    }

    fn insert(&mut self, path: PathBuf, handle: Handle) {
        let added = handle_bytes(&handle);
        if let Some(evicted) = self.entries.put(path, handle) {
            self.bytes = self.bytes.saturating_sub(handle_bytes(&evicted));
        }
        self.bytes = self.bytes.saturating_add(added);

        while self.bytes > THUMBNAIL_BUDGET_BYTES {
            let Some((_, evicted)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(handle_bytes(&evicted));
        }
    }

    fn remove(&mut self, path: &Path) {
        if let Some(handle) = self.entries.pop(path) {
            self.bytes = self.bytes.saturating_sub(handle_bytes(&handle));
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// Bytes retained by a decoded image handle.
// reason: `bytes::Bytes::len` is not `const`, so this cannot be a `const fn`.
#[allow(clippy::missing_const_for_fn)]
fn handle_bytes(handle: &Handle) -> usize {
    match handle {
        Handle::Rgba { width, height, .. } => (*width as usize)
            .saturating_mul(*height as usize)
            .saturating_mul(4),
        Handle::Bytes(_, bytes) => bytes.len(),
        Handle::Path(..) => 0,
    }
}

impl ImageCache {
    #[must_use]
    pub fn new(full_capacity: usize, thumbnail_capacity: usize) -> Self {
        // `max(1)` guarantees a non-zero capacity; the `unwrap_or` branch is
        // therefore unreachable and avoids a panicking path entirely.
        let full = NonZeroUsize::new(full_capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            full_images: Arc::new(Mutex::new(LruCache::new(full))),
            thumbnails: Arc::new(Mutex::new(ThumbnailStore::new(thumbnail_capacity))),
            pending: Arc::new(Mutex::new(HashSet::new())),
            pending_thumbnails: Arc::new(Mutex::new(HashSet::new())),
            pending_previews: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(20, 1000)
    }

    pub fn resize(&self, new_capacity: usize) {
        // `max(1)` guarantees a non-zero capacity; the `unwrap_or` branch is
        // therefore unreachable and avoids a panicking path entirely.
        let capacity = NonZeroUsize::new(new_capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        if let Ok(mut cache) = self.full_images.lock() {
            cache.resize(capacity);
        }
    }

    #[must_use]
    pub fn get_full(&self, path: &Path) -> Option<CachedImage> {
        self.full_images.lock().ok()?.get(path).cloned()
    }

    pub fn insert_full(&self, path: PathBuf, image: CachedImage) {
        self.clear_pending(&path);
        if let Ok(mut cache) = self.full_images.lock() {
            cache.put(path, image);
        }
    }

    /// Insert a preview unless an entry is already cached.
    pub fn insert_preview(&self, path: PathBuf, image: CachedImage) {
        self.clear_pending_preview(&path);
        if let Ok(mut cache) = self.full_images.lock()
            && cache.peek(&path).is_none()
        {
            cache.put(path, image);
        }
    }

    /// Drop full-resolution pixels for every cached image except `keep`.
    /// Display textures stay cached so navigation remains instant.
    pub fn release_full_pixels_except(&self, keep: &Path) {
        if let Ok(mut cache) = self.full_images.lock() {
            for (path, cached) in cache.iter_mut() {
                if path.as_path() != keep {
                    cached.image = None;
                }
            }
        }
    }

    /// Drop full-resolution pixels for one cached image.
    pub fn release_full_pixels(&self, path: &Path) {
        if let Ok(mut cache) = self.full_images.lock()
            && let Some(cached) = cache.get_mut(path)
        {
            cached.image = None;
        }
    }

    pub fn remove_full(&self, path: &PathBuf) {
        if let Ok(mut cache) = self.full_images.lock() {
            cache.pop(path);
        }
    }

    #[must_use]
    pub fn get_thumbnail(&self, path: &Path) -> Option<Handle> {
        self.thumbnails.lock().ok()?.get(path)
    }

    pub fn insert_thumbnail(&self, path: PathBuf, handle: Handle) {
        self.clear_pending_thumbnail(&path);
        if let Ok(mut cache) = self.thumbnails.lock() {
            cache.insert(path, handle);
        }
    }

    pub fn remove_thumbnail(&self, path: &Path) {
        if let Ok(mut cache) = self.thumbnails.lock() {
            cache.remove(path);
        }
    }

    /// Bytes currently retained by cached nav thumbnails.
    #[must_use]
    pub fn thumbnail_bytes(&self) -> usize {
        self.thumbnails.lock().map_or(0, |cache| cache.bytes)
    }

    #[must_use]
    pub fn pending_thumbnail_count(&self) -> usize {
        self.pending_thumbnails.lock().map_or(0, |set| set.len())
    }

    #[must_use]
    pub fn is_thumbnail_pending(&self, path: &PathBuf) -> bool {
        self.pending_thumbnails
            .lock()
            .is_ok_and(|set| set.contains(path))
    }

    pub fn set_thumbnail_pending(&self, path: PathBuf) {
        if let Ok(mut set) = self.pending_thumbnails.lock() {
            set.insert(path);
        }
    }

    pub fn clear_pending_thumbnail(&self, path: &PathBuf) {
        if let Ok(mut set) = self.pending_thumbnails.lock() {
            set.remove(path);
        }
    }

    #[must_use]
    pub fn is_pending(&self, path: &PathBuf) -> bool {
        self.pending.lock().is_ok_and(|set| set.contains(path))
    }

    pub fn set_pending(&self, path: PathBuf) {
        if let Ok(mut set) = self.pending.lock() {
            set.insert(path);
        }
    }

    /// Mark a full-resolution decode as pending.
    #[must_use]
    pub fn try_set_pending(&self, path: &Path) -> bool {
        self.pending
            .lock()
            .is_ok_and(|mut set| set.insert(path.to_path_buf()))
    }

    pub fn clear_pending(&self, path: &PathBuf) {
        if let Ok(mut set) = self.pending.lock() {
            set.remove(path);
        }
    }

    #[must_use]
    pub fn is_preview_pending(&self, path: &PathBuf) -> bool {
        self.pending_previews
            .lock()
            .is_ok_and(|set| set.contains(path))
    }

    /// Mark a preview decode as pending.
    #[must_use]
    pub fn try_set_preview_pending(&self, path: &Path) -> bool {
        self.pending_previews
            .lock()
            .is_ok_and(|mut set| set.insert(path.to_path_buf()))
    }

    pub fn clear_pending_preview(&self, path: &PathBuf) {
        if let Ok(mut set) = self.pending_previews.lock() {
            set.remove(path);
        }
    }

    pub fn clear_thumbnails(&self) {
        if let Ok(mut cache) = self.thumbnails.lock() {
            cache.clear();
        }
        if let Ok(mut set) = self.pending_thumbnails.lock() {
            set.clear();
        }
    }

    pub fn clear(&self) {
        if let Ok(mut cache) = self.full_images.lock() {
            cache.clear();
        }

        if let Ok(mut cache) = self.thumbnails.lock() {
            cache.clear();
        }

        if let Ok(mut set) = self.pending.lock() {
            set.clear();
        }

        if let Ok(mut set) = self.pending_thumbnails.lock() {
            set.clear();
        }

        if let Ok(mut set) = self.pending_previews.lock() {
            set.clear();
        }
    }
}

impl Default for ImageCache {
    fn default() -> Self {
        Self::with_defaults()
    }
}
