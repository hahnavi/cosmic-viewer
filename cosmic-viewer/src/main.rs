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
    iced::Limits,
    widget::icon,
};
use std::{
    env::args,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use crate::icon_cache::IconCache;

static ICON_CACHE: OnceLock<Mutex<IconCache>> = OnceLock::new();

pub fn icon_cache_get(name: &'static str) -> icon::Handle {
    let mut icon_cache = ICON_CACHE.get().unwrap().lock().unwrap();
    icon_cache.get(name)
}

fn main() -> cosmic::iced::Result {
    ICON_CACHE.get_or_init(|| Mutex::new(IconCache::new()));

    let settings = Settings::default()
        .exit_on_close(false)
        .size_limits(Limits::NONE.min_width(360.0).min_height(300.0));

    // Get the image if opened from the file manager or cli
    let optional_image = args().nth(1).map(PathBuf::from);

    run::<CosmicViewer>(settings, optional_image)
}
