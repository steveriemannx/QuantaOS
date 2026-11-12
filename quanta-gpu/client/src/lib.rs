//! Client for `/scheme/quanta-gpu`.
//!
//! The protocol is a single fullscreen surface with two ways to fill it:
//!
//! * **v0** — the client rasterizes into the mapped surface and submits damage
//!   rectangles ([`Connection::pixels_mut`] + [`Connection::present`]).
//! * **v1** — the client sends [`Clear`](quanta_gpu_protocol::OP_CLEAR),
//!   [`Rect`](quanta_gpu_protocol::OP_RECT),
//!   [`Shadow`](quanta_gpu_protocol::OP_SHADOW),
//!   [`Glyph`](quanta_gpu_protocol::OP_GLYPH) and
//!   [`Atlas`](quanta_gpu_protocol::OP_ATLAS) records through
//!   [`Connection::frame`]; the driver executes them. Check
//!   [`Info::features`] for `render` before using this path.
//!
//! There is no window model yet: one connection owns the whole scanout.
//!
//! ```no_run
//! use quanta_gpu_client::{Connection, PixelRect};
//!
//! let mut conn = Connection::open().unwrap();
//! let (width, height) = (conn.width(), conn.height());
//! conn.pixels_mut().fill(0);
//! conn.present(&[PixelRect::new(0, 0, width, height)]).unwrap();
//! ```
//!
//! The surface is 32-bit `0xAARRGGBB` (`argb8888`), which is what
//! `ferritic-soft` scenes are converted to for Orbital as well, and what
//! virtio-gpu's `B8G8R8A8_UNORM` scanout expects in memory on little-endian
//! hosts.

#![cfg(target_os = "redox")]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::ptr;

use libredox::{call, flag};
/// Re-exported so applications need only depend on the client crate.
pub use quanta_gpu_protocol::{
    AtlasCmd, CursorCmd, DeviceInfo, Error as ProtocolError, Features, FloatRect, Format,
    FrameEncoder, FrameInfo, GlyphCmd, InfoError, InputEvent, PixelRect, RectCmd, Rgba8, ShadowCmd,
    INPUT_BUTTON, INPUT_EVENT_LEN, INPUT_KEY, INPUT_MOUSE,
};

/// Device description, shared with the driver through the protocol crate.
pub use quanta_gpu_protocol::DeviceInfo as Info;

const INFO_PATH: &str = "/scheme/quanta-gpu/info";
const FB_PATH: &str = "/scheme/quanta-gpu/fb";
const CTL_PATH: &str = "/scheme/quanta-gpu/ctl";
const INPUT_PATH: &str = "/scheme/quanta-gpu/input";

/// An open connection to the display service.
pub struct Connection {
    info: Info,
    // Kept open for the lifetime of the mapping.
    _fb: File,
    ctl: File,
    input: File,
    map: *mut u8,
    map_len: usize,
    seq: u64,
}

impl Connection {
    /// Opens the scheme and maps the scanout surface.
    ///
    /// This fails with `NotFound` until `quanta-gpud` has registered the
    /// scheme; callers started at boot should retry.
    pub fn open() -> io::Result<Self> {
        let info = read_info()?;
        let map_len = info
            .surface_len()
            .ok_or_else(|| invalid("surface size overflows usize"))?;

        let fb = OpenOptions::new().read(true).write(true).open(FB_PATH)?;
        // Fences are opportunistic: `read_fence` reports `WouldBlock` when the
        // queue is empty, which requires a non-blocking control file.  A
        // blocking `read` here would stall the caller as soon as it caught up
        // with the driver.
        let ctl = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(flag::O_NONBLOCK as i32)
            .open(CTL_PATH)?;
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(flag::O_NONBLOCK as i32)
            .open(INPUT_PATH)?;

        let map = unsafe {
            call::mmap(call::MmapArgs {
                addr: ptr::null_mut(),
                length: map_len,
                prot: flag::PROT_READ | flag::PROT_WRITE,
                flags: flag::MAP_SHARED,
                fd: fb.as_raw_fd() as usize,
                offset: 0,
            })
        }
        .map_err(io::Error::from)? as *mut u8;

        if map.is_null() {
            return Err(invalid("driver returned a null surface mapping"));
        }

        Ok(Self {
            info,
            _fb: fb,
            ctl,
            input,
            map,
            map_len,
            seq: 0,
        })
    }

    pub fn info(&self) -> Info {
        self.info
    }

    pub fn width(&self) -> u32 {
        self.info.width
    }

    pub fn height(&self) -> u32 {
        self.info.height
    }

    /// Bytes per row, including padding. Always address pixels with this
    /// stride, not `width * 4`.
    pub fn stride(&self) -> u32 {
        self.info.stride
    }

    pub fn scale(&self) -> f32 {
        self.info.scale
    }

    pub fn format(&self) -> Format {
        self.info.format
    }

    /// The mapped `stride * height` surface buffer.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        // SAFETY: `map` is a live MAP_SHARED mapping of `map_len` bytes for as
        // long as the Connection exists, and `&mut self` excludes other
        // borrows of the same mapping.
        unsafe { std::slice::from_raw_parts_mut(self.map, self.map_len) }
    }

    /// Submits the damage rectangles accumulated in [`pixels_mut`].
    ///
    /// The driver applies the frame atomically and flushes it to the scanout.
    pub fn present(&mut self, damage: &[PixelRect]) -> io::Result<u64> {
        self.frame()?.present(damage)
    }

    /// Presents the whole surface.
    pub fn present_full(&mut self) -> io::Result<u64> {
        let damage = [PixelRect::new(0, 0, self.info.width, self.info.height)];
        self.present(&damage)
    }

    /// Starts a protocol frame. Drawing methods change the shared back buffer
    /// only when the driver executes the frame; the client surface is not
    /// touched.
    ///
    /// ```
    /// # use quanta_gpu_client::{Connection, Rgba8, RectCmd, FloatRect, PixelRect};
    /// # fn demo(conn: &mut Connection) -> std::io::Result<()> {
    /// conn.frame()?
    ///     .clear(Rgba8::BLACK)
    ///     .rect(&RectCmd {
    ///         rect: FloatRect::new(10.0, 10.0, 100.0, 40.0),
    ///         radius: 8.0,
    ///         border_width: 0.0,
    ///         fill: Rgba8::WHITE,
    ///         border: Rgba8::TRANSPARENT,
    ///         clip: None,
    ///     })
    ///     .present(&[])?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn frame(&mut self) -> io::Result<Frame<'_>> {
        self.seq += 1;
        let info = FrameInfo {
            seq: self.seq,
            width: self.info.width,
            height: self.info.height,
            scale: self.info.scale,
        };
        let encoder = FrameEncoder::new(info).map_err(protocol_error)?;
        Ok(Frame {
            conn: self,
            encoder,
            error: None,
        })
    }

    /// Reads one completed-present fence, if one is queued.
    ///
    /// The driver acknowledges every successful present with the frame's
    /// `seq`, or with an explicit `Fence` token if the client sent one.
    pub fn read_fence(&mut self) -> io::Result<Option<u64>> {
        let mut bytes = [0u8; 8];
        match self.ctl.read(&mut bytes) {
            Ok(8) => Ok(Some(u64::from_le_bytes(bytes))),
            Ok(count) => Err(invalid(&format!("short fence read ({count} bytes)"))),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Drains pending keyboard events. Returns `Ok(0)` when the queue is empty;
    /// the input file is non-blocking, so callers should poll it.
    pub fn read_input(&mut self, out: &mut Vec<InputEvent>) -> io::Result<usize> {
        let mut buffer = [0u8; INPUT_EVENT_LEN * 16];
        match self.input.read(&mut buffer) {
            Ok(0) => Ok(0),
            Ok(count) => {
                let mut parsed = 0;
                for chunk in buffer[..count].chunks_exact(INPUT_EVENT_LEN) {
                    if let Ok(event) = InputEvent::from_bytes(chunk) {
                        out.push(event);
                        parsed += 1;
                    }
                }
                Ok(parsed)
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(err) => Err(err),
        }
    }
}

/// A frame being encoded for the driver.
///
/// Encoding errors are recorded and returned by [`Frame::present`]; this keeps
/// scene-drawing code free of `?` noise while still refusing to send a
/// partially valid frame.
pub struct Frame<'a> {
    conn: &'a mut Connection,
    encoder: FrameEncoder,
    error: Option<ProtocolError>,
}

impl Frame<'_> {
    fn record(&mut self, result: Result<(), ProtocolError>) -> &mut Self {
        if self.error.is_none() {
            if let Err(err) = result {
                self.error = Some(err);
            }
        }
        self
    }

    pub fn clear(&mut self, color: Rgba8) -> &mut Self {
        let result = self.encoder.clear(color).map(|_| ());
        self.record(result)
    }

    pub fn rect(&mut self, cmd: &RectCmd) -> &mut Self {
        let result = self.encoder.rect(cmd).map(|_| ());
        self.record(result)
    }

    pub fn shadow(&mut self, cmd: &ShadowCmd) -> &mut Self {
        let result = self.encoder.shadow(cmd).map(|_| ());
        self.record(result)
    }

    pub fn glyph(&mut self, cmd: &GlyphCmd) -> &mut Self {
        let result = self.encoder.glyph(cmd).map(|_| ());
        self.record(result)
    }

    pub fn atlas(&mut self, cmd: &AtlasCmd<'_>) -> &mut Self {
        let result = self.encoder.atlas(cmd).map(|_| ());
        self.record(result)
    }

    pub fn set_cursor(&mut self, cmd: &CursorCmd) -> &mut Self {
        let result = self.encoder.set_cursor(cmd).map(|_| ());
        self.record(result)
    }

    pub fn fence(&mut self, token: u64) -> &mut Self {
        let result = self.encoder.fence(token).map(|_| ());
        self.record(result)
    }

    /// Ends the frame and writes it to `/scheme/quanta-gpu/ctl`.
    ///
    /// Pass an empty damage list for frames that consist only of v1 draw
    /// commands; the driver computes damage from the executed primitives.
    pub fn present(self, damage: &[PixelRect]) -> io::Result<u64> {
        let Frame {
            conn,
            mut encoder,
            error,
        } = self;
        if let Some(err) = error {
            return Err(protocol_error(err));
        }
        encoder.present(damage).map_err(protocol_error)?;
        let seq = encoder.info().seq;
        let bytes = encoder.finish().map_err(protocol_error)?;
        conn.ctl.write_all(&bytes)?;
        Ok(seq)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            let _ = call::munmap(self.map.cast::<()>(), self.map_len);
        }
    }
}

fn read_info() -> io::Result<Info> {
    let text = fs::read_to_string(INFO_PATH)?;
    DeviceInfo::parse(&text).map_err(info_error)
}

fn info_error(err: InfoError) -> io::Error {
    invalid(&format!("invalid device info: {err}"))
}

fn protocol_error(err: quanta_gpu_protocol::Error) -> io::Error {
    invalid(&format!("could not encode frame: {err}"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}
