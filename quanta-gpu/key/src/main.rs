//! `quanta-key` — inject or probe Redox input events.
//!
//! ```sh
//! quanta-key echo hi          # inject press/release events
//! quanta-key --probe echo hi  # allocate a consumer, activate its VT, inject,
//!                             # then print what the consumer receives
//! ```

#![cfg(target_os = "redox")]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use libredox::flag::O_NONBLOCK;

const EVENT_LEN: usize = 24;

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--drag") {
        let nums: Vec<i32> = args.filter_map(|value| value.parse().ok()).collect();
        if nums.len() == 4 {
            if let Err(err) = inject_drag(nums[0], nums[1], nums[2], nums[3]) {
                eprintln!("quanta-key: drag: {err}");
                std::process::exit(1);
            }
        } else {
            eprintln!("usage: quanta-key --drag X1 Y1 X2 Y2");
            std::process::exit(2);
        }
        return;
    }
    let probe = first.as_deref() == Some("--probe");
    let text = args.collect::<Vec<_>>().join(" ");

    let mut consumer = if probe {
        match OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK as i32)
            .open("/scheme/input/consumer")
        {
            Ok(file) => Some(file),
            Err(err) => {
                eprintln!("quanta-key: open consumer: {err}");
                None
            }
        }
    } else {
        None
    };

    if let Some(consumer) = &consumer {
        match consumer_vt(consumer) {
            Ok(vt) => {
                println!("consumer VT {vt}");
                if let Err(err) = activate_vt(vt) {
                    eprintln!("quanta-key: activate VT {vt}: {err}");
                }
            }
            Err(err) => eprintln!("quanta-key: consumer VT: {err}"),
        }
    }

    if let Err(err) = inject(&text) {
        eprintln!("quanta-key: inject: {err}");
        std::process::exit(1);
    }

    if let Some(mut consumer) = consumer.take() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buffer = [0u8; EVENT_LEN * 16];
        while Instant::now() < deadline {
            match consumer.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    for chunk in buffer[..count].chunks_exact(EVENT_LEN) {
                        let i64_at = |offset: usize| {
                            i64::from_ne_bytes(chunk[offset..offset + 8].try_into().unwrap())
                        };
                        println!("event code={} a={} b={}", i64_at(0), i64_at(8), i64_at(16));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    eprintln!("quanta-key: read: {err}");
                    break;
                }
            }
        }
    }
}

fn consumer_vt(consumer: &File) -> std::io::Result<usize> {
    let mut path = [0u8; 512];
    let len = libredox::call::fpath(consumer.as_raw_fd() as usize, &mut path)
        .map_err(std::io::Error::from)?;
    let text = std::str::from_utf8(&path[..len])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 fpath"))?;
    text.trim_end_matches('\0')
        .rsplit('/')
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected consumer path {text:?}"),
            )
        })
}

fn activate_vt(vt: usize) -> std::io::Result<()> {
    let mut control = OpenOptions::new()
        .write(true)
        .open("/scheme/input/control")?;
    let mut event = Vec::with_capacity(2 * std::mem::size_of::<usize>());
    event.extend_from_slice(&1usize.to_ne_bytes());
    event.extend_from_slice(&vt.to_ne_bytes());
    control.write_all(&event)
}

fn inject(text: &str) -> std::io::Result<()> {
    let mut producer = OpenOptions::new()
        .write(true)
        .open("/scheme/input/producer")?;
    for ch in text.chars() {
        let Some(scancode) = scancode(ch) else {
            eprintln!("quanta-key: no scancode for {ch:?}");
            continue;
        };
        for pressed in [true, false] {
            let mut event = [0u8; EVENT_LEN];
            event[0..8].copy_from_slice(&1i64.to_ne_bytes()); // EVENT_KEY
            event[8..16].copy_from_slice(&(ch as i64).to_ne_bytes());
            let packed = scancode as i64 | ((pressed as i64) << 8);
            event[16..24].copy_from_slice(&packed.to_ne_bytes());
            producer.write_all(&event)?;
        }
    }
    producer.write_all(&[0xf5])
}

fn scancode(ch: char) -> Option<u8> {
    Some(match ch {
        'a'..='z' => LETTERS[(ch as u8 - b'a') as usize],
        '0'..='9' => DIGITS[(ch as u8 - b'0') as usize],
        ' ' => 0x39,
        '\n' | '\r' => 0x1c,
        '\t' => 0x0f,
        '\x7f' => 0x0e,
        '\x1b' => 0x01,
        ':' | ';' => 0x27,
        '/' => 0x35,
        '.' => 0x34,
        ',' => 0x33,
        '-' => 0x0c,
        '=' => 0x0d,
        _ => return None,
    })
}

const LETTERS: [u8; 26] = [
    0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19,
    0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
];
const DIGITS: [u8; 10] = [0x0b, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a];

fn emit(producer: &mut File, code: i64, a: i64, b: i64) -> std::io::Result<()> {
    let mut event = [0u8; EVENT_LEN];
    event[0..8].copy_from_slice(&code.to_ne_bytes());
    event[8..16].copy_from_slice(&a.to_ne_bytes());
    event[16..24].copy_from_slice(&b.to_ne_bytes());
    producer.write_all(&event)
}

fn inject_drag(x1: i32, y1: i32, x2: i32, y2: i32) -> std::io::Result<()> {
    let mut producer = OpenOptions::new()
        .write(true)
        .open("/scheme/input/producer")?;
    // The input consumer uses Orbital's absolute 0..=65535 range.
    let (width, height) = screen_size().unwrap_or((640, 480));
    let norm = |value: i32, limit: u32| -> i64 {
        (value as i64 * 65535 / limit.max(1) as i64).clamp(0, 65535)
    };
    emit(&mut producer, 2, norm(x1, width), norm(y1, height))?; // EVENT_MOUSE
    emit(&mut producer, 3, 1, 0)?; // EVENT_BUTTON, left down
    emit(&mut producer, 2, norm(x2, width), norm(y2, height))?;
    emit(&mut producer, 3, 0, 0)?; // left up
    producer.write_all(&[0xf5])
}

fn screen_size() -> Option<(u32, u32)> {
    let info = std::fs::read_to_string("/scheme/quanta-gpu/info").ok()?;
    let mut width = None;
    let mut height = None;
    for line in info.lines() {
        if let Some(value) = line.strip_prefix("width=") {
            width = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("height=") {
            height = value.trim().parse().ok();
        }
    }
    Some((width?, height?))
}
