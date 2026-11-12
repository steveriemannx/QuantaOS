//! Pure-Rust reference executor for QuantaGPU v1 draw commands.
//!
//! The protocol crate defines *what* a frame means; this crate is a small,
//! dependency-free implementation of that meaning. It is used by
//! `quanta-gpud` as the fallback executor when no virgl backend is present,
//! and by host tests as the semantic reference for the v1 operations.
//!
//! The rasterization rules match `ferritic-soft`, so a scene rendered by the
//! client and the same scene submitted as protocol primitives produce the same
//! pixels:
//!
//! * rounded rectangles use the same signed distance field and 1px analytic AA;
//! * borders are the ring between the outer shape and `d + border_width`;
//! * shadows use the shader's smoothstep falloff over `blur`;
//! * glyphs sample the A8 atlas with bilinear filtering and are snapped to the
//!   physical pixel grid.
//!
//! Surfaces are `argb8888` (little-endian bytes `B, G, R, A`). All colors in
//! the protocol are straight-alpha sRGB.
//!
//! ```no_run
//! use quanta_gpu_protocol::{FrameEncoder, FrameInfo, PixelRect, Rgba8};
//! use quanta_gpu_raster::{Executor, Rasterizer, Surface};
//!
//! let mut pixels = vec![0u8; 64 * 64 * 4];
//! let mut rasterizer = Rasterizer::new();
//! let mut surface = Surface::new(&mut pixels, 64, 64, 64 * 4).unwrap();
//! let mut executor = Executor::new(surface, &mut rasterizer, 1.0);
//! // `decode_frame` replays the frame into the executor; one is encoded here
//! // for clarity.
//! let mut encoder = FrameEncoder::new(FrameInfo {
//!     seq: 1,
//!     width: 64,
//!     height: 64,
//!     scale: 1.0,
//! })
//! .unwrap();
//! encoder.clear(Rgba8::BLACK).unwrap().present(&[]).unwrap();
//! let frame = encoder.finish().unwrap();
//! quanta_gpu_protocol::decode_frame(&frame, &mut executor).unwrap();
//! let executed = executor.finish();
//! assert_eq!(executed.damage, [PixelRect::new(0, 0, 64, 64)]);
//! ```

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use core::fmt;
use core::mem;

use quanta_gpu_protocol::{
    AtlasCmd, FloatRect, FrameSink, GlyphCmd, PixelRect, RectCmd, Rgba8, ShadowCmd,
};

/// Maximum number of damage rectangles one frame may report. Mirrors the
/// protocol limit so the driver can use the result without trimming it.
pub const MAX_DAMAGE: usize = 256;

/// A validated, stride-aware view of an `argb8888` pixel buffer.
#[derive(Debug)]
pub struct Surface<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    stride: usize,
}

impl<'a> Surface<'a> {
    /// Wraps a pixel buffer.
    ///
    /// `stride` is in bytes and must be at least `width * 4`; extra row padding
    /// is preserved.
    pub fn new(
        data: &'a mut [u8],
        width: u32,
        height: u32,
        stride: usize,
    ) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::Empty);
        }
        let row = (width as usize)
            .checked_mul(4)
            .ok_or(SurfaceError::InvalidStride)?;
        if stride < row || !stride.is_multiple_of(4) {
            return Err(SurfaceError::InvalidStride);
        }
        let needed = stride
            .checked_mul(height as usize)
            .ok_or(SurfaceError::InvalidStride)?;
        if data.len() < needed {
            return Err(SurfaceError::BufferTooSmall);
        }
        Ok(Self {
            data,
            width,
            height,
            stride,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Fills the visible pixels, leaving stride padding untouched.
    pub fn clear(&mut self, color: Rgba8) {
        let pixel = [color.b, color.g, color.r, color.a];
        let row_bytes = self.width as usize * 4;
        for row in self.data.chunks_mut(self.stride).take(self.height as usize) {
            for chunk in row[..row_bytes].chunks_exact_mut(4) {
                chunk.copy_from_slice(&pixel);
            }
        }
    }

    fn pixel_range(&self, bounds: Bounds) -> Option<(u32, u32, u32, u32)> {
        let x0 = bounds.x0.max(0.0).floor() as i64;
        let y0 = bounds.y0.max(0.0).floor() as i64;
        let x1 = bounds.x1.min(self.width as f32).ceil().max(0.0) as i64;
        let y1 = bounds.y1.min(self.height as f32).ceil().max(0.0) as i64;
        if x0 >= x1 || y0 >= y1 {
            None
        } else {
            Some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
        }
    }

    /// Blends one straight-alpha source color over the destination.
    fn blend(&mut self, x: u32, y: u32, rgb: [f32; 3], alpha: f32) {
        let index = y as usize * self.stride + x as usize * 4;
        let Some(destination) = self.data.get_mut(index..index + 4) else {
            return;
        };
        let source_alpha = alpha.clamp(0.0, 1.0);
        if source_alpha <= 0.0 {
            return;
        }
        let inverse = 1.0 - source_alpha;
        let destination_alpha = destination[3] as f32 / 255.0;
        // Memory order is B, G, R, A.
        for (channel, source) in [(2usize, rgb[0]), (1, rgb[1]), (0, rgb[2])] {
            let destination_channel = destination[channel] as f32 / 255.0;
            let out = source * source_alpha + destination_channel * inverse;
            destination[channel] = (out * 255.0).clamp(0.0, 255.0).round() as u8;
        }
        let out_alpha = source_alpha + destination_alpha * inverse;
        destination[3] = (out_alpha * 255.0).clamp(0.0, 255.0).round() as u8;
    }
}

/// Errors returned when a buffer cannot be used as a surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    Empty,
    InvalidStride,
    BufferTooSmall,
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceError::Empty => write!(f, "surface has zero width or height"),
            SurfaceError::InvalidStride => write!(f, "surface stride is invalid"),
            SurfaceError::BufferTooSmall => write!(f, "surface buffer is too small"),
        }
    }
}

impl std::error::Error for SurfaceError {}

/// Persistent executor state (the A8 glyph atlas and pending damage).
///
/// One rasterizer is kept alive per client/connection by the driver so atlas
/// uploads survive across frames.
#[derive(Default)]
pub struct Rasterizer {
    atlas: Vec<u8>,
    atlas_width: u32,
    atlas_height: u32,
    damage: Damage,
}

impl Rasterizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new frame: pending damage from the previous one is discarded.
    pub fn begin_frame(&mut self) {
        self.damage.reset();
    }

    pub fn atlas_size(&self) -> Option<(u32, u32)> {
        (!self.atlas.is_empty()).then_some((self.atlas_width, self.atlas_height))
    }

    /// Applies an A8 atlas update (no pixels are modified, so no damage).
    pub fn atlas(&mut self, cmd: &AtlasCmd<'_>) {
        if cmd.atlas_width == 0 || cmd.atlas_height == 0 {
            return;
        }
        let Some(len) = (cmd.atlas_width as usize).checked_mul(cmd.atlas_height as usize) else {
            return;
        };
        if len > quanta_gpu_protocol::MAX_ATLAS_BYTES {
            return;
        }
        let Some(expected) = (cmd.rect.width as usize).checked_mul(cmd.rect.height as usize) else {
            return;
        };
        if expected != cmd.data.len() {
            return;
        }
        if cmd
            .rect
            .x
            .checked_add(cmd.rect.width)
            .is_none_or(|x| x > cmd.atlas_width)
            || cmd
                .rect
                .y
                .checked_add(cmd.rect.height)
                .is_none_or(|y| y > cmd.atlas_height)
        {
            return;
        }
        if self.atlas_width != cmd.atlas_width
            || self.atlas_height != cmd.atlas_height
            || self.atlas.len() != len
        {
            self.atlas = vec![0; len];
            self.atlas_width = cmd.atlas_width;
            self.atlas_height = cmd.atlas_height;
        }
        let row = cmd.rect.width as usize;
        for y in 0..cmd.rect.height as usize {
            let dst = (cmd.rect.y as usize + y) * cmd.atlas_width as usize + cmd.rect.x as usize;
            let src = y * row;
            self.atlas[dst..dst + row].copy_from_slice(&cmd.data[src..src + row]);
        }
    }

    pub fn clear(&mut self, surface: &mut Surface<'_>, color: Rgba8) {
        surface.clear(color);
        self.damage
            .add(PixelRect::new(0, 0, surface.width, surface.height));
    }

    pub fn rect(&mut self, surface: &mut Surface<'_>, scale: f32, cmd: &RectCmd) {
        let shape = physical_rect(cmd.rect, scale);
        if shape.is_empty() {
            return;
        }
        let clip = clip_bounds(surface, cmd.clip, scale);
        let radius = (cmd.radius * scale).max(0.0);
        let border_width = (cmd.border_width * scale).max(0.0);

        // Fast path for the opaque, square fills that make up window chrome,
        // cell backgrounds and the panel. Avoids one SDF evaluation per pixel.
        if radius == 0.0 && border_width == 0.0 && cmd.fill.a == 255 {
            let Some((x0, y0, x1, y1)) = surface.pixel_range(shape.expand(1.0).intersect(clip))
            else {
                return;
            };
            self.damage.add(PixelRect::new(x0, y0, x1 - x0, y1 - y0));
            let Some((fx0, fy0, fx1, fy1)) = surface.pixel_range(shape.intersect(clip)) else {
                return;
            };
            let pixel = [cmd.fill.b, cmd.fill.g, cmd.fill.r, cmd.fill.a];
            let stride = surface.stride;
            for y in fy0..fy1 {
                let row = &mut surface.data[y as usize * stride..];
                for x in fx0..fx1 {
                    let offset = x as usize * 4;
                    row[offset..offset + 4].copy_from_slice(&pixel);
                }
            }
            return;
        }

        let area = shape.expand(1.0).intersect(clip);
        let Some((x0, y0, x1, y1)) = surface.pixel_range(area) else {
            return;
        };
        self.damage.add(PixelRect::new(x0, y0, x1 - x0, y1 - y0));

        let border_width = (cmd.border_width * scale).max(0.0);
        let fill = [
            cmd.fill.r as f32 / 255.0,
            cmd.fill.g as f32 / 255.0,
            cmd.fill.b as f32 / 255.0,
        ];
        let border = [
            cmd.border.r as f32 / 255.0,
            cmd.border.g as f32 / 255.0,
            cmd.border.b as f32 / 255.0,
        ];
        let fill_alpha = cmd.fill.a as f32 / 255.0;
        let border_alpha = cmd.border.a as f32 / 255.0;

        for y in y0..y1 {
            let py = y as f32 + 0.5;
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let d = sd_rounded_box(px, py, shape, radius);
                let inside = (0.5 - d).clamp(0.0, 1.0);
                let (fill_coverage, border_coverage) = if border_width > 0.0 {
                    let inner = (0.5 - (d + border_width)).clamp(0.0, 1.0);
                    let border_coverage = (inside - inner).max(0.0);
                    ((inside - border_coverage).max(0.0), border_coverage)
                } else {
                    (inside, 0.0)
                };
                let alpha = fill_alpha * fill_coverage + border_alpha * border_coverage;
                if alpha <= 0.0 {
                    continue;
                }
                let rgb = [
                    (fill[0] * fill_alpha * fill_coverage
                        + border[0] * border_alpha * border_coverage)
                        / alpha,
                    (fill[1] * fill_alpha * fill_coverage
                        + border[1] * border_alpha * border_coverage)
                        / alpha,
                    (fill[2] * fill_alpha * fill_coverage
                        + border[2] * border_alpha * border_coverage)
                        / alpha,
                ];
                surface.blend(x, y, rgb, alpha);
            }
        }
    }

    pub fn shadow(&mut self, surface: &mut Surface<'_>, scale: f32, cmd: &ShadowCmd) {
        let shape = physical_rect(
            FloatRect::new(
                cmd.rect.x + cmd.offset_x,
                cmd.rect.y + cmd.offset_y,
                cmd.rect.width,
                cmd.rect.height,
            ),
            scale,
        );
        if shape.is_empty() {
            return;
        }
        let blur = (cmd.blur * scale).max(0.5);
        let clip = clip_bounds(surface, cmd.clip, scale);
        let area = shape.expand(blur + 1.0).intersect(clip);
        let Some((x0, y0, x1, y1)) = surface.pixel_range(area) else {
            return;
        };
        self.damage.add(PixelRect::new(x0, y0, x1 - x0, y1 - y0));

        let radius = (cmd.radius * scale).max(0.0);
        let rgb = [
            cmd.color.r as f32 / 255.0,
            cmd.color.g as f32 / 255.0,
            cmd.color.b as f32 / 255.0,
        ];
        let color_alpha = cmd.color.a as f32 / 255.0;
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let d = sd_rounded_box(px, py, shape, radius);
                let t = (d / blur).clamp(0.0, 1.0);
                let falloff = 1.0 - t * t * (3.0 - 2.0 * t);
                let alpha = color_alpha * falloff;
                if alpha > 0.0 {
                    surface.blend(x, y, rgb, alpha);
                }
            }
        }
    }

    pub fn glyph(&mut self, surface: &mut Surface<'_>, scale: f32, cmd: &GlyphCmd) {
        // Blade snaps glyph quads to the physical pixel grid before sampling.
        let left = (cmd.rect.x * scale).round();
        let top = (cmd.rect.y * scale).round();
        let right = ((cmd.rect.x + cmd.rect.width) * scale).round();
        let bottom = ((cmd.rect.y + cmd.rect.height) * scale).round();
        let shape = Bounds {
            x0: left,
            y0: top,
            x1: right,
            y1: bottom,
        };
        if shape.is_empty() {
            return;
        }
        let clip = clip_bounds(surface, cmd.clip, scale);
        let area = shape.intersect(clip);
        let Some((x0, y0, x1, y1)) = surface.pixel_range(area) else {
            return;
        };
        if self.atlas.is_empty() || self.atlas_width == 0 || self.atlas_height == 0 {
            return;
        }
        self.damage.add(PixelRect::new(x0, y0, x1 - x0, y1 - y0));

        let rgb = [
            cmd.color.r as f32 / 255.0,
            cmd.color.g as f32 / 255.0,
            cmd.color.b as f32 / 255.0,
        ];
        let color_alpha = cmd.color.a as f32 / 255.0;
        let span_x = shape.x1 - shape.x0;
        let span_y = shape.y1 - shape.y0;
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            let fy = ((py - shape.y0) / span_y).clamp(0.0, 1.0);
            let v = cmd.uv[1] + (cmd.uv[3] - cmd.uv[1]) * fy;
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                let fx = ((px - shape.x0) / span_x).clamp(0.0, 1.0);
                let u = cmd.uv[0] + (cmd.uv[2] - cmd.uv[0]) * fx;
                let mask = sample_atlas(&self.atlas, self.atlas_width, self.atlas_height, u, v);
                let alpha = color_alpha * mask;
                if alpha > 0.0 {
                    surface.blend(x, y, rgb, alpha);
                }
            }
        }
    }

    fn take_damage(&mut self) -> Vec<PixelRect> {
        self.damage.take()
    }
}

/// Result of one executed frame.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Executed {
    /// Union of draw damage and the frame's explicit `Present` damage, clipped
    /// to the surface. Empty when the frame did not touch any pixel.
    pub damage: Vec<PixelRect>,
    /// Client-requested fence token carried by the frame, if any.
    pub fence: Option<u64>,
}

/// A [`FrameSink`] that executes v1 draw commands into a [`Surface`].
pub struct Executor<'a, 'b> {
    surface: Surface<'a>,
    rasterizer: &'b mut Rasterizer,
    scale: f32,
    present_damage: Vec<PixelRect>,
    fence: Option<u64>,
}

impl<'a, 'b> Executor<'a, 'b> {
    /// Starts executing a frame. Pending damage from the previous frame is
    /// discarded.
    pub fn new(surface: Surface<'a>, rasterizer: &'b mut Rasterizer, scale: f32) -> Self {
        rasterizer.begin_frame();
        Self {
            surface,
            rasterizer,
            scale: if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            },
            present_damage: Vec::new(),
            fence: None,
        }
    }

    pub fn width(&self) -> u32 {
        self.surface.width
    }

    pub fn height(&self) -> u32 {
        self.surface.height
    }

    /// Finishes the frame and returns normalized damage plus the fence token.
    pub fn finish(mut self) -> Executed {
        let mut damage = self.rasterizer.take_damage();
        damage.append(&mut self.present_damage);
        let width = self.surface.width;
        let height = self.surface.height;
        let mut clipped = Vec::with_capacity(damage.len());
        for rect in damage {
            if let Some(rect) = rect.clipped_to(width, height) {
                clipped.push(rect);
            }
        }
        if clipped.len() > MAX_DAMAGE {
            let merged = clipped
                .drain(..)
                .fold(PixelRect::default(), |acc, rect| acc.union(rect));
            clipped.push(merged);
        }
        Executed {
            damage: clipped,
            fence: self.fence,
        }
    }
}

impl FrameSink for Executor<'_, '_> {
    fn clear(&mut self, color: Rgba8) {
        self.rasterizer.clear(&mut self.surface, color);
    }

    fn rect(&mut self, cmd: &RectCmd) {
        self.rasterizer.rect(&mut self.surface, self.scale, cmd);
    }

    fn shadow(&mut self, cmd: &ShadowCmd) {
        self.rasterizer.shadow(&mut self.surface, self.scale, cmd);
    }

    fn glyph(&mut self, cmd: &GlyphCmd) {
        self.rasterizer.glyph(&mut self.surface, self.scale, cmd);
    }

    fn atlas(&mut self, cmd: &AtlasCmd<'_>) {
        self.rasterizer.atlas(cmd);
    }

    fn submit(&mut self, _backend: u32, _data: &[u8]) {
        // Raw backend streams require a hardware executor (virgl). The
        // software executor ignores them, like an unknown protocol op.
    }

    fn set_cursor(&mut self, _cmd: &quanta_gpu_protocol::CursorCmd) {}

    fn fence(&mut self, token: u64) {
        self.fence = Some(token);
    }

    fn present(&mut self, damage: &[PixelRect]) {
        self.present_damage.extend_from_slice(damage);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Bounds {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Bounds {
    fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn expand(self, amount: f32) -> Self {
        Self {
            x0: self.x0 - amount,
            y0: self.y0 - amount,
            x1: self.x1 + amount,
            y1: self.y1 + amount,
        }
    }

    fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

fn physical_rect(rect: FloatRect, scale: f32) -> Bounds {
    Bounds {
        x0: rect.x * scale,
        y0: rect.y * scale,
        x1: (rect.x + rect.width) * scale,
        y1: (rect.y + rect.height) * scale,
    }
}

fn clip_bounds(surface: &Surface<'_>, clip: Option<FloatRect>, scale: f32) -> Bounds {
    let full = Bounds {
        x0: 0.0,
        y0: 0.0,
        x1: surface.width as f32,
        y1: surface.height as f32,
    };
    match clip {
        Some(clip) => full.intersect(physical_rect(clip, scale)),
        None => full,
    }
}

/// Signed distance to a rounded box, identical to the FerriticUI shader.
fn sd_rounded_box(px: f32, py: f32, shape: Bounds, radius: f32) -> f32 {
    let half_width = (shape.x1 - shape.x0) * 0.5;
    let half_height = (shape.y1 - shape.y0) * 0.5;
    if half_width <= 0.0 || half_height <= 0.0 {
        return f32::INFINITY;
    }
    let cx = shape.x0 + half_width;
    let cy = shape.y0 + half_height;
    let radius = radius.clamp(0.0, half_width.min(half_height));
    let qx = (px - cx).abs() - half_width + radius;
    let qy = (py - cy).abs() - half_height + radius;
    qx.max(qy).min(0.0) + qx.max(0.0).hypot(qy.max(0.0)) - radius
}

fn sample_atlas(atlas: &[u8], width: u32, height: u32, u: f32, v: f32) -> f32 {
    if width == 0 || height == 0 || atlas.is_empty() {
        return 0.0;
    }
    let x = (u.clamp(0.0, 1.0) * width as f32 - 0.5).clamp(0.0, (width - 1) as f32);
    let y = (v.clamp(0.0, 1.0) * height as f32 - 0.5).clamp(0.0, (height - 1) as f32);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let texel = |x: u32, y: u32| -> f32 {
        let index = (y as usize) * (width as usize) + x as usize;
        atlas.get(index).copied().unwrap_or(0) as f32
    };
    let top = texel(x0, y0) * (1.0 - tx) + texel(x1, y0) * tx;
    let bottom = texel(x0, y1) * (1.0 - tx) + texel(x1, y1) * tx;
    (top * (1.0 - ty) + bottom * ty) / 255.0
}

#[derive(Default)]
struct Damage {
    rects: Vec<PixelRect>,
}

impl Damage {
    fn reset(&mut self) {
        self.rects.clear();
    }

    fn add(&mut self, rect: PixelRect) {
        if rect.is_empty() {
            return;
        }
        if self.rects.len() < MAX_DAMAGE {
            self.rects.push(rect);
        } else {
            let merged = self
                .rects
                .drain(..)
                .fold(rect, |acc, other| acc.union(other));
            self.rects.push(merged);
        }
    }

    fn take(&mut self) -> Vec<PixelRect> {
        mem::take(&mut self.rects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quanta_gpu_protocol::{decode_frame, FrameEncoder, FrameInfo};

    fn surface(pixels: &mut [u8], width: u32, height: u32) -> Surface<'_> {
        Surface::new(pixels, width, height, width as usize * 4).unwrap()
    }

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * width + x) * 4) as usize;
        [
            pixels[index],
            pixels[index + 1],
            pixels[index + 2],
            pixels[index + 3],
        ]
    }

    fn info(seq: u64, width: u32, height: u32) -> FrameInfo {
        FrameInfo {
            seq,
            width,
            height,
            scale: 1.0,
        }
    }

    #[test]
    fn surface_rejects_bad_geometry() {
        let mut pixels = [0u8; 16];
        assert_eq!(
            Surface::new(&mut pixels, 0, 4, 16).unwrap_err(),
            SurfaceError::Empty
        );
        assert_eq!(
            Surface::new(&mut pixels, 4, 4, 8).unwrap_err(),
            SurfaceError::InvalidStride
        );
        assert_eq!(
            Surface::new(&mut pixels, 4, 4, 16).unwrap_err(),
            SurfaceError::BufferTooSmall
        );
    }

    #[test]
    fn clear_writes_argb_bytes_and_keeps_padding() {
        let mut pixels = [0xAAu8; 32];
        {
            let mut surface = Surface::new(&mut pixels, 2, 2, 16).unwrap();
            surface.clear(Rgba8::new(1, 2, 3, 4));
        }
        assert_eq!(&pixels[0..8], &[3, 2, 1, 4, 3, 2, 1, 4]);
        assert_eq!(&pixels[16..24], &[3, 2, 1, 4, 3, 2, 1, 4]);
        // Row padding is not part of the visible surface.
        assert_eq!(&pixels[8..16], &[0xAA; 8]);
    }

    #[test]
    fn rect_fills_inside_and_reports_damage() {
        let mut pixels = vec![0u8; 8 * 8 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 8, 8);
        rasterizer.rect(
            &mut surface,
            1.0,
            &RectCmd {
                rect: FloatRect::new(0.0, 0.0, 4.0, 4.0),
                radius: 0.0,
                border_width: 0.0,
                fill: Rgba8::new(255, 0, 0, 255),
                border: Rgba8::TRANSPARENT,
                clip: None,
            },
        );
        assert_eq!(pixel(&pixels, 8, 1, 1), [0, 0, 255, 255]);
        assert_eq!(pixel(&pixels, 8, 6, 6), [0, 0, 0, 0]);
        let damage = rasterizer.take_damage();
        assert_eq!(damage.len(), 1);
        assert_eq!(damage[0], PixelRect::new(0, 0, 5, 5));
    }

    #[test]
    fn border_is_inside_the_shape() {
        let mut pixels = vec![0u8; 10 * 10 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 10, 10);
        rasterizer.rect(
            &mut surface,
            1.0,
            &RectCmd {
                rect: FloatRect::new(0.0, 0.0, 10.0, 10.0),
                radius: 0.0,
                border_width: 2.0,
                fill: Rgba8::BLACK,
                border: Rgba8::WHITE,
                clip: None,
            },
        );
        assert_eq!(pixel(&pixels, 10, 0, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(&pixels, 10, 1, 5), [255, 255, 255, 255]);
        assert_eq!(pixel(&pixels, 10, 5, 5), [0, 0, 0, 255]);
    }

    #[test]
    fn rounded_corner_is_antialiased() {
        let mut pixels = vec![0u8; 16 * 16 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 16, 16);
        rasterizer.rect(
            &mut surface,
            1.0,
            &RectCmd {
                rect: FloatRect::new(0.0, 0.0, 16.0, 16.0),
                radius: 8.0,
                border_width: 0.0,
                fill: Rgba8::WHITE,
                border: Rgba8::TRANSPARENT,
                clip: None,
            },
        );
        let corner = pixel(&pixels, 16, 2, 2);
        let outside = pixel(&pixels, 16, 0, 0);
        let center = pixel(&pixels, 16, 8, 8);
        assert_eq!(center, [255, 255, 255, 255]);
        assert_eq!(outside, [0, 0, 0, 0]);
        assert!(corner[3] < center[3], "corner {corner:?} should be softer");
        assert!(
            corner[3] > 0,
            "corner {corner:?} should be partially covered"
        );
    }

    #[test]
    fn clip_limits_both_pixels_and_damage() {
        let mut pixels = vec![0u8; 8 * 8 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 8, 8);
        rasterizer.rect(
            &mut surface,
            1.0,
            &RectCmd {
                rect: FloatRect::new(0.0, 0.0, 8.0, 8.0),
                radius: 0.0,
                border_width: 0.0,
                fill: Rgba8::WHITE,
                border: Rgba8::TRANSPARENT,
                clip: Some(FloatRect::new(2.0, 2.0, 2.0, 2.0)),
            },
        );
        assert_eq!(pixel(&pixels, 8, 1, 1), [0, 0, 0, 0]);
        assert_eq!(pixel(&pixels, 8, 2, 2), [255, 255, 255, 255]);
        assert_eq!(pixel(&pixels, 8, 4, 4), [0, 0, 0, 0]);
        assert_eq!(rasterizer.take_damage(), [PixelRect::new(2, 2, 2, 2)]);
    }

    #[test]
    fn shadow_fades_outside_the_shape() {
        let mut pixels = vec![0u8; 32 * 32 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 32, 32);
        rasterizer.shadow(
            &mut surface,
            1.0,
            &ShadowCmd {
                rect: FloatRect::new(8.0, 8.0, 8.0, 8.0),
                radius: 2.0,
                blur: 6.0,
                offset_x: 0.0,
                offset_y: 0.0,
                color: Rgba8::new(0, 0, 0, 255),
                clip: None,
            },
        );
        let center = pixel(&pixels, 32, 12, 12)[3];
        let near = pixel(&pixels, 32, 17, 12)[3];
        let far = pixel(&pixels, 32, 25, 12)[3];
        assert!(
            center > near,
            "center {center} should be darker than {near}"
        );
        assert!(near >= far, "shadow should fade outward ({near} vs {far})");
        assert_eq!(far, 0);
    }

    #[test]
    fn glyph_samples_the_atlas() {
        let mut pixels = vec![0u8; 4 * 4 * 4];
        let mut rasterizer = Rasterizer::new();
        rasterizer.atlas(&AtlasCmd {
            atlas_width: 1,
            atlas_height: 1,
            rect: PixelRect::new(0, 0, 1, 1),
            data: &[255],
        });
        let mut surface = surface(&mut pixels, 4, 4);
        rasterizer.glyph(
            &mut surface,
            1.0,
            &GlyphCmd {
                rect: FloatRect::new(0.0, 0.0, 4.0, 4.0),
                uv: [0.0, 0.0, 1.0, 1.0],
                color: Rgba8::new(0, 255, 0, 255),
                clip: None,
            },
        );
        assert_eq!(pixel(&pixels, 4, 2, 2), [0, 255, 0, 255]);
        assert_eq!(rasterizer.take_damage(), [PixelRect::new(0, 0, 4, 4)]);
    }

    #[test]
    fn glyph_without_atlas_draws_nothing() {
        let mut pixels = vec![0u8; 4 * 4 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 4, 4);
        rasterizer.glyph(
            &mut surface,
            1.0,
            &GlyphCmd {
                rect: FloatRect::new(0.0, 0.0, 4.0, 4.0),
                uv: [0.0, 0.0, 1.0, 1.0],
                color: Rgba8::WHITE,
                clip: None,
            },
        );
        assert_eq!(pixel(&pixels, 4, 2, 2), [0, 0, 0, 0]);
        assert!(rasterizer.take_damage().is_empty());
    }

    #[test]
    fn executor_plays_a_full_frame() {
        let mut pixels = vec![0u8; 8 * 8 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut encoder = FrameEncoder::new(info(1, 8, 8)).unwrap();
        encoder
            .clear(Rgba8::BLACK)
            .unwrap()
            .atlas(&AtlasCmd {
                atlas_width: 1,
                atlas_height: 1,
                rect: PixelRect::new(0, 0, 1, 1),
                data: &[255],
            })
            .unwrap()
            .rect(&RectCmd {
                rect: FloatRect::new(1.0, 1.0, 2.0, 2.0),
                radius: 0.0,
                border_width: 0.0,
                fill: Rgba8::new(255, 0, 0, 255),
                border: Rgba8::TRANSPARENT,
                clip: None,
            })
            .unwrap()
            .glyph(&GlyphCmd {
                rect: FloatRect::new(5.0, 5.0, 2.0, 2.0),
                uv: [0.0, 0.0, 1.0, 1.0],
                color: Rgba8::new(0, 255, 0, 255),
                clip: None,
            })
            .unwrap()
            .fence(42)
            .unwrap()
            .present(&[PixelRect::new(0, 0, 8, 8)])
            .unwrap();
        let frame = encoder.finish().unwrap();

        let surface = surface(&mut pixels, 8, 8);
        let mut executor = Executor::new(surface, &mut rasterizer, 1.0);
        decode_frame(&frame, &mut executor).unwrap();
        let executed = executor.finish();
        assert_eq!(executed.fence, Some(42));
        assert!(!executed.damage.is_empty());
        assert_eq!(pixel(&pixels, 8, 2, 2), [0, 0, 255, 255]);
        assert_eq!(pixel(&pixels, 8, 5, 5), [0, 255, 0, 255]);
        assert_eq!(pixel(&pixels, 8, 7, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn invalid_frame_does_not_touch_the_surface() {
        let mut pixels = vec![0x11u8; 8 * 8 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut encoder = FrameEncoder::new(info(1, 8, 8)).unwrap();
        encoder
            .clear(Rgba8::WHITE)
            .unwrap()
            .rect(&RectCmd {
                rect: FloatRect::new(0.0, 0.0, 8.0, 8.0),
                radius: 0.0,
                border_width: 0.0,
                fill: Rgba8::WHITE,
                border: Rgba8::TRANSPARENT,
                clip: None,
            })
            .unwrap()
            .present(&[PixelRect::new(0, 0, 8, 8)])
            .unwrap();
        let mut frame = encoder.finish().unwrap();
        // Corrupt the Present damage count after the valid draw records.
        let count_at = frame.len() - 20;
        frame[count_at..count_at + 4].copy_from_slice(&999u32.to_le_bytes());

        let surface = surface(&mut pixels, 8, 8);
        let mut executor = Executor::new(surface, &mut rasterizer, 1.0);
        assert!(decode_frame(&frame, &mut executor).is_err());
        let executed = executor.finish();
        assert!(executed.damage.is_empty());
        assert!(pixels.iter().all(|byte| *byte == 0x11));
    }

    #[test]
    fn damage_is_clipped_to_the_surface() {
        let mut pixels = vec![0u8; 4 * 4 * 4];
        let mut rasterizer = Rasterizer::new();
        let surface = surface(&mut pixels, 4, 4);
        let mut executor = Executor::new(surface, &mut rasterizer, 1.0);
        executor.present(&[PixelRect::new(2, 2, 100, 100)]);
        let executed = executor.finish();
        assert_eq!(executed.damage, [PixelRect::new(2, 2, 2, 2)]);
    }

    #[test]
    fn scale_maps_logical_coordinates_to_pixels() {
        let mut pixels = vec![0u8; 8 * 8 * 4];
        let mut rasterizer = Rasterizer::new();
        let mut surface = surface(&mut pixels, 8, 8);
        rasterizer.rect(
            &mut surface,
            2.0,
            &RectCmd {
                rect: FloatRect::new(1.0, 1.0, 2.0, 2.0),
                radius: 0.0,
                border_width: 0.0,
                fill: Rgba8::WHITE,
                border: Rgba8::TRANSPARENT,
                clip: None,
            },
        );
        assert_eq!(pixel(&pixels, 8, 1, 1), [0, 0, 0, 0]);
        assert_eq!(pixel(&pixels, 8, 2, 2), [255, 255, 255, 255]);
        assert_eq!(pixel(&pixels, 8, 5, 5), [255, 255, 255, 255]);
    }
}
