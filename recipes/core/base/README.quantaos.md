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

The recipe now pins upstream `7d40d29b0110f1fa1ebeaa02e2e7977ac648f58f` and
lists these patches under `[source]`; cookbook applies them after checkout.
For offline development, point `[source] path` at a base checkout that already
contains the changes instead, since `COOKBOOK_OFFLINE=1` does not apply patches
for git sources.
