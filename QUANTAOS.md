# QuantaOS fork

Fork of the Redox OS build system. Goal: an operating system whose **default desktop
image is entirely Rust and MIT-licensed**.

QuantaOS is its own project repository (`github.com/steveriemannx/QuantaOS`, project
home and overview); the `quantaos` branch of this fork is its incremental
implementation.

## Decisions

| Topic | Decision |
|---|---|
| Baseline | branch `quantaos`, from Redox `master` @ `674d255f5` |
| GUI toolkit | FerriticUI — new CPU rasterizer + Orbital platform backend (route A) |
| Compliance scope | package closure of the default desktop image only (`config/desktop.toml` + includes) |
| Build host | remote Linux, repo on the `datalinux` volume |

## Phase 1 — remove COSMIC/iced (done)

Rationale: `libcosmic` and the `cosmic-*` apps are fetched from `github.com/pop-os/*`
and are **GPL-3.0**, which is incompatible with a MIT-only goal. `iced` itself is MIT —
it is removed as a design choice (static retained-mode FerriticUI replaces it).

- Deleted 16 recipe directories: `recipes/cosmic/*` (8), `recipes/libs/libcosmic`,
  `recipes/icons/cosmic-icons`, `recipes/demos/iced`, `recipes/wip/cosmic/*` (11),
  `recipes/wip/wayland/{iced-wayland,libcosmic-wayland}`, `recipes/wip/demos/{iced-7guis,soft-ratatui}`,
  `recipes/wip/{games/math/cosmic-ext-2048,math/cosmic-ext-calculator,monitors/cosmic-ext-observatory,sci/cosmic-weather,sci/weather/cosmic-ext-forecast,vid/cam/cosmic-ext-camera,sys/xdg-desktop-portal-cosmic}`
- Cleaned 64 references across 10 config files (`config/desktop.toml`, `demo.toml`,
  `wayland.toml`, `*/ci.toml`, `*/jeremy.toml`, `x86_64/rustconf2025.toml`)
- Updated `recipes/groups/desktop` meta package
- Updated `README.md` wording and screenshots

## Phase 2 — FerriticUI backends (done)

Two new crates, because FerriticUI currently cannot run on Redox:

- `ferritic-soft` — CPU rasterizer, replaces `ferritic-blade` (blade needs
  Vulkan/Metal/D3D12; Redox has none). SDF-quad rasterization plus an A8 glyph atlas,
  writing directly into the orbclient window buffer. **Done** on FerriticUI `main`:
  nine unit tests, and the `counter_snapshot` example renders the counter scene to
  PPM without a GPU. Against a blade render of the same scene on the same host the
  output is SSIM 0.9996 (mean absolute channel difference 0.04); the checked-in
  golden differs more (SSIM 0.94) because it was captured with a different host font.
  `SystemText` now falls back to a Latin-capable face instead of the first database
  entry, which on Linux could be a symbol font.
- `ferritic-orbital` — windowing platform, replaces `ferritic-winit` (X11/Wayland only).
  Talks the Orbital protocol through `orbclient`. **Done**: the `ferritic-demo` counter
  opens an orbclient window rendered by `ferritic-soft`, with the winit layer's
  key/mouse/wheel/focus/scale/clipboard mapping. It runs on the host through
  orbclient's SDL2 backend and on Redox through the Orbital backend.

Milestone: **achieved** — `build/x86_64/ferriticui/harddrive.img` boots in QEMU; the
counter window opens over Orbital and clicking `+` updates the count (verified with VGA
screendumps). The image is assembled from the cached official binary packages plus the
locally cooked `ferriticui` recipe; it intentionally omits the in-progress
mesa/libvulkan packages.

Recipe: `recipes/gui/ferriticui` builds the `ferritic-demo` package and installs an
Orbital launcher entry (`/usr/share/ui/apps/65_ferriticui`) with its own icon
(`/usr/share/icons/apps/ferriticui.png`) and a freedesktop
`/usr/share/applications/ferriticui.desktop`; it shows up in the taskbar after login.
Its source is a clean local export
(`/run/media/shxu/datalinux/ferriticui-redox-src`) so the recipe copy excludes `target/`
and `.git/`; switch it to a git source once the fork is pushed.

Side benefit of dropping blade: `mesa` + `llvm` (C/C++) and `winit` (Apache-2.0-only)
leave the dependency closure, which is exactly what the Rust/MIT goal needs.

## Phase 3 — desktop applications (in progress)

Port the desktop userland to FerriticUI: terminal (replaces `orbterm`), installer
(`installer-gui`), file manager, settings. Desktop shell goes through `orbutils`/`orbital`.

- Terminal — **v0 done**: `ferritic-term` (in the FerriticUI `ferriticui` package) uses
  ransid for ANSI/VT emulation, a `/dev/ptmx` PTY running `ion`, and ferritic-soft/Orbital
  for display. Verified in the test image (`echo hi` echoes and executes; launcher entry
  `Terminal`). It is opened from the launcher/taskbar after login; the earlier boot-time
  init.d autostart was removed so the login screen stays clean (`ferritic-orbital` still
  supports `FERRITIC_WINDOW_RETRY_MS` for autostarted apps). No scrollback,
  selection/copy or mouse reporting yet. `orbterm` remains ignored by the desktop config.

## Phase 4 — Rust + MIT compliance (planned)

Known violations inside the default desktop closure:

- `pop-icon-theme` (CC-BY-SA), `freefont` (GPL), `dejavu` (Bitstream Vera) — replace with MIT/OFL/CC0 assets
- `mesa` + `llvm` (C/C++) — removed by Phase 2 route A
- `binutils` (GPL), `findutils` (GPL), `strace` (C) — replace with Rust/MIT equivalents
- `netsurf` (MIT but C) — replace with a Rust browser or drop from the image

## Next workstreams (as of 2026-11-11)

Three parallel tracks. They share `ferritic-core`/`ferritic-text`, but not a rendering
backend.

### 1. `ferritic-soft` — polish the CPU rasterizer

Keep improving the CPU path: it is the renderer for every FerriticUI app on Redox,
because Orbital has no GPU API. Blade parity is already SSIM 0.9996 on the same host;
the remaining work is quality and features, not a platform bring-up.

### 2. `ferritic-blade` + Vulkan + Mesa — GPU route on Redox

Keep improving the GPU path until it runs on Redox. `libvulkan` and `mesa` (lavapipe)
now build and are part of the desktop and CI package sets; `vulkan-smoketest` checks
the loader with a dlopen + instance/device enumeration. Next: run `ferritic-blade`
through the loader in QEMU, then decide whether Mesa/lavapipe can stay in the default
image given the Phase 4 Rust/MIT scope.

### 3. QuantaGPU — Rust-native GPU protocol and driver

Keep improving the all-Rust route: protocol, driver and client in Rust, **no C**, no
Vulkan. Keep it narrow — one simple device, one scanout, one client model — and do
not generalize yet. Design and v0 scope: [`QUANTAOS_GPU.md`](QUANTAOS_GPU.md).

Implemented: the versioned protocol with `DeviceInfo` feature advertisement and
fence acknowledgements; `quanta-gpu-raster`, a pure-Rust SDF executor for the v1
`Clear`/`Rect`/`Shadow`/`Glyph`/`Atlas` records; `quanta-gpud` executing those
records and flipping the virtio-gpu 2D scanout; the client `frame()` API; and a
`quanta-demo` with `--v1` (driver executes protocol records) and `--surface`
(client rasterizes) modes. Next: boot the `quanta-gpu` test image in QEMU and
wire `ferritic-quanta` to the client. virgl stays optional and later.

## Environment notes

- Build host reachability: GitHub is blocked; `gitlab.redox-os.org` works over git
  protocol (most recipes fetch from there); `crates.io` index is slow — use the
  `rsproxy.cn` mirror in `.cargo/config.toml`.
- FerriticUI no longer has a GitHub git dependency: the `naga`/`naga-types` fork pin
  (`kvark/wgpu` rev `9964733`) is vendored under `FerriticUI/vendor/wgpu-naga` with its
  workspace-inherited fields inlined, and `vendor/blade-ferritic/Cargo.toml` points at
  that path. `Cargo.lock` has no git sources, so FerriticUI builds with
  `CARGO_NET_OFFLINE=false` even when GitHub is unreachable.
- FerriticUI recipe/image workflow (native build, no podman):
  ```sh
  rsync -a --delete --exclude target/ --exclude .git/ \
      /run/media/shxu/datalinux/FerriticUI/ /run/media/shxu/datalinux/ferriticui-redox-src/
  export PATH="$HOME/.cargo/bin:$PWD/.host-tools/bin:$PATH" CI=1
  make r.ferriticui PODMAN_BUILD=0 PREFIX_BINARY=1 REPO_BINARY=1 FSTOOLS_NO_MOUNT=1
  # image assembly needs fake root ownership:
  .host-tools/fakeroot/root/usr/bin/fakeroot-sysv -l \
      .host-tools/fakeroot/root/usr/lib/x86_64-linux-gnu/libfakeroot/libfakeroot-sysv.so \
      -f .host-tools/fakeroot/root/usr/bin/faked-sysv -- \
      make CONFIG_NAME=ferriticui PODMAN_BUILD=0 PREFIX_BINARY=1 REPO_BINARY=1 \
      FSTOOLS_NO_MOUNT=1 build/x86_64/ferriticui/harddrive.img
  ```
  After login the apps appear as taskbar/launcher icons (`Terminal`, `FerriticUI
  Counter`); they can also be started from the serial console
  (`root`/`password`).
- QEMU input for tests: boot with `-device usb-kbd` (no usb-tablet), log in graphically
  with `sendkey ret`, launch `ferritic-term &` from the serial console, click the window
  (`mouse_move 100 100`, then `mouse_button 1` / `0`), and type through the monitor
  (`sendkey e`, `sendkey spc`, `sendkey ret`, ...). Do not use host xdotool typing for
  Redox tests: it lands in whichever host window has focus.
- Build happens on the `datalinux` volume (`/run/media/shxu/datalinux`); it is a
  udisks2 auto-mounted volume, so it disappears on logout/reboot.
- There is no `podman`; QEMU 10.2.1 and gvncviewer are unpacked under `.host-tools/`.
  Boot the test image headlessly with `.host-tools/qemu`, a monitor socket for
  `screendump`, a serial socket for shell access, and `-vnc 127.0.0.1:3` for viewing.
