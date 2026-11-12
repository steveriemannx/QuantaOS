# QuantaGPU — Rust-native GPU protocol for QuantaOS

Goal: give FerriticUI a GPU path whose **protocol, driver and client are all Rust**,
with **no Vulkan and no C API** anywhere in the QuantaOS image. We do not need a
general-purpose GPU stack: exactly one simple device, one scanout, one client model.

This document is the design baseline. It is deliberately narrower than Vulkan/DRM.

Status (2026-11-12): the v0 present path, the v1 pure-Rust render path and a
small desktop are implemented. The image boots a wallpaper desktop with a
floating terminal window; `quanta-term` writes the wallpaper through the v0
shared surface and draws the window, title, terminal text and pointer with v1
`Rect`/`Shadow`/`Glyph`/`Atlas` records, which `quanta-gpud` executes with
`quanta-gpu-raster` (SDF rasterizer, no C, no Vulkan). The terminal runs `ion`
on a PTY and receives key/mouse events forwarded from the driver's VT input
consumer. `quanta-demo` still exercises the protocol-only scene (quadrants,
panel, `QUANTA` glyphs, moving bar) and its glyphs match the atlas exactly.
The virgl executor is still optional and not required for a working image.

## 1. What "pure Rust GPU protocol" means here

Three layers, all Rust:

```text
FerriticUI (ferritic-core Scene)
        │  encodes commands with quanta-gpu-protocol
        ▼
ferritic-quanta / quanta-gpu-client   (client backend, Rust library)
        │  /scheme/quanta-gpu  (Rust-defined command format, shared memory)
        ▼
quanta-gpud                           (userspace driver / display service, Rust)
        ├─ quanta-gpu-raster           (pure-Rust v1 executor: SDF/glyph)
        │  virtio-gpu 2D (and later virgl 3D) encoded in Rust
        ▼
virtio-gpud (existing, pure Rust)  →  QEMU / Cloud Hypervisor / virtio hardware
```

* `quanta-gpu-protocol` is a dependency-free crate: versioned little-endian wire
  format, no `bindgen`, no `libc`, no `drm` FFI, no Vulkan.
* `quanta-gpud` is a normal Redox userspace driver. It owns the display through the
  existing pure-Rust `virtio-gpud` display scheme and serves `/scheme/quanta-gpu`.
* `quanta-gpu-raster` is the dependency-free reference executor for the v1 draw
  records. It gives the protocol testable semantics on the host and a working
  render path on machines without virgl.
* `ferritic-quanta` is the FerriticUI backend. It converts `Prim::Rect`,
  `Prim::Shadow` and `Prim::Glyph` into QuantaGPU commands.

DRM ioctls / virtio register layouts are *numbers and structs*, not a C ABI at the
protocol boundary; we define the few we use ourselves in Rust. FerriticUI never
links Mesa, Vulkan, wgpu, blade or any C library.

## 2. Hardware reality (important)

virtio-gpu has two command sets:

| Mode | Guest commands | Can it draw? | Guest Rust? | Host requirement |
|---|---|---|---|---|
| 2D | resource create/attach, transfer, flush, scanout, cursor | **No.** No fill/blit/draw command. | Yes | none |
| 3D (virgl) | context/resource 3D + Gallium command buffers | Yes (GPU shader) | Yes | QEMU/CH + virglrenderer (C) |
| 3D (venus) | Vulkan over virtio | Yes | Yes | Vulkan host; **excluded by the no-Vulkan rule** |

Consequences:

* A fully Rust, dependency-free protocol **on top of virtio-gpu 2D** can make the
  whole display path Rust, but the GPU only scans out a framebuffer. SDF/glyph
  rasterization stays on the CPU (reuse `ferritic-soft`).
* If the GPU must actually execute per-primitive shading without Vulkan, the only
  available device protocol is **virgl**. The guest side (protocol, driver, command
  encoding, TGSI shaders) can be 100% Rust. The host emulator (QEMU + virglrenderer)
  is C, but it is not part of the QuantaOS image or its licence/language closure.
* "No C even on the host" is not achievable with QEMU. It would require a native
  driver for one real GPU (e.g. Intel via `ihdgd`), which is a much larger project.

## 3. Protocol v0 — present path (works on virtio-gpu 2D)

v0 makes `ferritic-quanta` a fullscreen client of a minimal display service.
Frames are rasterized by `ferritic-soft` into a shared `argb8888` surface; the driver
only transfers damage and flips. This is the shortest path to an all-Rust image and
is testable in QEMU today.

### Transport

* `/scheme/quanta-gpu/info` — read-only device description:
  width, height, stride, format, scale, and a `features` list. `present` is
  mandatory; `render` means the driver executes the v1 draw records; `submit`
  means a raw backend stream is accepted. Unknown features are ignored, and an
  older driver without the key is treated as `present` only.
* `/scheme/quanta-gpu/fb` — mmap RW; the scanout surface (one fullscreen
  surface in v0).
* `/scheme/quanta-gpu/ctl` — write a `Present` batch; read fences/VT events.

The surface is allocated by `quanta-gpud` through the existing virtio-gpu
driver's dumb-buffer interface. The client never calls DRM or virtio directly.

### Frame format

Implemented in [`quanta-gpu/protocol`](quanta-gpu/protocol/src/lib.rs) with
host-side encode/decode tests.

```text
Header (40 bytes, LE)
  magic      u32   b"QGPU"
  version    u16   0
  hdr_len    u16   40
  seq        u64   monotonically increasing
  width      u32   physical pixels
  height     u32
  scale_bits u32   f32 logical→physical scale
  cmd_len    u32   bytes following the header
  flags      u32   must be zero in v0
  reserved   u32

Record (4-byte aligned)
  op         u16
  flags      u16   must be zero in v0
  len        u32   payload bytes
  payload    [u8; len]
  pad        to 4 bytes

Ops (v0)
  1  Present   { damage_count u32, then damage_count × { x,y,w,h u32 } }
  2  Fence     { token u64 }
  3  SetCursor { x i32, y i32, visible u8, pad[3] }      (optional)
```

Rules: unknown ops are skipped; a frame is applied atomically at `Present`;
damage is intersected with the surface; `seq` must increase; malformed frames are
rejected without touching the scanout. Drivers validate the header and surface
before executing any draw operation (`peek_frame_info`).

### Driver implementation

1. Open `/scheme/input/consumer`, obtain the active VT display handle
   (`inputd::ConsumerHandle::open_display`).
2. Create two dumb buffers, map them, set the CRTC, page-flip on `Present`.
3. Serve `/scheme/quanta-gpu` using `redox-scheme`; expose surfaces via mmap.
4. On `Present`, transfer the damage rectangles to the host
   (`TRANSFER_TO_HOST_2D`) and flush; flip; report a fence.
5. Handle VT switch/hotplug minimally: one connector, no planes, no EDID, no
   multi-monitor. This is the "simple, not general GPU" scope.

`ferritic-quanta` maps one surface, runs `ferritic-soft::SoftRenderer` into it,
and calls `Present`. No Vulkan, no Orbital, no C.

### Driver fence acknowledgements

Every successful `Present` queues a fence token on `/scheme/quanta-gpu/ctl`:
the explicit `Fence` token if the frame carried one, otherwise the frame `seq`.
Clients read one `u64` per `read`; the queue is bounded so a client that never
reads cannot grow driver memory without limit. This gives clients a simple way
to throttle without inventing a timing protocol.

## 4. Protocol v1 — render path (pure-Rust executor now, virgl later)

The same protocol gains a render layer; the driver chooses where to execute it.
The command types are already encoded by the protocol crate.

```text
Ops (v1)
  10 Clear    { rgba u8×4 }
  11 Rect     { x,y,w,h, radius, border_width: f32,
                fill rgba, border rgba, clip: Option<Rect> }
  12 Shadow   { x,y,w,h, radius, blur, offset_x, offset_y: f32,
                color rgba, clip: Option<Rect> }
  13 Glyph    { x,y,w,h: f32, u0,v0,u1,v1: f32, color rgba,
                clip: Option<Rect> }
  14 Atlas    { atlas_w, atlas_h, x, y, w, h u32, A8 bytes }
  15 Submit   { backend_id u32, bytes }   // raw backend stream, e.g. virgl
```

`Submit` is ignored by the current software executor; a virgl driver will
advertise the `submit` feature and execute it.

These commands are exactly the three `Prim` variants FerriticUI emits.

### Software executor (implemented)

[`quanta-gpu-raster`](quanta-gpu/raster/src/lib.rs) is the reference
implementation. It is used by `quanta-gpud` when the `render` feature is
advertised, and by host tests as the semantic definition of the draw records:

* Rectangle/shadow: the same SDF and 1px analytic AA as `ferritic-soft`.
* Glyph: textured quad sampling the persistent A8 atlas with bilinear filtering.
* Scissor: the clip rectangle intersects both the drawn pixels and the damage
  reported to the scanout path.
* Damage is computed per primitive, unioned with the frame's explicit `Present`
  damage, clipped to the surface, and capped like a protocol damage list.

The executor is pure Rust and `#![forbid(unsafe_code)]`. It is not a GPU
shader, but it makes the wire protocol complete and testable today; the same
records can later be executed by the virgl backend without changing clients.

### virgl executor (optional)

### virgl executor

The following work is only needed to move the same commands onto the GPU:

* Extend the pure-Rust virtio-gpu driver to negotiate `VIRTIO_GPU_F_VIRGL`, create
  a 3D context (capset 1), and submit Gallium command buffers. The existing driver
  already has 2D resource/scanout/transfer code; 3D adds context, 3D resources and
  `SUBMIT_3D`.
* Encode virgl commands and TGSI shaders in Rust (`quanta-gpu-virgl`). No Vulkan.
* The host needs `virglrenderer`; QEMU's `virtio-gpu-gl-pci` is used only when that
  executor is enabled. The 2D executor remains the fallback.

This is the only route to real GPU shading without Vulkan on this device. It is
also the largest work item; it is *not* required for an all-Rust image.

## 5. Phases

| Phase | Deliverable | GPU renders? | Verifiable here? |
|---|---|---|---|
| v0 | protocol crate + `quanta-gpud` 2D + `ferritic-quanta` + fullscreen demo | no (scanout only) | yes, QEMU `gpu=virtio` + screendump |
| v1s | `quanta-gpu-raster` executes Clear/Rect/Shadow/Glyph/Atlas in `quanta-gpud`; client `frame()` API; `quanta-demo` renders the scene with protocol records | no (driver CPU, GPU-shaped commands) | **yes, verified** (host tests + QEMU screendump) |
| v1a | virgl driver bring-up: clear screen via a Rust virgl command buffer | yes | requires host virglrenderer; installable (247 KB) |
| v1b | SDF/glyph shaders + draw commands; FerriticUI on GPU | yes | host virglrenderer + NVIDIA/EGL |
| v2 | input/window model (multiple surfaces or Orbital integration) | — | QEMU |

## 6. Tests

* Host: protocol encode/decode round-trip, malformed-input rejection, sequencing,
  damage clipping, device-info parsing and feature advertisement.
* Host: `quanta-gpu-raster` pixel tests for fills, borders, rounded-corner AA,
  shadow falloff, clip, stride padding, glyph sampling, scale and damage.
* QEMU: boot with `gpu=virtio`, run `quanta-demo --v1` and `--surface`, capture
  the framebuffer via the QEMU monitor (`screendump`) and compare the two paths.
* v1 virgl: a Rust guest test that submits a minimal virgl clear/draw and
  verifies the screendump; then the Ferritic SDF/glyph golden set.

### QEMU verification (implemented)

The host test is [`quanta-gpu/scripts/qemu-test.py`](quanta-gpu/scripts/qemu-test.py).
It boots `build/x86_64/quanta-gpu/harddrive.img` with OVMF + `virtio-vga`,
`-display none`, and a QEMU monitor/serial socket pair, waits for
`quanta-term`, verifies the wallpaper and floating window with Pillow, then
injects `echo hi` through the input producer and checks that the terminal
repaints:

```sh
python3 quanta-gpu/scripts/qemu-test.py
```

Two non-obvious boot/protocol requirements were found while bringing this up:

* **UEFI, not SeaBIOS.** The image is assembled with the UEFI bootloader. With
  SeaBIOS the boot stops on a 720x400 text screen; `-bios .../ovmf/OVMF.fd`
  (or an equivalent pflash setup) reaches the Redox kernel and the
  virtio-gpu scanout.
* **The scheme must implement `fstat`/`getdents`, and `ctl` must not be
  `POSITIONED`.** The VFS calls `fstat` on every opened scheme path: without it
  `cat /scheme/quanta-gpu/info` fails with `EOPNOTSUPP` and clients see an
  empty device description. `ctl` is a message stream: if the kernel advances
  a file position, the second frame write arrives at a non-zero offset and is
  rejected with `EINVAL`, freezing the demo on its first frame. `fb`/`ctl` are
  therefore non-positioned and the driver ignores the stream offset. The client
  also opens `ctl` `O_NONBLOCK`, as `read_fence` is specified to report
  `WouldBlock` rather than sleep.
* **VT ownership is a race with the console handoff.** `fbcond` can activate
  the console VT after `quanta-gpud` has opened the display. The driver claims
  the VT back before the first `Present`; doing it after the flush can deadlock
  because the scanout flush itself waits for the display. Keep the headless
  test's serial console drained: the terminal and driver log there and a full
  console buffer must not back-pressure the renderer.
* **inputd returns a zero-length read when its queue is empty.** That is not
  EOF. The forwarding thread must keep the consumer open, otherwise inputd
  removes the VT and later activation fails with "non-existent VT".

Verified results on 2026-11-12: `info` reports version 0, 1280x800, stride
5120, `argb8888`, features `present,render`; `ls -l /scheme/quanta-gpu` lists
`ctl`, `fb`, `info`, `input`; the 1280x800 screendump has 222 colours, the
wallpaper and a floating 960x533 terminal window; `echo hi` typed through the
GTK console prints `hi`; `quanta-key --drag` moves the window and restores the
wallpaper underneath; the protocol-only demo still shows the four quadrant
colours, panel, moving bar and a 100% glyph-bitmap match.

Safety note for automation: the DSH harness on this host crashes its host
process when asked to decode/display an image with its Electron-incompatible
`sharp` build. The test keeps every screenshot as a PPM/PNG on disk and only
prints colour/geometry statistics; do not feed the captured image back through
an image tool in that environment.

## 7. Desktop and terminal (v1 client)

[`quanta-gpu/term`](quanta-gpu/term/src/main.rs) is a single fullscreen client
that looks like a small desktop. It does not use Orbital, FerriticUI, Mesa or
Vulkan:

* **Wallpaper.** `term/assets/wallpaper.ppm` is a 640x400 P6 image compiled
  into the binary. `quanta-term` scales it into the mapped surface once per
  frame (protocol v0) and then sends the v1 records, so the desktop background
  is a real image rather than a solid `Clear`.
* **Window.** The terminal is a floating, non-fullscreen window over the
  wallpaper: a title bar, border, offset shadow, close button and a software
  pointer. The window can be dragged by its title bar with mouse events.
* **Terminal.** `ransid` handles ANSI/VT escape sequences; a PTY runs
  `/usr/bin/ion`; an embedded 8x13 public-domain bitmap font is uploaded as an
  A8 atlas and each cell is one `Glyph` record. The grid size follows the
  window size, so the terminal is not fullscreen.
* **Input.** `quanta-gpud` reads its VT input consumer on a dedicated thread
  and exposes `/scheme/quanta-gpu/input`. `InputEvent` carries keys, pointer
  motion, buttons and scroll; `quanta-term` maps keys to PTY bytes and uses the
  pointer for drag/focus. `quanta-key` is a small diagnostic that injects the
  same records into `/scheme/input/producer`.

Two non-obvious input rules were found while wiring this up:

* inputd's consumer read returns **0 bytes** when its queue is empty; it is not
  EOF. Treating 0 as EOF made the forwarding thread exit and closed the VT,
  after which `inputd` rejected VT activation because the VT no longer existed.
  The thread now treats 0 as "poll again".
* inputd routes keyboard/pointer events only to consumers of the **active VT**.
  The boot console handoff can steal it. The terminal presents at least once a
  second and `quanta-gpud` re-asserts its VT at most once a second, which keeps
  the keyboard routed to the terminal while it is the visible desktop.

Verified in QEMU: the wallpaper and floating window render at 640x480 (GTK) and
1280x800 (headless); typing `echo hi` through the emulated keyboard produces
`hi`; injecting a title-bar drag with `quanta-key --drag` moves the window and
restores the wallpaper underneath. The test is
[`quanta-gpu/scripts/qemu-test.py`](quanta-gpu/scripts/qemu-test.py).

## 8. Open decision

The protocol and the all-Rust goal are identical in v0 and v1. The only fork is
whether the GPU must execute the shading:

* **v0 + v1 software** (chosen): ship the Rust protocol, driver, client and
  reference executor on virtio-gpu 2D now. This is an all-Rust image with a
  GPU-shaped command protocol, and it boots today. Keep `Submit` for virgl.
* **virgl next** (optional): move the same draw records onto the GPU. It adds
  the C host dependency (virglrenderer) and is the only route to real GPU
  shading without Vulkan, but it is not required for the all-Rust image.
