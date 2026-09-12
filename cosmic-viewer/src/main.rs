// SPDX-License-Identifier: GPL-3.0-only

pub mod app;
pub mod icon_cache;
pub mod key_binds;
pub mod localize;
pub mod menu;
pub mod message;
pub mod watcher;

use app::CosmicViewer;
use cosmic::{
    app::{Settings, run},
    cosmic_config::CosmicConfigEntry,
    iced::Limits,
    widget::icon,
};
use std::{
    env::args,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
use viewer_config::{RenderMode, ViewerConfig};

use crate::icon_cache::IconCache;

static ICON_CACHE: OnceLock<Mutex<IconCache>> = OnceLock::new();

pub fn icon_cache_get(name: &'static str) -> icon::Handle {
    let mut icon_cache = ICON_CACHE.get().unwrap().lock().unwrap();
    icon_cache.get(name)
}

fn main() -> cosmic::iced::Result {
    apply_render_mode();

    ICON_CACHE.get_or_init(|| Mutex::new(IconCache::new()));

    let settings = Settings::default()
        .exit_on_close(false)
        .size_limits(Limits::NONE.min_width(360.0).min_height(300.0));

    // Get the image if opened from the file manager or cli
    let optional_image = args().nth(1).map(PathBuf::from);

    run::<CosmicViewer>(settings, optional_image)
}

/// Set the CPU renderer before the runtime boots when the saved config asks for it.
/// `ICED_BACKEND` still wins if it is already set.
fn apply_render_mode() {
    if std::env::var_os("ICED_BACKEND").is_some() {
        return;
    }

    let software = viewer_config::config()
        .ok()
        .and_then(|handle| ViewerConfig::get_entry(&handle).ok())
        .is_some_and(|config| config.render_mode == RenderMode::Software);

    if software {
        // SAFETY: called on the main thread before the iced runtime, and any
        // thread that reads the environment, is started.
        unsafe {
            std::env::set_var("ICED_BACKEND", "tiny-skia");
        }
    }
}
