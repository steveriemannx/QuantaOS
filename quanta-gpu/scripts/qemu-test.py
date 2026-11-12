#!/usr/bin/env python3
"""Headless QEMU smoke test for QuantaGPU.

Boots ``build/x86_64/quanta-gpu/harddrive.img`` with OVMF and virtio-vga,
waits for ``quanta-demo`` to connect to ``/scheme/quanta-gpu``, then captures
the virtio-gpu scanout through the QEMU monitor and checks it with Pillow.

The test deliberately uses ``-display none`` and sockets for the monitor and
serial console.  It never opens a GUI window, never uses the QEMU stdio
multiplexer, and only prints text results: the DSH harness crashes when it is
asked to decode/display an image with its Electron-incompatible sharp build,
so this script must not hand screenshots back to the model as images.

Usage:
    python3 quanta-gpu/scripts/qemu-test.py            # build image first
    IMAGE=/path/to/harddrive.img python3 ...           # custom image
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
GLYPHS = [
    [0b00111100, 0b01000010, 0b01000010, 0b01000010, 0b01001010, 0b01000100, 0b00111010, 0b00000001],
    [0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b01000010, 0b00111100, 0b00000000],
    [0b00011000, 0b00100100, 0b01000010, 0b01000010, 0b01111110, 0b01000010, 0b01000010, 0b00000000],
    [0b01000010, 0b01100010, 0b01010010, 0b01001010, 0b01000110, 0b01000010, 0b01000010, 0b00000000],
    [0b01111110, 0b00011000, 0b00011000, 0b00011000, 0b00011000, 0b00011000, 0b00011000, 0b00000000],
    [0b00011000, 0b00100100, 0b01000010, 0b01000010, 0b01111110, 0b01000010, 0b01000010, 0b00000000],
]
RED = (220, 50, 47)
GREEN = (133, 153, 0)
BLUE = (38, 139, 210)
WHITE = (255, 255, 255)
YELLOW = (255, 214, 0)
PANEL = (18, 20, 28)


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


def hmp(sock_path: Path, command: str) -> str:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(3)
    sock.connect(str(sock_path))
    try:
        try:
            sock.recv(8192)
        except socket.timeout:
            pass
        sock.sendall(command.encode() + b"\n")
        time.sleep(0.6)
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
    px = im.load()
    width, height = im.size
    spots = {
        "top-left": (100, 100, RED),
        "top-right": (width - 100, 100, GREEN),
        "bottom-left": (100, height - 100, BLUE),
        "bottom-right": (width - 100, height - 100, WHITE),
    }
    failures = []
    for name, (x, y, expected) in spots.items():
        actual = px[x, y]
        # The full-height yellow bar can cover any sample point; that is still
        # a valid rendered frame.
        if actual != expected and actual != YELLOW:
            failures.append(f"{name}: {actual} != {expected}")

    bar = [x for x in range(width) if px[x, 10] == YELLOW]
    if not bar:
        failures.append("yellow bar missing")
    elif len(bar) != 64:
        failures.append(f"yellow bar width {len(bar)} != 64")

    # Panel geometry from quanta-demo: 25% x 30%, 50% x 34%.
    panel_x = width // 4
    panel_y = int(height * 0.3)
    panel_w = width // 2
    panel_h = int(height * 0.34)
    if px[panel_x + panel_w // 2, panel_y + panel_h // 2] not in (PANEL, YELLOW):
        failures.append("panel interior is not the demo colour")

    # Glyphs: 32x32 cells starting at the same place quanta-demo uses.
    text_x = panel_x + (panel_w - 6 * 32) // 2
    text_y = panel_y + panel_h - 32 - 20
    matches = 0
    for index, glyph in enumerate(GLYPHS):
        for row in range(8):
            for column in range(8):
                sx = text_x + index * 32 + column * 4
                sy = text_y + row * 4
                total = sum(px[sx + dx, sy + dy][0] for dx in range(4) for dy in range(4))
                on = total / 16 > 128
                expected = bool((glyph[row] >> (7 - column)) & 1)
                matches += on == expected
    glyph_ratio = matches / (len(GLYPHS) * 64)
    return failures, (min(bar), max(bar)) if bar else None, glyph_ratio


def main() -> int:
    if not IMAGE.exists():
        print(f"image not found: {IMAGE}", file=sys.stderr)
        print("build it with: make CONFIG_NAME=quanta-gpu ... build/x86_64/quanta-gpu/harddrive.img", file=sys.stderr)
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
        # OVMF leaves the Redox bootloader waiting for a mode selection.  Read
        # until the menu is actually on the wire before pressing Enter, then
        # keep reading until the demo has opened the scheme.
        boot = b""
        entered = False
        last_key = 0.0
        deadline = time.time() + 55
        while time.time() < deadline:
            try:
                data = s.recv(65536)
            except socket.timeout:
                data = b""
            if data:
                boot += data
                if b"1280x800 stride" in boot:
                    break
            if b"Arrow keys" in boot or b"best resolution" in boot:
                # The UEFI console is the serial port, so select the default
                # boot mode there. The bootloader may still be switching modes
                # when the menu text first appears, so retry until the kernel
                # starts. The driver re-activates its VT on the first frame, so
                # this input cannot leave the console VT covering QuantaGPU.
                now = time.time()
                if not entered:
                    s.sendall(b"\r")
                    entered = True
                    last_key = now
                elif b"Redox OS starting" not in boot and now - last_key > 3.0:
                    s.sendall(b"\r")
                    last_key = now
        text = boot.decode("utf-8", "replace")
        if "1280x800 stride" not in text:
            print("FAIL: quanta-demo never connected to /scheme/quanta-gpu")
            print(text[-3000:])
            return 1
        print("boot: quanta-demo connected")

        # The first v1 frame includes a CPU shadow pass and the VT handoff;
        # under QEMU TCG it can take a while. Wait for a frame with the yellow
        # bar instead of assuming a fixed delay.
        wait_shot = screen_dir / "quanta-test-wait.ppm"
        deadline = time.time() + 90
        first = None
        while time.time() < deadline:
            # Keep the serial console drained: the demo and driver log there,
            # and a full console buffer must never back-pressure the renderer.
            try:
                chunk = serial_drain(s, 0.05)
            except Exception:
                chunk = b""
            if b"quanta-demo" in chunk:
                print("demo:", chunk.decode("utf-8", "replace").replace("\r", " ")[-300:])
            screendump(mon, wait_shot)
            try:
                failures, bar, glyph = analyse(wait_shot)
            except OSError:
                time.sleep(1.0)
                continue
            if bar is not None:
                first = (failures, bar, glyph)
                print(f"first frame: bar={bar} glyph={glyph:.3f} core={failures}")
                break
            time.sleep(1.5)
        if first is None:
            print("FAIL: no frame reached the scanout within 90s")
            try:
                tail = serial_drain(s, 1.0)
                if tail:
                    print("serial tail:", tail.decode("utf-8", "replace")[-2000:])
            except Exception:
                pass
            return 1

        # Two frames separated by more than one present must show the moving
        # bar at different positions.
        shot_a = screen_dir / "quanta-test-a.ppm"
        shot_b = screen_dir / "quanta-test-b.ppm"
        screendump(mon, shot_a)
        time.sleep(3.0)
        screendump(mon, shot_b)
        fa, bara, ga = analyse(shot_a)
        fb, barb, gb = analyse(shot_b)
        print(f"frame A: bar={bara} glyph={ga:.3f} core={fa}")
        print(f"frame B: bar={barb} glyph={gb:.3f} core={fb}")
        if fa or fb:
            print("FAIL: core framebuffer checks failed")
            return 1
        if not bara or not barb or bara == barb:
            print("FAIL: the yellow bar did not advance between frames")
            return 1

        # Find one frame where the bar does not cover the QUANTA glyph row and
        # verify the atlas sampling bit-for-bit.
        best = (0.0, None)
        for index in range(16):
            shot = screen_dir / f"quanta-test-glyph-{index}.ppm"
            screendump(mon, shot)
            try:
                failures, bar, glyph = analyse(shot)
            except OSError:
                time.sleep(0.7)
                continue
            if glyph > best[0]:
                best = (glyph, bar)
            if not failures and glyph >= 0.98:
                print(f"glyphs: {glyph:.3f} at bar={bar}")
                print("PASS: v1 protocol frames reach the virtio-gpu scanout")
                return 0
            time.sleep(0.7)
        print(f"FAIL: glyph check did not find a clean frame (best {best[0]:.3f}, bar={best[1]})")
        return 1
        return 1
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
