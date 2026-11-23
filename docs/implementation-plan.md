# Database-Centric Desktop OS — Implementation Plan

Snapshot of the living doc as of 2026-11-20. The live, commentable version is at
<https://claude.ai/artifact/2AoVkkrF4cbFsHAEHQXKxe>.

This plan turns the [feature list](feature-list.md) into milestones M0–M14, from first boot to a daily-driver system.

## Approach

Build the bus and database first, and make every service run natively on macOS before it runs on the device. Only the display and input backends are Linux-only.

- **Hosted mode.** The compositor has two backends: `winit` (a window on macOS, using wgpu on Metal) and `kms` (DRM/KMS plus libinput on Linux). The bus, database, app host and most services are portable Rust and run on macOS directly. The usual dev loop is edit, `cargo run`, and see it in a Mac window in seconds. QEMU and the Pi are for integration testing.
- **Thin vertical slices.** Each milestone ends with something that runs end to end, even if crude. M4 already puts a TypeScript app in a window that reads the database.
- **Interfaces before implementations.** The bus envelope, scene-graph ops, query filter and permission rule formats are defined early in a `protocol` crate, with JSON Schema and generated TypeScript types, so Rust and TypeScript stay in sync.
- **Permissions from day one, enforced later.** Every bus message passes through a permission hook from M2. It allows everything until M6, so no code assumes unchecked access.
- **Snapshots early.** Basic database snapshots ship with the database in M3, using SQLite's online backup API. Full backup and restore UI comes in M8.
- **Assumed choices.** This plan follows the recommendations in the feature list's open questions: Buildroot, Loro, NetworkManager, network namespaces, and a separate secrets service.

## Milestone map

Arrows point from a milestone to the milestones that need it.

```
Phase 0 · Bring-up
   M0 First boot ───────────────────────────────► M1 Pixels
        │                                            │
Phase 1 · Core OS                                    │
        ▼                                            │
   M2 Message bus ──► M3 Database v1                 │
                           │                         │
                           ▼                         │
                      M4 App runtime ◄───────────────┘
                           ▼
                      M5 Widgets and WM
                           ▼
                      M6 Permissions
                           ▼
                      M7 First apps
Phase 2 · Daily driver     ▼
                      M8 Backup and updates
                           │
     ┌──────────┬──────────┼──────────┬──────────┬──────────┐
     ▼          ▼          ▼          ▼          ▼          ▼
  M9 Editor  M10 Email  M11 Music  M12 Chat  M13 Browser  M14 DB shell
                ▲                                │
                └──── full HTML email waits ─────┘
```

M1 and M2 can be built in parallel after M0. Phase 1 is a single chain because each milestone needs the one before it. Phase 2 fans out after M8.

## Repository layout

One repo: a Cargo workspace for everything in Rust, a pnpm workspace for TypeScript, and a Buildroot external tree for the image.

```
idealos/
├── Cargo.toml              # Rust workspace
├── package.json            # pnpm workspace for sdk/ and apps/
├── crates/
│   ├── protocol/           # message types, scene ops, query filter, grants; JSON Schema + TS codegen
│   ├── bus-client/         # client library used by every service and the app host
│   ├── busd/               # broker: routing, pub/sub, peer identity, permission checks
│   ├── db-core/            # storage engine: SQLite, Loro, query compiler, blobs, history
│   ├── dbd/                # database service on the bus
│   ├── permd/              # grants store, prompts, audit log
│   ├── secretsd/           # encrypted secrets store
│   ├── compositor/         # scene graph, widgets, layout, text, renderer, window manager
│   │   └── backends/       # winit (hosted) and kms (DRM/KMS + libinput)
│   ├── app-host/           # QuickJS runtime, one process per app
│   ├── session/            # starts and restarts services; launches apps in sandboxes
│   ├── netd/               # NetworkManager bridge
│   ├── ptyd/               # PTYs and terminal grids for the terminal app
│   ├── audiod/             # PipeWire playback
│   ├── maild/              # Gmail API sync
│   ├── chatd-dummy/        # fake chat service
│   ├── importd/            # file importers and exporters
│   ├── backupd/            # snapshots, backup, restore
│   ├── browser-host/       # Servo embedding, external surfaces
│   └── osctl/              # CLI: bus tracing, db queries, app install
├── sdk/                    # @idealos/sdk: bus, db, signals, widgets, permissions
├── apps/                   # settings, documents, terminal, editor, mail, music, chat, browser
├── schemas/                # system schemas: Contact, Email, Message, Track, ...
├── image/                  # Buildroot external tree
│   ├── configs/            # qemu_aarch64_defconfig, rpi4_defconfig
│   ├── board/              # partition layouts, RAUC config, overlays
│   └── package/            # Buildroot packages for our crates and apps
├── tools/                  # run-qemu, flash-sd, deploy-to-pi
└── docs/
```

## Development environment

Most work happens on the Mac in hosted mode. An aarch64 Linux build VM produces images and device binaries, QEMU checks Linux-only paths, and the Pi checks performance.

- **Build VM.** Buildroot does not run on macOS, so use an aarch64 Linux VM (Lima or OrbStack) as the build box. It is the same architecture as the Pi, so Rust builds there are native, not cross-compiled.
- **Dev image vs release image.** Both share one Buildroot config. The dev image adds SSH, a writable `/opt/idealos` overlay for pushing new binaries, and debug symbols. The release image is read-only with A/B slots.
- **QEMU.** `tools/run-qemu` starts `qemu-system-aarch64 -M virt -accel hvf` with `virtio-gpu-pci`, a Cocoa display, virtio disk and network, and an SSH port forward. Lavapipe is the default GPU path; a UTM profile adds the virgl GLES path.
- **Pi 4.** `tools/flash-sd` writes the image. `tools/deploy-to-pi` copies new binaries over SSH into the dev overlay and restarts the affected service through `osctl`.
- **Apps.** `osctl app install --watch apps/mail` bundles the app with esbuild, stores it as an app document, and restarts it on every save. This works in hosted mode, QEMU and on the Pi.

| Change | Run it on | How |
| --- | --- | --- |
| Bus, database, permission logic | Mac | `cargo test`, `cargo run` |
| Compositor rendering, widgets, layout | Mac | hosted `winit` backend |
| TypeScript apps and SDK | Mac | `osctl app install --watch` |
| KMS, libinput, sandboxing, NetworkManager | QEMU | dev image plus SSH deploy |
| GPU performance, memory, audio, Wi-Fi | Pi 4 | dev image plus SSH deploy |

## Key interfaces

These are first drafts. Each one lives in the `protocol` crate as Rust types, with JSON Schema and TypeScript types generated from them.

### Bus envelope

Frames are length-prefixed JSON (a 4-byte big-endian length, then UTF-8 JSON) over a Unix socket. The broker stamps `from` on every message; a client-supplied `from` is dropped.

```json
{"id": 42, "kind": "request", "to": "db", "method": "query", "params": {"type": "Email", "where": {"read": false}}}
{"id": 42, "kind": "response", "ok": true, "result": {"docs": []}}
{"id": 43, "kind": "response", "ok": false, "error": {"code": "permission_denied", "message": "read Email not granted"}}
{"id": 44, "kind": "request", "to": "db", "method": "watch", "stream": true, "params": {"type": "Email", "where": {"read": false}}}
{"id": 44, "kind": "stream", "data": {"kind": "diff", "added": [], "changed": [], "removed": []}}
{"kind": "subscribe", "id": 45, "topic": "ui.theme"}
{"kind": "publish", "topic": "ui.theme", "from": {"service": "settings"}, "data": {"dark": true}}
```

### Scene-graph ops

An app sends a batch of ops per transaction. Node IDs are allocated by the app, per connection. Node 1 is the window's root.

```json
{"id": 50, "kind": "request", "to": "compositor", "method": "commit", "params": {"window": 1, "ops": [
  {"op": "create", "id": 10, "type": "vstack", "props": {"gap": 8, "padding": 12}},
  {"op": "create", "id": 11, "type": "label", "props": {"text": "Inbox (3)", "style": "title"}},
  {"op": "create", "id": 12, "type": "button", "props": {"label": "Compose"}},
  {"op": "insert", "parent": 1, "child": 10, "index": 0},
  {"op": "insert", "parent": 10, "child": 11, "index": 0},
  {"op": "insert", "parent": 10, "child": 12, "index": 1},
  {"op": "listen", "id": 12, "events": ["click"]}
]}}
```

A later update is a single op: `{"op": "set", "id": 11, "props": {"text": "Inbox (4)"}}`. Events come back as `{"kind": "publish", "topic": "ui.event", "data": {"window": 1, "id": 12, "event": "click"}}`. A text widget can bind straight to a document field with `"props": {"bind": {"doc": "<doc id>", "path": "body"}}`, so typing goes from the compositor to the database without passing through the app.

### Database queries

The query filter is JSON with Mongo-style operators, compiled to SQL over the JSON copies of documents. `$link` follows links between documents.

```ts
const unread = db.live({
  type: "Email",
  where: { read: false, labels: { $contains: "INBOX" } },
  orderBy: [["date", "desc"]],
  limit: 50,
});

const fromFamily = db.query({
  type: "Email",
  where: { from: { $link: { type: "Contact", where: { tags: { $contains: "family" } } } } },
});

const hits = db.search("quarterly report", { type: "Email" });
```

### Permission grants

A grant names an app, the access it has, and a filter in the same query language. Capabilities such as network access are grants too.

```json
{"app": "org.idealos.mail", "access": ["read", "write"], "type": "Email", "filter": {}}
{"app": "com.example.frame", "access": ["read"], "type": "Image", "filter": {"album": "Family"}}
{"app": "org.idealos.browser", "capability": "network"}
```

### An app

```ts
import { app, db, ui, permissions } from "@idealos/sdk";

export default app(async () => {
  await permissions.request([{ access: ["read"], type: "Email" }]);
  const unread = db.live({ type: "Email", where: { read: false }, orderBy: [["date", "desc"]] });

  return ui.window({ title: "Inbox" },
    ui.vstack({ gap: 8 },
      ui.label(() => `Unread: ${unread().length}`),
      ui.list({ items: unread, row: (email) => ui.label(() => email().subject) }),
    ),
  );
});
```

## Phase 0: Bring-up

### M0: Toolchain and first boot

Goal: the same Buildroot config boots to a shell in QEMU and on the Pi 4, and Rust binaries deploy to both.

- [ ] Monorepo skeleton: Cargo workspace, pnpm workspace, CI running `cargo fmt`, `clippy`, `cargo test` and `tsc`.
- [ ] Script that sets up the aarch64 build VM (Lima or OrbStack).
- [ ] Buildroot external tree with `qemu_aarch64` and `rpi4` defconfigs.
- [ ] Kernel config: `virtio_gpu`, `vc4` and `v3d`, network namespaces, seccomp, Landlock.
- [ ] Partition layout: boot, root A, root B, data. Root is read-only; data is ext4.
- [ ] Packages: Mesa (lavapipe, V3DV, virgl), libdrm, libinput, libxkbcommon, PipeWire, NetworkManager, RAUC.
- [ ] BusyBox init starts `session`, a stub for now that opens a serial shell.
- [ ] Dev image extras: SSH, writable `/opt/idealos` overlay, debug symbols.
- [ ] `tools/run-qemu`, `tools/flash-sd`, `tools/deploy-to-pi`.

**Done when:** `make qemu` boots the dev image to a shell on the Mac, the Pi 4 boots from SD card, and a Rust binary deployed over SSH runs on both.

### M1: Pixels

Goal: the compositor draws a scene graph through wgpu on all three targets: hosted macOS, QEMU and the Pi.

- [ ] **Spike:** wgpu to KMS. Try dmabuf export plus KMS framebuffer import on V3DV and virgl, and the dumb-buffer copy on lavapipe. Record which path each target uses.
- [ ] **Spike:** wgpu on lavapipe and on virgl GLES inside QEMU.
- [ ] `compositor` crate with a `Backend` trait and two backends: `winit` (hosted) and `kms` (page-flip driven frame loop).
- [ ] Input: libinput for keyboard and pointer, xkbcommon for keymaps, a hardware cursor plane.
- [ ] Scene graph v0: node tree with rects (fill, border, corner radius), images and text, plus transforms and clipping.
- [ ] Renderer: batched quad pipeline, SDF rounded rects, glyph and image atlases.
- [ ] Text: cosmic-text for shaping and font fallback, with bundled UI and fallback fonts.
- [ ] Damage tracking so only changed regions are redrawn.
- [ ] Debug overlay showing frame time and damage regions.

**Done when:** a demo scene equal to about 20 windows of rects, images and text runs on the Pi 4 at 1080p and 60 fps, or the gap is measured and written down. The same code runs hosted and in QEMU.

## Phase 1: Core OS

### M2: Message bus

Goal: services and apps exchange requests and pub/sub messages through `busd`, with trusted identities.

- [ ] `protocol` crate: envelope, error codes, serde types; JSON Schema export and generated TypeScript types.
- [ ] `busd` on tokio: a Unix socket, length-prefixed frames, and a maximum frame size.
- [ ] Identity by construction: `session` creates a socket pair per app and hands the app its end, so identity comes from the connection, not from PIDs. Peer credentials are a second check.
- [ ] Service names (`db`, `compositor`, ...) can only be claimed by connections that `session` marks as system services.
- [ ] Request routing with ID correlation, timeouts and cancellation.
- [ ] Pub/sub topics with prefix wildcards; bounded queues per connection; slow consumers are dropped.
- [ ] File descriptor passing (`SCM_RIGHTS`), so blobs and GPU buffers travel as memfds and dmabufs rather than JSON.
- [ ] Permission hook trait in the routing path; allows everything until M6.
- [ ] `bus-client` crate with an async Rust client and typed helpers.
- [ ] `osctl bus tap`: live trace of messages.
- [ ] Benchmark round-trip latency and throughput on the Pi.

**Done when:** two services talk over `busd` on all three targets, `osctl bus tap` shows the traffic, and latency on the Pi is measured.

### M3: Database v1

Goal: a working document store with queries, live updates, full-text search, links, blobs, history, undo and snapshots.

- [ ] SQLite schema (rusqlite, WAL mode): `documents`, `doc_state` (Loro snapshot), `doc_json` (current JSON), `changes`, `change_sets`, `links`, `blobs`, and an FTS5 table.
- [ ] Document IDs are UUIDv7, so they sort by creation time.
- [ ] Write path: apply changes to the Loro doc, then update the JSON copy, links and FTS index in the same SQLite transaction.
- [ ] Query compiler from the filter JSON to SQL with `json_extract`, plus expression indexes on common fields of system schemas.
- [ ] Live queries: a registry of streamed `watch` requests; each batch of commits re-runs the queries for the affected types and streams the diff (added, changed, removed) back on the request.
- [ ] Blob store: files named by SHA-256 under `/data/db/blobs` (beside the database, so backups take both), uploaded and read through passed file descriptors, with reference counts.
- [ ] System schemas in `schemas/` as JSON Schema; writes are validated, and unknown fields are allowed.
- [ ] History API: list versions, read a document at a version, diff two versions.
- [ ] Change sets and global undo: undo writes a new change that reverts a change set, so the undo itself stays in history.
- [ ] Snapshots: hourly copies of the database using SQLite's online backup API. Blobs are immutable, so a snapshot only needs the database file.
- [ ] `osctl db`: put, get, query, search, link, history, undo, snapshot.
- [ ] Benchmark on the Pi: insert and query 10,000 email-sized documents.

**Done when:** `osctl` can create, query, search, link and undo documents; a live query subscriber gets diffs; snapshots restore; Pi benchmark numbers are recorded.

### M4: App runtime

Goal: a TypeScript app runs in its own process and shows a live database query in a window.

- [ ] `app-host` with rquickjs: ES modules, timers, and an event loop that includes the bus socket.
- [ ] `@idealos/sdk`: bus client, database API (`query`, `live`, `search`, `put`), permissions, UI.
- [ ] Signals library: `signal`, `effect`, `memo` in the SolidJS style. UI functions create nodes once, and effects emit `set` ops. Ops are batched into one commit per microtask.
- [ ] Packaging: esbuild bundles an app; the bundle is a blob and the app is an `App` document holding its manifest. (Built: `tools/build-app.ts`, with esbuild run through npx.)
- [ ] `session` starts one `app-host` per app with that app's bus connection.
- [ ] Compositor: window creation, ownership by connection, and a commit handler for the basic node types (stacks, label, button).
- [ ] `osctl app install --watch` for hot reload.
- [ ] Measure memory per app process on the Pi.

**Done when:** a counter app and an app showing a live list of notes run in windows, both hosted and on the Pi.

### M5: Widgets and window manager

Goal: a full widget set and a usable desktop with windows and workspaces.

- [ ] Layout with taffy (flexbox and grid).
- [ ] Widgets: label, button, text field, multiline text (cosmic-text editor), checkbox, toggle, slider, select, virtual list, scroll view with kinetic scrolling, image, split view, toolbar, menu, dialog.
- [ ] Focus handling and keyboard navigation.
- [ ] Text fields bound to document fields: the compositor edits the CRDT text through `dbd`.
- [ ] Clipboard owned by the compositor, carrying text and document references.
- [ ] Window manager: compositor-drawn title bars, move, resize, maximize, stacking, focus, workspaces with keyboard shortcuts, window switcher.
- [ ] Shell: dock or launcher listing `App` documents, workspace indicator, clock.
- [ ] Theme tokens (colors, spacing, type) in one place, ready for theming later.
- [ ] Widget gallery app for testing.

**Done when:** several apps share workspaces, every widget works in the gallery, and bound text edits survive a restart.

### M6: Permissions and sandboxing

Goal: apps get only what they are granted, for both data and network access.

- [ ] `permd`: grants and audit log stored in a system collection that apps cannot read.
- [ ] `busd` asks `permd` before delivering each message, with cached decisions.
- [ ] `dbd` ANDs grant filters into every query and checks writes before and after they apply.
- [ ] Permission prompts drawn by the compositor in a trusted layer that apps cannot cover or imitate.
- [ ] Sandbox in `session`: a network namespace per app (loopback only without the network grant), a seccomp filter, and Landlock rules that leave apps no filesystem access beyond their runtime directory.
- [ ] `secretsd`: encrypted store keyed by owning service.
- [ ] Bus API to list and revoke grants.

**Done when:** an app without grants gets `permission_denied`; a prompt grants access; revoking database access takes effect immediately; a network grant takes effect after restart; tests show apps cannot open files or sockets they were not given.

### M7: First apps

Goal: the Pi boots straight to a desktop that can join Wi-Fi, browse documents and run a shell. This ends Phase 1.

- [ ] `netd`: wraps NetworkManager's D-Bus API with zbus; bus API to list networks, connect and report status.
- [ ] Settings: Network, Permissions, Display, Storage (database size, snapshots), About.
- [ ] Document browser: types sidebar, search box (FTS), filter builder, document inspector, links, version history.
- [ ] `ptyd` and the terminal: PTYs and VT parsing in Rust; the compositor shows a terminal grid node fed by `ptyd`, so terminal output does not pass through QuickJS.
- [ ] Boot sequence: `session` starts `busd`, `dbd`, `permd`, `secretsd`, the compositor and `netd`, then the shell.

**Done when:** the Pi boots to the desktop, joins Wi-Fi from Settings, and the author can browse documents and use a shell.

## Phase 2: Daily driver

M8 comes first. After that, M9 to M14 depend only on Phase 1 and can be built in any order, except that full HTML email waits for the browser.

### M8: Backup, restore and updates

Goal: no data is lost to a dead SD card or a bad update.

- [ ] `backupd`: scheduled snapshots, and backup to a USB drive. Backups are incremental because blobs are named by hash.
- [ ] Optional encryption of backups, using a key held by `secretsd`.
- [ ] Restore: a recovery boot mode that restores the data partition from a backup.
- [ ] History compaction: keep everything for 30 days, then daily for a year, then weekly; pinned versions are kept; unreferenced blobs are swept.
- [ ] RAUC A/B system updates, with automatic rollback if the new slot fails to boot. How releases reach beta testers (GitHub Releases, a release image, signing, database rollback) is planned in [releases.md](releases.md).
- [ ] Settings: Storage and Backup pages.

**Done when:** after wiping the data partition, a restore from USB brings back every document and blob, and a deliberately broken update rolls back.

### M9: Text editor

- [ ] `TextDocument` schema with CRDT text.
- [ ] Editor app built on the bound multiline widget: open, create, rename, find and replace.
- [ ] Version history sidebar using the history API.
- [ ] Plain text and Markdown first; syntax highlighting later.

**Done when:** the author writes real documents in it.

### M10: Email (Gmail API)

- [ ] OAuth sign-in with the loopback redirect flow. Google's device flow does not appear to allow Gmail scopes; confirm in the spike. Until the browser exists, `osctl mail login` runs the flow on the Mac and sends the token to `secretsd` on the device.
- [ ] `maild`: full sync first, then incremental sync from the Gmail history ID; backoff for rate limits.
- [ ] Map Gmail data to `Email`, `Thread` and `Label` documents. Attachments become blobs.
- [ ] Write back read, archive, label and delete, and send through the API.
- [ ] Mail app: thread list (live query), reader, composer.
- [ ] Show plain text and sanitized simple HTML in the compositor; full HTML through a Servo surface after M13.

**Done when:** the inbox syncs incrementally, and reading, archiving, labeling, replying and sending all work.

### M11: Music player

- [ ] `audiod` on PipeWire, decoding with symphonia.
- [ ] Audio importer: tags read with lofty become `Track` documents; audio files and cover art become blobs.
- [ ] Player app: library by artist and album, playlists, queue, now playing.

**Done when:** an album imported from USB plays from the library.

### M12: Chat (dummy service)

- [ ] Define the chat service interface: the bus API every chat service implements (conversations, messages, send, presence).
- [ ] `chatd-dummy`: simulated contacts who reply after a delay, with presence changes.
- [ ] `Conversation` and `Message` schemas.
- [ ] Chat app built against the interface, not the dummy service.

**Done when:** the chat app works against the dummy service, and the interface is documented for real services.

### M13: Browser (Servo)

- [ ] `browser-host` embeds Servo and renders each tab into a buffer shared with the compositor as a dmabuf, shown through an external-surface node.
- [ ] Input forwarding from the compositor to Servo.
- [ ] Browser app: tabs, address bar, back and forward. History and bookmarks are documents.
- [ ] Downloads go through `importd`.
- [ ] Runs with the network grant, in its own sandbox.

**Done when:** common sites are usable on the Pi, and history and bookmarks show up in the document browser.

### M14: Database shell, import and export, history viewer

- [ ] Database shell: a Nushell-style pipeline language (`emails | where from.name == "Bob" | sort date | first 10`) that prints tables in the terminal.
- [ ] `importd`: detects USB drives and imports images (with EXIF), audio, PDFs, vCards and text. Export writes records back to files.
- [ ] History viewer: a timeline per document, plus a global list of change sets with undo.

**Done when (Phase 2 gate):** the author uses the system as their main computer for writing, mail and music for one week.

## Testing strategy

Most tests run on the Mac and in CI without hardware. QEMU covers boot and Linux-only paths, and a performance script on the Pi closes each milestone.

| Area | How it is tested |
| --- | --- |
| Query compiler | Property tests (proptest): random documents and filters; SQL results must match a simple in-memory evaluator |
| CRDT and JSON copy | Property tests: random edits; the JSON copy must always equal the Loro document's state |
| Database durability | Kill `dbd` during writes in a loop; check integrity and that no committed change is lost |
| Bus | Fuzz the frame parser (cargo-fuzz); every service method gets a permission-denied test |
| Compositor | Headless backend renders to an offscreen texture; golden-image tests on lavapipe for stable output |
| UI behavior | Record scene-op streams from real apps and replay them in tests |
| SDK and apps | Run in `app-host` against a mock bus |
| Boot and integration | CI boots the image in QEMU and runs a smoke script over SSH: desktop up, app installed, query answered, screenshot taken |
| Performance | Script on the Pi records frame time, input latency, bus round trip, query time and memory per app; numbers are tracked per milestone |

## First steps

Start with the repo skeleton, then run the six spikes. They are independent of each other, and their results update the feature list's risks table before M1 starts.

1. Repo skeleton, CI and the build VM (start of M0).
2. **Spike A:** wgpu to KMS in QEMU, on lavapipe with a dumb-buffer copy and on virgl GLES.
3. **Spike B:** wgpu to KMS on the Pi 4, on V3DV with dmabuf import.
4. **Spike C:** Loro plus SQLite on the Pi: write, JSON copy, FTS5 index and query, at 10,000 documents.
5. **Spike D:** memory and startup time of an rquickjs process on the Pi, times eight.
6. **Spike E:** build Servo for aarch64 and render a page into an offscreen buffer.
7. **Spike F:** Gmail OAuth sign-in and one mailbox sync from the Mac, to confirm scopes and token lifetime.
8. Draft the `protocol` crate: bus envelope and scene-graph ops.
