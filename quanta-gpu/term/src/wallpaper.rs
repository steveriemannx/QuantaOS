//! Embedded desktop wallpaper.
//!
//! The image is a plain PPM (P6) so the terminal has no image-decoder
//! dependency. It is scaled with nearest-neighbour into the shared
//! `argb8888` surface, which is the protocol's v0 path; the desktop window is
//! then drawn on top with v1 records.

use quanta_gpu_client::Connection;

const PPM: &[u8] = include_bytes!("../assets/wallpaper.ppm");

pub struct Wallpaper {
    width: usize,
    height: usize,
    rgb: &'static [u8],
}

impl Wallpaper {
    pub fn load() -> Option<Self> {
        let mut fields = PPM.splitn(3, |byte| *byte == b'\n');
        let magic = fields.next()?;
        if magic != b"P6" {
            return None;
        }
        let size = fields.next()?;
        fields.next()?.strip_prefix(b"255\n")?;
        let mut dims = size.split(|byte| *byte == b' ');
        let width = std::str::from_utf8(dims.next()?).ok()?.parse().ok()?;
        let height = std::str::from_utf8(dims.next()?).ok()?.parse().ok()?;
        let header_len = 3 + size.len() + 1 + 4; // "P6\n" + size + "\n255\n"
        let rgb = PPM.get(header_len..)?;
        if rgb.len() < width * height * 3 {
            return None;
        }
        Some(Self { width, height, rgb })
    }

    pub fn paint(&self, conn: &mut Connection) {
        let screen_w = conn.width() as usize;
        let screen_h = conn.height() as usize;
        let stride = conn.stride() as usize;
        let pixels = conn.pixels_mut();
        for y in 0..screen_h {
            let src_y = y * self.height / screen_h.max(1);
            let src_row = src_y * self.width;
            let dst_row = y * stride;
            for x in 0..screen_w {
                let src_x = x * self.width / screen_w.max(1);
                let src = (src_row + src_x) * 3;
                let dst = dst_row + x * 4;
                pixels[dst] = self.rgb[src + 2];
                pixels[dst + 1] = self.rgb[src + 1];
                pixels[dst + 2] = self.rgb[src];
                pixels[dst + 3] = 0xff;
            }
        }
    }
}

pub const FALLBACK: quanta_gpu_client::Rgba8 = quanta_gpu_client::Rgba8::new(22, 25, 34, 255);
