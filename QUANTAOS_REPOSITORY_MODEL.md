# QuantaOS repository and release model

Status: normative. This document defines how QuantaOS source code, upstream
dependencies and releases are organized. It applies to the `quantaos` branch of
the Redox fork and to the `QuantaOS` project repository.

## 1. Repository roles

- The **QuantaOS repository is the distribution/build repository**, not a
  monorepo of every component. It contains the cookbook, `recipes/`, image
  manifests under `config/`, local patches, docs, tests and release locks.
- **Upstream components** (kernel, relibc, base, ion, redoxfs, bootloader,
  coreutils, ...) live in their own repositories. Recipes describe how to
  fetch, patch and build them.
- **QuantaOS-owned components** (currently `quanta-gpu/`, later also UI and
  desktop pieces) may live in this repository or in their own repositories.
  Their location is an implementation detail, not part of the distribution
  contract.

The build must always be reproducible from a clean checkout by reading
`recipes/` and the committed revision locks; no build may depend on an
uncommitted absolute path on a developer machine.

## 2. Source rules

1. Every external component MUST declare an explicit source in its
   `recipe.toml`:
   - `git` with a full `rev` (preferred), or
   - `tar` with a `blake3` checksum, or
   - `path` only for in-tree source or a documented local override.
2. Committed recipes MUST NOT contain absolute host paths such as
   `/run/media/...` or `/home/...`.
3. Changes to external source MUST be carried as patch files next to the
   recipe and applied by the recipe. Patches MUST be small, reviewed and
   upstreamable where possible.
4. Upstream source MUST NOT be vendored into the repository for day-to-day
   development. A generated source snapshot/vendor directory MAY be produced
   for a release, offline builds or license compliance, but it is a build
   artifact, not the editable source of truth.
5. All external revisions used by a release MUST be pinned in `repo.toml` (or
   the equivalent lock) and committed with the release tag.
6. `COOKBOOK_OFFLINE=1` behaviour MUST be documented for recipes that need
   patches, because the cookbook does not apply git-source patches in offline
   mode.

Current examples:

- `recipes/core/base` — upstream git, pinned to
  `7d40d29b0110f1fa1ebeaa02e2e7977ac648f58f`, plus the virgl and
  `quanta-gpud` patches.
- `recipes/gui/quanta-gpu` — in-tree source, `path = "quanta-gpu"`.

## 3. QuantaOS-owned components

### Phase A — now

`quanta-gpu/` is an in-tree Cargo workspace (`protocol`, `raster`, `client`,
`gpud`, `demo`, `term`, `key`) and its recipe uses a relative path. This keeps
protocol and client changes atomic while the component is still evolving.

### Phase B — standalone component repository

A component SHOULD move to its own repository when one or more of these is true:

- it has consumers outside QuantaOS;
- it needs an independent release cadence or version number;
- it has its own CI, maintainers or permissions;
- its history or issue tracker is large enough to be unwieldy in the
  distribution repository.

The move changes only the recipe:

```toml
[source]
git = "https://.../quanta-gpu.git"
rev = "v0.2.0"
```

No QuantaOS code outside `recipes/` should need to change.

### Phase C — QuantaOS component monorepo

QuantaOS-owned components MAY later be consolidated into a single
workspace/monorepo (for example `quanta-gpu`, `quanta-term`, `quanta-ui`,
`quanta-key`). This is appropriate when they share APIs, change together and
ship on the same cadence.

This consolidation is independent of upstream handling: external projects
remain recipes with pinned revisions and patches. Do not turn the upstream
sources into a permanently forked monorepo.

### Split/merge decision rule

- **Keep in-tree** while the component is pre-1.0, tightly coupled, has a
  single maintainer/team and no external consumers.
- **Split out** when reuse, independent versioning, CI or ownership justifies
  the overhead.
- **Consolidate QuantaOS-owned components** when their API churn and releases
  are coupled.
- Never split solely for repository aesthetics, and never merge solely to
  reduce the number of repositories.

## 4. Upstream components

- Prefer **upstream + patch**. Offer patches upstream whenever practical.
- Fork an upstream project under the QuantaOS organization only when there is
  a long-term divergence or the upstream is unmaintained. Record the reason and
  pin the fork revision in the recipe.
- Keep license texts and copyright notices intact. New QuantaOS code is
  MIT-licensed unless a component explicitly says otherwise.
- Sync with upstream regularly. Avoid maintaining a large forked copy that is
  edited directly; it turns every upstream update into a merge project.

## 5. Relationship to Redox

- QuantaOS is an independent project **derived from** Redox, not an official
  Redox product. Documentation and the website MUST NOT imply endorsement by
  the Redox project.
- Keep the Redox copyright and license notices, and document the fork point
  and major divergences in `QUANTAOS.md`.
- Stay compatible with Redox where practical and upstream generally useful
  changes (drivers, build fixes, protocol work) rather than keeping them only
  in the fork.
- Redox trademarks and branding remain with the Redox project.

## 6. Release process

1. Start from a clean working tree; no uncommitted source or recipe changes.
2. Pin every external revision and update the `repo.toml` lock.
3. Build the release image from a clean checkout or CI with
   `config/quanta-gpu.toml`.
4. Run the host tests (`cargo test -p quanta-gpu-protocol -p
   quanta-gpu-raster`) and the QEMU smoke test
   (`python3 quanta-gpu/scripts/qemu-test.py`).
5. Tag the release (`quantaos-vX.Y.Z`) on the distribution repository.
6. Publish the image, its checksums, the revision lock, the patch list and
   license/SBOM information.
7. A binary package repository MAY be published for fast installs; source
   rebuildability remains mandatory.
8. A source snapshot/vendor tarball MAY be generated for offline or license
   compliance; it is generated from the pinned revisions, never hand-edited.

## 7. Current exceptions and follow-ups

- **FerriticUI** recipes (`recipes/gui/ferriticui` and
  `recipes/wip/dev/other/ferritic-blade-snapshot`) still point at a local
  export, `/run/media/shxu/datalinux/ferriticui-redox-src`. This is a
  temporary exception: convert it to a git source with a pinned revision (or a
  patch series) before it is used in a release image. It is not part of the
  current `quanta-gpu` image.
- `quanta-gpu` remains in-tree until it meets the Phase B criteria.
- `base` is the only upstream project carrying QuantaOS driver patches today;
  the patches are in `recipes/core/base/patches/`.
