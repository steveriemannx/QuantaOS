# QuantaGPU — Rust-native GPU protocol for QuantaOS

Goal: give FerriticUI a GPU path whose **protocol, driver and client are all Rust**,
with **no Vulkan and no C API** anywhere in the QuantaOS image. We do not need a
general-purpose GPU stack: exactly one simple device, one scanout, one client model.

This document is the design baseline. It is deliberately narrower than Vulkan/DRM.

## 1. What "pure Rust GPU protocol" means here

Three layers, all Rust:

```text
FerriticUI (ferritic-core Scene)
        │  encodes commands with quanta-gpu-protocol
        ▼
ferritic-quanta                       (client backend, Rust library)
        │  /scheme/quanta-gpu  (Rust-defined command format, shared memory)
        ▼
quanta-gpud                           (userspace driver / display service, Rust)
        │  virtio-gpu 2D (and later virgl 3D) encoded in Rust
        ▼
virtio-gpud (existing, pure Rust)  →  QEMU / Cloud Hypervisor / virtio hardware
```

* `quanta-gpu-protocol` is a dependency-free crate: versioned little-endian wire
  format, no `bindgen`, no `libc`, no `drm` FFI, no Vulkan.
* `quanta-gpud` is a normal Redox userspace driver. It owns the display through the
  existing pure-Rust `virtio-gpud` display scheme and serves `/scheme/quanta-gpu`.
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
Frames are rasterized by `ferritic-soft` into a shared RGBA8 surface; the driver
only transfers damage and flips. This is the shortest path to an all-Rust image and
is testable in QEMU today.

### Transport

* `/scheme/quanta-gpu/info` — read-only device description:
  width, height, stride, format, scale.
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
rejected without touching the scanout.

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

## 4. Protocol v1 — GPU render path (virgl, no Vulkan)

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

These commands are exactly the three `Prim` variants FerriticUI emits. A GPU
executor can implement them with one shader family:

* Rectangle/shadow: instanced quad + SDF fragment shader (same math as
  `ferritic-soft`/`ferritic-blade`).
* Glyph: textured quad sampling the A8 atlas.
* Scissor: clip rectangle in the instance data.

### virgl executor

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
| v1a | virgl driver bring-up: clear screen via a Rust virgl command buffer | yes | requires host virglrenderer; installable (247 KB) |
| v1b | SDF/glyph shaders + draw commands; FerriticUI on GPU | yes | host virglrenderer + NVIDIA/EGL |
| v2 | input/window model (multiple surfaces or Orbital integration) | — | QEMU |

## 6. Tests

* Host: protocol encode/decode round-trip, malformed-input rejection, sequencing,
  damage clipping.
* QEMU: boot with `gpu=virtio`, run the fullscreen demo, capture the framebuffer
  via the QEMU monitor (`screendump`) and compare against `ferritic-soft` golden
  output.
* v1: a Rust guest test that submits a minimal virgl clear/draw and verifies the
  screendump; then the Ferritic SDF/glyph golden set.

## 7. Open decision

The protocol and the all-Rust goal are identical in v0 and v1. The only fork is
whether the GPU must execute the shading:

* **v0 first** (recommended): ship the Rust protocol + driver + backend on
  virtio-gpu 2D now; keep `Draw`/`Submit` in the protocol for v1.
* **v1 first**: start with the virgl executor; accept the C host dependency and a
  much longer path to a booting demo.
