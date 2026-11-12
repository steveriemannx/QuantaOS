//! `/scheme/quanta-gpu`: `info`, `fb` and `ctl`.

use std::collections::{BTreeMap, VecDeque};

use quanta_gpu_protocol::{decode_frame, peek_frame_info, DeviceInfo, Features, Format, VERSION};
use quanta_gpu_raster::{Executor, Rasterizer, Surface};
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult};
use syscall::data::{Stat, StatVfs};
use syscall::dirent::{DirEntry, DirentBuf, DirentKind};
use syscall::flag::{MODE_DIR, MODE_FILE};
use syscall::schemev2::NewFdFlags;
use syscall::{Error, Result, EACCES, EBADF, EINVAL, EIO, EISDIR, ENOENT, ENOTDIR, EWOULDBLOCK};

use crate::display::Display;

/// Keep the per-client fence queue bounded if nobody reads it.
const MAX_PENDING_FENCES: usize = 64;

/// Record why a frame was rejected. The first few go to `/tmp` so a headless
/// QEMU test can read them over the serial console; after that we stay quiet
/// to avoid filling the filesystem with one line per frame.
fn log_reject(message: &str) {
    log_bounded(message, false);
}

/// Record frame traffic for the first few requests. This is cheap, bounded and
/// only used to diagnose the `ctl` path in QEMU.
fn log_trace(message: &str) {
    if std::env::var_os("QUANTA_GPU_TRACE").is_none() {
        return;
    }
    log_bounded(message, true);
}

fn log_bounded(message: &str, trace: bool) {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static REJECTS: AtomicUsize = AtomicUsize::new(0);
    static TRACES: AtomicUsize = AtomicUsize::new(0);

    let counter = if trace { &TRACES } else { &REJECTS };
    let count = counter.fetch_add(1, Ordering::Relaxed);
    if count >= 32 {
        return;
    }
    if !trace && count == 0 {
        eprintln!("quanta-gpud: {message}");
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/quanta-gpud.log")
    {
        let _ = writeln!(file, "{} {message}", if trace { "trace" } else { "reject" });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handle {
    Root,
    Info,
    Fb,
    Ctl,
}

pub struct QuantaScheme {
    handles: BTreeMap<usize, Handle>,
    next_id: usize,
    root: usize,
    info: String,
    display: Display,
    /// Atlas state survives across `write` calls, so a client uploads glyphs
    /// once and then only submits quads.
    rasterizer: Rasterizer,
    /// Completed `Present` sequence numbers (or explicit client tokens).
    fences: VecDeque<u64>,
    /// Whether the VT has been re-asserted for the first frame after the
    /// console handoff.
    activated: bool,
}

impl QuantaScheme {
    pub fn new(display: Display) -> Self {
        let device = DeviceInfo::new(
            VERSION,
            display.width(),
            display.height(),
            display.pitch(),
            display.scale(),
            Format::Argb8888,
            Features::PRESENT.union(Features::RENDER),
        )
        .expect("display reported an invalid device description");
        let mut scheme = Self {
            handles: BTreeMap::new(),
            next_id: 0,
            root: 0,
            info: device.to_text(),
            display,
            rasterizer: Rasterizer::new(),
            fences: VecDeque::new(),
            activated: false,
        };
        scheme.root = scheme.insert(Handle::Root);
        scheme
    }

    fn queue_fence(&mut self, token: u64) {
        if self.fences.len() >= MAX_PENDING_FENCES {
            self.fences.pop_front();
        }
        self.fences.push_back(token);
    }

    fn insert(&mut self, handle: Handle) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.handles.insert(id, handle);
        id
    }

    fn get(&self, id: usize) -> Result<Handle> {
        self.handles
            .get(&id)
            .copied()
            .ok_or_else(|| Error::new(EBADF))
    }
}

impl SchemeSync for QuantaScheme {
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(self.root)
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if self.get(dirfd)? != Handle::Root {
            return Err(Error::new(EACCES));
        }

        let path = path.trim_start_matches('/');
        let handle = match path {
            "" => Handle::Root,
            "info" => Handle::Info,
            "fb" => Handle::Fb,
            "ctl" => Handle::Ctl,
            _ => return Err(Error::new(ENOENT)),
        };
        let number = if handle == Handle::Root {
            self.root
        } else {
            self.insert(handle)
        };
        log_trace(&format!(
            "open {path:?} flags={_flags:#x} -> handle {number}"
        ));
        Ok(OpenResult::ThisScheme {
            number,
            // `info` is a normal positioned regular file. `ctl` is a message
            // stream: every write is one complete frame and must start at
            // offset 0, so the kernel must not advance a file position for it.
            flags: match handle {
                Handle::Info => NewFdFlags::POSITIONED,
                _ => NewFdFlags::empty(),
            },
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match self.get(id)? {
            Handle::Info => {
                let info = self.info.as_bytes();
                let offset = usize::try_from(offset).map_err(|_| Error::new(EINVAL))?;
                if offset >= info.len() {
                    return Ok(0);
                }
                let count = buf.len().min(info.len() - offset);
                buf[..count].copy_from_slice(&info[offset..offset + count]);
                Ok(count)
            }
            Handle::Ctl => {
                // `ctl` is a message stream: the fence queue is read from the
                // front regardless of the kernel-supplied file offset.
                let Some(token) = self.fences.front().copied() else {
                    return Err(Error::new(EWOULDBLOCK));
                };
                let bytes = token.to_le_bytes();
                if buf.len() < bytes.len() {
                    return Err(Error::new(EINVAL));
                }
                buf[..bytes.len()].copy_from_slice(&bytes);
                self.fences.pop_front();
                Ok(bytes.len())
            }
            Handle::Fb => Err(Error::new(EBADF)),
            Handle::Root => Err(Error::new(EISDIR)),
        }
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        if self.get(id)? != Handle::Ctl {
            return Err(Error::new(EBADF));
        }
        log_trace(&format!("write fd={id} len={} offset={_offset}", buf.len()));
        // `ctl` is a message stream. Every write is exactly one complete
        // frame, so any file offset the kernel may report is irrelevant.

        // Validate the header and the surface before executing any draw
        // command. A rejected frame must leave the scanout untouched.
        let info = match peek_frame_info(buf) {
            Ok(info) => info,
            Err(err) => {
                log_reject(&format!("rejecting frame header: {err}"));
                return Err(Error::new(EINVAL));
            }
        };
        if let Err(err) = self.display.validate_frame(&info) {
            log_reject(&format!("rejecting frame: {err:#}"));
            return Err(Error::new(EINVAL));
        }

        let (width, height, stride) = (
            self.display.width(),
            self.display.height(),
            self.display.stride(),
        );
        let scale = info.scale;
        let executed = {
            let surface = match Surface::new(self.display.surface_mut(), width, height, stride) {
                Ok(surface) => surface,
                Err(err) => {
                    log_reject(&format!("invalid surface: {err}"));
                    return Err(Error::new(EIO));
                }
            };
            let mut executor = Executor::new(surface, &mut self.rasterizer, scale);
            if let Err(err) = decode_frame(buf, &mut executor) {
                log_reject(&format!("rejecting frame commands: {err}"));
                return Err(Error::new(EINVAL));
            }
            executor.finish()
        };

        // The boot console handoff may have run after `Display::open`
        // activated our VT. Claim the display back *before* the first present:
        // a scanout flush on an inactive VT can block, which would otherwise
        // freeze the very frame that was meant to activate it.
        if !self.activated {
            self.activated = self.display.activate();
        }
        if let Err(err) = self.display.present(&info, &executed.damage) {
            log_reject(&format!("present failed: {err:#}"));
            return Err(Error::new(EIO));
        }
        // Every successful present is acknowledged, even if the client did
        // not ask for a fence, so clients can throttle without inventing
        // their own timing.
        self.queue_fence(executed.fence.unwrap_or(info.seq));
        log_trace(&format!(
            "present seq={} damage={}",
            info.seq,
            executed.damage.len()
        ));
        Ok(buf.len())
    }

    fn fsize(&mut self, id: usize, _ctx: &CallerCtx) -> Result<u64> {
        match self.get(id)? {
            Handle::Info => Ok(self.info.len() as u64),
            Handle::Fb => Ok(self.display.surface_len() as u64),
            Handle::Ctl => Ok(8),
            Handle::Root => Err(Error::new(EISDIR)),
        }
    }

    // The Redox VFS calls `fstat` before a path can be used as a regular
    // file. Without it every `open()` of `info`/`ctl` fails with EOPNOTSUPP
    // and clients such as `fs::read_to_string` see an empty device description.
    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        let (mode, size) = match self.get(id)? {
            Handle::Root => (MODE_DIR | 0o755, 0),
            Handle::Info => (MODE_FILE | 0o444, self.info.len() as u64),
            Handle::Fb => (MODE_FILE | 0o660, self.display.surface_len() as u64),
            Handle::Ctl => (MODE_FILE | 0o660, 8),
        };
        stat.st_mode = mode;
        stat.st_size = size;
        stat.st_nlink = 1;
        stat.st_blksize = 4096;
        stat.st_blocks = size.div_ceil(512);
        Ok(())
    }

    fn fstatvfs(&mut self, _id: usize, stat: &mut StatVfs, _ctx: &CallerCtx) -> Result<()> {
        stat.f_bsize = 4096;
        stat.f_blocks = (self.display.surface_len() as u64).div_ceil(4096);
        stat.f_bfree = 0;
        stat.f_bavail = 0;
        Ok(())
    }

    fn getdents<'buf>(
        &mut self,
        id: usize,
        mut buf: DirentBuf<&'buf mut [u8]>,
        opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        if self.get(id)? != Handle::Root {
            return Err(Error::new(ENOTDIR));
        }
        const ENTRIES: [&str; 3] = ["info", "fb", "ctl"];
        for (index, name) in ENTRIES.iter().enumerate().skip(opaque_offset as usize) {
            buf.entry(DirEntry {
                inode: 0,
                next_opaque_id: index as u64 + 1,
                name,
                kind: DirentKind::Regular,
            })?;
        }
        Ok(buf)
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        let path = match self.get(id)? {
            Handle::Root => "quanta-gpu",
            Handle::Info => "quanta-gpu/info",
            Handle::Fb => "quanta-gpu/fb",
            Handle::Ctl => "quanta-gpu/ctl",
        };
        if buf.len() < path.len() + 1 {
            return Err(Error::new(EINVAL));
        }
        buf[..path.len()].copy_from_slice(path.as_bytes());
        buf[path.len()] = 0;
        Ok(path.len() + 1)
    }

    fn mmap_prep(
        &mut self,
        id: usize,
        offset: u64,
        size: usize,
        _flags: syscall::MapFlags,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        if self.get(id)? != Handle::Fb {
            return Err(Error::new(EINVAL));
        }
        if offset != 0 || size > self.display.surface_len() {
            return Err(Error::new(EINVAL));
        }
        if size == 0 {
            return Ok(0);
        }
        Ok(self.display.surface_ptr() as usize)
    }

    fn munmap(
        &mut self,
        _id: usize,
        _offset: u64,
        _size: usize,
        _flags: syscall::MunmapFlags,
        _ctx: &CallerCtx,
    ) -> Result<()> {
        Ok(())
    }

    fn on_close(&mut self, id: usize) {
        if id != self.root {
            self.handles.remove(&id);
        }
    }
}
