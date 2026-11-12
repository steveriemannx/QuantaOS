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
use std::collections::VecDeque;
use std::env;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use quanta_gpu_protocol::InputEvent;
use redox_scheme::scheme::SchemeSync;
use redox_scheme::wrappers::ReadinessBased;
use redox_scheme::Socket;

use crate::scheme::{InputQueue, QuantaScheme, MAX_PENDING_INPUT};

fn main() {
    let mut display = match display::open_with_retry(50) {
        Ok(display) => display,
        Err(err) => {
            eprintln!("quanta-gpud: no usable display: {err:#}");
            std::process::exit(1);
        }
    };

    let activated = display.activate();
    let vt = display.vt();
    {
        eprintln!("quanta-gpud: VT {vt}");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/quanta-gpud.log")
        {
            use std::io::Write;
            let _ = writeln!(file, "vt {vt} activate={activated}");
        }
    }
    let input: InputQueue = Arc::new(Mutex::new(VecDeque::new()));
    match display.take_consumer() {
        Some(consumer) => spawn_input_reader(consumer, Arc::clone(&input)),
        None => eprintln!("quanta-gpud: input consumer was already taken"),
    }
    let mut scheme = QuantaScheme::new(display, input);
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

/// The VT input consumer emits `orbclient::Event { code: i64, a: i64, b: i64 }`
/// records (24 bytes). `code == EVENT_KEY`, `a` is the keymapped character and
/// `b` carries the XT scancode in the low byte plus the pressed bit at 0x100.
/// Forwarding is done on a dedicated thread because the scheme loop blocks on
/// requests; the queue is bounded so a stalled client cannot grow it forever.
fn spawn_input_reader(mut consumer: File, input: InputQueue) {
    thread::spawn(move || {
        const EVENT_LEN: usize = 24;
        const EVENT_KEY: i64 = 1;
        const EVENT_MOUSE: i64 = 2;
        const EVENT_BUTTON: i64 = 3;
        const EVENT_SCROLL: i64 = 4;
        let mut buffer = [0u8; EVENT_LEN * 32];
        let mut stale_reported = false;
        loop {
            match consumer.read(&mut buffer) {
                // inputd returns a zero-length read when the queue is empty;
                // it is not EOF. Keeping the consumer open is also what keeps
                // this VT registered with inputd.
                Ok(0) => thread::sleep(Duration::from_millis(8)),
                Ok(count) => {
                    for chunk in buffer[..count].chunks_exact(EVENT_LEN) {
                        let read_i64 = |offset: usize| {
                            i64::from_ne_bytes(chunk[offset..offset + 8].try_into().unwrap())
                        };
                        let code = read_i64(0);
                        let character = read_i64(8) as u32;
                        let packed = read_i64(16);
                        let scancode = (packed & 0xff) as u16;
                        let pressed = packed & 0x100 != 0;
                        {
                            use std::io::Write;
                            use std::sync::atomic::{AtomicUsize, Ordering};
                            static LOGGED: AtomicUsize = AtomicUsize::new(0);
                            if LOGGED.fetch_add(1, Ordering::Relaxed) < 128 {
                                if let Ok(mut file) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open("/tmp/quanta-gpud.log")
                                {
                                    let _ = writeln!(
                                        file,
                                        "raw code={code} ch={character} packed={packed} len={count}"
                                    );
                                }
                            }
                        }
                        let event = match code {
                            EVENT_KEY => InputEvent::key(character, scancode, pressed),
                            EVENT_MOUSE => InputEvent::mouse(character as i32, packed as i32),
                            EVENT_BUTTON => InputEvent::button(character as i32),
                            EVENT_SCROLL => InputEvent::scroll(character as i32, packed as i32),
                            _ => continue,
                        };
                        let mut queue = input.lock().unwrap();
                        if queue.len() >= MAX_PENDING_INPUT {
                            queue.pop_front();
                        }
                        queue.push_back(event);
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(8));
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => {
                    use std::io::Write;
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("/tmp/quanta-gpud.log")
                        .and_then(|mut file| writeln!(file, "input read error: {err}"));
                    if !stale_reported {
                        eprintln!("quanta-gpud: input consumer read failed: {err}");
                        stale_reported = true;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });
}
