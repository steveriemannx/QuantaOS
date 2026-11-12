//! QuantaGPU wire protocol.
//!
//! This crate is the contract between `ferritic-quanta` (a FerriticUI backend)
//! and `quanta-gpud` (the Redox GPU/display driver service). It is deliberately
//! small, dependency-free and FFI-free: no Vulkan, no `libc`, no `bindgen`.
//!
//! # Layers
//!
//! * **Presentation (v0).** A client renders into a driver-provided
//!   `argb8888` surface and submits a damage list. This runs on virtio-gpu 2D hardware,
//!   whose command set has no draw operations.
//! * **Rendering (v1).** The same frame may carry primitives
//!   ([`RectCmd`], [`ShadowCmd`], [`GlyphCmd`]) that the driver can execute
//!   with the pure-Rust reference rasterizer, or with a future virgl backend.
//!   [`DeviceInfo::features`] advertises whether the driver implements them.
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
/// Upper bound on records in one frame. A frame with more records would cost
/// more memory to validate than its wire size suggests.
pub const MAX_RECORDS: usize = 1 << 16;
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

/// Wire size of one [`InputEvent`].
pub const INPUT_EVENT_LEN: usize = 16;

/// Keyboard and pointer events forwarded by the display service.
///
/// The service owns the VT input consumer; clients read these events from
/// `/scheme/quanta-gpu/input`. The encoding is deliberately the same shape for
/// every event kind: `x`/`y` carry the payload (a keymap character, pointer
/// coordinates, a button mask or a scroll delta), `code` carries a scancode or
/// button, and `pressed` is meaningful for keys and buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub x: i32,
    pub y: i32,
    pub pressed: bool,
}

pub const INPUT_KEY: u16 = 1;
pub const INPUT_MOUSE: u16 = 2;
pub const INPUT_BUTTON: u16 = 3;
pub const INPUT_SCROLL: u16 = 4;
pub const INPUT_MOUSE_RELATIVE: u16 = 5;

impl InputEvent {
    pub const fn key(character: u32, scancode: u16, pressed: bool) -> Self {
        Self {
            kind: INPUT_KEY,
            code: scancode,
            x: character as i32,
            y: 0,
            pressed,
        }
    }

    pub const fn mouse(x: i32, y: i32) -> Self {
        Self {
            kind: INPUT_MOUSE,
            code: 0,
            x,
            y,
            pressed: false,
        }
    }

    pub const fn mouse_relative(dx: i32, dy: i32) -> Self {
        Self {
            kind: INPUT_MOUSE_RELATIVE,
            code: 0,
            x: dx,
            y: dy,
            pressed: false,
        }
    }

    pub const fn button(mask: i32) -> Self {
        Self {
            kind: INPUT_BUTTON,
            code: 0,
            x: mask,
            y: 0,
            pressed: mask & 1 != 0,
        }
    }

    pub const fn scroll(dx: i32, dy: i32) -> Self {
        Self {
            kind: INPUT_SCROLL,
            code: 0,
            x: dx,
            y: dy,
            pressed: false,
        }
    }

    /// Encodes the event in [`INPUT_EVENT_LEN`] little-endian bytes.
    pub fn to_bytes(self) -> [u8; INPUT_EVENT_LEN] {
        let mut bytes = [0u8; INPUT_EVENT_LEN];
        bytes[0..2].copy_from_slice(&self.kind.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.code.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.x.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.y.to_le_bytes());
        bytes[12] = u8::from(self.pressed);
        bytes
    }

    /// Decodes one event. Reserved bytes must be zero so a future extension
    /// cannot be silently misread as an event.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < INPUT_EVENT_LEN {
            return Err(Error::Truncated);
        }
        if bytes[13..16] != [0, 0, 0] {
            return Err(Error::BadPayload { op: 0 });
        }
        Ok(Self {
            kind: u16::from_le_bytes([bytes[0], bytes[1]]),
            code: u16::from_le_bytes([bytes[2], bytes[3]]),
            x: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            y: i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            pressed: bytes[12] != 0,
        })
    }
}

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

/// Pixel layout of the shared surface.
///
/// The scanout is little-endian `B8G8R8A8_UNORM`, which in memory is the same
/// as `0xAARRGGBB` (`argb8888`). Clients write pixels as `u32::to_le_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// 32-bit `0xAARRGGBB`.
    Argb8888,
}

impl Format {
    /// Stable name used in `/scheme/quanta-gpu/info`.
    pub const ARGB8888_NAME: &'static str = "argb8888";

    /// Bytes per pixel in memory.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Format::Argb8888 => 4,
        }
    }

    /// Stable name used in `/scheme/quanta-gpu/info`.
    pub const fn name(self) -> &'static str {
        match self {
            Format::Argb8888 => Self::ARGB8888_NAME,
        }
    }
}

/// Optional protocol features advertised in `/scheme/quanta-gpu/info`.
///
/// `present` is mandatory. `render` means the driver executes the v1 draw
/// records (`Clear`, `Rect`, `Shadow`, `Glyph`, `Atlas`); without it a client
/// must rasterize into the surface itself. `submit` means raw backend streams
/// are accepted (a future virgl executor).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features(u32);

impl Features {
    pub const PRESENT: Self = Self(1 << 0);
    pub const RENDER: Self = Self(1 << 1);
    pub const SUBMIT: Self = Self(1 << 2);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Parses a comma-separated feature list. Unknown names are ignored so a
    /// newer driver can add features without breaking older clients.
    pub fn parse(text: &str) -> Self {
        let mut features = Self::empty();
        for name in text.split(',') {
            let name = name.trim();
            features = features.union(match name {
                "present" => Self::PRESENT,
                "render" => Self::RENDER,
                "submit" => Self::SUBMIT,
                _ => Self::empty(),
            });
        }
        features
    }

    /// Stable comma-separated list for `DeviceInfo::to_text`.
    pub fn to_text(self) -> String {
        let mut names = Vec::new();
        if self.contains(Self::PRESENT) {
            names.push("present");
        }
        if self.contains(Self::RENDER) {
            names.push("render");
        }
        if self.contains(Self::SUBMIT) {
            names.push("submit");
        }
        names.join(",")
    }
}

/// Device description served by `/scheme/quanta-gpu/info`.
///
/// The on-disk form is a small `key=value` text file. Keeping the parser in
/// this crate means the client and the driver cannot disagree about defaults,
/// required fields or unknown keys.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceInfo {
    pub version: u16,
    /// Physical pixels.
    pub width: u32,
    /// Physical pixels.
    pub height: u32,
    /// Bytes per row, including padding.
    pub stride: u32,
    /// Logical-to-physical scale.
    pub scale: f32,
    pub format: Format,
    /// Protocol features the driver implements.
    pub features: Features,
}

impl DeviceInfo {
    /// Builds a description after checking every invariant.
    pub fn new(
        version: u16,
        width: u32,
        height: u32,
        stride: u32,
        scale: f32,
        format: Format,
        features: Features,
    ) -> Result<Self, InfoError> {
        let info = Self {
            version,
            width,
            height,
            stride,
            scale,
            format,
            features,
        };
        info.validate()?;
        Ok(info)
    }

    /// Checks the fields a client relies on before it maps the surface.
    pub fn validate(&self) -> Result<(), InfoError> {
        if self.version != VERSION {
            return Err(InfoError::UnsupportedVersion(self.version));
        }
        if self.width == 0 || self.height == 0 {
            return Err(InfoError::InvalidGeometry);
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return Err(InfoError::InvalidGeometry);
        }
        let row = self
            .width
            .checked_mul(self.format.bytes_per_pixel() as u32)
            .ok_or(InfoError::InvalidGeometry)?;
        if self.stride < row || !self.stride.is_multiple_of(4) {
            return Err(InfoError::InvalidGeometry);
        }
        if !self.features.contains(Features::PRESENT) {
            return Err(InfoError::MissingFeature("present"));
        }
        if self.surface_len().is_none() {
            return Err(InfoError::InvalidGeometry);
        }
        Ok(())
    }

    /// Size of the `stride * height` mapping, if it fits in `usize`.
    pub fn surface_len(&self) -> Option<usize> {
        (self.stride as usize).checked_mul(self.height as usize)
    }

    /// Serializes the description in the format parsed by [`DeviceInfo::parse`].
    pub fn to_text(&self) -> String {
        format!(
            "version={}\nwidth={}\nheight={}\nstride={}\nformat={}\nscale={}\nfeatures={}\n",
            self.version,
            self.width,
            self.height,
            self.stride,
            self.format.name(),
            self.scale,
            self.features.to_text()
        )
    }

    /// Parses the text served at `/scheme/quanta-gpu/info`.
    ///
    /// Unknown keys and `#` comments are ignored so the file can grow without
    /// breaking clients. Every required key must be present exactly in the
    /// documented spelling.
    pub fn parse(text: &str) -> Result<Self, InfoError> {
        let mut version = None;
        let mut width = None;
        let mut height = None;
        let mut stride = None;
        let mut scale = None;
        let mut format = None;
        let mut features = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "version" => version = Some(parse_info(value, "version")?),
                "width" => width = Some(parse_info(value, "width")?),
                "height" => height = Some(parse_info(value, "height")?),
                "stride" => stride = Some(parse_info(value, "stride")?),
                "scale" => scale = Some(parse_info(value, "scale")?),
                "format" => {
                    format = Some(match value {
                        Format::ARGB8888_NAME => Format::Argb8888,
                        _ => return Err(InfoError::UnknownFormat),
                    })
                }
                "features" => features = Some(Features::parse(value)),
                _ => {}
            }
        }

        let info = Self {
            version: version.ok_or(InfoError::MissingField("version"))?,
            width: width.ok_or(InfoError::MissingField("width"))?,
            height: height.ok_or(InfoError::MissingField("height"))?,
            stride: stride.ok_or(InfoError::MissingField("stride"))?,
            scale: scale.ok_or(InfoError::MissingField("scale"))?,
            format: format.ok_or(InfoError::MissingField("format"))?,
            // Older v0 drivers predate feature advertisement.
            features: features.unwrap_or(Features::PRESENT),
        };
        info.validate()?;
        Ok(info)
    }
}

fn parse_info<T: core::str::FromStr>(value: &str, field: &'static str) -> Result<T, InfoError> {
    value.parse().map_err(|_| InfoError::InvalidField { field })
}

/// Errors returned while reading or writing the device description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InfoError {
    MissingField(&'static str),
    MissingFeature(&'static str),
    InvalidField { field: &'static str },
    UnknownFormat,
    UnsupportedVersion(u16),
    InvalidGeometry,
}

impl fmt::Display for InfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InfoError::MissingField(field) => write!(f, "device info is missing {field}"),
            InfoError::MissingFeature(feature) => {
                write!(f, "device info is missing feature {feature}")
            }
            InfoError::InvalidField { field } => write!(f, "invalid device info {field}"),
            InfoError::UnknownFormat => write!(f, "unknown surface format"),
            InfoError::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            InfoError::InvalidGeometry => write!(f, "invalid surface geometry"),
        }
    }
}

impl std::error::Error for InfoError {}

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

    /// Exclusive right edge, saturating instead of wrapping.
    pub const fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    /// Exclusive bottom edge, saturating instead of wrapping.
    pub const fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Intersection with another rectangle, or `None` if they do not overlap.
    pub const fn intersection(self, other: Self) -> Option<Self> {
        let x = if self.x > other.x { self.x } else { other.x };
        let y = if self.y > other.y { self.y } else { other.y };
        let right = if self.right() < other.right() {
            self.right()
        } else {
            other.right()
        };
        let bottom = if self.bottom() < other.bottom() {
            self.bottom()
        } else {
            other.bottom()
        };
        if x >= right || y >= bottom {
            None
        } else {
            Some(Self::new(x, y, right - x, bottom - y))
        }
    }

    /// Clip to a `width` by `height` surface, returning `None` when nothing
    /// of the rectangle is visible.
    pub const fn clipped_to(self, width: u32, height: u32) -> Option<Self> {
        self.intersection(Self::new(0, 0, width, height))
    }

    /// Smallest rectangle containing both inputs. Empty inputs are ignored.
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(x, y, right - x, bottom - y)
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
    TooManyRecords { count: usize },
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
            Error::TooManyRecords { count } => write!(f, "too many records in frame ({count})"),
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
            return Err(Error::FrameTooLarge {
                len: self.buf.len(),
            });
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

/// Reads and validates the frame header without replaying any records.
///
/// Drivers use this to reject a frame (wrong size, stale sequence, wrong
/// surface) *before* executing draw operations against the scanout.
pub fn peek_frame_info(bytes: &[u8]) -> Result<FrameInfo, Error> {
    parse_header(bytes).map(|(info, _)| info)
}

fn parse_header(bytes: &[u8]) -> Result<(FrameInfo, usize), Error> {
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
    if command_len != bytes.len() - HEADER_LEN {
        return Err(if command_len > bytes.len() - HEADER_LEN {
            Error::Truncated
        } else {
            Error::BadHeader
        });
    }
    Ok((info, command_len))
}

/// Decodes a frame, calling `sink` for every operation in order.
///
/// The frame is validated completely before any operation is handed to the
/// sink. This is important for drivers: a malformed frame must not leave the
/// scanout half-updated.
pub fn decode_frame<F: FrameSink>(bytes: &[u8], sink: &mut F) -> Result<FrameInfo, Error> {
    let (info, _) = parse_header(bytes)?;
    let commands = &bytes[HEADER_LEN..];

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
    let mut records = 0usize;
    while offset < commands.len() {
        if state.present {
            return Err(Error::TrailingBytes);
        }
        records += 1;
        if records > MAX_RECORDS {
            return Err(Error::TooManyRecords { count: records });
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
                    clip: read_clip(&payload[32..52], op)?,
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
                    clip: read_clip(&payload[36..56], op)?,
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
                    clip: read_clip(&payload[36..56], op)?,
                };
                cmd.rect.validate()?;
                validate_clip(cmd.clip)?;
                sink.glyph(&cmd);
            }
            OP_ATLAS => {
                if len < 28 {
                    return Err(Error::BadPayload { op });
                }
                let field = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
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
                    || rect
                        .x
                        .checked_add(rect.width)
                        .is_none_or(|v| v > atlas_width)
                    || rect
                        .y
                        .checked_add(rect.height)
                        .is_none_or(|v| v > atlas_height)
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
    for (index, value) in [rect.x, rect.y, rect.width, rect.height].iter().enumerate() {
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

fn read_clip(input: &[u8], op: u16) -> Result<Option<FloatRect>, Error> {
    match input[0] {
        0 => Ok(None),
        1 => Ok(Some(read_float_rect(&input[4..20])?)),
        _ => Err(Error::BadPayload { op }),
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
            self.ops.push(format!(
                "atlas {}x{} data={}",
                cmd.rect.width,
                cmd.rect.height,
                cmd.data.len()
            ));
        }
        fn submit(&mut self, backend: u32, data: &[u8]) {
            self.ops
                .push(format!("submit {backend} data={}", data.len()));
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
        assert!(matches!(encoder.present(&[]), Err(Error::PresentNotLast)));
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
    fn device_info_round_trips() {
        let info = DeviceInfo::new(
            0,
            800,
            600,
            3200,
            1.25,
            Format::Argb8888,
            Features::PRESENT.union(Features::RENDER),
        )
        .unwrap();
        let text = info.to_text();
        assert!(text.contains("version=0\n"));
        assert!(text.contains("format=argb8888\n"));
        assert!(text.contains("features=present,render\n"));
        assert_eq!(DeviceInfo::parse(&text), Ok(info));
    }

    #[test]
    fn device_info_ignores_comments_and_unknown_keys() {
        let text = "# generated\nversion=0\nwidth=64\nheight=32\nstride=256\n\
                    format=argb8888\nscale=2\nfeatures=present,render,future\nfuture_key=1\n";
        let info = DeviceInfo::parse(text).unwrap();
        assert_eq!(info.width, 64);
        assert_eq!(info.height, 32);
        assert_eq!(info.stride, 256);
        assert_eq!(info.scale, 2.0);
        assert_eq!(info.format, Format::Argb8888);
        assert!(info.features.contains(Features::RENDER));
        assert!(!info.features.contains(Features::SUBMIT));
    }

    #[test]
    fn device_info_rejects_bad_fields() {
        let base = "version=0\nwidth=64\nheight=32\nstride=256\nformat=argb8888\nscale=1\n";
        assert_eq!(
            DeviceInfo::parse(base),
            Ok(DeviceInfo::new(0, 64, 32, 256, 1.0, Format::Argb8888, Features::PRESENT,).unwrap())
        );

        let missing = base.replace("stride=256\n", "");
        assert_eq!(
            DeviceInfo::parse(&missing),
            Err(InfoError::MissingField("stride"))
        );

        let bad_format = base.replace("argb8888", "rgb565");
        assert_eq!(
            DeviceInfo::parse(&bad_format),
            Err(InfoError::UnknownFormat)
        );

        let narrow = base.replace("stride=256", "stride=32");
        assert_eq!(DeviceInfo::parse(&narrow), Err(InfoError::InvalidGeometry));

        let version = base.replace("version=0", "version=9");
        assert_eq!(
            DeviceInfo::parse(&version),
            Err(InfoError::UnsupportedVersion(9))
        );

        let scale = base.replace("scale=1", "scale=NaN");
        assert_eq!(DeviceInfo::parse(&scale), Err(InfoError::InvalidGeometry));

        let no_present = base.replace("scale=1", "scale=1\nfeatures=render");
        assert_eq!(
            DeviceInfo::parse(&no_present),
            Err(InfoError::MissingFeature("present"))
        );
    }

    #[test]
    fn pixel_rect_helpers_clip_and_merge() {
        let rect = PixelRect::new(10, 20, 30, 40);
        assert_eq!(rect.right(), 40);
        assert_eq!(rect.bottom(), 60);
        assert!(!rect.is_empty());
        assert_eq!(
            rect.clipped_to(20, 30),
            Some(PixelRect::new(10, 20, 10, 10))
        );
        assert_eq!(rect.clipped_to(5, 5), None);
        assert_eq!(PixelRect::new(1, 1, 0, 4).clipped_to(100, 100), None);
        assert_eq!(
            PixelRect::new(0, 0, 10, 10).intersection(PixelRect::new(5, 5, 10, 10)),
            Some(PixelRect::new(5, 5, 5, 5))
        );
        assert_eq!(
            PixelRect::new(0, 0, 10, 10).intersection(PixelRect::new(10, 10, 5, 5)),
            None
        );
        assert_eq!(
            PixelRect::new(0, 0, 10, 10).union(PixelRect::new(20, 20, 5, 5)),
            PixelRect::new(0, 0, 25, 25)
        );
        assert_eq!(
            PixelRect::default().union(PixelRect::new(3, 4, 5, 6)),
            PixelRect::new(3, 4, 5, 6)
        );
    }

    #[test]
    fn malformed_clip_kind_reports_the_opcode() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.rect(&sample_rect()).unwrap().present(&[]).unwrap();
        let mut bytes = encoder.finish().unwrap();
        // Header (40) + record header (8) + payload offset 32 = clip tag.
        bytes[80] = 2;
        assert_eq!(
            decode_frame(&bytes, &mut Sink::default()),
            Err(Error::BadPayload { op: OP_RECT })
        );

        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder
            .shadow(&sample_shadow())
            .unwrap()
            .present(&[])
            .unwrap();
        let mut bytes = encoder.finish().unwrap();
        // Header (40) + record header (8) + payload offset 36 = clip tag.
        bytes[84] = 2;
        assert_eq!(
            decode_frame(&bytes, &mut Sink::default()),
            Err(Error::BadPayload { op: OP_SHADOW })
        );

        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder
            .glyph(&sample_glyph())
            .unwrap()
            .present(&[])
            .unwrap();
        let mut bytes = encoder.finish().unwrap();
        bytes[84] = 2;
        assert_eq!(
            decode_frame(&bytes, &mut Sink::default()),
            Err(Error::BadPayload { op: OP_GLYPH })
        );
    }

    #[test]
    fn feature_list_parses_and_ignores_unknown_names() {
        assert_eq!(
            Features::parse("present,render"),
            Features::PRESENT.union(Features::RENDER)
        );
        assert_eq!(Features::parse("present,future"), Features::PRESENT);
        assert_eq!(Features::parse(""), Features::empty());
        assert_eq!(
            Features::PRESENT.union(Features::RENDER).to_text(),
            "present,render"
        );
    }

    #[test]
    fn peek_reads_the_header_without_replaying_records() {
        let mut encoder = FrameEncoder::new(info()).unwrap();
        encoder.clear(Rgba8::BLACK).unwrap().present(&[]).unwrap();
        let bytes = encoder.finish().unwrap();

        assert_eq!(peek_frame_info(&bytes), Ok(info()));
        assert_eq!(
            peek_frame_info(&bytes[..HEADER_LEN - 1]),
            Err(Error::Truncated)
        );

        // A broken record does not affect header peeking, but decode rejects it.
        let mut broken = bytes.clone();
        broken[HEADER_LEN + 2] = 1; // record flags
        assert_eq!(peek_frame_info(&broken), Ok(info()));
        assert_eq!(
            decode_frame(&broken, &mut Sink::default()),
            Err(Error::BadHeader)
        );
    }

    #[test]
    fn too_many_records_are_rejected() {
        let mut commands = Vec::new();
        for _ in 0..=MAX_RECORDS {
            commands.extend_from_slice(&999u16.to_le_bytes()); // unknown op
            commands.extend_from_slice(&0u16.to_le_bytes());
            commands.extend_from_slice(&0u32.to_le_bytes());
        }
        commands.extend_from_slice(&OP_PRESENT.to_le_bytes());
        commands.extend_from_slice(&0u16.to_le_bytes());
        commands.extend_from_slice(&4u32.to_le_bytes());
        commands.extend_from_slice(&0u32.to_le_bytes());

        let mut bytes = vec![0u8; HEADER_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
        bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&1.0f32.to_bits().to_le_bytes());
        bytes[28..32].copy_from_slice(&(commands.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&commands);

        assert!(matches!(
            decode_frame(&bytes, &mut Sink::default()),
            Err(Error::TooManyRecords { .. })
        ));
    }

    #[test]
    fn input_events_round_trip() {
        let event = InputEvent::key('A' as u32, 0x1e, true);
        let bytes = event.to_bytes();
        assert_eq!(bytes.len(), INPUT_EVENT_LEN);
        assert_eq!(InputEvent::from_bytes(&bytes), Ok(event));
        assert!(InputEvent::from_bytes(&bytes[..4]).is_err());
        let mut bad = bytes;
        bad[15] = 1;
        assert!(InputEvent::from_bytes(&bad).is_err());
        assert_eq!(
            InputEvent::from_bytes(&InputEvent::mouse(10, 20).to_bytes()).unwrap(),
            InputEvent::mouse(10, 20)
        );
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
