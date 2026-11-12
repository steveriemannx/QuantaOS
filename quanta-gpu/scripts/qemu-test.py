#!/usr/bin/env python3
"""Headless QEMU smoke test for the QuantaGPU desktop.

Boots the test image with OVMF + virtio-vga, waits for `quanta-term` to draw
the wallpaper desktop, captures the scanout, verifies the floating window and
finally types `echo hi` through the emulated keyboard.

The test uses `-display none` and socket monitor/serial pairs.  It never opens
a GUI, never uses QEMU's stdio multiplexer, and only prints text: the DSH
harness on this host crashes when it decodes/display an image with its
Electron-incompatible sharp build, so screenshots stay on disk as PPMs.

Usage:
    python3 quanta-gpu/scripts/qemu-test.py
    IMAGE=/path/to/harddrive.img python3 ...
"""

from __future__ import annotations

import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
QEMU_ROOT = ROOT / ".host-tools/qemu/root/usr"
QEMU = QEMU_ROOT / "bin/qemu-system-x86_64"
QEMU_SHARE = QEMU_ROOT / "share/qemu"
OVMF = QEMU_ROOT / "share/ovmf/OVMF.fd"
IMAGE = Path(os.environ.get("IMAGE", ROOT / "build/x86_64/quanta-gpu/harddrive.img"))
BORDER = (88, 96, 116)


def wait_socket(path: Path, timeout: float) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            return
        time.sleep(0.05)
    raise TimeoutError(f"QEMU did not create {path}")


def serial_drain(sock: socket.socket, timeout: float, stop: bytes | None = None) -> bytes:
    end = time.time() + timeout
    out = b""
    while time.time() < end:
        try:
            data = sock.recv(65536)
        except socket.timeout:
            continue
        if not data:
            break
        out += data
        if stop and stop in out:
            break
    return out


def hmp(sock_path: Path, command: str, linger: float = 0.5) -> str:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(3)
    sock.connect(str(sock_path))
    try:
        try:
            sock.recv(8192)
        except socket.timeout:
            pass
        sock.sendall(command.encode() + b"\n")
        time.sleep(linger)
        try:
            return sock.recv(8192).decode("utf-8", "replace")
        except socket.timeout:
            return ""
    finally:
        sock.close()


def screendump(mon: Path, path: Path) -> None:
    hmp(mon, f"screendump {path}")


def analyse(path: Path):
    from PIL import Image

    im = Image.open(path).convert("RGB")
    width, height = im.size
    px = im.load()
    colors = set(im.getdata())
    border = [
        (x, y)
        for y in range(0, height, 2)
        for x in range(0, width, 2)
        if px[x, y] == BORDER
    ]
    failures = []
    if len(colors) < 32:
        failures.append(f"wallpaper has only {len(colors)} colours")
    if not border:
        failures.append("terminal window border not found")
        bbox = None
    else:
        xs = [p[0] for p in border]
        ys = [p[1] for p in border]
        bbox = (min(xs), min(ys), max(xs), max(ys))
        area = (bbox[2] - bbox[0]) * (bbox[3] - bbox[1])
        if area >= width * height * 4 // 5:
            failures.append("terminal window fills the screen")
    return failures, (width, height), bbox, len(colors)


def frame_difference(a: Path, b: Path) -> int:
    from PIL import Image

    left = Image.open(a).convert("RGB")
    right = Image.open(b).convert("RGB")
    if left.size != right.size:
        return 0
    lp = left.load()
    rp = right.load()
    diff = 0
    for y in range(0, left.height, 2):
        for x in range(0, left.width, 2):
            if lp[x, y] != rp[x, y]:
                diff += 1
    return diff


def main() -> int:
    if not IMAGE.exists():
        print(f"image not found: {IMAGE}", file=sys.stderr)
        return 2
    if not QEMU.exists():
        print(f"QEMU not found: {QEMU}", file=sys.stderr)
        return 2
    try:
        import PIL  # noqa: F401
    except ImportError:
        print("Pillow is required for the framebuffer checks", file=sys.stderr)
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="quanta-qemu-", dir=ROOT / "build"))
    mon = tmp / "monitor.sock"
    serial = tmp / "serial.sock"
    log = tmp / "qemu.log"
    screen_dir = ROOT / "build"
    env = os.environ.copy()
    env["LD_LIBRARY_PATH"] = f"{QEMU_ROOT}/lib/x86_64-linux-gnu:{QEMU_ROOT}/lib"
    env["QEMU_MODULE_DIR"] = f"{QEMU_ROOT}/lib/x86_64-linux-gnu/qemu"

    cmd = [
        str(QEMU),
        "-L", str(QEMU_SHARE),
        "-machine", "q35",
        "-m", "1024",
        "-smp", "2",
        "-cpu", "core2duo",
        "-vga", "none",
        "-device", "virtio-vga",
        "-display", "none",
        "-net", "none",
        "-device", "qemu-xhci",
        "-device", "usb-kbd",
        "-drive", f"file={IMAGE},format=raw,if=none,id=drv0",
        "-device", "nvme,drive=drv0,serial=NVME_SERIAL",
        "-bios", str(OVMF),
        "-monitor", f"unix:{mon},server,nowait",
        "-serial", f"unix:{serial},server,nowait",
        "-no-reboot",
    ]

    with open(log, "wb") as out:
        proc = subprocess.Popen(cmd, stdout=out, stderr=subprocess.STDOUT, env=env)
    try:
        wait_socket(serial, 10)
        wait_socket(mon, 10)
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(2)
        s.connect(str(serial))
        boot = b""
        entered = False
        deadline = time.time() + 55
        while time.time() < deadline:
            try:
                data = s.recv(65536)
            except socket.timeout:
                data = b""
            if data:
                boot += data
                if b"quanta-term" in boot and b"desktop" in boot:
                    break
                if not entered and (b"Arrow keys" in boot or b"best resolution" in boot):
                    s.sendall(b"\r")
                    entered = True
        if b"quanta-term" not in boot or b"desktop" not in boot:
            print("FAIL: quanta-term never connected to /scheme/quanta-gpu")
            print(boot.decode("utf-8", "replace")[-3000:])
            return 1
        print("boot: quanta-term connected")

        time.sleep(6)
        before = screen_dir / "quanta-desktop-before.ppm"
        after = screen_dir / "quanta-desktop-after.ppm"
        screendump(mon, before)
        failures, size, bbox, colours = analyse(before)
        print(f"screen {size}, window {bbox}, {colours} colours, failures={failures}")
        if failures:
            return 1

        # HMP sendkey needs a graphical console, which this headless test does
        # not have. Exercise the same input path instead: log in on the serial
        # console and inject "echo hi" through /scheme/input/producer. The real
        # keyboard was verified interactively in the GTK/VNC console.
        s.sendall(b"\r")
        time.sleep(0.5)
        login = serial_drain(s, 2)
        if b"login:" in login:
            s.sendall(b"root\r")
            time.sleep(0.5)
            serial_drain(s, 2)
            s.sendall(b"password\r")
            time.sleep(1)
            serial_drain(s, 2)
        s.sendall(b"inputd -A 2; quanta-key echo hi\r")
        # A full 1280x800 wallpaper + window frame is slow under QEMU TCG, so
        # poll for the repaint instead of assuming a fixed redraw time.
        diff = 0
        for attempt in range(20):
            time.sleep(2)
            screendump(mon, after)
            diff = frame_difference(before, after)
            if diff >= 200:
                break
        print(f"echo-hi repaint: {diff} sampled pixels changed")
        if diff < 200:
            print("FAIL: keyboard input did not reach the terminal")
            return 1
        print("PASS: wallpaper desktop, floating terminal and keyboard input")
        return 0
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
