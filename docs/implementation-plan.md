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
                      M8 Backup and restore
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

### M8: Backup and restore

Goal: no data is lost to a dead SD card. Encrypted backups, a recovery boot and system updates moved to a later milestone (see [Later: encryption, recovery and system updates](#later-encryption-recovery-and-system-updates)).

- [ ] `backupd`: scheduled snapshots, and backup to a USB drive. Backups are incremental because blobs are named by hash.
- [ ] Restore onto a running system (Settings > Backup, or `osctl backup restore`): a freshly flashed card boots, then takes on the backed-up device's database and identity.
- [ ] History compaction: keep everything for 30 days, then daily for a year, then weekly; pinned versions are kept; unreferenced blobs are swept.
- [ ] Settings: Storage and Backup pages.

**Done when:** after wiping the data partition, a restore from USB brings back every document and blob.

### M9: Text editor

Goal: a place to write that never loses anything. Documents are database records, so there are no files and no Save button: every keystroke is stored, history keeps every version, and undo survives a restart. Most of the work is UI; the database already does persistence, history, search and links.

Compositor work first:

- [x] **Bound fields follow live changes.** A bound field follows its document with a live query (`$id` picks the one document), read as its app. Another writer's change shows with the caret kept on its character; changes that arrive while the field's own edits are in flight wait until db has answered them. The Style Editor's Reload button is gone.
- [x] **Long documents.** Text is shaped and laid out a paragraph at a time and kept between frames (`paragraphs.rs`): typing reshapes one paragraph, and only what's on screen and above it is laid out. On an M3, a 200 KB document went from 127 ms a keystroke to under half a millisecond (`make text-bench`). Two bugs this showed up are fixed: clicking into an unfocused long text area jumped to its end, and Up and Down in a text area outside a layout container moved by rows wrapped at a made-up width. Still to do: measure it on a Pi.
- [ ] **Styled runs in a text area** (bold, headings, colours), for Markdown shown as you type and later syntax highlighting. Not needed for the first version, which previews beside the text.

The editor:

- [x] `TextDocument` schema with CRDT text (the body as Loro text, so edits merge); `format` is plain text or Markdown; `tags`. Text fields become Loro text the first time they're spliced, which the bound fields do.
- [x] Editor app built on the bound multiline widget: create, open, rename, delete (undoable). `delete` now answers with its change set, so the app can `undo` it.
- [x] A document list: recent first, full-text search, and filters by tag and format. Queries can order by `$updated` and `$created`.
- [x] Find and replace, with match case and whole word. Text widgets report their caret and selection to apps that listen (`select`), and apps can select text, focus a field and add their own change sets to its undo history.
- [x] Word and character counts, and the caret's line and column.
- [x] Version history sidebar using the history API: a timeline to scrub through, the diff of each change, and restoring a version (a new change, so it can be undone). Pinned versions (`pin`), such as "draft sent", which history compaction keeps. Apps may now pin versions of documents they may change; names are kept in the document's `pins`.
- [x] Markdown: a live preview beside the text (headings, lists, emphasis, links, code); links to other documents as `$link`, so backlinks work; images stored as blobs. Emphasis shows as plain text until styled runs exist.
- [x] Writing comfort: monospace or serif modes through the app's `style.css`, a focus mode that hides the toolbar, and the Emacs and macOS text keys. New: Command-Z and Command-Shift-Z (through db's change sets), Option-Up and -Down by paragraph, Page Up and Down, and Command-Up staying in a text area.

Later:

- [ ] Syntax highlighting for code.
- [ ] Several cursors; editing together across devices once sync exists.
- [ ] Export to `.md` and `.txt` (with M14's import and export).

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

Goal: your music library on the Pi, playing without a skip. Playback lives in `audiod`, a service, not in the app: the app's JavaScript can stall or quit, but audio mustn't, so the app is a remote control and music keeps playing when it's closed.

System work first:

- [ ] **Audio in the image:** ALSA (alsa-lib) and the Pi's audio drivers (headphone jack, HDMI, USB audio), with the output chosen in Settings; virtio-snd in QEMU, played through the Mac. No PipeWire: audiod is the only thing that makes sound, and it does its own mixing (its mixer is where system sounds would join). Done: alsa-lib and audiod are in the image and session starts audiod; QEMU has virtio-snd (output only), which `tools/run-qemu` plays through the Mac and the smoke test records to a WAV file to check that a Track's sound comes out. The Pi 4 has `dtparam=audio=on` for the headphone jack and HDMI audio through vc4, both loaded as modules, but is untested on hardware. Left: choosing the output (it's ALSA's default device today) in Settings, and USB audio checked on the Pi.
- [x] **A media control the compositor draws** in the top bar (play, pause, next, what's playing) and media keys, so playback can be controlled from anywhere. Trusted chrome, like the prompt. Done: the compositor follows audiod's `watch` and, while something is queued, shows previous, play or pause, next, and the title and artist after the workspace buttons; clicking the title brings Music's window forward, or starts it. Media keys (XF86Audio play, pause, stop, next, previous, volume up and down, mute) go to audiod from anywhere, a permission prompt included, and never reach apps; volume steps by 5% and Mute gives back the volume it took. The smoke test presses Play/Pause in QEMU and checks audiod answered. Not tried on a Pi keyboard. The skip buttons are drawn as doubled triangles, since Inter has no ⏮ or ⏭.
- [ ] **Finding files on USB drives:** the start of M14's `importd`, sharing backupd's drive detection and mounting.

`audiod`:

- [x] Decoding with symphonia (MP3, AAC, ALAC, FLAC, Vorbis, WAV, AIFF; not Opus), playing through ALSA with cpal (CoreAudio on a Mac: `make audio`), gapless between tracks. Each track's blob comes from db as an open file. One output rate, so sources can be mixed: tracks at other rates go through audiod's own windowed-sinc resampler, which keeps its state across tracks, so consecutive tracks join sample-exactly. Played on a Mac through its speakers: an album's tracks rolled into each other with no underrun.
- [x] Play, pause, next, previous, seek and volume; a queue it owns (play next, add to queue, shuffle, repeat). Pausing and volume changes fade over 10 ms. `osctl audio` drives it. Still to do: ReplayGain from the tags (importd doesn't keep them yet).
- [x] A `watch` stream of what's playing, for the app and the top bar. The position is sent with the time it was true, so watchers count on their own rather than getting a part every second.

The library:

- [x] An importer: tags read with lofty become `Track` documents; audio files and cover art become blobs, stored once (duplicates are skipped, since blobs are named by hash), with progress shown. Built as part of `importd` (M14), from a share or a folder; from USB drives is still to do.
- [x] Ratings, play counts and last played on each `Track`. Play counts and last played are set by audiod when a track is heard to its end; a `rating` of 1 to 5 stars is set from a menu of stars on each song in Music and on what's playing.
- [x] `Playlist` documents linking to tracks, and smart playlists as live queries ("rated 4+", "added this month", "never played") that keep themselves current. Done: a smart playlist's `smart` rule (minimum rating, added within so many days, played or never played, artist, genre, order, at most how many) becomes a `Track` query that Music follows live, so rating, playing or importing a track updates it at once. Queries can filter on `$created` and `$updated` for "added this month". Five smart playlists are built into Music rather than stored. A time window counts back from when its page opened.

The player app:

- [ ] Browse by artist, album, genre and year, with cover art; full-text search over titles, artists and albums. Done (`apps/music`): albums as a grid of covers, an album's tracks, artists, every song, and a search (each word, without case or accents) over titles, artists and albums. Albums group by album artist, or else by the folder the files came from, so a soundtrack stays one album by "Various artists". Lists are virtual, so a library of thousands costs only the rows on screen. Covers show in PNG or JPEG; an album without a picture shows its initials on a colour of its own. Left: genre and year.
- [ ] Now playing: large cover art, progress, and the queue beside it. Done: a bar along the bottom (cover, title, previous, play and pause, next, a position slider that seeks once a drag rests, shuffle, repeat, volume) and an Up Next view of audiod's queue (jump to, remove, clear). Each track has Play next and Add to Up Next. Left: a large now-playing view.
- [x] Playlists: make, edit and reorder them (drag and drop). Make one (or a smart one, with its rules in a dialog), add songs from a song's ⋯ menu or a whole album, drag a song to a new place (or Move up / Move down in its ⋯ menu, for the keyboard), remove it, rename, and delete with Undo. Dragging is the toolkit's: a `virtualList` given `onReorder` has rows the compositor drags (a drop line, scrolling near the edges, Escape cancels) and hears one `reorder` event on the drop.

Later:

- [ ] Lyrics from the tags, and an equaliser.
- [ ] Internet radio and podcasts (with the network grant).
- [ ] Playing from a backup drive without importing.

**Done when:** an album imported from USB plays from the library, keeps playing with the app closed, and is controlled from the top bar.

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
- [ ] `importd`: the border between files and the database. Files come in as typed documents (their bytes as blobs); documents go out as ordinary files. Parts of it come earlier: M11 needs the USB scanning and the audio importer, M13 hands it downloads.
  - **Finding files:** shares backupd's drive detection and mounting (mounted only while reading), adding FAT32 for cameras and older sticks. A scan lists what a drive holds by kind, with counts and sizes, before anything is imported. Downloads arrive as a file descriptor, as `put_blob` takes them.
  - **One importer per kind:** images (JPEG, PNG, HEIC) become `Image` documents with EXIF date, camera, location and size; audio becomes `Track` documents (lofty tags, cover art); PDFs a new `Document` type (title, author, pages, text for search, maybe later); vCards `Contact` documents; `.txt` and `.md` `TextDocument`s.
  - **Safe to repeat and undo:** blobs are named by hash, so re-plugging a card imports nothing twice; each import is one change set ("imported 312 photos from CAMERA"), so it undoes in one step; each document keeps where it came from (drive, path, date).
  - **Export:** chosen documents or a query ("all photos from 2025") written to a drive as files: images in their original bytes, contacts as `.vcf`, text as `.md`.
  - **Who may use it:** trusted apps with `service:import` (Settings, a Files app, Music for its own imports); it writes many types, so it acts with its own rights. It parses untrusted files, so it runs sandboxed like an app, and a bad file fails as one item, never the service.
  - **Network shares:** SMB, which macOS File Sharing, Windows and NAS boxes all serve. Computers are found by name over Bonjour (mDNS, `_smb._tcp`); the user signs in once and the password is kept in `secretsd`. A share is mounted read-only with the kernel's CIFS client (`nosuid`, `nodev`, `noexec`) only while scanning and importing, then goes through the same scan and importers as a USB drive. Bulk imports (a whole music archive) run in the background with progress, carry on with the window closed, and resume cheaply after an interruption, since anything already stored by hash is skipped. Needs CIFS in the kernel, mDNS discovery, and the `network` grant for `importd`.
  - **First slice (built):** enough for QEMU to browse a folder shared from the Mac and import its files into the database. CIFS in the QEMU kernel; `importd` connects to a share by address (QEMU's network reaches the Mac at `10.0.2.2`; Bonjour can't cross it, so discovery waits for the Pi), with the password in `secretsd`; `browse` lists folders and files; `import` copies the chosen ones in as blobs with their documents (text and images first, then audio), in the background with progress, skipping what's already stored. `osctl import` drives it, then a small Files page. Done when QEMU lists a Mac folder and imported files show up in the database. Built so far: the `import` service and protocol, shares mounted read-only with the kernel's CIFS client (in the image's kernel now), `dir:` sources for development, text and image importers, skipping by hash, and `osctl import`. Tried in QEMU against macOS File Sharing: browsing a 4,347-file Music library and importing its 160 pictures; importing again skipped them all. A Mac account needs Windows File Sharing ticked (Sharing > File Sharing > Options) before Linux can log in. Linux 6.12 could hang unmounting the share, so importd mounts with `nohandlecache` and unmounts lazily. Audio followed: music becomes `Track`s with lofty's tags, length and embedded cover (one blob per distinct cover); two Enya albums (MP3 and M4A) came in from the Mac with their tags. Then each import became one change set (`create` can join a change set its caller made; `import undo` takes it back), and the Files app browses a share and imports from it. Then the sandbox: importd decodes nothing itself. Music tags and picture sizes are read by `import-reader`, which importd starts per import with session's app sandbox moved into a `sandbox` crate and made tighter (its own Unix user, no network, no file it may open, nowhere to write, limits on memory, open files and file size), and hands open files over a socket pair; a reader that crashes, hangs (30 s) or answers nonsense fails that file and is replaced, and importd bounds every field it answers. Files are opened with `openat2` (beneath the share, no symbolic links), so a server can't swap in a link between listing and opening. Left: importd itself still runs as root to mount; it could keep only the mount right, or hand mounting to a smaller helper. A hostile server's attack on the kernel's CIFS client is beyond any process sandbox.
  - **A developer shortcut:** `make import HOST=… DIR=…` sends files from a Mac over SSH to `osctl import`, for testing the importers.
  - Later: photos straight from a phone (MTP); playing from a share without copying (index only), which breaks when the computer sleeps and leaves the music out of backups.
- [ ] History viewer: a timeline per document, plus a global list of change sets with undo.

**Done when (Phase 2 gate):** the author uses the system as their main computer for writing, mail and music for one week.

## Future projects

Not scheduled. Each needs its own go-ahead.

### Later: encryption, recovery and system updates

Moved out of M8, to come after the Phase 2 apps or when beta testers need them.

- [ ] Optional encryption of backups. The key must survive a dead SD card, so it needs a recovery code the user writes down or a passphrase, not only a copy in `secretsd`.
- [ ] A recovery boot mode that restores the data partition from a backup, for a device that can't start.
- [ ] RAUC A/B system updates, with automatic rollback if the new slot fails to boot. The image already has both root slots. How releases reach beta testers (GitHub Releases, a release image, signing, database rollback) is planned in [releases.md](releases.md).

**Done when:** an encrypted backup restores with only its recovery code, a device that won't boot restores from USB, and a deliberately broken update rolls back.

### Pi 3: OpenGL ES 2.0 renderer

The Pi 3's GPU (VideoCore IV, Mesa's `vc4` driver) only does OpenGL ES 2.0. It has no Vulkan driver (V3DV starts at the Pi 4's VideoCore VI), and wgpu's GL backend needs ES 3.0, so today the Pi 3 renders on the CPU through lavapipe. On a Pi 3B+ at 1080p a frame takes about 250 ms (4 fps at best), and the compositor uses about a full core even at rest. Rendering at 720p and letting the display hardware scale it up (`--render-height`) makes text fuzzy and didn't visibly help.

- [ ] A second renderer behind the backend-neutral `DrawList`, on EGL with GBM and GLES 2.0. The scene, window manager and apps don't change.
- [ ] Rounded rectangles and shadows as the same SDF shaders. ES 2.0 has no instancing, so build each quad's vertices on the CPU (a frame is a few hundred quads).
- [ ] Its own glyph atlas for text: glyphon is wgpu-only.
- [ ] Zero-copy scanout: render into GBM buffers and page-flip them, with no readback or copy.
- [ ] The KMS backend picks this renderer when wgpu finds only a CPU adapter and `vc4` can render; the hosted backend and QEMU keep wgpu.
- [ ] The image enables Mesa's `vc4` Gallium driver for the Pi 3.

**Done when:** the Pi 3 draws the desktop at native 1080p at 30 fps or better while a window is dragged, with sharp text.

### Styles, fonts and transitions

How things look, and how they move, without apps drawing anything themselves. Today's theme is a fixed set of constants (`theme.rs`) that apps name with `$accent`. Colour tokens are already looked up as each frame's draw list is built, so restyling is a table swap and region damage redraws what changed with no new code. Window textures are a later phase (see screen and window effects).

**Style sheets: CSS-like, but simpler.** Colours and fonts only; layout stays in props.

```css
:root   { --accent: #3b82f6; --text: #1f2937; font-family: "Inter Variable", sans-serif; font-size: 14px; }
button  { background: var(--control); color: var(--text); }
button.primary { background: var(--accent); color: var(--accent-text); }
button:pressed { background: var(--control-pressed); }
.code   { font-family: "DejaVu Sans Mono", monospace; }
@dark   { :root { --text: #e5e7eb; --surface: #111827; } }
```

- [ ] **Selectors:** a widget kind (`button`), a class (`.primary`, from a node's `class` prop), both (`button.primary`), each with at most one state (`:hover`, `:pressed`, `:focus`, `:disabled`, `:checked`), and `:root` for variables. Selectors separated by commas share a rule. No combinators, ids or attribute selectors, so a rule never depends on where a node sits. Specificity is 1 for a kind and 10 for a class or a state, as in CSS; at a tie the app's rule beats the system's, then the later one wins. A rule with a state sets colours only, so a state never changes a widget's size.
- [ ] **Properties:** `color`, `background`, `border-color`, `accent-color` (checked boxes, toggles, sliders), `caret-color`, `selection-color`, `placeholder-color`; `font-family` (a list ending in `sans-serif`, `serif` or `monospace`), `font-size`, `font-weight`, `font-style`. Variables are `--name` and `var(--name)`; today's `$accent` tokens become those variables, so existing apps keep working. `color`, `accent-color` and the font properties inherit down the node tree, as in CSS. At the top of an app's window the `:root` rules are applied again with the app's variables, so overriding `--accent` recolours its widgets.
- [ ] **One system style, overridable per app.** The system sheet ships in the image, with an `@dark` section (the one at-rule) that the user's light or dark choice turns on. An app may ship `style.css` in its bundle; it applies after the system sheet, to that app's windows only, so overriding `--accent` recolours its own widgets. A node's own colour props override both. App sheets never reach the frame, title bar or prompts: that chrome carries the trusted signals (permission tiers, workspace hues).
- [ ] **Parsing in the compositor,** by a small hand-written parser for exactly this subset, reporting the line and column of a mistake to the app (`osctl app install` shows it). Resolved styles are cached per node and recomputed when a class, state or sheet changes, not every frame.
- [ ] **Theme choice as a `Setting` document** the compositor follows with a live query: switching to dark restyles every window at once. Settings gets the light and dark choice.
- [ ] **Clean up raw colours:** `#ffffff` on the accent in Settings and the browser becomes `--accent-text`, and Notes' `#4b5563` becomes `--muted`.

**Fonts: several families, local only.**

- [ ] Inter stays built in as `sans-serif`. The image adds a monospace and a serif family as font files on the root file system, and the compositor loads that directory at start. `font-family` names any installed family, falling back along its list to the generic one.
- [ ] The terminal and code take `monospace` from the style instead of a fixed face.
- [ ] No downloaded or app-bundled fonts yet. A font file is parsed inside the trusted compositor, so fonts an app ships need checking first (or a separate parsing process) before they're allowed.

**Transitions: in the compositor only.** Apps set the end value; the compositor animates there, so motion never waits on an app's JavaScript or the bus.

- [ ] **What animates in this phase:** position (`x`, `y`, and the window manager's window positions) and colours (any colour prop and style-resolved colour, interpolated in Oklab so fades look even, including alpha for fading single elements). Not size, scale or whole-window opacity: those need window textures.
- [ ] **Declared, CSS-style:** a node's `transition` prop names props and durations in milliseconds (`{"x": 200, "color": 150}`), with an optional easing. Widget state colours (hover, pressed, focus) fade over a short system default.
- [ ] **One animator,** advanced from `Desktop::tick` and timed by the display's page-flip timestamps rather than the wall clock. It writes current values into compositor-owned `_` props that layout and drawing already read, and a value changed mid-animation retargets from where it is. The damage tracker compares draw lists, so it redraws exactly what moved.
- [ ] **Uses in the window manager:** windows slide when switching workspaces, windows glide when placed or snapped, and menus, the switcher and dialog backdrops fade in.
- [ ] **A reduce-motion setting** that makes every transition an instant change.
- [ ] **Later phase, with window textures:** fading or zooming whole windows as they open, close and minimize (a closing window fades from a saved copy of its last texture), animated resizing, and blur.

At 3440x1440 the Pi 4 moves a window at about 35 fps (a drag's cost, mostly the fixed tile cost of a partial frame), and 1080p should reach 60. Short transitions, 150 to 200 ms, hide a lower frame rate well.

**Done when:** switching to dark in Settings restyles every open app at once; an app overrides the accent colour and a font from its own `style.css`; the terminal's family comes from the style; switching workspaces slides the windows across, and hover and press colours fade, at full frame rate on the Pi 4 at 1080p.

### Grow /data to fill the card

- [ ] **On first boot, grow the data partition to fill the SD card.** `/data` is a fixed 2 GB in the image today, whatever the card's size, so a music or photo library soon fills it. `S05data` would grow the partition (it's the last one) and the ext4 filesystem on it once, at first boot, and record that it did. Needed before bulk imports from shares or USB; worth doing before M10 and M11.

### HiDPI scaling

- [ ] **A scale setting for high-density displays**: draw everything at 2× or 3× on a HiDPI screen (a 4K monitor, a laptop panel), so text and widgets stay a readable size and look sharp. The renderer and the layout already work in logical pixels with a scale factor (the hosted backend uses the Mac's), so this is a `display.scale` setting the compositor follows, the KMS backend dividing the mode's size by it, input coordinates scaled to match, and the hardware cursor drawn at that scale. Settings > Display picks 1×, 2× or 3× (and suggests one from the display's size and resolution).

### Screen and window effects

Shaders applied to the whole screen (a warm night tint) or to particular windows (dim inactive windows, grey out one that isn't responding, frosted glass, open and minimize animations). Each window is already its own layer in the draw list (`Scene::draw_list`: one layer per child of the root), which is the seam both kinds of window effect use.

Beyond the usual effects, one aim of IdealOS is to explore new uses of GPU effects in the window manager: showing what an app is allowed to do and is doing, not just decorating it. The compositor already knows enough for that without new plumbing: each window's app (`apps.rs`), that app's grants, live (it follows permd's table on `perm.grants`, so a revocation arrives within milliseconds), and each window's workspace (`wm.rs`).

- [ ] **Night tint in the display hardware.** Set the CRTC's gamma table (and its colour matrix where the driver has one) from `backend/kms/display.rs`. It costs nothing per frame, changing it redraws nothing (so it can fade at sunset), and it covers the hardware cursor, which nothing in our renderer reaches. KMS only; on the Mac, Night Shift does this. Check what `vc4` offers (its reported gamma size) on the Pi 4.
- [ ] **Window colour effects (level 1).** An `effect` on `Layer` (`scene.rs`), filled from a compositor-owned window prop, passed to `quad.wgsl` and `image.wgsl` as a colour matrix and opacity; text, one colour per run, gets the same matrix applied to its colour in `render.rs`. No extra passes or textures. Per-pixel colour only: no blur or movement, and opacity applies per element, so overlapping parts of a translucent window show through each other.
- [ ] **Per-window shaders (level 2), a later phase.** Render a window's layer into its own texture, then draw that texture into the frame through a shader chosen for the window: true whole-window opacity, blur behind, scaling and warping for animations, thumbnails in the window switcher. An unchanged window reuses its texture, a step toward drawing only what changed. About 8 MB of texture per 1080p-sized window and a compositing pass each: fine on the Pi 4's GPU, heavy on the Pi 3. Design it together with region damage and M13's external surfaces (Servo's tabs arrive as GPU buffers), which need windows as textures too.
- [ ] **A setting for it.** A `Setting` document the Settings app edits and the compositor follows with a live query. A night tint on a schedule needs the clock set first: nothing sets the time yet.
- [ ] **Who chooses.** The compositor, the window manager and the user choose effects. Apps may at most pick from a fixed set of system effects and never supply shader code: a shader would run in the trusted compositor and the GPU driver (on the Pi 3, compiled by lavapipe in the compositor's own process), outside the app sandbox, and a transparent or warped window could imitate another app or hide where clicks land.

Ideas for effects that carry meaning:

- [ ] **Tint by permission.** Collapse an app's grants into a few tiers a person reads at a glance: only its own data (no tint), reads shared documents, uses the network, full access to the device (`service:pty`, as the Terminal holds). Revoking a grant in Settings re-tints the window at once, which shows the revocation took.
- [ ] **Signals go in trusted chrome.** The title bar and frame are drawn by the compositor, where apps can't draw, the same reason permission prompts can be trusted. A tint inside the content area is decoration only: an app could imitate it, or cancel it out with complementary colours.
- [ ] **An edge glow, not a wash.** Multiplying a whole window's colours distorts photos and colour work; a soft glow around the window says the same thing. It's nearly free: the quad shader already computes each rounded rectangle's signed distance, so a glow is alpha falling off with distance outside the shape, one more instance per window and no extra pass, cheap even on the Pi 3.
- [ ] **Not colour alone.** About 1 in 12 men has some colour-vision deficiency, so pair each colour with a pattern the shader draws as cheaply (diagonal hatching on the frame for full device access) or a badge in the title bar.
- [ ] **A hue per workspace.** In each window's frame, the top bar's workspace buttons and the Alt+Tab switcher; a window moved to another workspace (Ctrl+Alt+Shift+arrow) sweeps smoothly to the new hue.
- [ ] **Workspaces as security contexts.** A workspace could carry rules: a "banking" workspace that blocks pasting into other workspaces, or marks any app there that holds `network` more strongly. Its hue then shows both where you are and which rules apply. This needs the window manager and the permission system to work together, which IdealOS can do because the compositor sees both.
- [ ] **Show data access as it happens.** As phones light a dot while the microphone is on, the compositor could pulse a glow on a window while its app reads documents: dbd sees every read and would publish access events ("Mail read 40 Contacts"). The event stream itself reveals what apps do, so only trusted services may subscribe.

Where each lands: the frame tint and edge glow need only the existing quad shader and theme tokens (`draw.rs`, `theme.rs`), with tiers worked out from the grants table the compositor already follows; content tints come with level 1 window effects; the data-access glow needs a new dbd event stream and a rule for who may subscribe.

**Done when:** a night tint fades in at a set time on the Pi, inactive windows can be dimmed, one per-window shader (blur behind a dialog, or an open animation) runs at full frame rate on the Pi 4, and windows show their permission tier and workspace in their frames.

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
