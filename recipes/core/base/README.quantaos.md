# QuantaOS base driver patches

`base` is an external repository (`https://gitlab.redox-os.org/redox-os/base`).
The QuantaGPU work touches two drivers there:

- `drivers/graphics/virtio-gpud` — virgl capset query, context/3D resource
  setup and `SUBMIT_3D` encoding (421 lines);
- `drivers/graphics/quanta-gpud` — a standalone single-scanout virtio-gpu 3D
  prototype kept for reference (1,015 lines, not part of the build).

Both are kept here as plain patches against upstream
`7d40d29b0110f1fa1ebeaa02e2e7977ac648f58f`:

```
patches/0001-virtio-gpud-virgl.patch
patches/0002-quanta-gpud-prototype.patch
```

Apply them to a clean base checkout with:

```sh
git clone https://gitlab.redox-os.org/redox-os/base base
git -C base checkout 7d40d29b0110f1fa1ebeaa02e2e7977ac648f58f
patch -d base -p1 < recipes/core/base/patches/0001-virtio-gpud-virgl.patch
patch -d base -p1 < recipes/core/base/patches/0002-quanta-gpud-prototype.patch
```

To build base from upstream in a networked checkout, the recipe source can be
switched to:

```toml
[source]
git = "https://gitlab.redox-os.org/redox-os/base.git"
rev = "7d40d29b0110f1fa1ebeaa02e2e7977ac648f58f"
patches = [
    "patches/0001-virtio-gpud-virgl.patch",
    "patches/0002-quanta-gpud-prototype.patch",
]
```

The local offline build instead uses `[source] path` pointing at a base
checkout that already contains the commits. `COOKBOOK_OFFLINE=1` does not
apply patch files for git sources, so the path form is kept for this host.
