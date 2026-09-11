// SPDX-License-Identifier: GPL-3.0-only

use cosmic::widget::image::Handle;
use fast_image_resize::{PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image as FirImage};
use image::{DynamicImage, RgbaImage};
use libheif_rs::{ColorSpace as HeifColorSpace, HeifContext, LibHeif, RgbChroma};
use resvg::{tiny_skia, usvg};
use std::{
    fmt::{self, Debug, Formatter},
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use turbojpeg::{Decompressor, Image, PixelFormat, ScalingFactor};
use zune_image::codecs::bmp::zune_core::colorspace::ColorSpace;
use zune_image::image::Image as ZuneImage;

// Cap texture uploads at 2048px on the long edge; the full-resolution image
// is kept separately so edits and saves operate on the real pixels, not the texture.
const MAX_TEX: u32 = 2048;

/// Limit concurrent full-resolution thumbnail decodes to reduce memory use.
static THUMBNAIL_DECODE_LIMIT: LazyLock<Semaphore> = LazyLock::new(|| {
    let permits = std::thread::available_parallelism()
        .map_or(2, std::num::NonZeroUsize::get)
        .clamp(2, 4);
    Semaphore::new(permits)
});

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("Failed to read file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to decode image: {0}")]
    Decode(#[from] image::ImageError),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Task cancelled")]
    Cancelled,
}

#[derive(Clone)]
pub struct LoadedImage {
    pub handle: Handle,
    pub image: Arc<DynamicImage>,
    pub width: u32,
    pub height: u32,
    pub path: PathBuf,
}

impl Debug for LoadedImage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // `handle` and `image` hold opaque pixel buffers; omit them from Debug.
        f.debug_struct("LoadedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Display texture, downscaled to `MAX_TEX`. The source image is left full-res.
/// Shared with `viewer-canvas` for display texture resizing.
#[must_use]
pub fn display_handle(image: &DynamicImage) -> Handle {
    let (width, height) = (image.width(), image.height());
    if (width > MAX_TEX || height > MAX_TEX)
        && let Ok((tw, th, pixels)) = fast_resize(image, MAX_TEX)
    {
        return Handle::from_rgba(tw, th, pixels);
    }
    let rgba = image.to_rgba8();
    Handle::from_rgba(width, height, rgba.into_raw())
}

/// Decode the image at `path` on a background thread.
///
/// # Errors
///
/// Returns [`LoadError`] if the file cannot be read, the format is unsupported
/// or fails to decode, or the decode task is cancelled before completion.
pub async fn load_image(path: PathBuf) -> Result<LoadedImage, LoadError> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    rayon::spawn(move || {
        let result = load_image_sync(&path);
        let _ = tx.send(result);
    });

    rx.await.map_err(|_| LoadError::Cancelled)?
}

fn load_image_sync(path: &Path) -> Result<LoadedImage, LoadError> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();

    if extension == "svg" {
        return load_svg_at(path, MAX_TEX);
    }
    if matches!(extension.as_str(), "heif" | "heic") {
        return load_heif(path);
    }

    // Use turbojpeg for JPEGs (faster than zune/image crate)
    if matches!(extension.as_str(), "jpg" | "jpeg")
        && let Ok(img) = load_jpeg_full(path)
    {
        return Ok(img);
    }
    // Fall through to other decoders if turbojpeg fails

    if is_zune_supported(&extension) {
        match load_with_zune(path) {
            Ok(img) => return Ok(img),
            Err(_) => {
                return load_with_image(path);
            }
        }
    }

    // Standard image formats via the 'image' crate
    load_with_image(path)
}

/// Load full JPEG using turbojpeg (faster than zune/image crate)
// JPEG header dimensions are far below u32::MAX, so the usize -> u32 casts
// cannot truncate.
#[allow(clippy::cast_possible_truncation)]
fn load_jpeg_full(path: &Path) -> Result<LoadedImage, LoadError> {
    let mut file = File::open(path)?;
    let mut jpeg_data = Vec::new();
    file.read_to_end(&mut jpeg_data)?;

    let mut decompressor = Decompressor::new()
        .map_err(|e| LoadError::UnsupportedFormat(format!("TurboJPEG init failed: {e}")))?;

    let header = decompressor
        .read_header(&jpeg_data)
        .map_err(|e| LoadError::UnsupportedFormat(format!("JPEG header error: {e}")))?;

    let width = header.width;
    let height = header.height;

    // Pre-allocate output buffer for RGBA (4 bytes per pixel)
    let mut pixels = vec![0u8; 4 * width * height];

    let mut output = Image {
        pixels: pixels.as_mut_slice(),
        width,
        pitch: 4 * width,
        height,
        format: PixelFormat::RGBA,
    };

    decompressor
        .decompress(&jpeg_data, output.as_deref_mut())
        .map_err(|e| LoadError::UnsupportedFormat(format!("JPEG decode error: {e}")))?;

    let rgba_image = RgbaImage::from_raw(width as u32, height as u32, pixels)
        .expect("pixel buffer matches dimensions");

    Ok(finish_loaded(DynamicImage::ImageRgba8(rgba_image), path))
}

fn is_zune_supported(extension: &str) -> bool {
    matches!(
        extension,
        "jpg"
            | "jpeg"
            | "png"
            | "ppm"
            | "pgm"
            | "pbm"
            | "pnm"
            | "bmp"
            | "qoi"
            | "ff"
            | "farbfeld"
            | "hdr"
            | "jxl"
    )
}

// Image dimensions originate from a decoded image header and are far below
// u32::MAX, so the usize -> u32 casts cannot truncate.
#[allow(clippy::cast_possible_truncation)]
fn load_with_zune(path: &Path) -> Result<LoadedImage, LoadError> {
    let mut img = ZuneImage::open(path).map_err(|e| LoadError::UnsupportedFormat(e.to_string()))?;

    img.convert_color(ColorSpace::RGBA)
        .map_err(|e| LoadError::UnsupportedFormat(e.to_string()))?;

    let (width, height) = img.dimensions();

    let pixels = img
        .flatten_to_u8()
        .into_iter()
        .next()
        .ok_or_else(|| LoadError::UnsupportedFormat("No pixel data".into()))?;

    let rgba_image = RgbaImage::from_raw(width as u32, height as u32, pixels)
        .expect("pixel buffer matches dimensions");

    Ok(finish_loaded(DynamicImage::ImageRgba8(rgba_image), path))
}

fn load_with_image(path: &Path) -> Result<LoadedImage, LoadError> {
    Ok(finish_loaded(image::open(path)?, path))
}

/// Decode the image at `path` and downscale it to fit `max_size` on its longest
/// edge, on a background thread.
///
/// # Errors
///
/// Returns [`LoadError`] if the file cannot be read, the format is unsupported
/// or fails to decode, or the decode task is cancelled before completion.
pub async fn load_thumbnail(path: PathBuf, max_size: u32) -> Result<LoadedImage, LoadError> {
    let _permit = THUMBNAIL_DECODE_LIMIT
        .acquire()
        .await
        .map_err(|_| LoadError::Cancelled)?;

    let (tx, rx) = tokio::sync::oneshot::channel();

    rayon::spawn(move || {
        let result = load_thumbnail_sync(&path, max_size);
        let _ = tx.send(result);
    });

    rx.await.map_err(|_| LoadError::Cancelled)?
}

fn load_thumbnail_sync(path: &Path, max_size: u32) -> Result<LoadedImage, LoadError> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();

    if extension == "svg" {
        return load_svg_at(path, max_size);
    }
    if matches!(extension.as_str(), "heif" | "heic") {
        let loaded = load_heif(path)?;
        if loaded.width <= max_size && loaded.height <= max_size {
            return Ok(loaded);
        }
        return resized_thumbnail(&loaded.image, max_size, path);
    }

    // 1. For JPEGs, try EXIF thumbnail extraction (no full decode)
    if matches!(extension.as_str(), "jpg" | "jpeg") {
        if let Ok(image) = extract_exif_thumbnail(path) {
            return finish_loaded_thumbnail(image, max_size, path);
        }

        // 2. For JPEGs without EXIF, use turbojpeg with DCT scaling (4-8x faster)
        if let Ok(image) = decode_jpeg_scaled(path, max_size) {
            return finish_loaded_thumbnail(image, max_size, path);
        }
    }

    // 3. Fall back to full decode + resize (non-JPEGs or if turbojpeg fails)
    let image = if is_zune_supported(&extension) {
        match decode_zune_image(path) {
            Ok(image) => image,
            Err(_) => image::open(path)?,
        }
    } else {
        image::open(path)?
    };

    finish_loaded_thumbnail(image, max_size, path)
}

/// Build a [`LoadedImage`], downscaling oversized images to `max_size`.
fn finish_loaded_thumbnail(
    image: DynamicImage,
    max_size: u32,
    path: &Path,
) -> Result<LoadedImage, LoadError> {
    if image.width() <= max_size && image.height() <= max_size {
        return Ok(finish_loaded(image, path));
    }
    resized_thumbnail(&image, max_size, path)
}

/// Build a [`LoadedImage`] from the downscaled pixels of a larger image.
fn resized_thumbnail(
    image: &DynamicImage,
    max_size: u32,
    path: &Path,
) -> Result<LoadedImage, LoadError> {
    let (width, height, pixels) = fast_resize(image, max_size)?;
    let handle = Handle::from_rgba(width, height, pixels.clone());
    let rgba_image =
        RgbaImage::from_raw(width, height, pixels).expect("pixel buffer matches dimensions");
    Ok(LoadedImage {
        handle,
        image: Arc::new(DynamicImage::ImageRgba8(rgba_image)),
        width,
        height,
        path: path.to_path_buf(),
    })
}

/// Build a [`LoadedImage`] by wrapping a fully decoded image and its display texture.
fn finish_loaded(image: DynamicImage, path: &Path) -> LoadedImage {
    let (width, height) = (image.width(), image.height());
    let handle = display_handle(&image);
    LoadedImage {
        handle,
        image: Arc::new(image),
        width,
        height,
        path: path.to_path_buf(),
    }
}

/// Extract embedded EXIF thumbnail from JPEG files.
/// Reads only a small portion of the file rather than decoding it fully.
fn extract_exif_thumbnail(path: &Path) -> Result<DynamicImage, LoadError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    let exif = exif::Reader::new()
        .read_from_container(&mut reader)
        .map_err(|e| LoadError::UnsupportedFormat(format!("No EXIF data: {e}")))?;

    // Get the thumbnail data
    let thumbnail = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)
        .zip(exif.get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL));

    if thumbnail.is_none() {
        return Err(LoadError::UnsupportedFormat("No EXIF thumbnail".into()));
    }

    // The exif crate doesn't directly expose thumbnail bytes; re-read the
    // file and extract the thumbnail using the offset/length.
    let thumb_bytes = extract_thumbnail_bytes(path, &exif)?;

    // Decode the embedded JPEG thumbnail; the caller downscales it when needed.
    image::load_from_memory_with_format(&thumb_bytes, image::ImageFormat::Jpeg)
        .map_err(LoadError::Decode)
}

/// Extract raw thumbnail bytes from JPEG using EXIF offset/length
fn extract_thumbnail_bytes(path: &Path, exif: &exif::Exif) -> Result<Vec<u8>, LoadError> {
    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)
        .and_then(|f| f.value.get_uint(0))
        .ok_or_else(|| LoadError::UnsupportedFormat("No thumbnail offset".into()))?;

    let length = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)
        .and_then(|f| f.value.get_uint(0))
        .ok_or_else(|| LoadError::UnsupportedFormat("No thumbnail length".into()))?;

    // Sanity check - thumbnails shouldn't be huge
    if length > 1_000_000 {
        return Err(LoadError::UnsupportedFormat("Thumbnail too large".into()));
    }

    let mut file = File::open(path)?;

    // EXIF data starts after APP1 marker, typically at offset 12 from file start
    // The offset in EXIF is relative to the TIFF header, which is inside APP1;
    // resolve it to the actual file offset.

    // Read the APP1 segment to find the TIFF header offset
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;

    // JPEG starts with FFD8, then APP1 marker FFE1, then 2-byte length, then "Exif\0\0"
    // TIFF header starts after "Exif\0\0"
    let tiff_offset = if &header[0..2] == b"\xFF\xD8" && &header[2..4] == b"\xFF\xE1" {
        // APP1 starts at offset 2, length is at 4-5, "Exif\0\0" is at 6-11
        // TIFF header is at offset 12
        12u64
    } else {
        // Fallback: scan for APP1 marker
        file.seek(SeekFrom::Start(0))?;
        find_tiff_header_offset(&mut file)?
    };

    // Seek to thumbnail position (TIFF header offset + thumbnail offset in EXIF)
    file.seek(SeekFrom::Start(tiff_offset + u64::from(offset)))?;

    let mut thumb_data = vec![0u8; length as usize];
    file.read_exact(&mut thumb_data)?;

    Ok(thumb_data)
}

/// Scan JPEG file to find the TIFF header offset within APP1
fn find_tiff_header_offset(file: &mut File) -> Result<u64, LoadError> {
    file.seek(SeekFrom::Start(0))?;

    let mut marker = [0u8; 2];
    file.read_exact(&mut marker)?;

    if marker != [0xFF, 0xD8] {
        return Err(LoadError::UnsupportedFormat("Not a JPEG file".into()));
    }

    loop {
        file.read_exact(&mut marker)?;

        if marker[0] != 0xFF {
            return Err(LoadError::UnsupportedFormat("Invalid JPEG marker".into()));
        }

        // Skip padding FF bytes
        while marker[1] == 0xFF {
            file.read_exact(&mut marker[1..2])?;
        }

        // Read segment length
        let mut len_bytes = [0u8; 2];
        file.read_exact(&mut len_bytes)?;
        let segment_len = u64::from(u16::from_be_bytes(len_bytes));

        if marker[1] == 0xE1 {
            // APP1 segment - check for EXIF
            let mut exif_header = [0u8; 6];
            file.read_exact(&mut exif_header)?;

            if &exif_header[0..4] == b"Exif" {
                // TIFF header starts here
                return Ok(file.stream_position()?);
            }

            // Non-EXIF APP1 - already consumed 6 bytes after len_bytes,
            // so remaining = segment_len - 2 (len) - 6 (header read) = segment_len - 8
            let current_pos = file.stream_position()?;
            file.seek(SeekFrom::Start(current_pos + segment_len - 8))?;
            continue;
        }

        // Skip to next segment
        let current_pos = file.stream_position()?;
        file.seek(SeekFrom::Start(current_pos + segment_len - 2))?;

        // End of image
        if marker[1] == 0xD9 {
            break;
        }
    }

    Err(LoadError::UnsupportedFormat(
        "No EXIF APP1 segment found".into(),
    ))
}

/// Decode JPEG with DCT scaling using turbojpeg (4-8x faster than full decode)
/// This decodes directly to a smaller resolution, skipping most IDCT computation
// JPEG header/scaled dimensions are far below u32::MAX, so the usize -> u32
// casts cannot truncate.
#[allow(clippy::cast_possible_truncation)]
fn decode_jpeg_scaled(path: &Path, max_size: u32) -> Result<DynamicImage, LoadError> {
    // Read the JPEG file
    let mut file = File::open(path)?;
    let mut jpeg_data = Vec::new();
    file.read_to_end(&mut jpeg_data)?;

    // Create decompressor
    let mut decompressor = Decompressor::new()
        .map_err(|e| LoadError::UnsupportedFormat(format!("TurboJPEG init failed: {e}")))?;

    // Read header to get original dimensions
    let header = decompressor
        .read_header(&jpeg_data)
        .map_err(|e| LoadError::UnsupportedFormat(format!("JPEG header error: {e}")))?;

    let (orig_width, orig_height) = (header.width as u32, header.height as u32);

    // Calculate the best scaling factor
    let scaling = calculate_jpeg_scale(orig_width, orig_height, max_size);

    decompressor
        .set_scaling_factor(scaling)
        .map_err(|e| LoadError::UnsupportedFormat(format!("Scale factor error: {e}")))?;

    let scaled = header.scaled(scaling);
    let width = scaled.width;
    let height = scaled.height;

    // Pre-allocate output buffer for RGBA (4 bytes per pixel)
    let mut pixels = vec![0u8; 4 * width * height];

    let mut output = Image {
        pixels: pixels.as_mut_slice(),
        width,
        pitch: 4 * width,
        height,
        format: PixelFormat::RGBA,
    };

    // Decompress with scaling directly to RGBA
    decompressor
        .decompress(&jpeg_data, output.as_deref_mut())
        .map_err(|e| LoadError::UnsupportedFormat(format!("JPEG decode error: {e}")))?;

    let rgba_image = RgbaImage::from_raw(width as u32, height as u32, pixels)
        .expect("pixel buffer matches dimensions");

    // If the scaled image is still larger than max_size, the caller resizes it.
    Ok(DynamicImage::ImageRgba8(rgba_image))
}

/// Calculate the best JPEG scaling factor to get close to target size
// max_dim is a JPEG dimension (<= u32::MAX) and the DCT ratios are 1/1..1/8, so
// the f32 round-trip is exact for realistic image sizes and the result is
// non-negative and in u32 range.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn calculate_jpeg_scale(width: u32, height: u32, target: u32) -> ScalingFactor {
    let max_dim = width.max(height);

    // Available scaling factors in turbojpeg (sorted largest to smallest)
    // These are the standard DCT scaling factors
    let ratios: [(usize, usize); 4] = [
        (1, 1), // 100%
        (1, 2), // 50%
        (1, 4), // 25%
        (1, 8), // 12.5%
    ];

    // Find the smallest scale that produces an image >= target size
    // (decode slightly larger, then resize down for quality)
    for &(num, denom) in ratios.iter().rev() {
        let scaled = (max_dim as f32 * num as f32 / denom as f32) as u32;
        if scaled >= target {
            return ScalingFactor::new(num, denom);
        }
    }

    // If even 1/8 is too large, use 1/8 and resize after
    ScalingFactor::ONE_EIGHTH
}

/// Decode an image to RGBA using zune, without resizing.
// Image dimensions originate from a decoded image header and are far below
// u32::MAX, so the usize -> u32 casts cannot truncate.
#[allow(clippy::cast_possible_truncation)]
fn decode_zune_image(path: &Path) -> Result<DynamicImage, LoadError> {
    let mut img = ZuneImage::open(path).map_err(|e| LoadError::UnsupportedFormat(e.to_string()))?;

    img.convert_color(ColorSpace::RGBA)
        .map_err(|e| LoadError::UnsupportedFormat(e.to_string()))?;

    let (width, height) = img.dimensions();

    let pixels = img
        .flatten_to_u8()
        .into_iter()
        .next()
        .ok_or_else(|| LoadError::UnsupportedFormat("No pixel data".into()))?;

    let rgba_image = RgbaImage::from_raw(width as u32, height as u32, pixels)
        .expect("pixel buffer matches dimensions");
    Ok(DynamicImage::ImageRgba8(rgba_image))
}

/// Fit dimensions within `max_size` while preserving aspect ratio.
// reason: positive dimensions and a bounded ratio keep the rounded results valid.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fit_dimensions(src_width: u32, src_height: u32, max_size: u32) -> (u32, u32) {
    if src_width <= max_size && src_height <= max_size {
        return (src_width.max(1), src_height.max(1));
    }

    let ratio = max_size as f32 / src_width.max(src_height) as f32;
    (
        ((src_width as f32 * ratio).round() as u32).max(1),
        ((src_height as f32 * ratio).round() as u32).max(1),
    )
}

/// Resize an image with SIMD acceleration, converting non-RGBA inputs first.
fn fast_resize(image: &DynamicImage, max_size: u32) -> Result<(u32, u32, Vec<u8>), LoadError> {
    let (src_width, src_height) = (image.width(), image.height());
    let (dst_width, dst_height) = fit_dimensions(src_width, src_height, max_size);

    let mut dst_image = FirImage::new(dst_width, dst_height, PixelType::U8x4);

    // Bilinear filtering balances resize speed and quality.
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(
        fast_image_resize::FilterType::Bilinear,
    ));

    let mut resizer = Resizer::new();
    if let DynamicImage::ImageRgba8(rgba) = image {
        resizer
            .resize(rgba, &mut dst_image, Some(&options))
            .map_err(|e| LoadError::UnsupportedFormat(e.to_string()))?;
    } else {
        let rgba = image.to_rgba8();
        resizer
            .resize(&rgba, &mut dst_image, Some(&options))
            .map_err(|e| LoadError::UnsupportedFormat(e.to_string()))?;
    }

    Ok((dst_width, dst_height, dst_image.into_vec()))
}

/// Scale an SVG to fill `max` in its larger dimension. SVG is vector, so
/// rendering above natural size stays crisp - no pixel upscaling.
// reason: vector sizes and the rounded fit result are small, non-negative f32s.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn svg_fit_dimensions(tree: &usvg::Tree, max: u32) -> (u32, u32) {
    let size = tree.size();
    let nat_w = size.width();
    let nat_h = size.height();
    let scale = (max as f32 / nat_w).min(max as f32 / nat_h);
    let w = (nat_w * scale).round().max(1.0) as u32;
    let h = (nat_h * scale).round().max(1.0) as u32;
    (w, h)
}

/// Rasterize an SVG tree to straight-alpha RGBA at the given pixel size.
// reason: un-premultiply arithmetic stays within u8 range by construction.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn render_svg(tree: &usvg::Tree, w: u32, h: u32) -> Result<Vec<u8>, LoadError> {
    let mut pixmap = tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| LoadError::UnsupportedFormat("SVG: failed to create pixmap".into()))?;

    let sx = w as f32 / tree.size().width();
    let sy = h as f32 / tree.size().height();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(sx, sy),
        &mut pixmap.as_mut(),
    );

    // tiny-skia pixels are premultiplied; `Handle::from_rgba` expects straight alpha.
    let mut pixels = pixmap.take();
    for chunk in pixels.chunks_exact_mut(4) {
        let a = u32::from(chunk[3]);
        if a > 0 && a < 255 {
            chunk[0] = ((u32::from(chunk[0]) * 255) / a).min(255) as u8;
            chunk[1] = ((u32::from(chunk[1]) * 255) / a).min(255) as u8;
            chunk[2] = ((u32::from(chunk[2]) * 255) / a).min(255) as u8;
        }
    }

    Ok(pixels)
}

fn load_svg_at(path: &Path, max: u32) -> Result<LoadedImage, LoadError> {
    let data = fs::read(path)?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|e| LoadError::UnsupportedFormat(format!("SVG: {e}")))?;

    let (w, h) = svg_fit_dimensions(&tree, max);
    let pixels = render_svg(&tree, w, h)?;

    let rgba_image = RgbaImage::from_raw(w, h, pixels).expect("pixel buffer matches dimensions");
    Ok(finish_loaded(DynamicImage::ImageRgba8(rgba_image), path))
}

fn load_heif(path: &Path) -> Result<LoadedImage, LoadError> {
    let name = path
        .to_str()
        .ok_or_else(|| LoadError::UnsupportedFormat("HEIF: non-UTF-8 path".into()))?;

    let lib = LibHeif::new();
    let ctx = HeifContext::read_from_file(name)
        .map_err(|e| LoadError::UnsupportedFormat(format!("HEIF: {e}")))?;

    let handle = ctx
        .primary_image_handle()
        .map_err(|e| LoadError::UnsupportedFormat(format!("HEIF: {e}")))?;

    let img = lib
        .decode(&handle, HeifColorSpace::Rgb(RgbChroma::Rgba), None)
        .map_err(|e| LoadError::UnsupportedFormat(format!("HEIF: {e}")))?;

    let plane = img
        .planes()
        .interleaved
        .ok_or_else(|| LoadError::UnsupportedFormat("HEIF: no interleaved plane".into()))?;

    // Use the DECODED image's dimensions, not the handle's: libheif applies
    // rotation/mirror (irot/imir) during decode, so a phone photo's handle reports
    // the pre-transform (ispe) size while the plane data is the post-transform size.
    // Copying with the handle's size then reads the wrong bytes (or out of bounds).
    let width = img.width();
    let height = img.height();
    if width == 0 || height == 0 {
        return Err(LoadError::UnsupportedFormat(
            "HEIF: invalid dimensions".into(),
        ));
    }

    let stride = plane.stride;
    let row_bytes = width as usize * 4;
    if stride < row_bytes || plane.data.len() < stride * height as usize {
        return Err(LoadError::UnsupportedFormat(
            "HEIF: truncated pixel data".into(),
        ));
    }
    // Copy row-by-row: the plane stride may exceed the packed row width.
    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let row_start = y * stride;
        pixels.extend_from_slice(&plane.data[row_start..row_start + row_bytes]);
    }

    let rgba_image =
        RgbaImage::from_raw(width, height, pixels).expect("pixel buffer matches dimensions");

    Ok(finish_loaded(DynamicImage::ImageRgba8(rgba_image), path))
}

/// Read DPI from EXIF data (JPEG/TIFF only)
// The DPI value is rounded and clamped into [0, u32::MAX] before the cast, so
// it cannot truncate or lose a sign.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[must_use]
pub fn read_dpi(path: &Path) -> Option<u32> {
    let mut file = BufReader::new(File::open(path).ok()?);
    let exif = exif::Reader::new().read_from_container(&mut file).ok()?;
    let x_res = exif.get_field(exif::Tag::XResolution, exif::In::PRIMARY)?;

    match x_res.value {
        // A malformed EXIF rational could be negative or absurdly large; round
        // and clamp into u32 range rather than letting `as` wrap silently.
        exif::Value::Rational(ref v) if !v.is_empty() => {
            Some(v[0].to_f64().round().clamp(0.0, f64::from(u32::MAX)) as u32)
        }
        _ => None,
    }
}
