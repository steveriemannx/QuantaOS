//! `/scheme/quanta-gpu`: `info`, `fb` and `ctl`.

use std::collections::{BTreeMap, VecDeque};

use quanta_gpu_protocol::{decode_frame, peek_frame_info, DeviceInfo, Features, Format, VERSION};
use quanta_gpu_raster::{Executor, Rasterizer, Surface};
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult};
use syscall::schemev2::NewFdFlags;
use syscall::{Error, Result, EACCES, EBADF, EINVAL, EIO, EISDIR, ENOENT, EWOULDBLOCK};

use crate::display::Display;

/// Keep the per-client fence queue bounded if nobody reads it.
const MAX_PENDING_FENCES: usize = 64;

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
        Ok(OpenResult::ThisScheme {
            number,
            flags: NewFdFlags::empty(),
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
                if offset != 0 {
                    return Err(Error::new(EINVAL));
                }
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
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        if self.get(id)? != Handle::Ctl {
            return Err(Error::new(EBADF));
        }
        if offset != 0 {
            return Err(Error::new(EINVAL));
        }

        // Validate the header and the surface before executing any draw
        // command. A rejected frame must leave the scanout untouched.
        let info = match peek_frame_info(buf) {
            Ok(info) => info,
            Err(err) => {
                eprintln!("quanta-gpud: rejecting frame header: {err}");
                return Err(Error::new(EINVAL));
            }
        };
        if let Err(err) = self.display.validate_frame(&info) {
            eprintln!("quanta-gpud: rejecting frame: {err:#}");
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
                    eprintln!("quanta-gpud: invalid surface: {err}");
                    return Err(Error::new(EIO));
                }
            };
            let mut executor = Executor::new(surface, &mut self.rasterizer, scale);
            if let Err(err) = decode_frame(buf, &mut executor) {
                eprintln!("quanta-gpud: rejecting frame commands: {err}");
                return Err(Error::new(EINVAL));
            }
            executor.finish()
        };

        if let Err(err) = self.display.present(&info, &executed.damage) {
            eprintln!("quanta-gpud: present failed: {err:#}");
            return Err(Error::new(EIO));
        }
        // Every successful present is acknowledged, even if the client did
        // not ask for a fence, so clients can throttle without inventing
        // their own timing.
        self.queue_fence(executed.fence.unwrap_or(info.seq));
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
