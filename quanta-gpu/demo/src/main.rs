//! Fullscreen QuantaGPU demo.
//!
//! Two modes render the same scene through the two halves of the v0/v1
//! protocol:
//!
//! * default (`--v1`): the client sends `Clear`, `Shadow`, `Rect`, `Atlas` and
//!   `Glyph` records; `quanta-gpud` executes them with the pure-Rust reference
//!   rasterizer. The client never touches the framebuffer.
//! * `--surface`: the client rasterizes the scene into the shared surface and
//!   submits damage rectangles (protocol v0).
//!
//! Both are easy to verify in a QEMU screendump: four coloured quadrants, a
//! black centre panel with a white border, a moving yellow bar and the word
//! `QUANTA` drawn from an A8 atlas.

#![cfg(target_os = "redox")]

use std::thread;
use std::time::Duration;

use quanta_gpu_client::{
    AtlasCmd, Connection, Features, FloatRect, GlyphCmd, PixelRect, RectCmd, Rgba8, ShadowCmd,
};

const RED: Rgba8 = Rgba8::new(220, 50, 47, 255);
const GREEN: Rgba8 = Rgba8::new(133, 153, 0, 255);
const BLUE: Rgba8 = Rgba8::new(38, 139, 210, 255);
const WHITE: Rgba8 = Rgba8::new(255, 255, 255, 255);
const BLACK: Rgba8 = Rgba8::new(0, 0, 0, 255);
const YELLOW: Rgba8 = Rgba8::new(255, 214, 0, 255);
const BACKGROUND: Rgba8 = Rgba8::new(16, 18, 24, 255);

/// 8x8 A8 glyphs for `QUANTA`, one byte per row, MSB first.
const GLYPH_SIZE: u32 = 8;
const FONT: [[u8; GLYPH_SIZE as usize]; 6] = [
    // Q
    [
        0b0011_1100,
        0b0100_0010,
        0b0100_0010,
        0b0100_0010,
        0b0100_1010,
        0b0100_0100,
        0b0011_1010,
        0b0000_0001,
    ],
    // U
    [
        0b0100_0010,
        0b0100_0010,
        0b0100_0010,
        0b0100_0010,
        0b0100_0010,
        0b0100_0010,
        0b0011_1100,
        0b0000_0000,
    ],
    // A
    [
        0b0001_1000,
        0b0010_0100,
        0b0100_0010,
        0b0100_0010,
        0b0111_1110,
        0b0100_0010,
        0b0100_0010,
        0b0000_0000,
    ],
    // N
    [
        0b0100_0010,
        0b0110_0010,
        0b0101_0010,
        0b0100_1010,
        0b0100_0110,
        0b0100_0010,
        0b0100_0010,
        0b0000_0000,
    ],
    // T
    [
        0b0111_1110,
        0b0001_1000,
        0b0001_1000,
        0b0001_1000,
        0b0001_1000,
        0b0001_1000,
        0b0001_1000,
        0b0000_0000,
    ],
    // A
    [
        0b0001_1000,
        0b0010_0100,
        0b0100_0010,
        0b0100_0010,
        0b0111_1110,
        0b0100_0010,
        0b0100_0010,
        0b0000_0000,
    ],
];

fn main() {
    let mode = std::env::args().nth(1);
    let mut conn = connect();
    eprintln!(
        "quanta-demo: {}x{} stride {} format {:?}",
        conn.width(),
        conn.height(),
        conn.stride(),
        conn.format()
    );

    match mode.as_deref() {
        None | Some("--v1") => {
            if conn.info().features.contains(Features::RENDER) {
                run_protocol(&mut conn);
            } else {
                eprintln!("quanta-demo: driver does not advertise v1 render; using --surface");
                run_surface(&mut conn);
            }
        }
        Some("--surface") => run_surface(&mut conn),
        Some(other) => {
            eprintln!("usage: quanta-demo [--v1|--surface]");
            eprintln!("unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

fn connect() -> Connection {
    for _ in 0..50 {
        match Connection::open() {
            Ok(conn) => return conn,
            Err(err) => {
                eprintln!("quanta-demo: waiting for quanta-gpu: {err}");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
    eprintln!("quanta-demo: quanta-gpu did not appear");
    std::process::exit(1);
}

fn run_protocol(conn: &mut Connection) {
    let atlas = build_atlas();
    let mut uploaded = false;

    let mut frame_number = 0u64;
    loop {
        let width = conn.width() as f32;
        let height = conn.height() as f32;
        let panel = FloatRect::new(width * 0.25, height * 0.3, width * 0.5, height * 0.34);

        let mut frame = match conn.frame() {
            Ok(frame) => frame,
            Err(err) => {
                eprintln!("quanta-demo: could not start frame: {err}");
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        if !uploaded {
            frame.atlas(&AtlasCmd {
                atlas_width: atlas.width,
                atlas_height: GLYPH_SIZE,
                rect: PixelRect::new(0, 0, atlas.width, GLYPH_SIZE),
                data: &atlas.data,
            });
            uploaded = true;
        }

        frame.clear(BACKGROUND);
        // Four quadrants.
        frame
            .rect(&fill(0.0, 0.0, width / 2.0, height / 2.0, RED))
            .rect(&fill(width / 2.0, 0.0, width / 2.0, height / 2.0, GREEN))
            .rect(&fill(0.0, height / 2.0, width / 2.0, height / 2.0, BLUE))
            .rect(&fill(
                width / 2.0,
                height / 2.0,
                width / 2.0,
                height / 2.0,
                WHITE,
            ));

        // A soft shadow and a bordered panel, the same shape `ferritic-soft`
        // draws for a card.
        frame.shadow(&ShadowCmd {
            rect: panel,
            radius: 18.0,
            blur: 28.0,
            offset_x: 0.0,
            offset_y: 10.0,
            color: Rgba8::new(0, 0, 0, 150),
            clip: None,
        });
        frame.rect(&RectCmd {
            rect: panel,
            radius: 18.0,
            border_width: 4.0,
            fill: Rgba8::new(18, 20, 28, 255),
            border: WHITE,
            clip: None,
        });

        // Text from the A8 atlas.
        let scale = 4.0;
        let text_w = atlas.width as f32 * scale;
        let text_h = GLYPH_SIZE as f32 * scale;
        let text_x = panel.x + (panel.width - text_w) / 2.0;
        let text_y = panel.y + panel.height - text_h - 20.0;
        for index in 0..FONT.len() {
            let u0 = index as f32 * GLYPH_SIZE as f32 / atlas.width as f32;
            let u1 = (index as f32 + 1.0) * GLYPH_SIZE as f32 / atlas.width as f32;
            frame.glyph(&GlyphCmd {
                rect: FloatRect::new(
                    text_x + index as f32 * GLYPH_SIZE as f32 * scale,
                    text_y,
                    GLYPH_SIZE as f32 * scale,
                    text_h,
                ),
                uv: [u0, 0.0, u1, 1.0],
                color: WHITE,
                clip: None,
            });
        }

        // A moving bar makes it obvious that presents are reaching the
        // scanout. `Clear` above redraws the background, so it leaves no trail.
        let bar_width = (width / 20.0).max(8.0);
        let bar_x = ((frame_number as f32) * bar_width.max(16.0)) % width;
        frame.rect(&RectCmd {
            rect: FloatRect::new(bar_x, 0.0, bar_width, height),
            radius: 4.0,
            border_width: 0.0,
            fill: YELLOW,
            border: Rgba8::TRANSPARENT,
            clip: None,
        });

        match frame.present(&[]) {
            Ok(seq) => {
                if frame_number.is_multiple_of(10) {
                    eprintln!("quanta-demo: v1 frame {seq}");
                }
                // Drain one acknowledgement so the queue stays bounded.
                let _ = conn.read_fence();
            }
            Err(err) => {
                eprintln!("quanta-demo: v1 present failed: {err}");
                thread::sleep(Duration::from_secs(1));
            }
        }

        frame_number = frame_number.wrapping_add(1);
        thread::sleep(Duration::from_millis(500));
    }
}

fn run_surface(conn: &mut Connection) {
    let mut frame = 0u64;
    loop {
        draw_surface(conn, frame);
        match conn.present_full() {
            Ok(seq) => {
                if frame.is_multiple_of(10) {
                    eprintln!("quanta-demo: v0 frame {seq}");
                }
                let _ = conn.read_fence();
            }
            Err(err) => {
                eprintln!("quanta-demo: v0 present failed: {err}");
                thread::sleep(Duration::from_secs(1));
            }
        }
        frame = frame.wrapping_add(1);
        thread::sleep(Duration::from_millis(500));
    }
}

fn fill(x: f32, y: f32, width: f32, height: f32, color: Rgba8) -> RectCmd {
    RectCmd {
        rect: FloatRect::new(x, y, width, height),
        radius: 0.0,
        border_width: 0.0,
        fill: color,
        border: Rgba8::TRANSPARENT,
        clip: None,
    }
}

struct Atlas {
    width: u32,
    data: Vec<u8>,
}

fn build_atlas() -> Atlas {
    let width = GLYPH_SIZE * FONT.len() as u32;
    let mut data = vec![0u8; (width * GLYPH_SIZE) as usize];
    for (index, glyph) in FONT.iter().enumerate() {
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..GLYPH_SIZE {
                if *bits & (0x80 >> column) != 0 {
                    let x = index as u32 * GLYPH_SIZE + column;
                    let y = row as u32;
                    data[(y * width + x) as usize] = 255;
                }
            }
        }
    }
    Atlas { width, data }
}

/// The v0 path: render pixels on the CPU into the shared surface.
fn draw_surface(conn: &mut Connection, frame: u64) {
    let width = conn.width();
    let height = conn.height();
    let stride = conn.stride() as usize;
    let pixels = conn.pixels_mut();

    for y in 0..height {
        let row = &mut pixels[y as usize * stride..(y as usize + 1) * stride];
        for x in 0..width {
            let color = if x < width / 2 {
                if y < height / 2 {
                    RED
                } else {
                    BLUE
                }
            } else if y < height / 2 {
                GREEN
            } else {
                WHITE
            };
            put(row, x as usize, color);
        }
    }

    let panel_w = width / 2;
    let panel_h = height / 3;
    let panel_x = (width - panel_w) / 2;
    let panel_y = (height - panel_h) / 2;
    for y in panel_y..panel_y + panel_h {
        let row = &mut pixels[y as usize * stride..(y as usize + 1) * stride];
        for x in panel_x..panel_x + panel_w {
            let border = x < panel_x + 4
                || x >= panel_x + panel_w - 4
                || y < panel_y + 4
                || y >= panel_y + panel_h - 4;
            put(row, x as usize, if border { WHITE } else { BLACK });
        }
    }

    let bar_w = (width / 20).max(8);
    let bar_x = (frame.wrapping_mul(bar_w.max(16) as u64) as u32) % width;
    for y in 0..height {
        let row = &mut pixels[y as usize * stride..(y as usize + 1) * stride];
        for x in bar_x..(bar_x + bar_w).min(width) {
            put(row, x as usize, YELLOW);
        }
    }
}

fn put(row: &mut [u8], x: usize, color: Rgba8) {
    let offset = x * 4;
    row[offset..offset + 4].copy_from_slice(&[color.b, color.g, color.r, color.a]);
}
