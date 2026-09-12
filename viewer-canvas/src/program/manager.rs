// SPDX-License-Identifier: GPL-3.0-only

use super::ViewerCanvas;
use crate::{CanvasImage, CanvasMessage, state::ToolKind};
use cosmic::{
    Element, Theme,
    iced::advanced::{
        Clipboard, Layout, Renderer as CoreRenderer, Shell,
        layout::Node,
        widget::{Tree, tree},
    },
    iced::{
        Event, Length, Limits, Point, Rectangle, Renderer, Size, Vector,
        advanced::renderer as iced_renderer,
        mouse::{self, Button, Cursor, Event as MouseEvent},
        overlay,
        time::Instant,
    },
    widget::{self, Operation, Widget, canvas::Cache},
};
use image::DynamicImage;
use std::cell::Cell;
use std::sync::Arc;
use viewer_tools::{
    ToolOperation,
    crop::{CropSelection, DragHandle},
};

/// Zoom/pan smoothing time constant in seconds.
const TRANSITION_TIME_CONSTANT: f32 = 0.045;

/// Maximum time step per tick.
const TRANSITION_MAX_STEP: f32 = 0.05;

/// Relative zoom threshold for snapping to the target.
const TRANSITION_ZOOM_EPSILON: f32 = 0.001;

/// Pan threshold for snapping to the target, in logical pixels.
const TRANSITION_PAN_EPSILON: f32 = 0.1;

/// Smooth, interruptible zoom/pan transition.
#[derive(Debug, Clone, Copy)]
struct ViewTransition {
    target_zoom: f32,
    target_pan: Vector,
    viewport_size: Size,
    last_tick: Instant,
}

/// Orchestrator that owns the canvas state, edit operations, and undo/redo history.
pub struct ViewportManager {
    image: Option<CanvasImage>,
    cache: Cache,
    dirty: Cell<bool>,
    working_image: Option<Arc<DynamicImage>>,
    /// Version of the working pixels, used to skip redundant display rebuilds.
    working_version: u64,
    display_version: u64,
    zoom: f32,
    pan: Vector,
    transition: Option<ViewTransition>,
    active_tool: Option<ToolKind>,
    pub tool_dragging: bool,
    /// Committed operations (undo stack)
    operations: Vec<Box<dyn ToolOperation>>,
    redo_stack: Vec<Box<dyn ToolOperation>>,
    // Live preview for the active tool
    active_preview: Option<Box<dyn ToolOperation>>,
    last_bounds: Cell<Rectangle>,
    crop_pan: Cell<Option<(Point, Vector)>>,
}

impl Default for ViewportManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewportManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            image: None,
            cache: Cache::new(),
            dirty: Cell::new(false),
            working_image: None,
            working_version: 0,
            display_version: 0,
            zoom: 1.0,
            pan: Vector::ZERO,
            transition: None,
            active_tool: None,
            tool_dragging: false,
            operations: Vec::new(),
            redo_stack: Vec::new(),
            active_preview: None,
            last_bounds: Cell::new(Rectangle::new(Point::new(0.0, 0.0), Size::ZERO)),
            crop_pan: Cell::new(None),
        }
    }

    pub const fn last_bounds(&self) -> &Cell<Rectangle> {
        &self.last_bounds
    }

    pub fn operations(&self) -> &Vec<Box<dyn ToolOperation>> {
        &self.operations
    }

    pub fn operations_mut(&mut self) -> &mut Vec<Box<dyn ToolOperation>> {
        &mut self.operations
    }

    pub fn set_image(&mut self, image: Option<CanvasImage>, base: Option<Arc<DynamicImage>>) {
        let image = match (image, base.as_ref()) {
            (Some(mut img), Some(base)) => {
                img.width = base.width();
                img.height = base.height();
                Some(img)
            }
            (img, _) => img,
        };
        self.image = image;
        self.working_image = base;
        self.touch_working();
        self.display_version = self.working_version;
        self.zoom = 1.0;
        self.pan = Vector::ZERO;
        self.transition = None;
        self.active_preview = None;
        self.active_tool = None;
        self.cache.clear();
        self.dirty.set(true);
    }

    pub const fn image(&self) -> Option<&CanvasImage> {
        self.image.as_ref()
    }

    pub fn rebuild_image(&mut self, original: &Arc<DynamicImage>) {
        let mut working = Arc::clone(original);
        for op in &self.operations {
            op.apply(Arc::make_mut(&mut working));
        }

        let (width, height) = (working.width(), working.height());
        let handle = viewer_core::display_handle(&working);
        self.image = Some(CanvasImage {
            handle,
            width,
            height,
        });
        self.zoom = 1.0;
        self.pan = Vector::ZERO;
        self.transition = None;

        self.operations.clear();
        self.redo_stack.clear();
        self.working_image = Some(working);
        self.touch_working();
        self.display_version = self.working_version;
    }

    /// Rebuild the display texture when the working pixels change.
    pub fn rebuild_display(&mut self) {
        let Some(ref working) = self.working_image else {
            return;
        };
        if self.image.is_some() && self.display_version == self.working_version {
            return;
        }

        let (width, height) = (working.width(), working.height());
        let handle = viewer_core::display_handle(working);
        self.image = Some(CanvasImage {
            handle,
            width,
            height,
        });
        self.display_version = self.working_version;
    }

    /// Mark the working pixels as changed, invalidating the display texture.
    const fn touch_working(&mut self) {
        self.working_version = self.working_version.wrapping_add(1);
    }

    pub const fn working_image(&self) -> Option<&Arc<DynamicImage>> {
        self.working_image.as_ref()
    }

    /// Access working pixels mutably, cloning shared data when needed.
    pub fn working_image_mut(&mut self) -> Option<&mut DynamicImage> {
        self.touch_working();
        self.working_image.as_mut().map(Arc::make_mut)
    }

    /// Replace working pixels without rebuilding the display texture.
    pub fn set_working_image(&mut self, image: Arc<DynamicImage>) {
        self.working_image = Some(image);
        self.touch_working();
    }

    /// Current view zoom, including any in-flight smooth transition.
    pub const fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Whether a smooth zoom/pan transition is currently running.
    pub const fn is_animating(&self) -> bool {
        self.transition.is_some()
    }

    /// Minimum zoom so the image fills the given frame size in both dimensions.
    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn fill_zoom(&self, frame_size: Size) -> f32 {
        self.image.as_ref().map_or(1.0, |image| {
            let bounds = self.last_bounds.get();
            let fit_scale = super::fit_scale(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            );
            let zx = frame_size.width / (image.width as f32 * fit_scale);
            let zy = frame_size.height / (image.height as f32 * fit_scale);
            zx.max(zy).max(1.0)
        })
    }

    /// Set the zoom immediately, cancelling any running transition.
    pub const fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom;
        self.transition = None;
    }

    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn actual_percent(&self, viewport_size: Size) -> f32 {
        let Some(img) = self.image.as_ref() else {
            return 100.0;
        };

        let fit_scale = super::fit_scale(
            viewport_size.width,
            viewport_size.height,
            img.width as f32,
            img.height as f32,
        );

        self.zoom * fit_scale * 100.0
    }

    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn set_actual_percent(&mut self, percent: f32, viewport_size: Size) {
        let Some(img) = self.image.as_ref() else {
            return;
        };

        let fit_scale = super::fit_scale(
            viewport_size.width,
            viewport_size.height,
            img.width as f32,
            img.height as f32,
        );

        if fit_scale > 0.0 {
            self.zoom = percent / (fit_scale * 100.0);
            self.transition = None;
        }
    }

    pub fn zoom_to_actual_size(&mut self, viewport_size: Size) {
        self.set_actual_percent(100.0, viewport_size);
        self.set_pan(Vector::ZERO);
    }

    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    fn clamp_pan(&self, pan: Vector, zoom: f32, viewport_size: Size) -> Vector {
        let Some(image) = self.image.as_ref() else {
            return pan;
        };
        let fit_scale = (viewport_size.width / image.width as f32)
            .min(viewport_size.height / image.height as f32);
        let max_x =
            ((image.width as f32 * zoom).mul_add(fit_scale, -viewport_size.width)).max(0.0) / 2.0;
        let max_y =
            ((image.height as f32 * zoom).mul_add(fit_scale, -viewport_size.height)).max(0.0) / 2.0;
        Vector::new(pan.x.clamp(-max_x, max_x), pan.y.clamp(-max_y, max_y))
    }

    /// Start a smooth zoom step around a canvas-local anchor.
    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn zoom_by(&mut self, factor: f32, anchor: Point, viewport_size: Size, now: Instant) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        if viewport_size.width <= 0.0 || viewport_size.height <= 0.0 {
            return;
        }
        let Some(image) = self.image.as_ref() else {
            return;
        };

        let (from_zoom, from_pan) = self
            .transition
            .as_ref()
            .map_or((self.zoom, self.pan), |transition| {
                (transition.target_zoom, transition.target_pan)
            });

        let fit_scale = super::fit_scale(
            viewport_size.width,
            viewport_size.height,
            image.width as f32,
            image.height as f32,
        );
        let is_crop = self.active_tool == Some(ToolKind::Crop);
        let floor = if is_crop { 1.0 } else { 0.1 };
        let ceiling = if is_crop || fit_scale <= 0.0 {
            5.0
        } else {
            5.0 / fit_scale
        };

        let mut target_zoom = from_zoom * factor;

        let before_percent = from_zoom * fit_scale * 100.0;
        let after_percent = target_zoom * fit_scale * 100.0;
        if (before_percent < 99.9 && after_percent > 100.0)
            || (before_percent > 100.1 && after_percent < 100.0)
        {
            target_zoom = 1.0 / fit_scale;
        }

        target_zoom = target_zoom.clamp(floor, ceiling);

        let center = Point::new(viewport_size.width / 2.0, viewport_size.height / 2.0);
        let anchor_offset = Vector::new(anchor.x - center.x, anchor.y - center.y);
        let ratio = target_zoom / from_zoom;
        let mut target_pan = anchor_offset - (anchor_offset - from_pan) * ratio;
        if !is_crop {
            target_pan = self.clamp_pan(target_pan, target_zoom, viewport_size);
        }

        self.transition = Some(ViewTransition {
            target_zoom,
            target_pan,
            viewport_size,
            last_tick: now,
        });
    }

    /// Advance the transition to `now`.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(mut transition) = self.transition else {
            return false;
        };

        let dt = now
            .saturating_duration_since(transition.last_tick)
            .as_secs_f32()
            .min(TRANSITION_MAX_STEP);
        transition.last_tick = now;

        if dt <= 0.0 {
            self.transition = Some(transition);
            return true;
        }

        let k = 1.0 - (-dt / TRANSITION_TIME_CONSTANT).exp();
        let log_zoom = self.zoom.max(f32::MIN_POSITIVE).ln();
        let target_log_zoom = transition.target_zoom.max(f32::MIN_POSITIVE).ln();
        let mut zoom = (target_log_zoom - log_zoom).mul_add(k, log_zoom).exp();
        let mut pan = self.pan + (transition.target_pan - self.pan) * k;

        let is_crop = self.active_tool == Some(ToolKind::Crop);
        if !is_crop {
            pan = self.clamp_pan(pan, zoom, transition.viewport_size);
        }

        let settled_pan = if is_crop {
            transition.target_pan
        } else {
            self.clamp_pan(
                transition.target_pan,
                transition.target_zoom,
                transition.viewport_size,
            )
        };
        let zoom_settled = (zoom / transition.target_zoom).ln().abs() <= TRANSITION_ZOOM_EPSILON;
        let pan_delta = pan - settled_pan;
        let pan_settled = pan_delta.x.hypot(pan_delta.y) <= TRANSITION_PAN_EPSILON;

        if zoom_settled && pan_settled {
            zoom = transition.target_zoom;
            pan = settled_pan;
        }

        let old_zoom = self.zoom;
        self.zoom = zoom;
        self.pan = pan;

        if (zoom - old_zoom).abs() > f32::EPSILON
            && let (Some(image_size), Some(preview)) =
                (self.image_size(), self.active_preview.as_deref_mut())
        {
            preview.on_zoom_changed(old_zoom, zoom, image_size);
        }

        if zoom_settled && pan_settled {
            self.transition = None;
            false
        } else {
            self.transition = Some(transition);
            true
        }
    }

    pub const fn pan(&self) -> Vector {
        self.pan
    }

    /// Set the pan and update any active transition target.
    pub const fn set_pan(&mut self, pan: Vector) {
        self.pan = pan;
        if let Some(transition) = self.transition.as_mut() {
            transition.target_pan = pan;
        }
    }

    pub const fn active_tool(&self) -> Option<ToolKind> {
        self.active_tool
    }

    pub const fn set_active_tool(&mut self, tool: Option<ToolKind>) {
        self.active_tool = tool;
    }

    /// Commit an operation directly to the undo stack.
    // Clears redo stack.
    pub fn commit(&mut self, op: Box<dyn ToolOperation>) {
        self.operations.push(op);
        self.redo_stack.clear();
    }

    /// Commit the active preview via its own `commit()` method.
    /// Returns true if a commit was made.
    pub fn apply_tool(&mut self) -> bool {
        if let Some(ref preview) = self.active_preview
            && let Some(committed) = preview.commit()
        {
            self.operations.push(committed);
            self.redo_stack.clear();
            self.active_preview = None;
            self.active_tool = None;
            self.tool_dragging = false;
            return true;
        }

        false
    }

    /// Cancel the active tool. Clears the preview without committing.
    pub fn cancel_tool(&mut self) {
        self.active_preview = None;
        self.active_tool = None;
        self.tool_dragging = false;
    }

    /// Mutable access to the active preview for tool specific config.
    pub fn preview_mut(&mut self) -> Option<&mut (dyn ToolOperation + 'static)> {
        self.active_preview.as_deref_mut()
    }

    /// Undo the last committed operation.
    pub fn undo(&mut self) -> Option<&dyn ToolOperation> {
        if let Some(op) = self.operations.pop() {
            self.redo_stack.push(op);
            self.redo_stack.last().map(std::convert::AsRef::as_ref)
        } else {
            None
        }
    }

    /// Redo the last undone operation.
    pub fn redo(&mut self) -> Option<&dyn ToolOperation> {
        if let Some(op) = self.redo_stack.pop() {
            self.operations.push(op);
            self.operations.last().map(std::convert::AsRef::as_ref)
        } else {
            None
        }
    }

    /// Clear all operations and redo history.
    pub fn revert_all(&mut self) {
        self.operations.clear();
        self.redo_stack.clear();
        self.active_preview = None;
        self.working_image = None;
        self.touch_working();
    }

    /// Set the active tool's live preview; not committed to undo stack.
    pub fn set_preview(&mut self, preview: Option<Box<dyn ToolOperation>>) {
        self.active_preview = preview;
    }

    pub fn preview_ref(&self) -> Option<&(dyn ToolOperation + 'static)> {
        self.active_preview.as_deref()
    }

    /// Convert a screen space point to image coordinates.
    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn screen_to_image(&self, point: Point, bounds: Rectangle) -> Option<Point> {
        let image = self.image.as_ref()?;
        let fit_scale = if self.active_tool == Some(ToolKind::Crop) {
            super::fit_scale_uncapped(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            )
        } else {
            super::fit_scale(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            )
        };
        let effective_scale = self.zoom * fit_scale;
        let center_x = bounds.width / 2.0;
        let center_y = bounds.height / 2.0;
        let img_x = (point.x - center_x - self.pan.x) / effective_scale + image.width as f32 / 2.0;
        let img_y = (point.y - center_y - self.pan.y) / effective_scale + image.height as f32 / 2.0;

        if img_x >= 0.0
            && img_y >= 0.0
            && img_x <= image.width as f32
            && img_y <= image.height as f32
        {
            Some(Point::new(img_x, img_y))
        } else {
            None
        }
    }

    // For ToolDrag - clamp to image bounds so strokes end at the edge
    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn screen_to_image_clamped(&self, point: Point, bounds: Rectangle) -> Option<Point> {
        let image = self.image.as_ref()?;
        let fit_scale = super::fit_scale(
            bounds.width,
            bounds.height,
            image.width as f32,
            image.height as f32,
        );
        let effective_scale = self.zoom * fit_scale;
        let center_x = bounds.width / 2.0;
        let center_y = bounds.height / 2.0;
        let img_x = (point.x - center_x - self.pan.x) / effective_scale + image.width as f32 / 2.0;
        let img_y = (point.y - center_y - self.pan.y) / effective_scale + image.height as f32 / 2.0;

        Some(Point::new(
            img_x.clamp(0.0, image.width as f32),
            img_y.clamp(0.0, image.height as f32),
        ))
    }

    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn screen_to_image_fit(&self, point: Point, bounds: Rectangle) -> Option<Point> {
        let image = self.image.as_ref()?;
        let fit_scale = if self.active_tool == Some(ToolKind::Crop) {
            super::fit_scale_uncapped(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            )
        } else {
            super::fit_scale(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            )
        };

        let center_x = bounds.width / 2.0;
        let center_y = bounds.height / 2.0;
        let img_x = (point.x - center_x) / fit_scale + image.width as f32 / 2.0;
        let img_y = (point.y - center_y) / fit_scale + image.height as f32 / 2.0;

        Some(Point::new(img_x, img_y))
    }

    /// Fit-space mapping WITHOUT clamping. Used to hit-test crop handles that sit flush
    /// against the image edge, where a grab can legitimately land just outside the image;
    /// the handle's own grab radius bounds how far past the edge still counts as a hit.
    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn screen_to_image_fit_unclamped(&self, point: Point, bounds: Rectangle) -> Option<Point> {
        let image = self.image.as_ref()?;
        let fit_scale = if self.active_tool == Some(ToolKind::Crop) {
            super::fit_scale_uncapped(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            )
        } else {
            super::fit_scale(
                bounds.width,
                bounds.height,
                image.width as f32,
                image.height as f32,
            )
        };
        let center_x = bounds.width / 2.0;
        let center_y = bounds.height / 2.0;
        Some(Point::new(
            (point.x - center_x) / fit_scale + image.width as f32 / 2.0,
            (point.y - center_y) / fit_scale + image.height as f32 / 2.0,
        ))
    }

    /// Get the image dimensions as a Size.
    // reason: image dimensions are pixel counts used for rendering geometry; f32 precision is ample.
    #[allow(clippy::cast_precision_loss)]
    pub fn image_size(&self) -> Option<Size> {
        self.image
            .as_ref()
            .map(|img| Size::new(img.width as f32, img.height as f32))
    }

    pub fn can_undo(&self) -> bool {
        !self.operations.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub const fn tool_dragging(&self) -> bool {
        self.tool_dragging
    }

    /// Build the element for use in `view()`
    pub fn element(&self) -> Element<'_, CanvasMessage> {
        Element::new(Viewport { manager: self })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ZOOM_STEP;
    use cosmic::iced::time::Duration;

    fn base_image(width: u32, height: u32) -> Arc<DynamicImage> {
        Arc::new(DynamicImage::new_rgba8(width, height))
    }

    #[test]
    fn rebuild_display_reuses_texture_when_pixels_unchanged() {
        let mut manager = ViewportManager::new();
        let base = base_image(64, 64);
        let handle = viewer_core::display_handle(&base);
        manager.set_image(
            Some(CanvasImage {
                handle,
                width: 64,
                height: 64,
            }),
            Some(Arc::clone(&base)),
        );

        let before = manager.image().unwrap().handle.id();

        // Overlay-only updates call this unconditionally; the texture must be reused.
        manager.rebuild_display();
        assert_eq!(
            manager.image().unwrap().handle.id(),
            before,
            "unchanged working pixels must not rebuild the display texture"
        );

        // A pixel change must invalidate the cached texture.
        manager
            .working_image_mut()
            .unwrap()
            .as_mut_rgba8()
            .unwrap()
            .put_pixel(0, 0, image::Rgba([1, 2, 3, 4]));
        manager.rebuild_display();
        assert_ne!(
            manager.image().unwrap().handle.id(),
            before,
            "changed working pixels must rebuild the display texture"
        );
    }

    /// A manager showing an `image_w` x `image_h` image in a `viewport`-sized canvas.
    fn manager_with_image(image_w: u32, image_h: u32, viewport: Size) -> ViewportManager {
        let mut manager = ViewportManager::new();
        let base = base_image(image_w, image_h);
        let handle = viewer_core::display_handle(&base);
        manager.set_image(
            Some(CanvasImage {
                handle,
                width: image_w,
                height: image_h,
            }),
            Some(base),
        );
        manager
            .last_bounds
            .set(Rectangle::new(Point::new(0.0, 0.0), viewport));
        manager
    }

    /// Advance the clock in 60 fps steps until the transition settles.
    fn settle(manager: &mut ViewportManager, start: Instant) -> Instant {
        let mut now = start;
        for _ in 0..600 {
            if !manager.tick(now) {
                break;
            }
            now += Duration::from_millis(16);
        }
        now
    }

    #[test]
    fn zoom_step_transitions_smoothly_to_its_target() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(1000, 1000, viewport);
        let start = Instant::now();

        manager.zoom_by(ZOOM_STEP, Point::new(400.0, 400.0), viewport, start);
        assert!(manager.is_animating());

        let mut now = start;
        let mut previous = manager.zoom();
        for _ in 0..600 {
            if !manager.tick(now) {
                break;
            }
            assert!(
                manager.zoom() >= previous,
                "zoom must approach its target monotonically"
            );
            previous = manager.zoom();
            now += Duration::from_millis(16);
        }

        assert!(!manager.is_animating());
        assert!((manager.zoom() - ZOOM_STEP).abs() < 1e-4);
    }

    #[test]
    fn rapid_zoom_steps_compound_onto_the_pending_target() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(1000, 1000, viewport);
        let start = Instant::now();
        let anchor = Point::new(400.0, 400.0);

        manager.zoom_by(ZOOM_STEP, anchor, viewport, start);
        manager.zoom_by(
            ZOOM_STEP,
            anchor,
            viewport,
            start + Duration::from_millis(4),
        );
        let _ = settle(&mut manager, start);

        let expected = ZOOM_STEP * ZOOM_STEP;
        assert!((manager.zoom() - expected).abs() < 1e-4);
    }

    #[test]
    fn zoom_does_not_stick_at_actual_size_while_animating() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(1000, 1000, viewport);
        manager.set_zoom(1.125); // 90% of actual size
        let start = Instant::now();
        let anchor = Point::new(400.0, 400.0);


        manager.zoom_by(ZOOM_STEP, anchor, viewport, start);
        manager.zoom_by(
            ZOOM_STEP,
            anchor,
            viewport,
            start + Duration::from_millis(4),
        );
        let _ = settle(&mut manager, start);

        let expected = (1.0 / 0.8) * ZOOM_STEP; // actual size * one step
        assert!((manager.zoom() - expected).abs() < 1e-4);
    }

    #[test]
    fn transitions_are_frame_rate_independent() {
        for step_ms in [8_u64, 33] {
            let viewport = Size::new(800.0, 800.0);
            let mut manager = manager_with_image(1000, 1000, viewport);
            let start = Instant::now();
            manager.zoom_by(ZOOM_STEP, Point::new(400.0, 400.0), viewport, start);

            let mut now = start;
            for _ in 0..600 {
                if !manager.tick(now) {
                    break;
                }
                now += Duration::from_millis(step_ms);
            }

            assert!(!manager.is_animating());
            assert!((manager.zoom() - ZOOM_STEP).abs() < 1e-4);
        }
    }

    #[test]
    fn zoom_snaps_through_actual_size() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(1000, 1000, viewport);
        manager.set_zoom(1.2); // 96% of actual size
        let start = Instant::now();

        manager.zoom_by(ZOOM_STEP, Point::new(400.0, 400.0), viewport, start);
        let _ = settle(&mut manager, start);

        assert!((manager.actual_percent(viewport) - 100.0).abs() < 0.01);
    }

    #[test]
    fn zoom_limits_are_enforced() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(4000, 4000, viewport);
        let mut now = Instant::now();
        let center = Point::new(400.0, 400.0);

        for _ in 0..40 {
            manager.zoom_by(2.0, center, viewport, now);
            now = settle(&mut manager, now);
        }
        let percent = manager.actual_percent(viewport);
        assert!(
            (percent - 500.0).abs() < 0.1,
            "zoom in must stop at 500% actual size, got {percent}"
        );

        for _ in 0..40 {
            manager.zoom_by(0.5, center, viewport, now);
            now = settle(&mut manager, now);
        }
        assert!((manager.zoom() - 0.1).abs() < 1e-4);
    }

    #[test]
    fn wheel_zoom_keeps_the_anchored_point_fixed() {
        let viewport = Size::new(800.0, 800.0);
        let bounds = Rectangle::new(Point::new(0.0, 0.0), viewport);
        let mut manager = manager_with_image(2000, 1500, viewport);
        let anchor = Point::new(650.0, 240.0);

        let before = manager
            .screen_to_image(anchor, bounds)
            .expect("anchor starts over the image");

        let start = Instant::now();
        manager.zoom_by(2.0, anchor, viewport, start);
        let _ = settle(&mut manager, start);

        let after = manager
            .screen_to_image(anchor, bounds)
            .expect("anchor stays over the image");

        assert!(
            (after.x - before.x).abs() < 0.05 && (after.y - before.y).abs() < 0.05,
            "anchored image point moved from {before:?} to {after:?}"
        );
    }

    #[test]
    fn panning_retargets_a_running_transition() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(2000, 1500, viewport);
        let start = Instant::now();

        manager.zoom_by(2.0, Point::new(400.0, 400.0), viewport, start);
        manager.set_pan(Vector::new(120.0, 60.0));
        let _ = settle(&mut manager, start);

        assert_eq!(manager.pan(), Vector::new(120.0, 60.0));
    }

    #[test]
    fn direct_zoom_resets_cancel_the_transition() {
        let viewport = Size::new(800.0, 800.0);
        let mut manager = manager_with_image(1000, 1000, viewport);
        let start = Instant::now();

        manager.zoom_by(ZOOM_STEP, Point::new(400.0, 400.0), viewport, start);
        assert!(manager.is_animating());

        manager.set_zoom(2.0);
        assert!(!manager.is_animating());
        assert!((manager.zoom() - 2.0).abs() < f32::EPSILON);
        assert!(!manager.tick(start + Duration::from_millis(16)));
    }
}

/// Private widget returned by `ViewportManager::element()`
/// Builds the canvas internally and wraps it with GPU clipping.
struct Viewport<'a> {
    manager: &'a ViewportManager,
}

impl Viewport<'_> {
    /// Image only canvas (no tool overlays).
    fn canvas_element(&self) -> Element<'_, CanvasMessage> {
        let mgr = self.manager;
        let canvas = ViewerCanvas {
            image: mgr.image.as_ref(),
            cache: &mgr.cache,
            zoom: mgr.zoom,
            pan: mgr.pan,
            active_tool: mgr.active_tool,
            operations: &[],
            preview: None,
            overlay_only: false,
        };

        widget::canvas(canvas)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// Overlay only canvas (tool overlays, no image).
    /// When crop is active, the crop preview is excluded here and drawn separately.
    fn overlay_element(&self) -> Element<'_, CanvasMessage> {
        let mgr = self.manager;
        let is_crop = mgr.active_tool == Some(ToolKind::Crop);
        let canvas = ViewerCanvas {
            image: mgr.image.as_ref(),
            cache: &mgr.cache,
            zoom: mgr.zoom,
            pan: mgr.pan,
            active_tool: mgr.active_tool,
            operations: &mgr.operations,
            preview: if is_crop {
                None
            } else {
                mgr.active_preview.as_deref()
            },
            overlay_only: true,
        };

        widget::canvas(canvas)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn crop_overlay_element(&self) -> Element<'_, CanvasMessage> {
        let mgr = self.manager;
        let canvas = ViewerCanvas {
            image: mgr.image.as_ref(),
            cache: &mgr.cache,
            zoom: 1.0,
            pan: Vector::ZERO,
            active_tool: mgr.active_tool,
            operations: &[],
            preview: mgr.active_preview.as_deref(),
            overlay_only: true,
        };

        widget::canvas(canvas)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// Crop interaction: pan on interior clicks, fit-to-view coords for handles.
    /// Returns true when the event was captured and base-canvas handling should be skipped.
    fn handle_crop_event(
        &self,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
        shell: &mut Shell<'_, CanvasMessage>,
    ) -> bool {
        let mgr = self.manager;
        if matches!(
            event,
            Event::Mouse(
                MouseEvent::CursorEntered
                    | MouseEvent::CursorLeft
                    | MouseEvent::ButtonReleased(Button::Left)
            )
        ) && mgr.tool_dragging
        {
            mgr.crop_pan.set(None);
            shell.publish(CanvasMessage::ToolEnd);
            shell.capture_event();
            return true;
        }

        let Some(position) = cursor.position_in(bounds).or_else(|| {
            if mgr.tool_dragging {
                let mut pos = cursor.position().unwrap_or_default();
                pos.x = pos.x - bounds.x;
                pos.y = pos.y - bounds.y;
                return Some(pos);
            }
            None
        }) else {
            return false;
        };

        let Event::Mouse(mouse_event) = event else {
            return false;
        };

        // Release crop pan even if cursor left the canvas
        if mgr.crop_pan.get().is_some()
            && matches!(
                event,
                Event::Mouse(MouseEvent::ButtonReleased(Button::Left))
            )
        {
            mgr.crop_pan.set(None);
            shell.capture_event();
            return true;
        }

        if let Some((start, origin)) = mgr.crop_pan.get()
            && let MouseEvent::CursorMoved { .. } = mouse_event
        {
            let delta = Vector::new(position.x - start.x, position.y - start.y);
            shell.publish(CanvasMessage::Pan(origin + delta));
            shell.capture_event();
            return true;
        }

        match mouse_event {
            MouseEvent::CursorEntered => {}

            MouseEvent::ButtonPressed(Button::Left) => {
                let crop = mgr
                    .active_preview
                    .as_deref()
                    .and_then(|preview| preview.as_any().downcast_ref::<CropSelection>());

                // Grab a handle first, hit-tested in unclamped fit space so a handle laid
                // flush against the image edge stays grabbable when the press lands just
                // outside the image (the strict over-image gate below would reject it).
                if let Some(crop) = crop
                    && let Some(hit_pt) = mgr.screen_to_image_fit_unclamped(position, bounds)
                    && crop.hit_test(hit_pt) != DragHandle::None
                    && let Some(pt) = mgr.screen_to_image_fit(position, bounds)
                {
                    shell.publish(CanvasMessage::ToolStart(pt));
                    shell.capture_event();
                    return true;
                }

                if mgr.screen_to_image(position, bounds).is_some()
                    && let Some(pt) = mgr.screen_to_image_fit(position, bounds)
                {
                    // Interior press (not on a handle) pans the image behind the frame.
                    if let Some(crop) = crop
                        && crop.hit_test(pt) == DragHandle::None
                        && crop.region().contains(pt)
                    {
                        mgr.crop_pan.set(Some((position, mgr.pan)));
                        shell.capture_event();
                        return true;
                    }

                    shell.publish(CanvasMessage::ToolStart(pt));
                    shell.capture_event();
                    return true;
                }

                // Press outside the image, the dead area, dismisses the popover
                shell.publish(CanvasMessage::ContextMenu(None));
                shell.capture_event();
                return true;
            }
            MouseEvent::CursorMoved { .. } => {
                if mgr.tool_dragging
                    && let Some(pt) = mgr.screen_to_image_fit(position, bounds)
                {
                    shell.publish(CanvasMessage::ToolDrag(pt));
                    shell.capture_event();
                    return true;
                }
            }
            MouseEvent::ButtonReleased(Button::Left) if mgr.tool_dragging => {
                shell.publish(CanvasMessage::ToolEnd);
                shell.capture_event();
                return true;
            }
            _ => {}
        }

        false
    }

    /// Non-crop tool interaction (clamped image-space coords).
    /// Returns true when the event was captured and base-canvas handling should be skipped.
    fn handle_tool_event(
        &self,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
        shell: &mut Shell<'_, CanvasMessage>,
    ) -> bool {
        let mgr = self.manager;

        if matches!(
            event,
            Event::Mouse(MouseEvent::CursorLeft | MouseEvent::ButtonReleased(Button::Left))
        ) && mgr.tool_dragging
        {
            shell.publish(CanvasMessage::ToolEnd);
            shell.capture_event();
            return true;
        }
        let (Event::Mouse(mouse_event), Some(position)) = (event, cursor.position_in(bounds))
        else {
            return false;
        };

        match mouse_event {
            MouseEvent::ButtonPressed(Button::Left) => {
                if let Some(pt) = mgr.screen_to_image(position, bounds) {
                    shell.publish(CanvasMessage::ToolStart(pt));
                    shell.capture_event();
                    return true;
                }
            }
            MouseEvent::CursorMoved { .. } => {
                if mgr.tool_dragging
                    && let Some(pt) = mgr.screen_to_image_clamped(position, bounds)
                {
                    shell.publish(CanvasMessage::ToolDrag(pt));
                    shell.capture_event();
                    return true;
                }
            }
            MouseEvent::ButtonReleased(Button::Left) if mgr.tool_dragging => {
                shell.publish(CanvasMessage::ToolEnd);
                shell.capture_event();
                return true;
            }
            _ => {}
        }

        false
    }
}

impl Widget<CanvasMessage, Theme, Renderer> for Viewport<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &Limits) -> Node {
        let mut element = self.canvas_element();
        let child = element
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let size = child.size();
        Node::with_children(size, vec![child])
    }

    fn tag(&self) -> tree::Tag {
        self.canvas_element().as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.canvas_element().as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.canvas_element())]
    }

    fn diff(&mut self, tree: &mut Tree) {
        if self.manager.dirty.get() {
            self.manager.dirty.set(false);

            if let Some(child) = tree.children.first_mut() {
                *child = Tree::new(self.canvas_element());
            }
        } else {
            let element = self.canvas_element();
            tree.diff_children(&mut [element]);
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &iced_renderer::Style,
        layout: Layout<'_>,
        cursor: Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        self.manager.last_bounds.set(bounds);

        // Layer 1: Image
        renderer.with_layer(bounds, |renderer| {
            let element = self.canvas_element();
            element.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout
                    .children()
                    .next()
                    .expect("layout() always builds one child node"),
                cursor,
                viewport,
            );
        });

        // Layer 2: Tool overlays (operations + non-crop preview)
        let is_crop = self.manager.active_tool == Some(ToolKind::Crop);
        if !self.manager.operations.is_empty()
            || (self.manager.active_preview.is_some() && !is_crop)
        {
            renderer.with_layer(bounds, |renderer| {
                let overlay = self.overlay_element();
                overlay.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    layout
                        .children()
                        .next()
                        .expect("layout() always builds one child node"),
                    cursor,
                    viewport,
                );
            });
        }

        // Layer 3: Crop preview in screen space
        if is_crop && self.manager.active_preview.is_some() {
            renderer.with_layer(bounds, |renderer| {
                let crop_overlay = self.crop_overlay_element();
                crop_overlay.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    layout
                        .children()
                        .next()
                        .expect("layout() always builds one child node"),
                    cursor,
                    viewport,
                );
            });
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, CanvasMessage>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        self.manager.last_bounds.set(bounds);

        let is_crop = self.manager.active_tool == Some(ToolKind::Crop);

        if is_crop {
            if self.handle_crop_event(event, bounds, cursor, shell) {
                return;
            }
        } else if self.manager.active_tool.is_some()
            && self.handle_tool_event(event, bounds, cursor, shell)
        {
            return;
        }

        // Fall through to base canvas for zoom, context menu, etc.
        let mut element = self.canvas_element();
        element.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout
                .children()
                .next()
                .expect("layout() always builds one child node"),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();

        if self.manager.active_tool.is_some() {
            // An in-progress pan behind the crop frame is the closed-hand cursor.
            if self.manager.crop_pan.get().is_some() {
                return mouse::Interaction::Grabbing;
            }
            if let Some(position) = cursor.position_in(bounds) {
                // The crop frame lives in fit space; hit-test the cursor there so the
                // grab/handle zones line up when the image is zoomed or panned.
                let img_point = if self.manager.active_tool == Some(ToolKind::Crop) {
                    self.manager.screen_to_image_fit(position, bounds)
                } else {
                    self.manager.screen_to_image(position, bounds)
                };
                if let Some(img_point) = img_point {
                    if let Some(preview) = self.manager.active_preview.as_deref() {
                        return preview.cursor_at(img_point);
                    }
                    return mouse::Interaction::Crosshair;
                }
            }

            // Cursor is in the viewport but not over the image
            return mouse::Interaction::default();
        }

        // Delegate to base canvas for non-tool cursors
        let element = self.canvas_element();
        element.as_widget().mouse_interaction(
            &tree.children[0],
            layout
                .children()
                .next()
                .expect("layout() always builds one child node"),
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let mut element = self.canvas_element();
        element.as_widget_mut().operate(
            &mut tree.children[0],
            layout
                .children()
                .next()
                .expect("layout() always builds one child node"),
            renderer,
            operation,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        _tree: &'b mut Tree,
        _layout: Layout<'_>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'b, CanvasMessage, Theme, Renderer>> {
        None
    }
}

impl<'a> From<Viewport<'a>> for Element<'a, CanvasMessage> {
    fn from(widget: Viewport<'a>) -> Self {
        Element::new(widget)
    }
}
