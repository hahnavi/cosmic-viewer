// SPDX-License-Identifier: GPL-3.0-only

//! Lazy playback of animated GIFs.
//!
//! Frames are decoded and composited one at a time from the source file.

use crate::loader::LoadError;
use gif::{ColorOutput, DecodeOptions, Decoder, DisposalMethod, Frame};
use image::{Rgba, RgbaImage};
use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    time::Duration,
};

/// An animated GIF on disk and the timing of its first frame.
#[derive(Clone, Debug)]
pub struct Animation {
    path: PathBuf,
    first_delay: Duration,
}

impl Animation {
    /// The GIF being played.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How long the first frame stays on screen.
    #[must_use]
    pub const fn first_delay(&self) -> Duration {
        self.first_delay
    }

    /// Create a player positioned before the first frame.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] if the file cannot be opened or its header parsed.
    pub fn player(&self) -> Result<GifPlayer, LoadError> {
        GifPlayer::open(&self.path)
    }
}

/// Build an [`Animation`] from its raw parts; used by the loader.
pub(crate) const fn animation_from_parts(path: PathBuf, first_delay: Duration) -> Animation {
    Animation { path, first_delay }
}

/// A decoded GIF frame and how long it should be displayed.
pub struct DecodedFrame {
    pub image: RgbaImage,
    pub delay: Duration,
}

/// Stateful GIF decoder that composites frames into a persistent canvas.
pub struct GifPlayer {
    path: PathBuf,
    decoder: Decoder<BufReader<File>>,
    /// Composited canvas for the current pass.
    canvas: RgbaImage,
    width: u32,
    height: u32,
    finished: bool,
}

impl GifPlayer {
    /// Open a GIF from disk.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] if the file cannot be opened or its header parsed.
    pub fn open(path: &Path) -> Result<Self, LoadError> {
        let decoder = make_decoder(path)?;
        let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
        let canvas = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 0]));
        Ok(Self {
            path: path.to_path_buf(),
            decoder,
            canvas,
            width,
            height,
            finished: false,
        })
    }

    /// Logical dimensions of the GIF.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Decode and composite the next frame of the current pass.
    ///
    /// Returns `Ok(None)` at the end of the animation; call
    /// [`Self::next_frame_looping`] for continuous playback.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] if a frame cannot be decoded.
    pub fn next_frame(&mut self) -> Result<Option<DecodedFrame>, LoadError> {
        if self.finished {
            return Ok(None);
        }

        let Some(frame) = self
            .decoder
            .read_next_frame()
            .map_err(|e| LoadError::UnsupportedFormat(format!("GIF: {e}")))?
        else {
            self.finished = true;
            return Ok(None);
        };

        let dispose = frame.dispose;
        let delay = gif_frame_delay(frame.delay);
        let left = u32::from(frame.left);
        let top = u32::from(frame.top);
        let frame_w = u32::from(frame.width);
        let frame_h = u32::from(frame.height);

        // Save the region needed by `Previous` disposal.
        let restore = (dispose == DisposalMethod::Previous)
            .then(|| copy_region(&self.canvas, left, top, frame_w, frame_h));

        composite_region(&mut self.canvas, frame, left, top, frame_w, frame_h);

        // Display the frame before preparing the canvas for the next one.
        let output = self.canvas.clone();

        match dispose {
            DisposalMethod::Any | DisposalMethod::Keep => {}
            DisposalMethod::Background => {
                clear_region(&mut self.canvas, left, top, frame_w, frame_h);
            }
            DisposalMethod::Previous => {
                if let Some(region) = restore {
                    restore_region(&mut self.canvas, left, top, &region);
                }
            }
        }

        Ok(Some(DecodedFrame {
            image: output,
            delay,
        }))
    }

    /// Like [`Self::next_frame`], but restarts from the first frame when the
    /// animation ends. Returns `Ok(None)` only for a GIF with no frames.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] if a frame cannot be decoded.
    pub fn next_frame_looping(&mut self) -> Result<Option<DecodedFrame>, LoadError> {
        if let Some(frame) = self.next_frame()? {
            return Ok(Some(frame));
        }
        self.rewind()?;
        self.next_frame()
    }

    /// Restart the animation from its first frame.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] if the file can no longer be opened.
    pub fn rewind(&mut self) -> Result<(), LoadError> {
        self.decoder = make_decoder(&self.path)?;
        clear_all(&mut self.canvas);
        self.finished = false;
        Ok(())
    }
}

/// Read buffer used while streaming frames from disk.
const READ_BUFFER_BYTES: usize = 64 * 1024;

fn make_decoder(path: &Path) -> Result<Decoder<BufReader<File>>, LoadError> {
    let file = File::open(path)?;
    let mut options = DecodeOptions::new();
    options.set_color_output(ColorOutput::RGBA);
    options
        .read_info(BufReader::with_capacity(READ_BUFFER_BYTES, file))
        .map_err(|e| LoadError::UnsupportedFormat(format!("GIF: {e}")))
}

/// Return a watchable delay for a GIF frame.
fn gif_frame_delay(delay: u16) -> Duration {
    let millis = u64::from(delay) * 10;
    if millis < 20 {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(millis)
    }
}

/// Draw a frame over the composited canvas.
fn composite_region(
    canvas: &mut RgbaImage,
    frame: &Frame<'_>,
    left: u32,
    top: u32,
    w: u32,
    h: u32,
) {
    let (canvas_w, canvas_h) = canvas.dimensions();
    for y in 0..h {
        let cy = top + y;
        if cy >= canvas_h {
            break;
        }
        for x in 0..w {
            let cx = left + x;
            if cx >= canvas_w {
                break;
            }
            let idx = (y as usize * w as usize + x as usize) * 4;
            let Some(pixel) = frame.buffer.get(idx..idx + 4) else {
                break;
            };
            if pixel[3] == 0 {
                continue;
            }
            canvas.put_pixel(cx, cy, Rgba([pixel[0], pixel[1], pixel[2], pixel[3]]));
        }
    }
}

/// Copy a canvas region, using transparent pixels outside the canvas.
fn copy_region(canvas: &RgbaImage, left: u32, top: u32, w: u32, h: u32) -> RgbaImage {
    let mut region = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    for y in 0..h {
        for x in 0..w {
            if let Some(pixel) = canvas.get_pixel_checked(left + x, top + y) {
                region.put_pixel(x, y, *pixel);
            }
        }
    }
    region
}

fn restore_region(canvas: &mut RgbaImage, left: u32, top: u32, region: &RgbaImage) {
    let (w, h) = region.dimensions();
    let (canvas_w, canvas_h) = canvas.dimensions();
    for y in 0..h {
        let cy = top + y;
        if cy >= canvas_h {
            break;
        }
        for x in 0..w {
            let cx = left + x;
            if cx >= canvas_w {
                break;
            }
            canvas.put_pixel(cx, cy, *region.get_pixel(x, y));
        }
    }
}

fn clear_region(canvas: &mut RgbaImage, left: u32, top: u32, w: u32, h: u32) {
    let (canvas_w, canvas_h) = canvas.dimensions();
    for y in 0..h {
        let cy = top + y;
        if cy >= canvas_h {
            break;
        }
        for x in 0..w {
            let cx = left + x;
            if cx >= canvas_w {
                break;
            }
            canvas.put_pixel(cx, cy, Rgba([0, 0, 0, 0]));
        }
    }
}

fn clear_all(canvas: &mut RgbaImage) {
    for pixel in canvas.pixels_mut() {
        *pixel = Rgba([0, 0, 0, 0]);
    }
}
