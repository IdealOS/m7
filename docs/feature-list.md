# Database-Centric Desktop OS — Feature List

Snapshot of the living doc as of 2026-11-20. The live, commentable version is at
<https://claude.ai/artifact/TPqCusvLEnsRnSztbSBtas>; the implementation plan is in
[implementation-plan.md](implementation-plan.md).

## Vision

A research desktop OS built around one question: what would a desktop look like if a persistent, versioned, syncable database replaced the filesystem?

- **Audience:** the author and other people who want to experiment. The goal is a research OS the author can use every day.
- **Core idea:** every piece of user data is a database document. That includes contacts, email, settings, app state, documents, and the apps themselves. Users never see files.
- **Base:** a minimal Linux. The kernel, drivers and CLI utilities are reused under the hood. No existing GUI apps run.
- **Target hardware:** Raspberry Pi 4 with 8 GB RAM. The development target is an aarch64 QEMU VM on Apple Silicon macOS.

## Decisions made

These choices are settled. The implementation plan builds on them.

| Area | Decision |
| --- | --- |
| Linux base | Minimal Linux; init and libc are implementation details |
| OS image | Immutable, read-only root with A/B update partitions; only the database is writable |
| Users | Single user |
| Legacy apps | No existing GUI apps; CLIs and system utilities reused under the hood |
| Display protocol | New protocol built on message passing; assumes GPU acceleration |
| Graphics | WebGPU (wgpu) on DRM/KMS; fast 2D first, effects later |
| Compositor model | Scene graph, thick: owns widgets, input, scrolling, text editing |
| Text | Compositor shapes and lays out all text |
| Window management | Standard overlapping windows plus workspaces |
| Data model | Schemaless documents plus system-defined common schemas |
| Storage | SQLite |
| Versioning | CRDT per document; undo anything, anywhere |
| Sync | Designed in via CRDTs; built later |
| Links | First-class links between documents, with graph-style queries |
| Queries | TypeScript query filter, live subscriptions, full-text search |
| Blobs | Content-addressed, deduplicated, versioned with their document |
| Import and export | Importers turn files into records; records export back to files |
| Permissions | Apps start with none, including network access; runtime prompts; grants by type, collection or rule; apps own what they create; revocable and audited |
| Message bus | JSON, request/response plus pub/sub; the bus is the permission broker |
| Core language | Rust |
| App language | TypeScript on QuickJS, one process per app |
| App UI | SolidJS-style signals |
| App install | Apps are database documents |
| Browser engine | Servo, shown through an external-surface node |
| Shells | Both a raw Linux shell and a database-aware shell |
| Chat | Dummy chat service for testing; real protocols later |
| Networking | Existing network daemon, controlled by Settings over the bus |
| Backup | Backup and snapshots are early features |
| Repo | One monorepo: Rust workspace plus TypeScript apps |

## Architecture overview

```
┌────────────────────────────────────────────────────────────────────────┐
│ Apps · TypeScript on QuickJS, one process each                         │
│ [Settings] [Documents] [Terminal] [Editor] [Email] [Music] [Chat] [Browser]
└───────────────────────────────────┬────────────────────────────────────┘
                                    │ SDK calls, UI trees, input events
┌───────────────────────────────────┴────────────────────────────────────┐
│ Message bus · JSON request/response + pub/sub · permission broker      │
└───────┬─────────────────┬──────────────────┬─────────────────┬─────────┘
┌───────┴───────┐ ┌───────┴───────┐ ┌────────┴──────┐ ┌────────┴────────┐
│ Compositor+WM │ │ Database      │ │ Permissions   │ │ System services │
│ wgpu scene    │ │ Loro CRDT docs│ │ grants by type│ │ network, audio  │
│ thick widgets │ │ SQLite + FTS5 │ │ runtime prompt│ │ email sync, chat│
│ windows, wksp │ │ live queries  │ │ audit, revoke │ │ importers       │
│ Servo surfaces│ │ blobs, history│ │               │ │ backup          │
└───────┬───────┘ └───────┬───────┘ └────────┬──────┘ └────────┬────────┘
┌───────┴─────────────────┴──────────────────┴─────────────────┴─────────┐
│ Minimal Linux base                                                     │
│ DRM/KMS · libinput · PipeWire · network daemon · CLIs · RO root, A/B   │
└────────────────────────────────────────────────────────────────────────┘
```

Apps never talk to services directly. Every message crosses the bus, which checks the sender's grants before delivering it. The compositor and the database are peer services behind the bus.

## Phase 0: Bring-up

Goal: boot to pixels. The compositor draws a scene graph on screen in both QEMU and on the Pi.

- **Image build:** minimal aarch64 image built with Buildroot or a slim Debian base. Read-only root, a writable data partition, and A/B slots laid out from the start.
- **Dev loop:** one command builds the image and boots it in QEMU with Hypervisor.framework (HVF) and virtio-gpu.
- **Graphics tiers in QEMU:** lavapipe software Vulkan always works. virgl GLES through UTM is the faster tier. wgpu hides the difference.
- **Pi 4 image:** boots from SD card and renders through the V3DV Vulkan driver.
- **Compositor skeleton:** wgpu output to KMS, libinput for keyboard and mouse, a frame loop driven by vsync.
- **Scene graph v0:** rects, images and shaped text (cosmic-text or parley), with damage tracking.
- **Session manager:** a small init stage that starts the bus, database and compositor, and restarts them if they crash.

## Phase 1: Core OS (MVP)

Goal: a working system where TypeScript apps run in windows, read and write the database through the bus, and ask for permissions.

**Message bus**

- JSON messages over Unix sockets, with request/response and pub/sub.
- Process identity from socket peer credentials, so an app cannot fake who it is.
- Permission broker: every message is checked against the sender's grants before it is delivered.

**Database service**

- Each document is stored as a Loro CRDT, the source of truth. A JSON copy of its current state goes into SQLite for queries.
- Schemaless documents, plus system schemas: Contact, Email, Message, Track, Playlist, TextDocument, Setting, App, Image.
- First-class links between documents, with graph-style queries.
- TypeScript query-filter API, live query subscriptions, FTS5 full-text search.
- Content-addressed, deduplicated blob store, versioned through the documents that reference it.
- Full history. Writes are grouped into change sets tagged with the app and an intent; global undo reverses a change set.

**Permissions**

- Apps start with no grants, for both data and network access, and ask through runtime prompts.
- Grants by document type, collection or rule. An app always has access to documents it created.
- Grants can be revoked, and every grant is recorded in an audit log visible in Settings.

**App runtime**

- One QuickJS process per app, with a TypeScript SDK covering the bus, database, UI and permissions.
- SolidJS-style signals that map directly to scene-graph mutations.
- Apps are installed as database documents; a supervisor launches and monitors them.

**Compositor and window manager**

- Thick widgets: window, label, button, text field, multiline text, virtual list, scroll view, image, flex and stack layout.
- Hover, scroll, focus, text selection and editing happen in the compositor without a round trip to the app.
- Overlapping windows, focus handling and workspaces.
- External-surface node that imports a dmabuf, as the escape hatch for Servo.

**Shell UI**

- App launcher or dock, workspace switcher, permission prompt dialogs.

**First apps**

- **Settings:** network through the daemon, permissions, displays, about.
- **Document browser:** search, filter, follow links, view history.
- **Terminal:** raw Linux shell. VT parsing and the text grid run in Rust (`vte` or `alacritty_terminal`), not in QuickJS.

## Phase 2: Daily driver

Goal: the author can use the system every day for writing, mail, music, chat and browsing, without losing data.

- **Backup and snapshots:** scheduled database snapshots, full export to external storage, and restore. This comes first in the phase, because all data lives in one database.
- **Text editor:** documents stored as CRDT text. The compositor text widget can bind straight to a document field.
- **Email:** a sync service built on the Gmail API writes messages and labels into the database and sends mail through the same API. The mail app is a set of queries plus a composer.
- **Music player:** an audio service on PipeWire or ALSA. Tracks and playlists are documents; audio files are blobs.
- **Chat:** a dummy chat service that produces fake conversations, plus a chat app. A service interface leaves room for real protocols later.
- **Browser:** Servo renders into an external surface. The browser must be granted network access. Downloads go through the importer.
- **Database shell:** a Nushell-style shell for queries such as `emails | where from.name == "Bob"`.
- **Import and export:** importers turn files from USB or downloads into typed records (images, audio, PDFs, contacts). Export turns records back into plain files.
- **History viewer:** a time-travel view for any document, and the UI for global undo.

## Phase 3: Later

- Sync across devices, using the CRDT history already in place.
- Real chat services behind the chat service interface.
- An automation and scripting layer built on bus messages and database queries.
- Theming.
- Visual effects: blur, shadows, animations, 3D.
- HiDPI and multi-monitor.
- Accessibility, touch input, voice and AI features.

## Technical risks and early spikes

Each of these should be prototyped before the plan relies on it.

| Risk | Why it matters | Spike |
| --- | --- | --- |
| wgpu output to KMS | wgpu does not present to KMS directly | Render to a texture, export a dmabuf, import it as a KMS framebuffer and page-flip; fall back to copying into a dumb buffer on lavapipe |
| GPU in QEMU on macOS | Vulkan pass-through is unreliable on macOS hosts | Confirm lavapipe works on plain virtio-gpu, then try virgl GLES through UTM |
| Pi 4 GPU limits | V3DV is Vulkan 1.2 with low fill rate | Measure frame time at 1080p with 20 or more windows; set downlevel wgpu limits |
| Servo on aarch64 | Servo renders with its own GL stack, separate from wgpu | Build Servo for the Pi and share its frames with the compositor as dmabufs |
| CRDT plus SQLite | Two copies of each document must stay consistent and fast | Benchmark Loro writes plus JSON updates and FTS5 indexing on the Pi |
| Live query cost | Re-running queries on every change can get expensive | Start by re-running queries; add incremental updates where needed |
| QuickJS memory and speed | 8 or more app processes on the Pi | Measure resident memory per app and signal-update throughput |
| Gmail API access | Needs a Google Cloud OAuth client; restricted Gmail scopes in an unverified app stay in testing mode, which limits test users and shortens how long sign-ins last | Register an OAuth client, sign in from the device, sync one mailbox, and check how long the refresh token lasts |

## Open questions for the implementation plan

- [ ] Buildroot or a slim Debian base for the image?

  **Recommendation: Buildroot.** It has a `raspberrypi4_64` config and QEMU aarch64 configs, builds a small read-only root by default, and packages Mesa (lavapipe, V3DV), libinput, PipeWire, NetworkManager and RAUC for A/B updates. Debian is quicker to start with but fights an immutable image.

- [ ] Loro or Automerge as the CRDT library?

  **Recommendation: Loro.** It is Rust-native and fast, its format is stable since 1.0, and it has built-in version checkout (time travel), rich text, movable lists and trees, and shallow snapshots for trimming history. Automerge is the more mature alternative. Hide the choice behind a small `DocStore` trait so it can be swapped.

- [ ] How does an app say which part of the scene graph it owns, and how are widget events sent back to it over the bus?

  **Proposal:** each window is a subtree owned by the app's bus connection. Apps allocate their own node IDs (u64 per connection), so they never wait for the compositor to create a node. Apps subscribe to events per node (`click`, `change`, `submit`, `key`); the compositor sends events tagged with node ID only for subscribed nodes. Hover and scroll stay inside the compositor.

- [ ] Is the UI tree sent as full trees or as patches, and what does a single scene-graph mutation message look like in JSON?

  **Proposal: patches, batched per transaction.** Ops are `create`, `set`, `insert`, `remove` and `destroy`. The first render is just a batch of `create` ops. SolidJS-style signals produce exactly these fine-grained ops. The message format is in the implementation plan's Key interfaces section.

- [ ] What does a permission rule look like? For example, type = Email and from in Contacts tagged family.

  **Proposal:** a grant is `{app, access: read | write | create, type, filter}`, where `filter` uses the same query-filter language apps use. The database ANDs the grant filter into every query the app runs (row-level security) and checks writes against it before and after the change.

- [ ] How is network access enforced per app? Options: each app runs in a network namespace with no interfaces until granted, or all traffic goes through a proxy service on the bus.

  **Recommendation: network namespaces.** The supervisor starts every app in its own network namespace with only loopback. Apps with the grant start in the host namespace. Changing the grant restarts the app. Add seccomp to block raw sockets, and Landlock network rules where the kernel supports them. A proxy can come later for per-host rules.

- [ ] Where are OAuth tokens and other secrets stored, so only the service that owns them can read them?

  **Recommendation: a separate secrets service.** It keeps an encrypted SQLite file outside the main database, so secrets never show up in queries, history or sync. Each secret is keyed by its owning service, and the bus checks the caller's identity. The Pi 4 has no TPM, so the key comes from the user's login passphrase, with an option to store it on the device for no-login setups.

- [ ] How are app documents signed or trusted before they are allowed to run?

  **Proposal:** an app document holds a manifest (name, entry point, requested permissions) and the hash of its code bundle blob. The manifest is signed with an ed25519 developer key. The system trusts a list of keys; developer mode lets the user trust a new key or run unsigned apps.

- [ ] When are old versions garbage-collected so history does not fill the SD card?

  **Proposal:** keep every change for 30 days, then compact to daily snapshots for a year, then weekly. Versions the user pins are never pruned. Blobs no longer referenced by any kept version are removed by a mark-and-sweep pass. Settings shows history size and warns before storage runs low.

- [ ] Which network daemon: NetworkManager or iwd plus systemd-networkd?

  **Recommendation: NetworkManager.** One D-Bus API covers Wi-Fi, Ethernet, VPN and hotspot. A small `netd` service wraps it (with the `zbus` crate) and exposes a bus API to Settings. iwd plus networkd is lighter but splits the work across two APIs.
