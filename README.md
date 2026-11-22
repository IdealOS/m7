# idealos

A research desktop OS built around one question: what would a desktop look like if a
persistent, versioned, syncable database replaced the filesystem?

It is a minimal Linux with its own hardware-accelerated scene-graph compositor, a
message bus that brokers permissions, a CRDT document database, and TypeScript apps.
Targets: Raspberry Pi 4 (8 GB) and an aarch64 QEMU VM on Apple Silicon.

- [docs/feature-list.md](docs/feature-list.md): vision, decisions, architecture, risks
- [docs/implementation-plan.md](docs/implementation-plan.md): milestones M0–M14
- [docs/spikes.md](docs/spikes.md): spike results (database and QuickJS so far)

![Phase 0 compositor demo](docs/screenshots/phase0.png)

The same compositor on the QEMU image, through its KMS backend:
[docs/screenshots/m1-kms-qemu.png](docs/screenshots/m1-kms-qemu.png).

## Quick start (macOS)

```sh
make hosted        # compositor demo in a window: drag title bars, click to raise, Option+Tab
make bench         # release build, redraws every frame, logs frame times
make spike-db      # Spike C: Loro + SQLite + FTS5 at 10,000 docs
make spike-js      # Spike D: memory and startup of 8 QuickJS app processes
make spike-kms     # Spike A: wgpu -> KMS on virtio-gpu in a QEMU VM (needs lima + qemu)
make screenshot    # render the demo offscreen to docs/screenshots/phase0.png
make test          # all Rust tests (includes a real GPU render-and-readback test)
make lint          # rustfmt, clippy, and the device-only compositor build
make lint-device   # clippy on the Linux-only KMS code, in the build VM
make sdk           # type-check the TypeScript SDK
make tools-check   # type-check and unit-test the TypeScript tools in tools/
```

Device images need two extra tools: `brew install lima qemu`.

```sh
make vm            # create the aarch64 Linux build VM (once)
make check-defconfigs  # seconds: fail if Buildroot would silently drop a defconfig option
make image-qemu    # Buildroot image for QEMU  -> image/output/qemu (first build ~3 h)
make smoke-qemu    # boot it headless and check it, input included (~20 s); exits 1 on any failure
make smoke-deploy  # ...then rebuild the binaries, deploy them over SSH, check the restart
make smoke-report  # ...then run the Pi's first-boot hardware report against it
make qemu          # boot it; serial console here, display in a window
make image-rpi4    # Buildroot image for the Pi -> image/output/rpi4
tools/flash-sd /dev/diskN
make deploy HOST=idealos.local          # rebuild and push binaries to a running dev image
make deploy HOST=localhost PORT=2222   # ...or to QEMU
make pi-report HOST=idealos.local       # hardware report + Spike A-D numbers -> image/output/reports
```

`tools/build-image` makes an SSH key, `image/output/dev-key`, and bakes its public
half into dev images as root's only login. `make deploy` uses it, so reflash after
deleting `image/output`. Deploy builds with the image's own Buildroot toolchain
(`make device-bins`), copies the binaries to the writable `/opt/idealos/bin` overlay,
and sends `session` SIGTERM; init restarts it and it prefers the overlay's binaries.

On a new board, run `make pi-report` first. It records the board, storage speed, DRM
cards and connectors, Vulkan drivers and input devices, then stops the desktop to run
kms-probe with two and three buffers (Spikes A/B), runs the compositor flat out for
10 s, runs both benchmarks (Spikes C and D), and restores the desktop. A copy stays
on the device in `/data/reports`.

## Layout

```
crates/protocol     bus envelope and scene-graph ops (shared by Rust and, via sdk/, TypeScript)
crates/compositor   scene graph, wgpu renderer, window manager, hosted + KMS backends
crates/session      first process after init: starts and supervises services
spikes/             db-bench (C), js-bench (D), kms-probe (A/B): benchmarks to rerun on the Pi
sdk/                @idealos/sdk (wire types today; bus, db, signals and widgets in M4)
image/              Buildroot external tree: defconfigs, kernel fragments, rootfs overlay
tools/              build VM, image build, QEMU, SD card, deploy scripts
docs/               feature list, implementation plan, screenshots
```

## Status: Phase 0

| Item | State |
| --- | --- |
| Monorepo, Makefile, CI workflow | Done. CI has not run yet (no remote). |
| `protocol`: bus envelope, framing, scene ops | Done, tested against the plan's JSON examples |
| Compositor scene graph (groups, rects, text, clipping) | Done, tested |
| Renderer: instanced SDF rounded rects, glyphon text per layer | Done; renders at 60 fps hosted on an M3 |
| Window manager v0: raise on click, drag by title bar, Alt+Tab | Done, tested. Both backends feed one `Seat`, so mouse, tablet and keyboard drive the same code. |
| Damage tracking | Frame-level only: an idle scene draws nothing. Region damage is still to do. |
| Headless render to PNG | Done (screenshots, future golden-image tests) |
| `session` supervisor with restart backoff | Done, tested on macOS. Stops its services on SIGTERM (SIGKILL after 2 s), so a restart leaves nothing behind. |
| Images, transforms beyond translation, bundled fonts | Not started. System fonts for now; the compositor panics if none are installed. |
| KMS backend | **Output and input work** (M1): `compositor --backend kms` draws the scene with three page-flipped dumb buffers (Spike A's copy path), redraws only when the scene changes, and takes keyboard and pointer input through libinput and xkbcommon. The pointer is a hardware cursor, so moving it costs no redraw. It picks the first DRM card with a connected display (on a Pi 4, `vc4`, not `v3d`). Not yet: the zero-copy Pi path (Spike B), muting the kernel console's keyboard, and a visual check of the cursor (QEMU screendumps leave it out). |
| Buildroot image for QEMU | **Built and booted** (Buildroot 2026.08.2, kernel 6.12.58). Boots to `session` in ~0.4 s under HVF; `/data` and the dev overlay mount; the compositor shows its first frame on virtio-gpu (lavapipe), finds QEMU's virtio keyboard and tablet, and moves a window dragged by its title bar and raises the next on Alt+Tab. `make smoke-qemu` checks all of this. |
| Buildroot image for the Pi | **Built** on the first try (~45 min, reusing downloads): Pi kernel `stable_20250916`, V3DV Vulkan driver, firmware with the `vc4-kms-v3d-pi4` overlay, and the dev SSH key, in a 3.3 GB `sdcard.img`. **Never booted**: no Pi on hand yet. |
| Lima build VM, image build, QEMU boot | Run end to end. |
| SSH deploy | **Works against QEMU**: `make smoke-deploy` rebuilds, pushes, and checks that `session` and the compositor restarted from the overlay with the old shell gone. Not yet run against a Pi. |
| Hardware report (`make pi-report`) | **Works against QEMU**: `make smoke-report` runs it end to end and checks every section, both kms-probe runs, the compositor finding input, both benchmark tables, and that the desktop comes back. Not yet run on a Pi. |
| SD card flashing | `tools/flash-sd` is syntax-checked but **never run**. |

### Spikes

| Spike | State |
| --- | --- |
| C: Loro + SQLite + FTS5, 10,000 docs | Measured on the Mac: ~0.1 ms per write, copies never diverge; live queries need expression indexes. Pi run pending. |
| D: 8 QuickJS app processes | Measured on the Mac: ~7.5 MiB and ~10 ms per app; 790k signal updates/s. Pi run pending. |
| A: wgpu to KMS in QEMU | Done, in a stock Ubuntu VM and in the idealos image: lavapipe + dumb-buffer copy + page flip, ~11 ms/frame at 1280x800 with three buffers. virgl isn't in Homebrew QEMU. |
| B: wgpu to KMS on the Pi | Needs a Pi. `make pi-report` runs kms-probe there, with the compositor stopped. |
| E, F: Servo, Gmail OAuth | Not started. Servo needs ~30 GB of free disk; Gmail needs a Google Cloud OAuth client. |

### Image build notes

- Buildroot rejects paths containing spaces, so `tools/build-image` copies the repo into
  the VM and builds there.
- Buildroot silently drops a defconfig option whose dependencies aren't met. The first
  build lost glibc, Rust and the idealos package this way, so run `make check-defconfigs`
  after editing a defconfig.
- `idealos-userspace` is a local package, so Buildroot doesn't vendor its crates;
  its `.mk` runs `cargo vendor` itself. `tools/build-image` cleans it on every build so
  the image always has the current code.
- Buildroot's `<pkg>-rebuild` copies a local package's source again but never deletes
  files, so a moved source file breaks the build. `tools/build-device-bins` mirrors
  the source with `rsync --delete` first, keeping cargo's `target` directory so the
  build stays incremental.
- Buildroot doesn't notice changes to `board/common/busybox.fragment`; rebuild BusyBox
  with `tools/build-image qemu busybox-dirclean all`.
- The root filesystem is read-only, so dropbear's host keys live on `/data`
  (`/etc/dropbear` is a link). They survive reboots but not a reflash.
- The workspace needs Rust 1.90 or newer (edition 2024; cosmic-text needs 1.89 and
  ordered-float 1.90). Buildroot 2026.08 ships 1.97; the 2026.02 LTS branch is stuck on
  1.88, so `tools/build-image` defaults to 2026.08.2.
