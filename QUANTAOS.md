# QuantaOS fork

Fork of the Redox OS build system. Goal: an operating system whose **default desktop
image is entirely Rust and MIT-licensed**.

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

## Phase 2 — FerriticUI backends (next)

Two new crates, because FerriticUI currently cannot run on Redox:

- `ferritic-orbital` — windowing platform, replaces `ferritic-winit` (X11/Wayland only).
  Talks the Orbital protocol through `orbclient`.
- `ferritic-soft` — CPU rasterizer, replaces `ferritic-blade` (blade needs
  Vulkan/Metal/D3D12; Redox has none). SDF-quad rasterization plus an A8 glyph atlas,
  writing directly into the orbclient window buffer.

Milestone: the `counter` demo renders into a real Orbital window on Redox.
Recipe: `recipes/gui/ferriticui`.

Side benefit of dropping blade: `mesa` + `llvm` (C/C++) and `winit` (Apache-2.0-only)
leave the dependency closure, which is exactly what the Rust/MIT goal needs.

## Phase 3 — desktop applications (planned)

Port the desktop userland to FerriticUI: terminal (replaces `orbterm`), installer
(`installer-gui`), file manager, settings. Desktop shell goes through `orbutils`/`orbital`.

## Phase 4 — Rust + MIT compliance (planned)

Known violations inside the default desktop closure:

- `pop-icon-theme` (CC-BY-SA), `freefont` (GPL), `dejavu` (Bitstream Vera) — replace with MIT/OFL/CC0 assets
- `mesa` + `llvm` (C/C++) — removed by Phase 2 route A
- `binutils` (GPL), `findutils` (GPL), `strace` (C) — replace with Rust/MIT equivalents
- `netsurf` (MIT but C) — replace with a Rust browser or drop from the image

## Environment notes

- Build host reachability: GitHub is blocked; `gitlab.redox-os.org` works over git
  protocol (most recipes fetch from there); `crates.io` index is slow — use the
  `rsproxy.cn` mirror in `.cargo/config.toml`.
- Build happens on the `datalinux` volume (`/run/media/shxu/datalinux`); it is a
  udisks2 auto-mounted volume, so it disappears on logout/reboot.
- `qemu-system-x86_64` and `podman` are not installed on the build host yet.
