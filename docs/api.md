# API reference

Everything in IdealOS talks over one message bus. This page lists what each
service on it offers, how apps build their windows, how style sheets work, and
the TypeScript SDK apps are written with. The Rust types in `crates/protocol`
are the source of truth: `make protocol` generates `sdk/src/protocol.ts` and
`crates/protocol/schema.json` from them, and each module's documentation there
has the full detail. For how the pieces fit together, see the
[architecture map](architecture.html).

## The bus

`busd` is the broker. Every process connects to its Unix socket
(`/run/idealos/bus.sock` on the device). A frame is a 4-byte big-endian length
and one JSON message. The broker stamps each message's sender on it (a system
service, a tool, or an app by its id), so no one can pretend to be someone else.

- **Requests** go to a service by name and get one answer: a result, or an
  error with a `code` and a `message`. A **streamed** request gets any number of
  parts until either side cancels it, which is how live queries, windows and
  terminals work.
- **Topics** carry publications to every subscriber (`perm.grants`, for one).
- **Names**: a service claims a name (`db`, `net`, ...). System services and
  tools may claim names; apps may not.
- **Grants** decide what an app may reach. An app calls a service `NAME` only
  with the `service:NAME` capability, and reads or writes documents only as its
  data rules allow (see [perm](#perm-permissions)). System services and tools are
  trusted.

From a shell, `osctl` speaks the bus: `osctl bus list`, `osctl bus call TO
METHOD [JSON]`, `osctl bus stream TO METHOD [JSON]`, `osctl bus sub PATTERN`, and
shorthands for the services below. Error codes the broker sends:
`bad_request`, `no_such_service`, `name_taken`, `permission_denied`, `timeout`,
`service_gone` and `limit_exceeded`; services add their own.

The broker's own methods, to `bus`: `hello` (who am I), `claim` and `release`
(a name), `list` (connections and names), `adopt` (session hands a connection
an app identity).

## db: documents

`dbd` serves the document store. Every piece of data is a document: an id
(UUIDv7), a type, its data (JSON, at most 1 MiB), and its history. Bulk data
lives in blobs, named by their SHA-256.

| method | params | result |
| --- | --- | --- |
| `create` | `{type, data}` | the new document |
| `get` | `{id}` | the document |
| `update` | `{id, patch}` (a JSON merge patch) | the document |
| `splice` | `{id, path, at, delete, insert}` | `{change_set}`: an edit to a text field, as collaborative text |
| `delete` | `{id}` | null (its history stays, so it can be undone) |
| `query` | a query (below) | `{docs}` |
| `search` | `{text, type?, limit?}` | `{docs}`: every word matches |
| `watch` | a query, streamed | the result, then each change: `{kind: "initial", docs}`, then `{kind: "diff", added, changed, removed, order?}` |
| `links` | `{id}` | `{outgoing, incoming}` |
| `history` | `{id}` | `{versions}`: each change, with `pinned` |
| `at` | `{id, change_set}` | `{data}` as it was then |
| `diff` | `{id, from, to}` | `{changes}`: fields that differ |
| `changes` | `{limit?}` | `{change_sets}`, newest first |
| `undo` | `{change_set}` | `{change_set}`: the undo, itself undoable |
| `put_blob` | a file descriptor | `{hash, size, ...}` |
| `get_blob` | `{hash}` | the blob's info, and a read-only descriptor |
| `blob_info` | `{hash}` | `{hash, size, created, refs}` |
| `schemas`, `types`, `stats` | none | the system schemas; types and counts; sizes (and `device`) |
| `snapshot`, `snapshots` | none | take one now; list them |
| `restore` | `{name, adopt?}` | `{saved}`: the snapshot of what it replaced |
| `pin` | `{id, change_set, pinned?}` | null: history compaction keeps that version |
| `compact` | none | `{versions, change_sets, blobs, blob_bytes}`: what it removed |

`restore`, `pin` and `compact` are for system services and tools; `stats` and
the snapshot methods need the `storage` capability.

**Queries** look like `{"type": "Email", "where": {"from.address": "a@x"},
"orderBy": [["date", "desc"]], "limit": 50}`. Filters use `$eq`, `$ne`, `$gt`,
`$gte`, `$lt`, `$lte`, `$in`, `$nin`, `$contains`, `$exists`, `$and`, `$or`,
`$not`, and for links `$linksTo` and `$linkedFrom`; `$createdBy` matches the app
that made a document.

**Links and blobs** live in document data: `{"$link": "<id>"}` and `{"$blob":
"<sha-256>"}`. A blob must be stored before a document refers to it.

**History** keeps every version for 30 days; older, each document's last
version of each day, and after a year, of each week. Creations and pinned
versions always stay. dbd compacts once a day and then deletes blobs no live
document and no kept version uses.

**System types** have JSON Schemas (`schemas/`): App, Contact, Email, Image,
Message, Playlist, Setting, StyleSheet, TextDocument and Track. Writes of those types are
checked against them.

Shorthands: `osctl db create TYPE JSON`, `get`, `update`, `delete`, `query`,
`search`, `watch`, `history`, `at`, `diff`, `undo`, `links`, `snapshot`,
`snapshots`, `restore`, `pin`, `unpin`, `compact`, `put-blob`, `get-blob`.

## backup: backups to a USB drive

`backupd` copies the database and its blobs to a USB drive formatted as exFAT,
and restores them. Apps need `service:backup` (Settings has it).

| method | params | result |
| --- | --- | --- |
| `status` | none | `{drives, busy?, error?}` |
| `watch` | none, streamed | the status now, then at each change (progress included) |
| `setup` | `{drive}` | null: the drive is now a backup drive |
| `back_up` | `{drive}` | `{backup, blobs_copied, bytes_copied, pruned}` once done |
| `backups` | `{drive}` | `{backups}`, newest first, every device's |
| `restore` | `{drive, name}` | `{saved}`: dbd's snapshot of the state it replaced |

A drive (`sda1`) is a backup drive once it holds an `idealos-backup` folder. It
gets a backup when it's set up, whenever it's plugged in and its newest backup
is over an hour old, and daily while it stays in. A backup is a fresh snapshot of
the database plus the blobs it records that the drive doesn't have yet. Each
device keeps its newest 30. Restoring a backup of another device makes this
device that one. Error codes: `not_found`, `busy`, `not_set_up`, `failed`.

On the drive: `idealos-backup/<device>/backup-<ms>-<change set>.db` and
`idealos-backup/blobs/ab/cdef…`. Backups hold documents and blobs only, not
secrets or Wi-Fi passwords, and aren't encrypted yet.

Shorthands: `osctl backup status`, `setup DRIVE`, `now DRIVE`, `list DRIVE`,
`restore DRIVE NAME`.

## net: networking

`netd` wraps NetworkManager. Apps need `service:net`.

| method | params | result |
| --- | --- | --- |
| `status` | none | `{state, connections, wifi_enabled, wifi_present}` |
| `watch` | none, streamed | the status now, then each change |
| `networks` | `{rescan?}` | `{networks}`: Wi-Fi networks in range |
| `connect` | `{ssid, password?}` | null, once connected |
| `disconnect` | none | null |
| `forget` | `{ssid}` | null |
| `set_wifi` | `{enabled}` | null |

## perm: permissions

`permd` keeps the grants and the audit log, and asks the user through the
compositor's trusted prompt.

| method | params | result | who |
| --- | --- | --- | --- |
| `grants` | `{app?}` | `{version, grants}` | anyone; an app sees its own |
| `request` | `{rules, reason?}` | `{granted}` | apps: prompts the user |
| `grant` | `{app, rules}` | the new grants | managers |
| `revoke` | `{ids}` or `{app}` | what was revoked | managers |
| `audit` | `{limit?}` | the log, newest first | managers |

A rule is a data rule, `{"type": "Note", "access": ["read", "write",
"create"], "filter": {...}}`, or a capability, `{"capability": "network"}`
(also `clipboard`, `permissions`, `storage`, `service:NAME`). Managers are
system services, tools and apps with `permissions`. permd publishes the whole
table on `perm.grants` whenever it changes; dbd, the broker and the compositor
follow it, so a revocation takes effect at once.

An app's manifest lists the rules it's granted when installed; `osctl perm
list`, `grant`, `revoke` and `audit` manage them by hand.

## secrets

`secretsd` keeps secrets for whoever stored them, encrypted at rest. Only
services and apps may keep secrets (each sees only its own).

| method | params | result |
| --- | --- | --- |
| `put` | `{name, value}` | null |
| `get` | `{name}` | `{value}` |
| `delete` | `{name}` | null |
| `list` | none | `{names}` |

## pty: terminals

`ptyd` runs shells on pseudo-terminals and parses their output into screens.
Apps need `service:pty`.

| method | params | result |
| --- | --- | --- |
| `open` | `{cols, rows}`, streamed | `{event: "opened", id}`, then `title` and `exited` events |
| `attach` | `{id}`, streamed | screen frames |
| `write` | `{id, data}` | null |
| `resize` | `{id, cols, rows}` | null |

An app usually opens a session and shows it with a `terminal` node, which the
compositor attaches to itself; the app never sees the screen.

## session

`session` starts and supervises the services and runs each app in its own
sandboxed `app-host`.

| method | params | result | who |
| --- | --- | --- | --- |
| `launch` | `{app}` | null | system |
| `stop` | `{app}` | null | system |
| `apps` | none | the running apps | system |
| `info` | none | version, hostname, kernel, machine, uptime, services | apps with `service:session` |

`osctl app install DIR`, `list`, `launch ID` and `stop ID` use these.

## compositor: windows, widgets and styles

The compositor owns the screen and input. Apps describe their windows as a tree
of nodes; the compositor lays them out, draws them and drives the widgets
itself, so typing, scrolling and dragging never wait on an app.

| method | params | result |
| --- | --- | --- |
| `open_window` | `{title, width?, height?, style?}`, streamed | `{kind: "opened", window, width, height}`, then the window's events |
| `commit` | `{window, ops}` | null |
| `get_clipboard` | none | `{text, doc?}` (needs `clipboard`) |
| `set_clipboard` | `{text, doc?}` | null |
| `screen` | none | `{width, height, workspace, workspaces}` |
| `check_style` | `{css}` | `{error?: {line, col, message}}`: whether the sheet parses |
| `system_style` | none | `{css}`: the built-in system sheet |
| `prompt` | for permd only | the user's answer |

A window lasts as long as its `open_window` request. Its events are `event`
(a node the app listens to was clicked, changed, submitted ...), `resized` and
`close`. A `style` that doesn't parse fails `open_window` with its line and
column.

**Ops** in a commit: `{op: "create", id, type, props}`, `{op: "set", id,
props}`, `{op: "insert", parent, child, index}`, `{op: "remove", parent,
child}`, `{op: "destroy", id}`, `{op: "listen", id, events}`. The app picks node
ids; node 1 is the window's content area. A commit is checked whole before any
of it applies.

**Node types**: primitives `group`, `rect`, `text`, `image`; containers
`vstack`, `hstack`, `toolbar`, `grid`, `split`, `scroll`, `dialog`; widgets
`label`, `button`, `checkbox`, `toggle`, `slider`, `select`, `menu`,
`text_field`, `text_area`, `vlist` (a virtual list) and `terminal`.

**Common props**: `x`, `y`, `w`, `h` (pixels, `"50%"` or `"fill"`), `grow`,
`min_w`/`min_h`/`max_w`/`max_h`, `padding`, `gap`, `align`, `justify`,
`hidden`, `class` (style classes, separated by spaces) and `transition` (see
[Transitions](#transitions)). Colour props
(`color`, `text_color`, `border_color`) take `#rrggbb`, `#rrggbbaa`, or a style
variable as `$accent` or `var(--accent)`; text takes `size` and `weight:
"bold"`. Props starting with `_` belong to the compositor (focus, caret,
selection, scroll, hover) and apps can't set them. A text widget can bind to a
document field, `{"bind": {"doc": id, "path": "body"}}`, so typing goes straight
to the database.

### Style sheets

Colours and fonts come from CSS-like style sheets: the system's, and an app's
own, sent with `open_window`. Layout stays in props.

```css
:root  { --accent: #d9480f; }
@dark  { :root { --accent: #ff8a4c; } }
.primary { background: var(--accent); color: var(--accent-text); }
.primary:pressed { background: #b33c0b; }
.code  { font-family: "DejaVu Sans Mono", monospace; font-size: 13px; }
```

- **Selectors**: a widget kind (`button`), a class (`.primary`) or both
  (`button.primary`), each with at most one state: `:hover`, `:pressed`,
  `:focus`, `:disabled`, `:checked`. `:root` holds variables and the starting
  colour and font. Commas share a rule. No combinators.
- **Properties**: `color`, `background`, `border-color`, `accent-color`,
  `caret-color`, `selection-color`, `placeholder-color`, `font-family`,
  `font-size` (px), `font-weight` (`normal`, `bold`, 100–900), `font-style`
  (`normal`, `italic`). A rule with a state sets colours only.
- **Values**: colours are `#rgb`, `#rrggbb`, `#rrggbbaa`, `transparent` or
  `var(--name)`; variables (set in `:root`) hold colours. `font-family` is a list
  of installed families ending in `sans-serif`, `serif` or `monospace`.
- **Cascade**: the more specific rule wins (1 for a kind, 10 for a class or a
  state), then the app's over the system's, then the later one. A node's own
  colour props beat both. `color`, `accent-color` and fonts inherit.
- **Themes**: the system sheet's `@dark` section applies in the dark theme.
  The compositor follows the `Setting` documents with keys `appearance.theme`
  (value `"dark"` or `"light"`) and `appearance.reduce_motion`, which Settings >
  Display writes. An app's sheet
  applies inside its windows only, never to the frame, title bar or prompts.

### Editing style sheets live

Edits to a sheet are a `StyleSheet` document, `{"target": "system" | "app:<id>",
"css": "..."}`, which the compositor follows with a live query: the screen
restyles as the document changes. Edits come after the built-in system sheet
(target `system`) or the app's own sheet (`app:<id>`), so they win where they
match equally; empty edits put things back. A sheet with a mistake isn't applied,
and the last good one stays. The permission prompt always keeps the built-in
look. Three ways to edit:

- The **Style Editor** app: its text area is bound to the document, with the
  parse status and a preview beside it.
- `osctl style get [system | app:ID | builtin]`, `set TARGET FILE` (`-` for
  standard input; it checks first), `check FILE` and `reset TARGET`.
- From the Mac: `make style-watch HOST=localhost:2222 FILE=my.css
  [TARGET=app:org.idealos.notes]` sends the file each time it's saved.

### Transitions

The compositor moves positions and colours to where they're set instead of
jumping; apps only set the end value. A node's `transition` prop names which
props move and for how long, in milliseconds, with an optional easing
(`linear`, `ease-in`, `ease-out`, the default, or `ease-in-out`):

```ts
rect({ x: () => (moved() ? 220 : 0), color: () => (moved() ? "#10b981" : "$accent"),
       transition: { x: [400, "ease-in-out"], color: 400 } })
```

- **What moves**: `x` and `y` (for nodes placed by them, not by a stack) and
  the colour props `color`, `text_color` and `border_color`, at most 5 seconds.
  The prop holds the end value at once; only what's drawn moves.
- **Without asking**: style colours fade when a widget's state changes (120 ms;
  a press shows at once, and letting go fades)
  and when the theme switches (250 ms). Windows slide in on a workspace switch;
  new windows rise into place; menus and dialog backdrops fade in.
- Colours move in Oklab. A value set again mid-way moves on from where it is.
- **Reduce motion**: the `Setting` with key `appearance.reduce_motion` and value
  `true` (Settings > Display) makes every change instant.
- Size, scale and whole-window opacity don't move yet: they need windows drawn
  as textures.

The system's variables: `--desktop`, `--surface`, `--surface-alt`, `--border`,
`--text`, `--muted`, `--accent`, `--accent-text`, `--control`,
`--control-hover`, `--control-pressed` (a control held down), `--control-border`,
`--track` (a toggle that's off, a slider beyond its value), `--track-hover`,
`--titlebar`, `--titlebar-active`, `--titlebar-hover`, `--titlebar-pressed`,
`--scrollbar-hover`, `--scrollbar-active`, `--divider-hover`, `--focus`,
`--selection`, `--danger`, `--backdrop`, `--bar`, `--scrim`, `--scrollbar`,
`--terminal`, `--terminal-text`, `--terminal-cursor`.

Window title bars are styled by the classes `.titlebar` (the bar: `:focus` while
its window is active, `:hover`, and `:pressed` while it's dragged), `.titlebar-line`
(the line under it) and `.window-title` (`:focus` too), so edits to the system
sheet can restyle them; an app's own sheet can't reach them.

## The app SDK

Apps are TypeScript, bundled with `@idealos/sdk` into one module and run in
QuickJS by `app-host`, sandboxed. `node tools/build-app.ts apps/NAME` bundles
one; `osctl app install apps/NAME` installs and starts it. The manifest is the
`idealos` section of the app's `package.json`: `id`, `name`, `autostart`, and
the `permissions` rules it's granted.

```ts
import style from "./style.css";
import { openWindow, vstack, label, button, signal, db } from "@idealos/sdk";

await openWindow({ title: "Counter", width: 300, height: 200, style }, () => {
  const [count, setCount] = signal(0);
  return vstack({ padding: 16, gap: 12 },
    label({ text: () => `Count: ${count()}`, size: 20 }),
    button({ label: "Add one", class: "primary", onClick: () => setCount((n) => n + 1) }));
});
```

- **Bus**: `request(to, method, params?, {timeoutMs?})`, `stream(to, method,
  params, onPart, onEnd)` (returns a cancel function), `exit(code?)`.
- **Database** (`db.`): `query(q)`, `get(id)`, `put(type, data)`, `update(id,
  patch)`, `remove(id)`, `search(text, {type?, limit?})`, and `live(q)`: a
  signal of the query's result that stays current (`ready()`, `error()`,
  `close()`).
- **Signals**: `signal(initial)` gives `[get, set]`; `effect(fn)`, `memo(fn)`,
  `batch(fn)`, `untrack(fn)`, `root(fn)`, `onCleanup(fn)`. A prop given as a
  function is reactive: when a signal it reads changes, the SDK sends a `set` op.
  Ops are sent as one commit per microtask.
- **Windows and widgets**: `openWindow(options, build)` with `title`, `width`,
  `height`, `style` and `onClose`; builders `vstack`, `hstack`, `toolbar`,
  `grid`, `scroll`, `split`, `dialog`, `label`, `button`, `checkbox`, `toggle`,
  `slider`, `select`, `menu`, `textField`, `textArea`, `image`, `rect`,
  `virtualList`, `terminal`, and `each(list, key, render)` for lists. Widgets
  report through `onClick`, `onChange`, `onSubmit`, `onCancel`, `onChoose` and
  `onRange`.
- **Permissions** (`permissions.`): `request(rules, reason?)` asks the user;
  `granted()` lists the app's grants.
- **Terminals** (`pty.`): `open(cols?, rows?, onTitle?)` gives a session to show
  in a `terminal` node.
