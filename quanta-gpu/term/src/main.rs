//! QuantaOS desktop and terminal on the pure-Rust QuantaGPU protocol.
//!
//! The window, terminal text and desktop panel are all v1 records; the driver
//! executes them with `quanta-gpu-raster`. The shell runs on `/dev/ptmx` and
//! keyboard events arrive through `/scheme/quanta-gpu/input`, which
//! `quanta-gpud` forwards from its VT consumer.

#![cfg(target_os = "redox")]

mod font;
mod session;
mod terminal;
mod ui;
mod wallpaper;

use std::thread;
use std::time::{Duration, Instant};

use quanta_gpu_client::{Connection, InputEvent, INPUT_BUTTON, INPUT_KEY, INPUT_MOUSE};

use crate::session::Session;
use crate::terminal::Terminal;
use crate::ui::Desktop;

// XT/orbclient scancodes used by the VT keymap.
const K_ESC: u16 = 0x01;
const K_BKSP: u16 = 0x0e;
const K_TAB: u16 = 0x0f;
const K_ENTER: u16 = 0x1c;
const K_LEFT_CTRL: u16 = 0x1d;
const K_RIGHT_CTRL: u16 = 0x64;
const K_HOME: u16 = 0x47;
const K_UP: u16 = 0x48;
const K_LEFT: u16 = 0x4b;
const K_RIGHT: u16 = 0x4d;
const K_END: u16 = 0x4f;
const K_DOWN: u16 = 0x50;

fn main() {
    let mut shell = std::env::var("SHELL").unwrap_or_else(|_| "/usr/bin/ion".to_string());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shell" => {
                if let Some(value) = args.next() {
                    shell = value;
                }
            }
            "--help" | "-h" => {
                println!("usage: quanta-term [--shell PATH]");
                return;
            }
            other => {
                eprintln!("quanta-term: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let mut conn = connect();
    let desktop = Desktop::new(conn.width(), conn.height());
    let layout = desktop.layout();
    let cols = layout.cols;
    let rows = layout.rows;
    eprintln!(
        "quanta-term: {}x{} desktop, terminal {}x{}",
        conn.width(),
        conn.height(),
        cols,
        rows
    );

    let mut term = Terminal::new(cols, rows);
    let mut desktop = desktop;
    let mut session = match Session::spawn(&shell, cols as u16, rows as u16) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("quanta-term: could not start {shell}: {err}");
            std::process::exit(1);
        }
    };

    let trace = std::env::var_os("QUANTA_TERM_TRACE").is_some();
    trace_note("started");
    let mut ctrl = false;
    let mut input_events = Vec::new();
    let mut pty_bytes = Vec::new();
    let mut dirty = true;
    let mut last_present = Instant::now() - Duration::from_secs(1);

    loop {
        input_events.clear();
        match conn.read_input(&mut input_events) {
            Ok(_) => {}
            Err(err) => eprintln!("quanta-term: input read failed: {err}"),
        }
        for event in &input_events {
            match event.kind {
                INPUT_MOUSE => {
                    desktop.pointer(event.x, event.y);
                    dirty = true;
                    continue;
                }
                INPUT_BUTTON => {
                    desktop.button(event.x);
                    dirty = true;
                    continue;
                }
                _ => {}
            }
            if let Some(bytes) = key_bytes(event, &mut ctrl) {
                trace_note(&format!("key {event:?} -> {bytes:?}"));
                if trace {
                    eprintln!("quanta-term: key {:?} -> {:?}", event, bytes);
                }
                if let Err(err) = session.write(&bytes) {
                    eprintln!("quanta-term: pty write failed: {err}");
                }
            }
        }

        pty_bytes.clear();
        match session.read(&mut pty_bytes) {
            Ok(0) => {}
            Ok(_) => {
                trace_note(&format!("pty {} bytes", pty_bytes.len()));
                if trace {
                    eprintln!(
                        "quanta-term: pty -> {:?}",
                        String::from_utf8_lossy(&pty_bytes)
                    );
                }
                term.write(&pty_bytes);
                dirty = true;
            }
            Err(err) => eprintln!("quanta-term: pty read failed: {err}"),
        }
        let response = term.take_input();
        if !response.is_empty() {
            let _ = session.write(&response);
        }

        // Present at least once a second even when idle. Besides flushing a
        // late first frame, this re-asserts the VT: inputd only routes the
        // keyboard to the consumer of the active VT.
        if last_present.elapsed() >= Duration::from_secs(1) {
            dirty = true;
        }
        if dirty && last_present.elapsed() >= Duration::from_millis(33) {
            match desktop.render(&mut conn, &term) {
                Ok(_) => {
                    last_present = Instant::now();
                    dirty = false;
                }
                Err(err) => {
                    eprintln!("quanta-term: present failed: {err}");
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }

        if session.exited() {
            // Give the final output one more frame, then stay on screen. The
            // image is a demo, so exiting here would leave the VT black.
            if dirty {
                let _ = desktop.render(&mut conn, &term);
            }
            thread::sleep(Duration::from_millis(500));
        } else {
            thread::sleep(Duration::from_millis(8));
        }
    }
}

fn connect() -> Connection {
    for _ in 0..100 {
        match Connection::open() {
            Ok(conn) => return conn,
            Err(err) => {
                eprintln!("quanta-term: waiting for quanta-gpu: {err}");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
    eprintln!("quanta-term: quanta-gpu did not appear");
    std::process::exit(1);
}

fn key_bytes(event: &InputEvent, ctrl: &mut bool) -> Option<Vec<u8>> {
    if event.kind != INPUT_KEY {
        return None;
    }
    if event.code == K_LEFT_CTRL || event.code == K_RIGHT_CTRL {
        *ctrl = event.pressed;
        return None;
    }
    if !event.pressed {
        return None;
    }
    let bytes = match event.code {
        K_ENTER => b"\r".to_vec(),
        K_BKSP => b"\x7f".to_vec(),
        K_TAB => b"\t".to_vec(),
        K_ESC => b"\x1b".to_vec(),
        K_UP => b"\x1b[A".to_vec(),
        K_DOWN => b"\x1b[B".to_vec(),
        K_RIGHT => b"\x1b[C".to_vec(),
        K_LEFT => b"\x1b[D".to_vec(),
        K_HOME => b"\x1b[H".to_vec(),
        K_END => b"\x1b[F".to_vec(),
        _ => {
            let ch = char::from_u32(event.x as u32).unwrap_or('\0');
            if *ctrl && ch.is_ascii() {
                let byte = (ch.to_ascii_lowercase() as u8) & 0x1f;
                if (1..=26).contains(&byte) {
                    return Some(vec![byte]);
                }
            }
            if ch == '\0' {
                return None;
            }
            let mut bytes = [0u8; 4];
            ch.encode_utf8(&mut bytes).as_bytes().to_vec()
        }
    };
    Some(bytes)
}

fn trace_note(message: &str) {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNT: AtomicUsize = AtomicUsize::new(0);
    if COUNT.fetch_add(1, Ordering::Relaxed) >= 128 {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/quanta-term.log")
    {
        let _ = writeln!(file, "{message}");
    }
}
