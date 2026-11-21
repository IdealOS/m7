# idealos

A research desktop OS built around one question: what would a desktop look like if a
persistent, versioned, syncable database replaced the filesystem?

It is a minimal Linux with its own hardware-accelerated scene-graph compositor, a
message bus that brokers permissions, a CRDT document database, and TypeScript apps.
Targets: Raspberry Pi 4 (8 GB) and an aarch64 QEMU VM on Apple Silicon.

- [docs/feature-list.md](docs/feature-list.md): vision, decisions, architecture, risks
- [docs/implementation-plan.md](docs/implementation-plan.md): milestones M0–M14

![Phase 0 compositor demo](docs/screenshots/phase0.png)

## Quick start (macOS)

```sh
make hosted        # compositor demo in a window: drag title bars, click to raise
make bench         # release build, redraws every frame, logs frame times
make screenshot    # render the demo offscreen to docs/screenshots/phase0.png
make test          # all Rust tests (includes a real GPU render-and-readback test)
make lint          # rustfmt, clippy, and the device-only compositor build
make sdk           # type-check the TypeScript SDK
```

Device images need two extra tools: `brew install lima qemu`.

```sh
make vm            # create the aarch64 Linux build VM (once)
make image-qemu    # Buildroot image for QEMU  -> image/output/qemu
make qemu          # boot it; serial console here, display in a window
make image-rpi4    # Buildroot image for the Pi -> image/output/rpi4
tools/flash-sd /dev/diskN
make deploy HOST=idealos.local          # push new binaries to a running dev image
make deploy HOST=localhost PORT=2222   # ...or to QEMU
```

## Layout

```
crates/protocol     bus envelope and scene-graph ops (shared by Rust and, via sdk/, TypeScript)
crates/compositor   scene graph, wgpu renderer, window manager, hosted + KMS backends
crates/session      first process after init: starts and supervises services
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
| Window manager v0: raise on click, drag by title bar | Done, tested |
| Damage tracking | Frame-level only: an idle scene draws nothing. Region damage is still to do. |
| Headless render to PNG | Done (screenshots, future golden-image tests) |
| `session` supervisor with restart backoff | Done, tested on macOS |
| Images, transforms beyond translation, bundled fonts | Not started (system fonts for now) |
| KMS backend | Stub. Waits on Spike A (QEMU) and Spike B (Pi 4). |
| Buildroot tree, kernel fragments, partition layouts | Written, **never built**. Expect to fix config symbol names, the kernel version and the Pi kernel tag on the first build. |
| Lima build VM, QEMU, flash and deploy scripts | Written and syntax-checked, **never run** (Lima and QEMU are not installed here) |

### Known risks for the first image build

- Buildroot rejects paths containing spaces, so `tools/build-image` copies the repo into
  the VM and builds there.
- The workspace needs Rust 1.88 or newer (edition 2024, let chains). If Buildroot's bundled
  Rust is older, build the userspace with rustup in the VM and copy it into the overlay
  (as `tools/deploy-to-pi` already does).
