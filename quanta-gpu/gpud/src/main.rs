//! `quanta-gpud` — the QuantaGPU display service.
//!
//! This is a normal Redox userspace daemon started by init:
//!
//! * it opens the active display handle through `/scheme/input/consumer`;
//! * it owns a virtio-gpu 2D dumb buffer and serves `/scheme/quanta-gpu`;
//! * it maps a shared `argb8888` back buffer to clients over the scheme's
//!   `fb` file;
//! * on `ctl` it executes v1 draw records with the pure-Rust reference
//!   rasterizer, then copies damage into the scanout and flushes it.
//!
//! There is no Vulkan, no Mesa and no C in this path.

#![cfg(target_os = "redox")]

mod display;
mod scheme;

use std::cell::RefCell;
use std::env;
use std::os::fd::RawFd;

use redox_scheme::scheme::SchemeSync;
use redox_scheme::wrappers::ReadinessBased;
use redox_scheme::Socket;

use crate::scheme::QuantaScheme;

fn main() {
    let display = match display::open_with_retry(50) {
        Ok(display) => display,
        Err(err) => {
            eprintln!("quanta-gpud: no usable display: {err:#}");
            std::process::exit(1);
        }
    };

    let mut scheme = QuantaScheme::new(display);
    let socket = match Socket::create() {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("quanta-gpud: could not create scheme socket: {err:?}");
            std::process::exit(1);
        }
    };
    let mut readiness = ReadinessBased::new(&socket, 16);

    if let Err(err) = notify_init_ready(&socket, &mut scheme) {
        eprintln!("quanta-gpud: could not register with init: {err:?}");
        std::process::exit(1);
    }

    let scheme = RefCell::new(scheme);
    loop {
        match readiness.read_requests() {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) => {
                eprintln!("quanta-gpud: scheme read failed: {err:?}");
                break;
            }
        }
        readiness.process_requests(|| scheme.borrow_mut());
        if let Err(err) = readiness.write_responses() {
            eprintln!("quanta-gpud: scheme write failed: {err:?}");
            break;
        }
    }
}

fn notify_init_ready(socket: &Socket, scheme: &mut QuantaScheme) -> syscall::Result<()> {
    let notify: RawFd = env::var("INIT_NOTIFY")
        .expect("quanta-gpud must be started by init (INIT_NOTIFY is missing)")
        .parse()
        .expect("INIT_NOTIFY is not a file descriptor");

    let root = scheme.scheme_root()?;
    let cap_fd = socket.create_this_scheme_fd(0, root, 0, 0)?;
    libredox::call::call_wo(
        notify as usize,
        &cap_fd.to_ne_bytes(),
        syscall::CallFlags::FD,
        &[],
    )?;
    Ok(())
}
