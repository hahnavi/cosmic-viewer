// SPDX-License-Identifier: GPL-3.0-only

use image::{
    Delay, Frame, Rgb, RgbImage, Rgba, RgbaImage,
    codecs::{
        gif::{GifEncoder, Repeat},
        jpeg::JpegEncoder,
    },
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use viewer_core::{load_image, load_preview, load_thumbnail};

fn write_solid_jpeg(path: &Path, width: u32, height: u32) {
    let image = RgbImage::from_pixel(width, height, Rgb([20, 80, 160]));
    let file = File::create(path).expect("create test JPEG");
    JpegEncoder::new(file)
        .encode_image(&image)
        .expect("encode test JPEG");
}

fn handle_dims(handle: &cosmic::widget::image::Handle) -> (u32, u32) {
    match handle {
        cosmic::widget::image::Handle::Rgba { width, height, .. } => (*width, *height),
        _ => panic!("expected an RGBA texture handle"),
    }
}

fn write_animated_gif(path: &Path, frames: u32, delay_ms: u32) {
    let file = File::create(path).expect("create test GIF");
    let mut encoder = GifEncoder::new(file);
    encoder.set_repeat(Repeat::Infinite).expect("set repeat");
    for i in 0..frames {
        let color = Rgba([u8::try_from(i * 60).expect("small test index"), 0, 0, 255]);
        let image = RgbaImage::from_pixel(8, 8, color);
        let frame = Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
        encoder.encode_frame(frame).expect("encode frame");
    }
}

fn test_images_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../test_images"))
}

#[tokio::test]
async fn load_jpeg_has_valid_dimensions() {
    let path = test_images_dir().join("1315754.jpg");
    let loaded = load_image(path.clone()).await.expect("should load JPEG");
    assert!(loaded.width > 0);
    assert!(loaded.height > 0);
    assert_eq!(loaded.path, path);
}

#[tokio::test]
async fn load_png_has_valid_dimensions() {
    let path = test_images_dir().join("test_format.png");
    let loaded = load_image(path.clone()).await.expect("should load PNG");
    assert!(loaded.width > 0);
    assert!(loaded.height > 0);
    assert_eq!(loaded.path, path);
}

#[tokio::test]
async fn load_webp_has_valid_dimensions() {
    let path = test_images_dir().join("test_format.webp");
    let loaded = load_image(path.clone()).await.expect("should load WebP");
    assert!(loaded.width > 0);
    assert!(loaded.height > 0);
}

#[tokio::test]
async fn load_nonexistent_file_returns_error() {
    let path = test_images_dir().join("no_such_file_at_all.jpg");
    assert!(load_image(path).await.is_err());
}

#[tokio::test]
async fn load_non_image_file_returns_error() {
    let tmp = std::env::temp_dir().join("cosmic-viewer-test-not-image.txt");
    std::fs::write(&tmp, b"this is not an image").unwrap();
    let result = load_image(tmp.clone()).await;
    assert!(result.is_err());
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn thumbnail_smaller_than_original() {
    let path = test_images_dir().join("1315754.jpg");
    let full = load_image(path.clone()).await.expect("full load");
    let thumb = load_thumbnail(path, 128).await.expect("thumb load");

    let full_pixels = u64::from(full.width) * u64::from(full.height);
    let thumb_pixels = u64::from(thumb.width) * u64::from(thumb.height);
    assert!(thumb_pixels < full_pixels);
    assert!(thumb.width <= 128);
    assert!(thumb.height <= 128);
}

#[tokio::test]
async fn preview_of_jpeg_below_texture_cap_keeps_source_resolution() {
    let path = std::env::temp_dir().join("cosmic-viewer-test-small-preview.jpg");
    write_solid_jpeg(&path, 512, 384);

    let preview = load_preview(path.clone(), viewer_core::MAX_TEX)
        .await
        .expect("preview load");
    assert_eq!((preview.width, preview.height), (512, 384));
    assert_eq!(
        handle_dims(&preview.handle),
        (512, 384),
        "a JPEG smaller than the texture cap must not be decoded at a reduced DCT scale"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn thumbnail_of_jpeg_below_target_keeps_source_resolution() {
    let path = std::env::temp_dir().join("cosmic-viewer-test-small-thumb.jpg");
    write_solid_jpeg(&path, 96, 64);

    let thumb = load_thumbnail(path.clone(), 128)
        .await
        .expect("thumbnail load");
    assert_eq!(
        handle_dims(&thumb.handle),
        (96, 64),
        "a source smaller than the thumbnail target must not be downscaled"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn load_same_image_twice_deterministic() {
    let path = test_images_dir().join("1315754.jpg");
    let a = load_image(path.clone()).await.expect("first load");
    let b = load_image(path).await.expect("second load");
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
}

#[tokio::test]
async fn load_bmp_has_valid_dimensions() {
    let path = test_images_dir().join("test_format.bmp");
    let loaded = load_image(path).await.expect("should load BMP");
    assert!(loaded.width > 0 && loaded.height > 0);
}

#[tokio::test]
async fn load_gif_has_valid_dimensions() {
    let path = test_images_dir().join("test_format.gif");
    let loaded = load_image(path).await.expect("should load GIF");
    assert!(loaded.width > 0 && loaded.height > 0);
}

#[tokio::test]
async fn load_static_gif_has_no_animation() {
    let path = test_images_dir().join("test_format.gif");
    let loaded = load_image(path).await.expect("should load GIF");
    assert!(
        loaded.animation.is_none(),
        "a single-frame GIF must stay static"
    );
}

#[tokio::test]
async fn load_animated_gif_exposes_lazy_frames() {
    let path = std::env::temp_dir().join("cosmic-viewer-test-animated.gif");
    write_animated_gif(&path, 3, 100);

    let loaded = load_image(path.clone())
        .await
        .expect("should load animated GIF");
    assert_eq!(loaded.width, 8);
    assert_eq!(loaded.height, 8);

    let animation = loaded
        .animation
        .expect("animated GIF should expose an animation");
    assert_eq!(
        animation.path(),
        path.as_path(),
        "the animation streams frames from the file instead of retaining them"
    );
    assert_eq!(animation.first_delay(), Duration::from_millis(100));

    let mut player = animation.player().expect("player");
    let mut delays = Vec::new();
    let mut colors = Vec::new();
    for _ in 0..3 {
        let frame = player
            .next_frame()
            .expect("decode frame")
            .expect("three frames");
        delays.push(frame.delay);
        colors.push(*frame.image.get_pixel(0, 0));
    }
    assert_eq!(delays, [Duration::from_millis(100); 3]);
    assert_ne!(colors[0], colors[1], "each frame has its own pixels");

    // Playback loops back to the first frame after the last one.
    let wrapped = player
        .next_frame_looping()
        .expect("decode wrapped frame")
        .expect("loops");
    assert_eq!(wrapped.delay, Duration::from_millis(100));
    assert_eq!(wrapped.image.get_pixel(0, 0), &colors[0]);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn fast_gif_frames_are_clamped_to_100ms() {
    let path = std::env::temp_dir().join("cosmic-viewer-test-fast.gif");
    // 10 ms is GIF delay unit 1, which browsers render as 100 ms.
    write_animated_gif(&path, 2, 10);

    let loaded = load_image(path.clone()).await.expect("should load GIF");
    let animation = loaded.animation.expect("animated GIF");
    let mut player = animation.player().expect("player");
    for _ in 0..2 {
        let frame = player
            .next_frame()
            .expect("decode frame")
            .expect("two frames");
        assert_eq!(
            frame.delay,
            Duration::from_millis(100),
            "sub-20 ms delays must be clamped instead of playing at 10 ms"
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn animated_gif_first_frame_matches_static_image() {
    let path = std::env::temp_dir().join("cosmic-viewer-test-animated-first.gif");
    write_animated_gif(&path, 2, 100);

    let loaded = load_image(path.clone())
        .await
        .expect("should load animated GIF");
    assert!(
        loaded.animation.is_some(),
        "two-frame GIF should report an animation"
    );
    // The editable/saveable pixels are the composited first frame.
    assert_eq!(loaded.image.width(), 8);
    assert_eq!(loaded.image.height(), 8);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn load_preview_is_display_sized_with_source_dimensions() {
    let path = test_images_dir().join("1315754.jpg");
    let full = load_image(path.clone()).await.expect("full");
    let preview = load_preview(path, 64).await.expect("preview");

    assert_eq!(
        (preview.width, preview.height),
        (full.width, full.height),
        "preview reports the source dimensions, not the texture size"
    );
}

#[tokio::test]
async fn load_preview_keeps_gif_animation_lazy() {
    let path = std::env::temp_dir().join("cosmic-viewer-test-preview.gif");
    write_animated_gif(&path, 3, 100);

    let preview = load_preview(path.clone(), 4).await.expect("preview");
    assert_eq!((preview.width, preview.height), (8, 8));
    let animation = preview
        .animation
        .expect("preview of an animated GIF is playable");
    let mut player = animation.player().expect("player");
    assert!(
        player.next_frame().expect("frame").is_some(),
        "player decodes from the preview's source"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn load_tiff_has_valid_dimensions() {
    let path = test_images_dir().join("test_format.tiff");
    let loaded = load_image(path).await.expect("should load TIFF");
    assert!(loaded.width > 0 && loaded.height > 0);
}

#[tokio::test]
async fn load_ico_has_valid_dimensions() {
    let path = test_images_dir().join("test_format.ico");
    let loaded = load_image(path).await.expect("should load ICO");
    assert!(loaded.width > 0 && loaded.height > 0);
}

#[tokio::test]
async fn thumbnail_at_multiple_sizes() {
    let path = test_images_dir().join("1315754.jpg");
    for max in [64, 128, 256] {
        let thumb = load_thumbnail(path.clone(), max)
            .await
            .unwrap_or_else(|e| panic!("thumb at {max} failed: {e}"));
        assert!(thumb.width <= max && thumb.height <= max);
        assert!(thumb.width > 0 && thumb.height > 0);
    }
}

#[tokio::test]
async fn thumbnail_preserves_aspect_ratio() {
    let path = test_images_dir().join("1315754.jpg");
    let full = load_image(path.clone()).await.expect("full");
    let thumb = load_thumbnail(path, 256).await.expect("thumb");

    let full_ratio = f64::from(full.width) / f64::from(full.height);
    let thumb_ratio = f64::from(thumb.width) / f64::from(thumb.height);
    assert!(
        (full_ratio - thumb_ratio).abs() < 0.05,
        "aspect ratio diverged: full={full_ratio:.3}, thumb={thumb_ratio:.3}",
    );
}
