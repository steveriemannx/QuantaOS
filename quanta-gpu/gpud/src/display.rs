//! The virtio-gpu 2D scanout owned by `quanta-gpud`.
//!
//! The DRM device is the one exposed by the pure-Rust `virtio-gpud` driver.
//! The client never talks to DRM: it renders into the mmap'd back buffer here,
//! and `Present` copies the damaged rows into the dumb buffer and flushes it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::ptr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::connector::State;
use drm::control::dumbbuffer::{DumbBuffer, DumbMapping};
use drm::control::{framebuffer, ClipRect, Device as _};
use drm::{Device as _, DriverCapability};
use libredox::flag::{O_CLOEXEC, O_NONBLOCK, O_RDWR};
use quanta_gpu_protocol::{FrameInfo, PixelRect};
use syscall::{Map, MAP_PRIVATE, PROT_READ, PROT_WRITE};

/// Path used by every VT consumer.
const INPUT_CONSUMER: &str = "/scheme/input/consumer";
const INPUT_CONTROL: &str = "/scheme/input/control";

/// The inputd control event kind that activates a VT.
const CONTROL_ACTIVATE_VT: usize = 1;

struct DrmDevice(File);

impl AsFd for DrmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for DrmDevice {}
impl drm::control::Device for DrmDevice {}

/// One fullscreen scanout surface plus the shared client back buffer.
pub struct Display {
    consumer: File,
    dev: DrmDevice,
    // Declared before `buffer` so the mapping is dropped first.
    mapping: DumbMapping<'static>,
    buffer: DumbBuffer,
    fb: framebuffer::Handle,
    width: u32,
    height: u32,
    pitch: u32,
    back: *mut u8,
    back_len: usize,
    last_seq: u64,
}

impl Display {
    pub fn open() -> Result<Self> {
        // The VT is allocated when the consumer is opened; inputd hands out the
        // display handle for that VT via openat("display").
        let consumer = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(INPUT_CONSUMER)
            .context("open /scheme/input/consumer")?;
        let consumer_fd = consumer.as_raw_fd() as usize;

        let display_fd =
            libredox::call::openat(consumer_fd, "display", O_CLOEXEC | O_NONBLOCK | O_RDWR, 0)
                .context("open VT display handle")?;
        let display_file = unsafe { File::from_raw_fd(display_fd as RawFd) };

        let dev = DrmDevice(display_file);
        if dev
            .get_driver_capability(DriverCapability::DumbBuffer)
            .map_err(|err| anyhow!("query dumb buffer capability: {err:?}"))?
            != 1
        {
            bail!("display driver does not support dumb buffers");
        }

        let resources = dev
            .resource_handles()
            .map_err(|err| anyhow!("query DRM resources: {err:?}"))?;
        let connector = resources
            .connectors()
            .iter()
            .find_map(|&connector| {
                let info = dev.get_connector(connector, true).ok()?;
                (info.state() == State::Connected && !info.modes().is_empty()).then_some(info)
            })
            .ok_or_else(|| anyhow!("no connected display"))?;

        let mode = connector.modes()[0];
        let (width, height) = mode.size();
        let (width, height) = (u32::from(width), u32::from(height));
        if width == 0 || height == 0 {
            bail!("display has an empty mode");
        }
        if width > u16::MAX as u32 || height > u16::MAX as u32 {
            bail!("display mode {width}x{height} is too large for DRM damage rectangles");
        }

        let encoder = *connector
            .encoders()
            .first()
            .ok_or_else(|| anyhow!("connected display has no encoder"))?;
        let possible_crtcs = dev
            .get_encoder(encoder)
            .map_err(|err| anyhow!("query encoder: {err:?}"))?
            .possible_crtcs();
        let crtc = *resources
            .filter_crtcs(possible_crtcs)
            .first()
            .ok_or_else(|| anyhow!("display has no usable CRTC"))?;

        let mut buffer = dev
            .create_dumb_buffer((width, height), DrmFourcc::Argb8888, 32)
            .map_err(|err| anyhow!("create dumb buffer: {err:?}"))?;
        let pitch = buffer.pitch();
        let mapping = dev
            .map_dumb_buffer(&mut buffer)
            .map_err(|err| anyhow!("map dumb buffer: {err:?}"))?;
        // SAFETY: `buffer` is owned by Display and is not moved or rebuilt
        // while the mapping exists; the field order keeps the mapping alive no
        // longer than the buffer.
        let mapping = unsafe { mem::transmute::<DumbMapping<'_>, DumbMapping<'static>>(mapping) };

        let fb = dev
            .add_framebuffer(&buffer, 32, 32)
            .map_err(|err| anyhow!("add framebuffer: {err:?}"))?;
        dev.set_crtc(crtc, Some(fb), (0, 0), &[connector.handle()], Some(mode))
            .map_err(|err| anyhow!("set CRTC: {err:?}"))?;

        let back_len = (pitch as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow!("scanout size overflows usize"))?;
        // A dedicated page-aligned mapping; clients map these same pages via
        // the scheme's mmap_prep.
        let back = unsafe {
            syscall::fmap(
                !0,
                &Map {
                    offset: 0,
                    size: back_len,
                    flags: MAP_PRIVATE | PROT_READ | PROT_WRITE,
                    address: 0,
                },
            )
            .map_err(|err| anyhow!("allocate surface: {err:?}"))? as *mut u8
        };
        if back.is_null() {
            bail!("surface allocation returned a null pointer");
        }
        unsafe {
            ptr::write_bytes(back, 0, back_len);
        }

        let mut display = Self {
            consumer,
            dev,
            mapping,
            buffer,
            fb,
            width,
            height,
            pitch,
            back,
            back_len,
            last_seq: 0,
        };
        // The first frame is a black screen, but still make sure the scanout
        // is initialised since some hosts show uninitialised memory otherwise.
        display.flush_full()?;
        display.activate();
        Ok(display)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    pub fn scale(&self) -> f32 {
        1.0
    }

    pub fn surface_ptr(&self) -> *mut u8 {
        self.back
    }

    pub fn surface_len(&self) -> usize {
        self.back_len
    }

    pub fn stride(&self) -> usize {
        self.pitch as usize
    }

    /// The shared back buffer. Clients write pixels here through the scheme's
    /// `fb` mapping; the driver's v1 executor also renders into it.
    pub fn surface_mut(&mut self) -> &mut [u8] {
        // SAFETY: `back` points to a live `back_len` page-aligned mapping owned
        // by Display. `&mut self` excludes concurrent access through this API.
        unsafe { std::slice::from_raw_parts_mut(self.back, self.back_len) }
    }

    /// Rejects a frame before any draw command touches the back buffer.
    pub fn validate_frame(&self, info: &FrameInfo) -> Result<()> {
        if info.width != self.width || info.height != self.height {
            bail!(
                "frame is {}x{} but the scanout is {}x{}",
                info.width,
                info.height,
                self.width,
                self.height
            );
        }
        if !info.scale.is_finite() || info.scale <= 0.0 {
            bail!("frame has invalid scale {}", info.scale);
        }
        if info.seq <= self.last_seq {
            bail!(
                "frame sequence {} did not advance past {}",
                info.seq,
                self.last_seq
            );
        }
        Ok(())
    }

    /// Applies a validated frame. Damage rectangles are clipped to the
    /// surface; a frame with no damage still advances the sequence.
    pub fn present(&mut self, info: &FrameInfo, damage: &[PixelRect]) -> Result<()> {
        self.validate_frame(info)?;

        let mut clips = Vec::with_capacity(damage.len());
        let buffer_pitch = self.buffer.pitch() as usize;
        for rect in damage {
            let Some(rect) = rect.clipped_to(self.width, self.height) else {
                continue;
            };

            let row_bytes = rect.width as usize * 4;
            for row in 0..rect.height {
                let src = (rect.y + row) as usize * self.pitch as usize + rect.x as usize * 4;
                let dst = (rect.y + row) as usize * buffer_pitch + rect.x as usize * 4;
                unsafe {
                    ptr::copy_nonoverlapping(
                        self.back.add(src),
                        self.mapping.as_mut_ptr().add(dst),
                        row_bytes,
                    );
                }
            }

            clips.push(ClipRect::new(
                rect.x as u16,
                rect.y as u16,
                rect.right() as u16,
                rect.bottom() as u16,
            ));
        }

        if !clips.is_empty() {
            self.dev
                .dirty_framebuffer(self.fb, &clips)
                .map_err(|err| anyhow!("flush framebuffer: {err:?}"))?;
        }

        self.last_seq = info.seq;
        Ok(())
    }

    fn flush_full(&mut self) -> Result<()> {
        // Initialise the scanout from the (zeroed) client surface. Dirtying
        // the framebuffer alone would leave whatever the host had in the dumb
        // buffer visible until the first client frame arrives.
        let buffer_pitch = self.buffer.pitch() as usize;
        let row_bytes = self.width as usize * 4;
        for row in 0..self.height as usize {
            unsafe {
                ptr::copy_nonoverlapping(
                    self.back.add(row * self.pitch as usize),
                    self.mapping.as_mut_ptr().add(row * buffer_pitch),
                    row_bytes,
                );
            }
        }
        self.dev
            .dirty_framebuffer(
                self.fb,
                &[ClipRect::new(0, 0, self.width as u16, self.height as u16)],
            )
            .map_err(|err| anyhow!("initial framebuffer flush: {err:?}"))
    }

    /// Re-asserts this surface's VT as the active one.
    ///
    /// `open` activates the VT, but the console handoff can complete afterwards
    /// and take the display back. The driver calls this again before the first
    /// present so a client drawing after the handoff is actually visible.
    pub fn activate(&mut self) -> bool {
        self.activate_vt()
    }

    /// Makes our VT the active one, like `orbital` does with `inputd -A`.
    fn activate_vt(&mut self) -> bool {
        match self.vt_id() {
            Ok(vt) => match activate_vt(vt) {
                Ok(()) => true,
                Err(err) => {
                    eprintln!("quanta-gpud: could not activate VT {vt}: {err}");
                    false
                }
            },
            Err(err) => {
                eprintln!("quanta-gpud: could not determine VT id: {err}");
                false
            }
        }
    }

    fn vt_id(&self) -> Result<usize> {
        let mut path = [0u8; 256];
        let len = syscall::fpath(self.consumer.as_raw_fd() as usize, &mut path)
            .map_err(|err| anyhow!("fpath(consumer): {err:?}"))?;
        let len = len.min(path.len());
        let bytes = &path[..len];
        let bytes = bytes.split(|byte| *byte == 0).next().unwrap_or(bytes);
        let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("VT path is not UTF-8"))?;
        let vt = text
            .rsplit('/')
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| anyhow!("unexpected VT path {text:?}"))?;
        Ok(vt)
    }
}

fn activate_vt(vt: usize) -> io::Result<()> {
    let mut control = OpenOptions::new().write(true).open(INPUT_CONTROL)?;
    let mut event = Vec::with_capacity(2 * mem::size_of::<usize>());
    event.extend_from_slice(&CONTROL_ACTIVATE_VT.to_ne_bytes());
    event.extend_from_slice(&vt.to_ne_bytes());
    control.write_all(&event)
}

/// A retry loop used at boot while the display driver and inputd settle.
pub fn open_with_retry(attempts: usize) -> Result<Display> {
    let mut last = None;
    for attempt in 0..attempts {
        match Display::open() {
            Ok(display) => return Ok(display),
            Err(err) => {
                if attempt + 1 == attempts {
                    return Err(err);
                }
                if attempt == 0 {
                    eprintln!("quanta-gpud: waiting for a display: {err:#}");
                }
                last = Some(err);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no display")))
}
