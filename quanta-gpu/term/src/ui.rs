//! A small desktop: wallpaper, top panel and one floating terminal window.
//!
//! The wallpaper is written through the v0 shared surface; everything else is
//! drawn with v1 records (`Rect`, `Shadow`, `Glyph`, `Atlas`) and executed by
//! the pure-Rust rasterizer in `quanta-gpud`.

use std::io;

use quanta_gpu_client::{
    AtlasCmd, Connection, FloatRect, Frame, GlyphCmd, PixelRect, RectCmd, Rgba8,
};

use crate::font;
use crate::terminal::Terminal;
use crate::wallpaper::{Wallpaper, FALLBACK};

const CELL_W: u32 = 16;
const CELL_H: u32 = 26;

const PANEL: Rgba8 = Rgba8::new(26, 30, 42, 220);
const PANEL_TEXT: Rgba8 = Rgba8::new(200, 206, 222, 255);
const WINDOW: Rgba8 = Rgba8::new(8, 10, 15, 255);
const TITLE: Rgba8 = Rgba8::new(44, 50, 66, 245);
const BORDER: Rgba8 = Rgba8::new(88, 96, 116, 255);
const TITLE_TEXT: Rgba8 = Rgba8::new(228, 231, 240, 255);
const CURSOR: Rgba8 = Rgba8::new(235, 238, 245, 120);
const POINTER: Rgba8 = Rgba8::new(250, 250, 255, 255);

pub struct Layout {
    pub topbar_h: u32,
    pub win_w: u32,
    pub win_h: u32,
    pub title_h: u32,
    pub pad: u32,
    pub cols: usize,
    pub rows: usize,
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        let topbar_h = (height / 28).clamp(22, 32);
        let margin = (width / 40).clamp(16, 36);
        let title_h = 30;
        let pad = 10;

        let max_w = width.saturating_sub(margin * 2).max(480);
        let max_h = height.saturating_sub(topbar_h + margin).max(320);
        let win_w = ((width as u64 * 3 / 4) as u32).clamp(480.min(max_w), max_w);
        let win_h = ((height as u64 * 2 / 3) as u32).clamp(320.min(max_h), max_h);

        let cols = (win_w.saturating_sub(pad * 2) / CELL_W).max(8) as usize;
        let rows = (win_h.saturating_sub(title_h + pad * 2) / CELL_H).max(4) as usize;

        Self {
            topbar_h,
            win_w,
            win_h,
            title_h,
            pad,
            cols,
            rows,
        }
    }
}

/// The A8 atlas: 16 columns of 8x13 glyphs, six rows for ASCII 32..=126.
pub struct Glyphs {
    data: Vec<u8>,
    atlas_w: u32,
    atlas_h: u32,
}

impl Glyphs {
    pub fn new() -> Self {
        let atlas_w = 16 * font::WIDTH;
        let atlas_h = 6 * font::HEIGHT;
        let mut data = vec![0u8; (atlas_w * atlas_h) as usize];
        for index in 0..font::COUNT {
            let col = (index % 16) as u32;
            let row = (index / 16) as u32;
            let glyph = &font::GLYPHS[index];
            for y in 0..font::HEIGHT {
                let dst = ((row * font::HEIGHT + y) * atlas_w + col * font::WIDTH) as usize;
                for x in 0..font::WIDTH {
                    if glyph[y as usize] & (0x80 >> x) != 0 {
                        data[dst + x as usize] = 0xff;
                    }
                }
            }
        }
        Self {
            data,
            atlas_w,
            atlas_h,
        }
    }

    fn uv(&self, ch: char) -> Option<[f32; 4]> {
        let code = ch as u32;
        if !(font::FIRST as u32..=font::LAST as u32).contains(&code) {
            return None;
        }
        let index = (code - font::FIRST as u32) as u32;
        let col = index % 16;
        let row = index / 16;
        let u0 = (col * font::WIDTH) as f32 / self.atlas_w as f32;
        let v0 = (row * font::HEIGHT) as f32 / self.atlas_h as f32;
        let u1 = ((col + 1) * font::WIDTH) as f32 / self.atlas_w as f32;
        let v1 = ((row + 1) * font::HEIGHT) as f32 / self.atlas_h as f32;
        Some([u0, v0, u1, v1])
    }
}

pub struct Desktop {
    glyphs: Glyphs,
    uploaded: bool,
    layout: Layout,
    wallpaper: Option<Wallpaper>,
    win_x: i32,
    win_y: i32,
    pointer: (i32, i32),
    dragging: Option<(i32, i32)>,
    left_down: bool,
    full_damage: bool,
}

impl Desktop {
    pub fn new(width: u32, height: u32) -> Self {
        let layout = Layout::new(width, height);
        let win_x = ((width as i32 - layout.win_w as i32) / 2).max(0);
        let win_y = (((height as i32 - layout.win_h as i32) / 2) + layout.topbar_h as i32 / 2)
            .max(layout.topbar_h as i32 + 4);
        Self {
            glyphs: Glyphs::new(),
            uploaded: false,
            layout,
            wallpaper: Wallpaper::load(),
            win_x,
            win_y,
            pointer: (width as i32 / 2, height as i32 / 2),
            dragging: None,
            left_down: false,
            full_damage: true,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn pointer(&mut self, x: i32, y: i32) {
        self.pointer = (x, y);
        if let Some((dx, dy)) = self.dragging {
            self.win_x = x - dx;
            self.win_y = y - dy;
            self.clamp_window();
            self.full_damage = true;
        }
    }

    pub fn button(&mut self, mask: i32) {
        let left = mask & 1 != 0;
        if left && !self.left_down {
            let (px, py) = self.pointer;
            if px >= self.win_x
                && px < self.win_x + self.layout.win_w as i32
                && py >= self.win_y
                && py < self.win_y + self.layout.title_h as i32
            {
                self.dragging = Some((px - self.win_x, py - self.win_y));
            }
        }
        if !left {
            self.dragging = None;
        }
        self.left_down = left;
    }

    fn clamp_window(&mut self) {
        // The render path also clamps against the current connection; this
        // keeps the title bar reachable while dragging.
        if self.win_x < 0 {
            self.win_x = 0;
        }
        if self.win_y < self.layout.topbar_h as i32 {
            self.win_y = self.layout.topbar_h as i32;
        }
    }

    pub fn render(&mut self, conn: &mut Connection, term: &Terminal) -> io::Result<u64> {
        let width = conn.width();
        let height = conn.height();
        if self.layout.win_w != Layout::new(width, height).win_w
            || self.layout.win_h != Layout::new(width, height).win_h
        {
            // A resize only happens on startup in this image; keep the window
            // fully on screen when it does.
            self.layout = Layout::new(width, height);
            self.full_damage = true;
        }
        let layout = &self.layout;
        if self.win_x + layout.win_w as i32 > width as i32 {
            self.win_x = (width as i32 - layout.win_w as i32).max(0);
            self.full_damage = true;
        }
        if self.win_y + layout.win_h as i32 > height as i32 {
            self.win_y = (height as i32 - layout.win_h as i32).max(layout.topbar_h as i32);
            self.full_damage = true;
        }

        // v0 wallpaper: write it into the shared surface before the v1 frame.
        if let Some(wallpaper) = &self.wallpaper {
            wallpaper.paint(conn);
        } else {
            paint_fallback(conn, FALLBACK);
        }

        let mut frame = conn.frame()?;
        if !self.uploaded {
            frame.atlas(&AtlasCmd {
                atlas_width: self.glyphs.atlas_w,
                atlas_height: self.glyphs.atlas_h,
                rect: PixelRect::new(0, 0, self.glyphs.atlas_w, self.glyphs.atlas_h),
                data: &self.glyphs.data,
            });
            self.uploaded = true;
        }

        // Top panel.
        frame.rect(&rect(0.0, 0.0, width as f32, layout.topbar_h as f32, PANEL));
        self.text(
            &mut frame,
            layout.pad as f32,
            ((layout.topbar_h as f32) - font::HEIGHT as f32) / 2.0,
            "QuantaOS",
            PANEL_TEXT,
        );
        let version = "QuantaGPU v1  |  ion";
        let text_w = version.chars().count() as f32 * font::WIDTH as f32;
        self.text(
            &mut frame,
            width as f32 - layout.pad as f32 - text_w,
            ((layout.topbar_h as f32) - font::HEIGHT as f32) / 2.0,
            version,
            PANEL_TEXT,
        );

        let wx = self.win_x as f32;
        let wy = self.win_y as f32;
        let ww = layout.win_w as f32;
        let wh = layout.win_h as f32;

        // Offset shadow + border + body.
        frame.rect(&rect(wx + 7.0, wy + 7.0, ww, wh, Rgba8::new(0, 0, 0, 90)));
        frame.rect(&rect(wx, wy, ww, wh, BORDER));
        frame.rect(&rect(wx + 1.0, wy + 1.0, ww - 2.0, wh - 2.0, WINDOW));
        frame.rect(&rect(
            wx + 1.0,
            wy + 1.0,
            ww - 2.0,
            layout.title_h as f32,
            TITLE,
        ));
        let text_x = self.win_x + layout.pad as i32;
        let text_y = self.win_y + layout.title_h as i32 + layout.pad as i32;
        self.text(
            &mut frame,
            text_x as f32,
            wy + (layout.title_h as f32 - font::HEIGHT as f32) / 2.0,
            "Terminal",
            TITLE_TEXT,
        );
        frame.rect(&RectCmd {
            rect: FloatRect::new(
                (self.win_x + layout.win_w as i32 - 24) as f32,
                wy + 8.0,
                14.0,
                14.0,
            ),
            radius: 7.0,
            border_width: 0.0,
            fill: Rgba8::new(224, 96, 96, 255),
            border: Rgba8::TRANSPARENT,
            clip: None,
        });

        let clip = Some(FloatRect::new(
            text_x as f32,
            text_y as f32,
            (layout.cols as u32 * CELL_W) as f32,
            (layout.rows as u32 * CELL_H) as f32,
        ));
        let default_bg = term.default_bg();
        for row in 0..layout.rows {
            for col in 0..layout.cols {
                let Some(cell) = term.cell(col, row) else {
                    continue;
                };
                let x = text_x + col as i32 * CELL_W as i32;
                let y = text_y + row as i32 * CELL_H as i32;
                if cell.bg != default_bg {
                    frame.rect(&RectCmd {
                        rect: FloatRect::new(x as f32, y as f32, CELL_W as f32, CELL_H as f32),
                        radius: 0.0,
                        border_width: 0.0,
                        fill: cell.bg,
                        border: Rgba8::TRANSPARENT,
                        clip,
                    });
                }
                if cell.ch != ' ' && cell.ch != '\0' {
                    if let Some(uv) = self.glyphs.uv(cell.ch) {
                        frame.glyph(&GlyphCmd {
                            rect: FloatRect::new(x as f32, y as f32, CELL_W as f32, CELL_H as f32),
                            uv,
                            color: cell.fg,
                            clip,
                        });
                    }
                }
            }
        }

        if let Some((col, row)) = term.cursor() {
            if col < layout.cols && row < layout.rows {
                frame.rect(&RectCmd {
                    rect: FloatRect::new(
                        (text_x + col as i32 * CELL_W as i32) as f32,
                        (text_y + row as i32 * CELL_H as i32) as f32,
                        CELL_W as f32,
                        CELL_H as f32,
                    ),
                    radius: 0.0,
                    border_width: 0.0,
                    fill: CURSOR,
                    border: Rgba8::TRANSPARENT,
                    clip,
                });
            }
        }

        // Software pointer: a small outlined cross so it is visible on both
        // the wallpaper and the window.
        let (px, py) = self.pointer;
        frame.rect(&rect(
            px as f32 - 1.0,
            py as f32 - 1.0,
            14.0,
            3.0,
            Rgba8::BLACK,
        ));
        frame.rect(&rect(
            px as f32 - 1.0,
            py as f32 - 1.0,
            3.0,
            20.0,
            Rgba8::BLACK,
        ));
        frame.rect(&rect(px as f32, py as f32, 12.0, 1.0, POINTER));
        frame.rect(&rect(px as f32, py as f32, 1.0, 18.0, POINTER));

        let mut damage: Vec<PixelRect> = Vec::new();
        if self.full_damage {
            damage.push(PixelRect::new(0, 0, width, height));
            self.full_damage = false;
        }
        let result = frame.present(&damage);
        result
    }

    fn text(&self, frame: &mut Frame<'_>, x: f32, y: f32, text: &str, color: Rgba8) {
        let mut cursor = x;
        for ch in text.chars() {
            if let Some(uv) = self.glyphs.uv(ch) {
                frame.glyph(&GlyphCmd {
                    rect: FloatRect::new(cursor, y, font::WIDTH as f32, font::HEIGHT as f32),
                    uv,
                    color,
                    clip: None,
                });
            }
            cursor += font::WIDTH as f32;
        }
    }
}

fn paint_fallback(conn: &mut Connection, color: Rgba8) {
    let stride = conn.stride() as usize;
    let height = conn.height() as usize;
    let pixels = conn.pixels_mut();
    for y in 0..height {
        let row = &mut pixels[y * stride..(y + 1) * stride];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&[color.b, color.g, color.r, color.a]);
        }
    }
}

fn rect(x: f32, y: f32, w: f32, h: f32, fill: Rgba8) -> RectCmd {
    RectCmd {
        rect: FloatRect::new(x, y, w, h),
        radius: 0.0,
        border_width: 0.0,
        fill,
        border: Rgba8::TRANSPARENT,
        clip: None,
    }
}
