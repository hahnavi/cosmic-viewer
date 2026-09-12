// SPDX-License-Identifier: GPL-3.0-only

pub mod animation;
pub mod cache;
pub mod clipboard;
pub mod loader;
pub mod nav;

// Re-exports
pub use animation::{Animation, GifPlayer};
pub use cache::{CachedImage, ImageCache};
pub use clipboard::{ClipboardImage, image_mime_type};
pub use loader::{
    LoadError, LoadedImage, MAX_TEX, PreviewImage, display_handle, display_handle_owned,
    load_image, load_preview, load_thumbnail, read_dpi,
};
pub use nav::{NavState, get_image_dir, is_supported_image, scan_dir};
