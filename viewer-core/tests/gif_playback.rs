// SPDX-License-Identifier: GPL-3.0-only

//! Compositing tests for the lazy GIF player: disposal methods must produce
//! the same full-canvas frames as a straightforward GIF decoder.

use gif::{DisposalMethod, Encoder, Frame as GifFrame, Repeat};
use image::Rgba;
use std::{
    fs::File,
    path::{Path, PathBuf},
};
use viewer_core::load_image;

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];

fn solid_frame(w: u16, h: u16, color: [u8; 4], dispose: DisposalMethod) -> GifFrame<'static> {
    let mut pixels = vec![0u8; usize::from(w) * usize::from(h) * 4];
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&color);
    }
    let mut frame = GifFrame::from_rgba(w, h, &mut pixels);
    frame.dispose = dispose;
    frame.delay = 10;
    frame
}

fn rect_frame(fw: u16, fh: u16, left: u16, top: u16, color: [u8; 4]) -> GifFrame<'static> {
    let mut pixels = vec![0u8; usize::from(fw) * usize::from(fh) * 4];
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&color);
    }
    let mut frame = GifFrame::from_rgba(fw, fh, &mut pixels);
    frame.left = left;
    frame.top = top;
    frame.dispose = DisposalMethod::Keep;
    frame.delay = 10;
    frame
}

fn write_gif(path: &Path, w: u16, h: u16, frames: &[GifFrame<'_>]) {
    let file = File::create(path).expect("create GIF");
    let mut encoder = Encoder::new(file, w, h, &[]).expect("encoder");
    encoder.set_repeat(Repeat::Infinite).expect("repeat");
    for frame in frames {
        encoder.write_frame(frame).expect("write frame");
    }
}

async fn player_for(
    name: &str,
    frames: &[GifFrame<'_>],
    w: u16,
    h: u16,
) -> (viewer_core::GifPlayer, PathBuf) {
    let path = std::env::temp_dir().join(name);
    write_gif(&path, w, h, frames);
    let loaded = load_image(path.clone()).await.expect("load GIF");
    let player = loaded
        .animation
        .expect("animated GIF")
        .player()
        .expect("player");
    (player, path)
}

#[tokio::test]
async fn background_disposal_clears_the_canvas_for_the_next_frame() {
    let frames = [
        solid_frame(4, 4, RED, DisposalMethod::Keep),
        solid_frame(4, 4, BLUE, DisposalMethod::Background),
        rect_frame(2, 2, 1, 1, GREEN),
    ];
    let (mut player, path) = player_for("cosmic-viewer-test-disposal-bg.gif", &frames, 4, 4).await;

    assert_eq!(
        *player
            .next_frame()
            .expect("f0")
            .expect("frame")
            .image
            .get_pixel(0, 0),
        Rgba(RED)
    );
    assert_eq!(
        *player
            .next_frame()
            .expect("f1")
            .expect("frame")
            .image
            .get_pixel(3, 3),
        Rgba(BLUE)
    );

    let frame = player.next_frame().expect("f2").expect("frame").image;
    assert_eq!(*frame.get_pixel(1, 1), Rgba(GREEN));
    assert_eq!(
        frame.get_pixel(0, 0).0[3],
        0,
        "background disposal must leave the previous frame's area transparent"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn previous_disposal_restores_the_prior_canvas() {
    let frames = [
        solid_frame(4, 4, RED, DisposalMethod::Keep),
        solid_frame(4, 4, BLUE, DisposalMethod::Previous),
        rect_frame(2, 2, 1, 1, GREEN),
    ];
    let (mut player, path) =
        player_for("cosmic-viewer-test-disposal-prev.gif", &frames, 4, 4).await;

    let _ = player.next_frame().expect("f0").expect("frame");
    let _ = player.next_frame().expect("f1").expect("frame");

    let frame = player.next_frame().expect("f2").expect("frame").image;
    assert_eq!(*frame.get_pixel(1, 1), Rgba(GREEN));
    assert_eq!(
        *frame.get_pixel(0, 0),
        Rgba(RED),
        "previous disposal must restore the frame before the blue one"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn playback_loops_back_to_the_first_frame() {
    let frames = [
        solid_frame(2, 2, RED, DisposalMethod::Keep),
        solid_frame(2, 2, BLUE, DisposalMethod::Keep),
    ];
    let (mut player, path) = player_for("cosmic-viewer-test-loop.gif", &frames, 2, 2).await;

    assert!(player.next_frame().expect("f0").is_some());
    assert!(player.next_frame().expect("f1").is_some());
    let frame = player
        .next_frame_looping()
        .expect("f0 again")
        .expect("loops");
    assert_eq!(*frame.image.get_pixel(0, 0), Rgba(RED));
    let _ = std::fs::remove_file(&path);
}
