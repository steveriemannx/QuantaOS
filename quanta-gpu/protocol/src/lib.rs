//! QuantaGPU wire protocol.
//!
//! This crate is the contract between `ferritic-quanta` (a FerriticUI backend)
//! and `quanta-gpud` (the Redox GPU/display driver service). It is deliberately
//! small, dependency-free and FFI-free: no Vulkan, no `libc`, no `bindgen`.
//!
//! # Layers
//!
//! * **Presentation (v0).** A client renders into a driver-provided RGBA8
//!   surface and submits a damage list. This runs on virtio-gpu 2D hardware,
//!   whose command set has no draw operations.
//! * **Rendering (v1).** The same frame may carry primitives
//!   ([`RectCmd`], [`ShadowCmd`], [`GlyphCmd`]) that a GPU executor can render
//!   with a small shader family. The driver chooses where to execute them.
//!
//! # Encoding
//!
//! All integers are little-endian. A frame is a [`HEADER_LEN`]-byte header
//! followed by 4-byte-aligned records. Each record is
//!
//! ```text
//! op    u16
//! flags u16   (must be zero for v0)
//! len   u32   (payload length in bytes)
//! data  [u8; len]
//! pad   [u8; (4 - len % 4) % 4]
//! ```
//!
//! Unknown operations are skipped, so the protocol can grow without bumping
//! the version. A frame must contain exactly one [`OP_PRESENT`] and it must be
//! the last record; frames are applied atomically.
//!
//! # Safety
//!
//! Decoding never panics on untrusted input. All reads are bounds-checked,
//! geometry must be finite, and payload sizes are capped.

#![forbid(unsafe_code)]

use core::fmt;

/// Magic at the start of every frame: ASCII `QGPU`.
pub const MAGIC: [u8; 4] = *b"QGPU";
/// Protocol version implemented by this crate.
pub const VERSION: u16 = 0;
/// Size of [`FrameInfo`]'s wire header, including magic and command length.
pub const HEADER_LEN: usize = 40;
/// Upper bound on a complete frame, including the header.
pub const MAX_FRAME_LEN: usize = 32 * 1024 * 1024;
/// Upper bound on one damage list.
pub const MAX_DAMAGE: usize = 256;
/// Upper bound on one atlas upload.
pub const MAX_ATLAS_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on one raw backend submission.
pub const MAX_SUBMIT_BYTES: usize = 16 * 1024 * 1024;

pub const OP_PRESENT: u16 = 1;
pub const OP_FENCE: u16 = 2;
pub const OP_SET_CURSOR: u16 = 3;
pub const OP_CLEAR: u16 = 10;
pub const OP_RECT: u16 = 11;
pub const OP_SHADOW: u16 = 12;
pub const OP_GLYPH: u16 = 13;
pub const OP_ATLAS: u16 = 14;
pub const OP_SUBMIT: u16 = 15;

/// A straight-alpha sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self::new(0, 0, 0, 0);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);
    pub const WHITE: Self = Self::new(255, 255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

/// Canvas metadata shared by every operation in a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameInfo {
    pub seq: u64,
    /// Physical pixels.
    pub width: u32,
    /// Physical pixels.
    pub height: u32,
    /// Logical-to-physical scale used by SDF anti-aliasing.
    pub scale: f32,
}

impl FrameInfo {
    fn validate(&self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidGeometry);
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return Err(Error::NonFinite);
        }
        Ok(())
    }
}

/// A rectangle in physical pixels, used for damage and atlas placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A rectangle in the scene's coordinate space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloatRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl FloatRect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn validate(&self) -> Result<(), Error> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
        {
            return Err(Error::NonFinite);
        }
        if self.width < 0.0 || self.height < 0.0 {
            return Err(Error::InvalidGeometry);
        }
        Ok(())
    }
}

/// An SDF rounded rectangle with an optional inside border and scissor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectCmd {
    pub rect: FloatRect,
    pub radius: f32,
    pub border_width: f32,
    pub fill: Rgba8,
    pub border: Rgba8,
    pub clip: Option<FloatRect>,
}

/// A soft shadow cast by a rounded rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowCmd {
    pub rect: FloatRect,
    pub radius: f32,
    pub blur: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub color: Rgba8,
    pub clip: Option<FloatRect>,
}

/// A glyph from the A8 atlas, drawn as a colored textured quad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphCmd {
    pub rect: FloatRect,
    /// Normalized atlas coordinates: `u0, v0, u1, v1`.
    pub uv: [f32; 4],
    pub color: Rgba8,
    pub clip: Option<FloatRect>,
}

/// An A8 atlas update. `data` is row-major, `width * height` bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasCmd<'a> {
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub rect: PixelRect,
    pub data: &'a [u8],
}

/// Cursor placement. Drivers may ignore this until cursors are implemented.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CursorCmd {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

/// Errors returned while encoding or decoding a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    UnsupportedVersion(u16),
    BadHeader,
    Truncated,
    FrameTooLarge { len: usize },
    NonFinite,
    InvalidGeometry,
    BadPayload { op: u16 },
    MissingPresent,
    DuplicatePresent,
    PresentNotLast,
    TooManyDamage { count: usize },
    BadAtlas,
    TooLarge { what: &'static str, len: usize },
    TrailingBytes,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadMagic => write!(f, "not a QGPU frame"),
            Error::UnsupportedVersion(version) => {
                write!(f, "unsupported QGPU version {version}")
            }
            Error::BadHeader => write!(f, "malformed QGPU header"),
            Error::Truncated => write!(f, "truncated QGPU frame"),
            Error::FrameTooLarge { len } => write!(f, "QGPU frame too large ({len} bytes)"),
            Error::NonFinite => write!(f, "non-finite geometry value"),
            Error::InvalidGeometry => write!(f, "invalid geometry"),
            Error::BadPayload { op } => write!(f, "malformed payload for opcode {op}"),
            Error::MissingPresent => write!(f, "frame has no Present"),
            Error::DuplicatePresent => write!(f, "frame has more than one Present"),
            Error::PresentNotLast => write!(f, "Present must be the last record"),
            Error::TooManyDamage { count } => write!(f, "too many damage rectangles ({count})"),
            Error::BadAtlas => write!(f, "invalid atlas update"),
            Error::TooLarge { what, len } => write!(f, "{what} too large ({len} bytes)"),
            Error::TrailingBytes => write!(f, "trailing bytes after Present"),
        }
    }
}

impl std::error::Error for Error {}

/// Receives the operations of a decoded frame in order.
///
/// The default methods ignore an operation, which lets a 2D-only driver accept
/// frames that contain drawing operations.
pub trait FrameSink {
    fn clear(&mut self, color: Rgba8) {
        let _ = color;
    }
    fn rect(&mut self, cmd: &RectCmd) {
        let _ = cmd;
    }
    fn shadow(&mut self, cmd: &ShadowCmd) {
        let _ = cmd;
    }
    fn glyph(&mut self, cmd: &GlyphCmd) {
        let _ = cmd;
    }
    fn atlas(&mut self, cmd: &AtlasCmd<'_>) {
        let _ = cmd;
    }
    fn submit(&mut self, backend: u32, data: &[u8]) {
        let _ = (backend, data);
    }
    fn set_cursor(&mut self, cmd: &CursorCmd) {
        let _ = cmd;
    }
    fn fence(&mut self, token: u64) {
        let _ = token;
    }
    fn present(&mut self, damage: &[PixelRect]) {
        let _ = damage;
    }
}

/// Builds a frame. Every mutating method returns `&mut Self` for chaining.
#[derive(Debug)]
pub struct FrameEncoder {
    info: FrameInfo,
    buf: Vec<u8>,
    present: bool,
}

impl FrameEncoder {
    /// Starts a frame. The 40-byte header is reserved and filled in by
    /// [`finish`](Self::finish).
    pub fn new(info: FrameInfo) -> Result<Self, Error> {
        info.validate()?;
        let mut buf = vec![0; HEADER_LEN];
        buf[..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&VERSION.to_le_bytes());
        buf[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        buf[8..16].copy_from_slice(&info.seq.to_le_bytes());
        buf[16..20].copy_from_slice(&info.width.to_le_bytes());
        buf[20..24].copy_from_slice(&info.height.to_le_bytes());
        buf[24..28].copy_from_slice(&info.scale.to_bits().to_le_bytes());
        Ok(Self {
            info,
            buf,
            present: false,
        })
    }

    pub fn info(&self) -> &FrameInfo {
        &self.info
    }

    pub fn clear(&mut self, color: Rgba8) -> Result<&mut Self, Error> {
        let mut payload = [0u8; 4];
        write_rgba(&mut payload, color);
        self.push(OP_CLEAR, &payload)
    }

    pub fn rect(&mut self, cmd: &RectCmd) -> Result<&mut Self, Error> {
        cmd.rect.validate()?;
        if !finite_non_negative(cmd.radius) || !finite_non_negative(cmd.border_width) {
            return Err(Error::InvalidGeometry);
        }
        validate_clip(cmd.clip)?;

        let mut payload = [0u8; 52];
        write_float_rect(&mut payload[0..16], cmd.rect);
        payload[16..20].copy_from_slice(&cmd.radius.to_bits().to_le_bytes());
        payload[20..24].copy_from_slice(&cmd.border_width.to_bits().to_le_bytes());
        write_rgba(&mut payload[24..28], cmd.fill);
        write_rgba(&mut payload[28..32], cmd.border);
        write_clip(&mut payload[32..52], cmd.clip)?;
        self.push(OP_RECT, &payload)
    }

    pub fn shadow(&mut self, cmd: &ShadowCmd) -> Result<&mut Self, Error> {
        cmd.rect.validate()?;
        if !finite_non_negative(cmd.radius) || !finite_non_negative(cmd.blur) {
            return Err(Error::InvalidGeometry);
        }
        if !cmd.offset_x.is_finite() || !cmd.offset_y.is_finite() {
            return Err(Error::NonFinite);
        }
        validate_clip(cmd.clip)?;

        let mut payload = [0u8; 56];
        write_float_rect(&mut payload[0..16], cmd.rect);
        payload[16..20].copy_from_slice(&cmd.radius.to_bits().to_le_bytes());
        payload[20..24].copy_from_slice(&cmd.blur.to_bits().to_le_bytes());
        payload[24..28].copy_from_slice(&cmd.offset_x.to_bits().to_le_bytes());
        payload[28..32].copy_from_slice(&cmd.offset_y.to_bits().to_le_bytes());
        write_rgba(&mut payload[32..36], cmd.color);
        write_clip(&mut payload[36..56], cmd.clip)?;
        self.push(OP_SHADOW, &payload)
    }

    pub fn glyph(&mut self, cmd: &GlyphCmd) -> Result<&mut Self, Error> {
        cmd.rect.validate()?;
        if cmd.uv.iter().any(|value| !value.is_finite()) {
            return Err(Error::NonFinite);
        }
        validate_clip(cmd.clip)?;

        let mut payload = [0u8; 56];
        write_float_rect(&mut payload[0..16], cmd.rect);
        for (index, value) in cmd.uv.iter().enumerate() {
            let offset = 16 + index * 4;
            payload[offset..offset + 4].copy_from_slice(&value.to_bits().to_le_bytes());
        }
        write_rgba(&mut payload[32..36], cmd.color);
        write_clip(&mut payload[36..56], cmd.clip)?;
        self.push(OP_GLYPH, &payload)
    }

    pub fn atlas(&mut self, cmd: &AtlasCmd<'_>) -> Result<&mut Self, Error> {
        if cmd.atlas_width == 0
            || cmd.atlas_height == 0
            || cmd.rect.width == 0
            || cmd.rect.height == 0
        {
            return Err(Error::BadAtlas);
        }
        let right = cmd
            .rect
            .x
            .checked_add(cmd.rect.width)
            .ok_or(Error::BadAtlas)?;
        let bottom = cmd
            .rect
            .y
            .checked_add(cmd.rect.height)
            .ok_or(Error::BadAtlas)?;
        if right > cmd.atlas_width || bottom > cmd.atlas_height {
            return Err(Error::BadAtlas);
        }
        let expected = (cmd.rect.width as usize)
            .checked_mul(cmd.rect.height as usize)
            .ok_or(Error::BadAtlas)?;
        if cmd.data.len() != expected {
            return Err(Error::BadAtlas);
        }
        if cmd.data.len() > MAX_ATLAS_BYTES {
            return Err(Error::TooLarge {
                what: "atlas update",
                len: cmd.data.len(),
            });
        }

        let mut payload = Vec::with_capacity(28 + cmd.data.len());
        for value in [
            cmd.atlas_width,
            cmd.atlas_height,
            cmd.rect.x,
            cmd.rect.y,
            cmd.rect.width,
            cmd.rect.height,
            cmd.data.len() as u32,
        ] {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        payload.extend_from_slice(cmd.data);
        self.push(OP_ATLAS, &payload)
    }

    pub fn submit(&mut self, backend: u32, data: &[u8]) -> Result<&mut Self, Error> {
        if data.len() > MAX_SUBMIT_BYTES {
            return Err(Error::TooLarge {
                what: "backend submission",
                len: data.len(),
            });
        }
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&backend.to_le_bytes());
        payload.extend_from_slice(data);
        self.push(OP_SUBMIT, &payload)
    }

    pub fn set_cursor(&mut self, cmd: &CursorCmd) -> Result<&mut Self, Error> {
        let mut payload = [0u8; 12];
        payload[0..4].copy_from_slice(&cmd.x.to_le_bytes());
        payload[4..8].copy_from_slice(&cmd.y.to_le_bytes());
        payload[8] = u8::from(cmd.visible);
        self.push(OP_SET_CURSOR, &payload)
    }

    /// Marks the end of a frame. Present must be the last record.
    pub fn present(&mut self, damage: &[PixelRect]) -> Result<&mut Self, Error> {
        if damage.len() > MAX_DAMAGE {
            return Err(Error::TooManyDamage {
                count: damage.len(),
            });
        }
        let mut payload = Vec::with_capacity(4 + damage.len() * 16);
        payload.extend_from_slice(&(damage.len() as u32).to_le_bytes());
        for rect in damage {
            for value in [rect.x, rect.y, rect.width, rect.height] {
                payload.extend_from_slice(&value.to_le_bytes());
            }
        }
        self.push(OP_PRESENT, &payload)?;
        self.present = true;
        Ok(self)
    }

    /// A client-requested fence value, echoed by the driver in its event queue.
    pub fn fence(&mut self, token: u64) -> Result<&mut Self, Error> {
        self.push(OP_FENCE, &token.to_le_bytes())
    }

    /// Finishes the frame and returns its bytes.
    pub fn finish(mut self) -> Result<Vec<u8>, Error> {
        if !self.present {
            return Err(Error::MissingPresent);
        }
        if self.buf.len() > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge { len: self.buf.len() });
        }
        let command_len = (self.buf.len() - HEADER_LEN) as u32;
        self.buf[28..32].copy_from_slice(&command_len.to_le_bytes());
        // bytes 32..36 = flags, 36..40 = reserved; both already zero
        Ok(self.buf)
    }

    fn push(&mut self, op: u16, payload: &[u8]) -> Result<&mut Self, Error> {
        if self.present {
            return Err(Error::PresentNotLast);
        }
        if payload.len() > u32::MAX as usize {
            return Err(Error::TooLarge {
                what: "operation",
                len: payload.len(),
            });
        }
        let record_len = 8 + payload.len();
        if self.buf.len() + record_len + 3 > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge {
                len: self.buf.len() + record_len,
            });
        }
        self.buf.extend_from_slice(&op.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.buf
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(payload);
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
        Ok(self)
    }
}

/// Decodes a frame, calling `sink` for every operation in order.
///
/// The frame is validated completely before any operation is handed to the
/// sink. This is important for drivers: a malformed frame must not leave the
/// scanout half-updated (callers can also validate into a null sink first).
pub fn decode_frame<F: FrameSink>(bytes: &[u8], sink: &mut F) -> Result<FrameInfo, Error> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    if bytes.len() > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge { len: bytes.len() });
    }
    if bytes[0..4] != MAGIC {
        return Err(Error::BadMagic);
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    if version != VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    let header_len = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    if header_len as usize != HEADER_LEN {
        return Err(Error::BadHeader);
    }
    let info = FrameInfo {
        seq: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        width: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        height: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        scale: f32::from_bits(u32::from_le_bytes(bytes[24..28].try_into().unwrap())),
    };
    info.validate()?;
    let command_len = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
    let flags = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
    if flags != 0 {
        return Err(Error::BadHeader);
    }
    let commands = &bytes[HEADER_LEN..];
    if command_len > commands.len() {
        return Err(Error::Truncated);
    }
    if command_len != commands.len() {
        return Err(Error::BadHeader);
    }

    // Decode once into a validating sink, then replay to the real sink. This
    // keeps the "atomic frame" rule for callers that cannot roll back.
    let mut validate = ValidatingSink::default();
    let inner = decode_records(commands, &mut validate)?;
    if !inner.present {
        return Err(Error::MissingPresent);
    }

    let mut replay = ReplaySink {
        ops: &validate.ops,
        sink,
    };
    replay.run()?;
    Ok(info)
}

#[derive(Default)]
struct ValidatingSink {
    ops: Vec<RecordedOp>,
}

#[derive(Debug)]
enum RecordedOp {
    Clear(Rgba8),
    Rect(RectCmd),
    Shadow(ShadowCmd),
    Glyph(GlyphCmd),
    Atlas {
        atlas_width: u32,
        atlas_height: u32,
        rect: PixelRect,
        data: Vec<u8>,
    },
    Submit {
        backend: u32,
        data: Vec<u8>,
    },
    Cursor(CursorCmd),
    Fence(u64),
    Present(Vec<PixelRect>),
}

impl FrameSink for ValidatingSink {
    fn clear(&mut self, color: Rgba8) {
        self.ops.push(RecordedOp::Clear(color));
    }
    fn rect(&mut self, cmd: &RectCmd) {
        self.ops.push(RecordedOp::Rect(*cmd));
    }
    fn shadow(&mut self, cmd: &ShadowCmd) {
        self.ops.push(RecordedOp::Shadow(*cmd));
    }
    fn glyph(&mut self, cmd: &GlyphCmd) {
        self.ops.push(RecordedOp::Glyph(*cmd));
    }
    fn atlas(&mut self, cmd: &AtlasCmd<'_>) {
        self.ops.push(RecordedOp::Atlas {
            atlas_width: cmd.atlas_width,
            atlas_height: cmd.atlas_height,
            rect: cmd.rect,
            data: cmd.data.to_vec(),
        });
    }
    fn submit(&mut self, backend: u32, data: &[u8]) {
        self.ops.push(RecordedOp::Submit {
            backend,
            data: data.to_vec(),
        });
    }
    fn set_cursor(&mut self, cmd: &CursorCmd) {
        self.ops.push(RecordedOp::Cursor(*cmd));
    }
    fn fence(&mut self, token: u64) {
        self.ops.push(RecordedOp::Fence(token));
    }
    fn present(&mut self, damage: &[PixelRect]) {
        self.ops.push(RecordedOp::Present(damage.to_vec()));
    }
}

struct ReplaySink<'a, F: FrameSink> {
    ops: &'a [RecordedOp],
    sink: &'a mut F,
}

impl<F: FrameSink> ReplaySink<'_, F> {
    fn run(&mut self) -> Result<(), Error> {
        for op in self.ops {
            match op {
                RecordedOp::Clear(color) => self.sink.clear(*color),
                RecordedOp::Rect(cmd) => self.sink.rect(cmd),
                RecordedOp::Shadow(cmd) => self.sink.shadow(cmd),
                RecordedOp::Glyph(cmd) => self.sink.glyph(cmd),
                RecordedOp::Atlas {
                    atlas_width,
                    atlas_height,
                    rect,
                    data,
                } => self.sink.atlas(&AtlasCmd {
                    atlas_width: *atlas_width,
                    atlas_height: *atlas_height,
                    rect: *rect,
                    data,
                }),
                RecordedOp::Submit { backend, data } => self.sink.submit(*backend, data),
                RecordedOp::Cursor(cmd) => self.sink.set_cursor(cmd),
                RecordedOp::Fence(token) => self.sink.fence(*token),
                RecordedOp::Present(damage) => self.sink.present(damage),
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct DecodeState {
    present: bool,
}

fn decode_records<F: FrameSink>(commands: &[u8], sink: &mut F) -> Result<DecodeState, Error> {
    let mut state = DecodeState::default();
    let mut offset = 0usize;
    while offset < commands.len() {
        if state.present {
            return Err(Error::TrailingBytes);
        }
        if commands.len() - offset < 8 {
            return Err(Error::Truncated);
        }
        let op = u16::from_le_bytes(commands[offset..offset + 2].try_into().unwrap());
        let flags = u16::from_le_bytes(commands[offset + 2..offset + 4].try_into().unwrap());
        if flags != 0 {
            return Err(Error::BadHeader);
        }
        let len = u32::from_le_bytes(commands[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset += 8;
        let Some(payload) = commands.get(offset..offset + len) else {
            return Err(Error::Truncated);
        };
        offset += len;
        let pad = (4 - (len % 4)) % 4;
        if offset + pad > commands.len() {
            return Err(Error::Truncated);
        }
        // Padding bytes are ignored (forward compatibility).
        offset += pad;

        match op {
            OP_PRESENT => {
                if state.present {
                    return Err(Error::DuplicatePresent);
                }
                if len < 4 {
                    return Err(Error::BadPayload { op });
                }
                let count = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                if count > MAX_DAMAGE {
                    return Err(Error::TooManyDamage { count });
                }
                if len != 4 + count * 16 {
                    return Err(Error::BadPayload { op });
                }
                let mut damage = Vec::with_capacity(count);
                for index in 0..count {
                    let base = 4 + index * 16;
                    let field = |at: usize| {
                        u32::from_le_bytes(payload[base + at..base + at + 4].try_into().unwrap())
                    };
                    damage.push(PixelRect::new(field(0), field(4), field(8), field(12)));
                }
                sink.present(&damage);
                state.present = true;
            }
            OP_FENCE => {
                if len != 8 {
                    return Err(Error::BadPayload { op });
                }
                sink.fence(u64::from_le_bytes(payload[0..8].try_into().unwrap()));
            }
            OP_SET_CURSOR => {
                if len != 12 {
                    return Err(Error::BadPayload { op });
                }
                if payload[9] != 0 || payload[10] != 0 || payload[11] != 0 {
                    return Err(Error::BadPayload { op });
                }
                sink.set_cursor(&CursorCmd {
                    x: i32::from_le_bytes(payload[0..4].try_into().unwrap()),
                    y: i32::from_le_bytes(payload[4..8].try_into().unwrap()),
                    visible: match payload[8] {
                        0 => false,
                        1 => true,
                        _ => return Err(Error::BadPayload { op }),
                    },
                });
            }
            OP_CLEAR => {
                if len != 4 {
                    return Err(Error::BadPayload { op });
                }
                sink.clear(read_rgba(payload));
            }
            OP_RECT => {
                if len != 52 {
                    return Err(Error::BadPayload { op });
                }
                let rect = read_float_rect(&payload[0..16])?;
                let radius = read_f32(&payload[16..20])?;
                let border_width = read_f32(&payload[20..24])?;
                if !finite_non_negative(radius) || !finite_non_negative(border_width) {
                    return Err(Error::InvalidGeometry);
                }
                let cmd = RectCmd {
                    rect,
                    radius,
                    border_width,
                    fill: read_rgba(&payload[24..28]),
                    border: read_rgba(&payload[28..32]),
                    clip: read_clip(&payload[32..52])?,
                };
                cmd.rect.validate()?;
                validate_clip(cmd.clip)?;
                sink.rect(&cmd);
            }
            OP_SHADOW => {
                if len != 56 {
                    return Err(Error::BadPayload { op });
                }
                let cmd = ShadowCmd {
                    rect: read_float_rect(&payload[0..16])?,
                    radius: read_f32(&payload[16..20])?,
                    blur: read_f32(&payload[20..24])?,
                    offset_x: read_f32(&payload[24..28])?,
                    offset_y: read_f32(&payload[28..32])?,
                    color: read_rgba(&payload[32..36]),
                    clip: read_clip(&payload[36..56])?,
                };
                cmd.rect.validate()?;
                if !finite_non_negative(cmd.radius) || !finite_non_negative(cmd.blur) {
                    return Err(Error::InvalidGeometry);
                }
                validate_clip(cmd.clip)?;
                sink.shadow(&cmd);
            }
            OP_GLYPH => {
                if len != 56 {
                    return Err(Error::BadPayload { op });
                }
                let uv = [
                    read_f32(&payload[16..20])?,
                    read_f32(&payload[20..24])?,
                    read_f32(&payload[24..28])?,
                    read_f32(&payload[28..32])?,
                ];
                let cmd = GlyphCmd {
                    rect: read_float_rect(&payload[0..16])?,
                    uv,
                    color: read_rgba(&payload[32..36]),
                    clip: read_clip(&payload[36..56])?,
                };
                cmd.rect.validate()?;
                validate_clip(cmd.clip)?;
                sink.glyph(&cmd);
            }
            OP_ATLAS => {
                if len < 28 {
                    return Err(Error::BadPayload { op });
                }
                let field = |at: usize| {
                    u32::from_le_bytes(payload[at..at + 4].try_into().unwrap())
                };
                let atlas_width = field(0);
                let atlas_height = field(4);
                let rect = PixelRect::new(field(8), field(12), field(16), field(20));
                let data_len = field(24) as usize;
                if data_len != len - 28 {
                    return Err(Error::BadPayload { op });
                }
                if data_len > MAX_ATLAS_BYTES {
                    return Err(Error::TooLarge {
                        what: "atlas update",
                        len: data_len,
                    });
                }
                let data = &payload[28..];
                if atlas_width == 0
                    || atlas_height == 0
                    || rect.width == 0
                    || rect.height == 0
                    || rect.x.checked_add(rect.width).is_none_or(|v| v > atlas_width)
                    || rect.y.checked_add(rect.height).is_none_or(|v| v > atlas_height)
                    || (rect.width as usize).checked_mul(rect.height as usize) != Some(data_len)
                {
                    return Err(Error::BadAtlas);
                }
                sink.atlas(&AtlasCmd {
                    atlas_width,
                    atlas_height,
                    rect,
                    data,
                });
            }
            OP_SUBMIT => {
                if len < 4 {
                    return Err(Error::BadPayload { op });
                }
                let backend = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let data = &payload[4..];
                if data.len() > MAX_SUBMIT_BYTES {
                    return Err(Error::TooLarge {
                        what: "backend submission",
                        len: data.len(),
                    });
                }
                sink.submit(backend, data);
            }
            _ => {
                // Forward compatibility: unknown records are skipped.
            }
        }
    }
    Ok(state)
}

fn finite_non_negative(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

fn validate_clip(clip: Option<FloatRect>) -> Result<(), Error> {
    if let Some(clip) = clip {
        clip.validate()?;
    }
    Ok(())
}

fn write_rgba(out: &mut [u8], color: Rgba8) {
    out[..4].copy_from_slice(&[color.r, color.g, color.b, color.a]);
}

fn read_rgba(input: &[u8]) -> Rgba8 {
    Rgba8::new(input[0], input[1], input[2], input[3])
}

fn write_float_rect(out: &mut [u8], rect: FloatRect) {
    for (index, value) in [rect.x, rect.y, rect.width, rect.height]
        .iter()
        .enumerate()
    {
        out[index * 4..index * 4 + 4].copy_from_slice(&value.to_bits().to_le_bytes());
    }
}

fn read_float_rect(input: &[u8]) -> Result<FloatRect, Error> {
    let rect = FloatRect::new(
        read_f32(&input[0..4])?,
        read_f32(&input[4..8])?,
        read_f32(&input[8..12])?,
        read_f32(&input[12..16])?,
    );
    rect.validate()?;
    Ok(rect)
}

fn write_clip(out: &mut [u8], clip: Option<FloatRect>) -> Result<(), Error> {
    match clip {
        Some(rect) => {
            out[0] = 1;
            write_float_rect(&mut out[4..20], rect);
        }
        None => {
            out[0] = 0;
        }
    }
    Ok(())
}

fn read_clip(input: &[u8]) -> Result<Option<FloatRect>, Error> {
    match input[0] {
        0 => Ok(None),
        1 => Ok(Some(read_float_rect(&input[4..20])?)),
        _ => Err(Error::BadPayload { op: OP_RECT }),
    }
}

fn read_f32(input: &[u8]) -> Result<f32, Error> {
    let value = f32::from_bits(u32::from_le_bytes(input.try_into().unwrap()));
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Error::NonFinite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Sink {
        ops: Vec<String>,
        damage: Vec<PixelRect>,
    }

    impl FrameSink for Sink {
        fn clear(&mut self, color: Rgba8) {
            self.ops.push(format!("clear {color:?}"));
        }
        fn rect(&mut self, cmd: &RectCmd) {
            self.ops.push(format!("rect {:?}", cmd.rect));
        }
        fn shadow(&mut self, cmd: &ShadowCmd) {
            self.ops.push(format!("shadow {:?}", cmd.rect));
        }
        fn glyph(&mut self, cmd: &GlyphCmd) {
            self.ops.push(format!("glyph {:?}", cmd.rect));
        }
        fn atlas(&mut self, cmd: &AtlasCmd<'_>) {
            self.ops
                .push(format!("atlas {}x{} data={}", cmd.rect.width, cmd.rect.height, cmd.data.len()));
        }
        fn submit(&mut self, backend: u32, data: &[u8]) {
            self.ops.push(format!("submit {backend} data={}", data.len()));
        }
        fn set_cursor(&mut self, cmd: &CursorCmd) {
            self.ops.push(format!("cursor {cmd:?}"));
        }
        fn fence(&mut self, token: u64) {
            self.ops.push(format!("fence {token}"));
        }
        fn present(&mut self, damage: &[PixelRect]) {
            self.damage = damage.to_vec();
            self.ops.push(format!("present {}", damage.len()));
        }
    }

    fn info() -> FrameInfo {
        FrameInfo {
            seq: 7,
            width: 800,
            height: 600,
            scale: 1.5,
        }
    }

    fn sample_rect() -> RectCmd {
        RectCmd {
            rect: FloatRect::new(10.0, 20.0, 100.0, 40.0),
            radius: 8.0,
            border_width: 2.0,
            fill: Rgba8::new(1, 2, 3, 4),
            border: Rgba8::new(5, 6, 7, 8),
            clip: Some(FloatRect::new(0.0, 0.0, 640.0, 480.0)),
        }
    }

    fn sample_shadow() -> ShadowCmd {
        ShadowCmd {
            rect: FloatRect::new(5.0, 6.0, 50.0, 20.0),
            radius: 4.0,
            blur: 12.0,
            offset_x: 2.0,
            offset_y: 3.0,
            color: Rgba8::new(9, 10, 11, 12),
            clip: None,
        }
    }

    fn sample_glyph() -> GlyphCmd {
        GlyphCmd {
            rect: FloatRect::new(1.0, 2.0, 8.0, 12.0),
            uv: [0.25, 0.5, 0.5, 0.75],
            color: Rgba8::WHITE,
            clip: None,
        }
    }

    #[test]
    fn round_trip_all_ops() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder
            .clear(Rgba8::BLACK)
            .unwrap()
            .atlas(&AtlasCmd {
                atlas_width: 4,
                atlas_height: 2,
                rect: PixelRect::new(1, 0, 2, 2),
                data: &[1, 2, 3, 4],
            })
            .unwrap()
            .rect(&sample_rect())
            .unwrap()
            .shadow(&sample_shadow())
            .unwrap()
            .glyph(&sample_glyph())
            .unwrap()
            .submit(7, &[9, 8, 7])
            .unwrap()
            .set_cursor(&CursorCmd {
                x: -3,
                y: 4,
                visible: true,
            })
            .unwrap()
            .fence(0xfeed)
            .unwrap()
            .present(&[PixelRect::new(0, 0, 800, 600)])
            .unwrap();
        let bytes = encoder.finish().unwrap();

        // Header.
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), VERSION);
        assert_eq!(
            u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as usize,
            HEADER_LEN
        );
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 800);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 600);
        assert_eq!(
            f32::from_bits(u32::from_le_bytes(bytes[24..28].try_into().unwrap())),
            1.5
        );
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize,
            bytes.len() - HEADER_LEN
        );

        let mut sink = Sink::default();
        let decoded = decode_frame(&bytes, &mut sink).unwrap();
        assert_eq!(decoded, info());
        assert_eq!(sink.damage, [PixelRect::new(0, 0, 800, 600)]);
        assert_eq!(
            sink.ops,
            [
                format!("clear {:?}", Rgba8::BLACK),
                "atlas 2x2 data=4".to_string(),
                format!("rect {:?}", sample_rect().rect),
                format!("shadow {:?}", sample_shadow().rect),
                format!("glyph {:?}", sample_glyph().rect),
                "submit 7 data=3".to_string(),
                format!(
                    "cursor {:?}",
                    CursorCmd {
                        x: -3,
                        y: 4,
                        visible: true
                    }
                ),
                "fence 65261".to_string(),
                "present 1".to_string(),
            ]
        );
    }

    #[test]
    fn present_is_required() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.clear(Rgba8::BLACK).unwrap();
        assert_eq!(encoder.finish(), Err(Error::MissingPresent));
    }

    #[test]
    fn records_after_present_are_rejected() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.present(&[]).unwrap();
        assert!(matches!(
            encoder.rect(&sample_rect()),
            Err(Error::PresentNotLast)
        ));
    }

    #[test]
    fn duplicate_present_cannot_be_encoded() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.present(&[]).unwrap();
        assert!(matches!(
            encoder.present(&[]),
            Err(Error::PresentNotLast)
        ));
    }

    #[test]
    fn malformed_inputs_are_rejected() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.clear(Rgba8::BLACK).unwrap().present(&[]).unwrap();
        let bytes = encoder.finish().unwrap();

        let mut sink = Sink::default();
        assert_eq!(decode_frame(&[], &mut sink), Err(Error::Truncated));

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert_eq!(decode_frame(&bad_magic, &mut sink), Err(Error::BadMagic));

        let mut bad_version = bytes.clone();
        bad_version[4..6].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(
            decode_frame(&bad_version, &mut sink),
            Err(Error::UnsupportedVersion(9))
        );

        let mut too_long = bytes.clone();
        too_long[28..32].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(decode_frame(&too_long, &mut sink), Err(Error::Truncated));

        let mut trailing = bytes.clone();
        trailing[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_frame(&trailing, &mut sink), Err(Error::BadHeader));

        let truncated = &bytes[..bytes.len() - 1];
        assert_eq!(decode_frame(truncated, &mut sink), Err(Error::Truncated));

        let mut zero_canvas = bytes.clone();
        zero_canvas[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            decode_frame(&zero_canvas, &mut sink),
            Err(Error::InvalidGeometry)
        );
    }

    #[test]
    fn nan_geometry_is_rejected() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        let mut cmd = sample_rect();
        cmd.rect.width = f32::NAN;
        assert!(matches!(encoder.rect(&cmd), Err(Error::NonFinite)));

        let mut bad_info = info();
        bad_info.scale = f32::INFINITY;
        assert_eq!(FrameEncoder::new(bad_info).unwrap_err(), Error::NonFinite);
    }

    #[test]
    fn unknown_opcode_is_skipped() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.present(&[]).unwrap();
        let mut bytes = encoder.finish().unwrap();
        let command_len = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
        // Insert an unknown op before Present: rebuild the frame by hand.
        let present = bytes.split_off(bytes.len() - command_len);
        bytes.extend_from_slice(&999u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        bytes.extend_from_slice(&present);
        let new_len = (bytes.len() - HEADER_LEN) as u32;
        bytes[28..32].copy_from_slice(&new_len.to_le_bytes());

        let mut sink = Sink::default();
        decode_frame(&bytes, &mut sink).unwrap();
        assert_eq!(sink.ops, ["present 0"]);
    }

    #[test]
    fn too_many_damage_rectangles_are_rejected() {
        let damage = vec![PixelRect::new(0, 0, 1, 1); MAX_DAMAGE + 1];
        let mut encoder = FrameEncoder::new(info()).unwrap();
        assert!(matches!(
            encoder.present(&damage),
            Err(Error::TooManyDamage { count }) if count == MAX_DAMAGE + 1
        ));
    }

    #[test]
    fn atlas_bounds_are_checked() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        let mut cmd = AtlasCmd {
            atlas_width: 4,
            atlas_height: 4,
            rect: PixelRect::new(3, 0, 2, 2),
            data: &[0; 4],
        };
        assert!(matches!(encoder.atlas(&cmd), Err(Error::BadAtlas)));
        // In bounds, but the data length does not match width * height.
        cmd.rect = PixelRect::new(0, 0, 2, 2);
        cmd.data = &[0; 3];
        assert!(matches!(encoder.atlas(&cmd), Err(Error::BadAtlas)));
    }

    #[test]
    fn valid_atlas_is_accepted() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder
            .atlas(&AtlasCmd {
                atlas_width: 4,
                atlas_height: 4,
                rect: PixelRect::new(2, 2, 2, 2),
                data: &[0x5a; 4],
            })
            .unwrap()
            .present(&[])
            .unwrap();
        let bytes = encoder.finish().unwrap();
        let mut sink = Sink::default();
        decode_frame(&bytes, &mut sink).unwrap();
        assert_eq!(sink.ops, ["atlas 2x2 data=4", "present 0"]);
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut encoder = FrameEncoder::new(FrameInfo {
            seq: 0,
            width: 1,
            height: 1,
            scale: 1.0,
        })
        .unwrap();
        // One byte over the submission cap.
        let data = vec![0u8; MAX_SUBMIT_BYTES + 1];
        assert!(matches!(
            encoder.submit(0, &data),
            Err(Error::TooLarge {
                what: "backend submission",
                ..
            })
        ));
    }
}
