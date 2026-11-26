// Settings: the network (through netd), what apps may do (through permd),
// the display, storage (the database and its snapshots), backups to a USB drive
// (through backupd) and about.
//
// Each section loads when it's shown. The network and backup sections follow
// their service's status live; the rest refresh each time they're opened.

import {
  button,
  db,
  dialog,
  each,
  effect,
  hstack,
  label,
  openWindow,
  request,
  scroll,
  signal,
  split,
  stream,
  textField,
  toggle,
  vstack,
  untrack,
  type AuditEntry,
  type BackedUp,
  type BackupInfo,
  type BackupStatus,
  type BackupsResult,
  type Drive,
  type Grant,
  type InfoResult,
  type NetStatus,
  type NetworksResult,
  type ScreenInfo,
  type SnapshotInfo,
  type StatsResult,
  type UiNode,
  type WifiNetwork,
} from "@idealos/sdk";

const SECTIONS = ["Network", "Permissions", "Display", "Storage", "Backup", "About"] as const;
type Section = (typeof SECTIONS)[number];

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Run `load` each time a section is shown (so it shows fresh numbers). */
function whenShown(shown: () => boolean, load: () => unknown): void {
  effect(() => {
    if (shown()) untrack(() => void load());
  });
}

function heading(text: string): UiNode {
  return label({ text, size: 18, weight: "bold" });
}

function muted(text: string | (() => string)): UiNode {
  return label({ text, color: "$muted", size: 13 });
}

function bytes(n: number): string {
  const units = ["bytes", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return i === 0 ? `${n} bytes` : `${n.toFixed(1)} ${units[i]}`;
}

function since(ms: number): string {
  return new Date(ms).toISOString().replace("T", " ").slice(0, 16);
}

/** A grant, in words (as permd describes rules). */
function describe(g: Grant): string {
  if ("capability" in g) {
    const c = g.capability;
    if (c === "network") return "use the network";
    if (c === "clipboard") return "read what you copy";
    if (c === "permissions") return "change what other apps may do";
    if (c === "storage") return "see how much is stored, and make backups";
    if (c === "service:pty") return "run commands with full access to this device";
    if (c === "service:net") return "change your network connections";
    return c.startsWith("service:") ? `use the ${c.slice(8)} service` : c;
  }
  const verbs = new Set<string>();
  if (g.access.includes("read") || g.access.includes("write")) verbs.add("read");
  if (g.access.includes("write")) verbs.add("change");
  if (g.access.includes("create")) verbs.add("add");
  const what = g.type === "*" ? "all documents" : `${g.type} documents`;
  const filter = g.filter && Object.keys(g.filter).length > 0 ? ` matching ${JSON.stringify(g.filter)}` : "";
  return `${[...verbs].join(", ")} ${what}${filter}`;
}

// --- Network ---------------------------------------------------------------

function network(): UiNode {
  const [status, setStatus] = signal<NetStatus | null>(null);
  const [networks, setNetworks] = signal<WifiNetwork[]>([]);
  const [message, setMessage] = signal("");
  const [joining, setJoining] = signal<string | null>(null);
  const [password, setPassword] = signal("");
  stream<NetStatus>("net", "watch", null, setStatus, (e) => e && setMessage(`netd: ${e.message}`));

  const scan = async (rescan: boolean) => {
    setMessage(rescan ? "Scanning…" : "");
    try {
      setNetworks((await request<NetworksResult>("net", "networks", { rescan })).networks);
      setMessage("");
    } catch (e) {
      setMessage(errorText(e));
    }
  };
  const join = async (ssid: string, pass?: string) => {
    setMessage(`Joining ${ssid}…`);
    try {
      await request("net", "connect", pass === undefined ? { ssid } : { ssid, password: pass }, { timeoutMs: 45_000 });
      setMessage(`Joined ${ssid}.`);
    } catch (e) {
      setMessage(errorText(e));
    }
    void scan(false);
  };
  const act = async (method: string, params: unknown) => {
    try {
      await request("net", method, params);
    } catch (e) {
      setMessage(errorText(e));
    }
    void scan(false);
  };
  void scan(false);

  const summary = () => {
    const s = status();
    if (!s) return "Asking netd…";
    if (s.state === "unavailable") return "No network manager is running.";
    return `${s.state[0].toUpperCase()}${s.state.slice(1)}`;
  };
  return vstack(
    { gap: 12 },
    heading("Network"),
    label({ text: summary }),
    each(
      () => status()?.connections ?? [],
      (c) => `${c.device}/${c.name}`,
      (c) => muted(() => `${c().kind === "wifi" ? "Wi-Fi" : "Wired"}: ${c().name} on ${c().device}${c().ip4 ? `, ${c().ip4}` : ""}`),
      { gap: 4 },
    ),
    hstack(
      { gap: 12, align: "center", hidden: () => !status()?.wifi_present },
      toggle({ label: "Wi-Fi", checked: () => status()?.wifi_enabled ?? false, onChange: (on) => void act("set_wifi", { enabled: on }) }),
      button({ label: "Scan", onClick: () => void scan(true) }),
    ),
    muted(message),
    each(
      networks,
      (n) => n.ssid,
      (n) =>
        hstack(
          { gap: 10, align: "center" },
          label({ text: () => n().ssid, w: 180 }),
          muted(() => `${n().strength}%${n().secure ? " · secured" : ""}${n().active ? " · connected" : n().known ? " · saved" : ""}`),
          button({
            label: () => (n().active ? "Disconnect" : "Join"),
            onClick: () => {
              const it = n();
              if (it.active) void act("disconnect", null);
              else if (it.secure && !it.known) {
                setPassword("");
                setJoining(it.ssid);
              } else void join(it.ssid);
            },
          }),
          button({ label: "Forget", hidden: () => !n().known, onClick: () => void act("forget", { ssid: n().ssid }) }),
        ),
      { gap: 6 },
    ),
    dialog(
      { hidden: () => joining() === null, onCancel: () => setJoining(null), gap: 10, padding: 16 },
      label({ text: () => `Join “${joining() ?? ""}”`, size: 16, weight: "bold" }),
      textField({
        placeholder: "Password",
        secret: true,
        value: password,
        onChange: setPassword,
        onSubmit: () => {
          const ssid = joining();
          setJoining(null);
          if (ssid) void join(ssid, password());
        },
        w: 260,
      }),
      hstack(
        { gap: 8 },
        button({ label: "Cancel", onClick: () => setJoining(null) }),
        button({
          label: "Join",
          onClick: () => {
            const ssid = joining();
            setJoining(null);
            if (ssid) void join(ssid, password());
          },
        }),
      ),
    ),
  );
}

// --- Permissions -------------------------------------------------------------

function permissionsSection(shown: () => boolean): UiNode {
  const [grants, setGrants] = signal<Grant[]>([]);
  const [audit, setAudit] = signal<AuditEntry[]>([]);
  const [names, setNames] = signal<Record<string, string>>({});
  const [message, setMessage] = signal("");
  const refresh = async () => {
    try {
      setGrants((await request<{ grants: Grant[] }>("perm", "grants", {})).grants);
      setAudit((await request<{ entries: AuditEntry[] }>("perm", "audit", { limit: 20 })).entries);
      const apps = await db.query({ type: "App", limit: 500 });
      setNames(Object.fromEntries(apps.map((a) => [String(a.data.id), String(a.data.name)])));
      setMessage("");
    } catch (e) {
      setMessage(errorText(e));
    }
  };
  const name = (app: string) => names()[app] ?? app;
  whenShown(shown, refresh);
  return vstack(
    { gap: 10, hidden: () => !shown() },
    heading("Permissions"),
    muted("What each app may do. Apps always have the documents they made."),
    muted(message),
    each(
      grants,
      (g) => String(g.id),
      (g) =>
        hstack(
          { gap: 10, align: "center" },
          label({ text: () => name(g().app), w: 140, weight: "bold" }),
          label({ text: () => describe(g()), grow: 1 }),
          button({
            label: "Revoke",
            onClick: async () => {
              try {
                await request("perm", "revoke", { ids: [g().id] });
              } catch (e) {
                setMessage(errorText(e));
              }
              void refresh();
            },
          }),
        ),
      { gap: 6 },
    ),
    label({ text: "Recent changes", weight: "bold", size: 14 }),
    each(
      audit,
      (a) => String(a.id),
      (a) => muted(() => `${since(a().at)}  ${a().action} ${name(a().app)}: ${describe(a().rule as Grant)} (by ${a().by})`),
      { gap: 2 },
    ),
  );
}

// --- Display, Storage, About ---------------------------------------------------

/** The settings the compositor follows: the light or dark theme, and reduce motion. */
const THEME_KEY = "appearance.theme";
const MOTION_KEY = "appearance.reduce_motion";

function displaySection(shown: () => boolean): UiNode {
  const [screen, setScreen] = signal<ScreenInfo | null>(null);
  const [message, setMessage] = signal("");
  const load = () => request<ScreenInfo>("compositor", "screen", null).then(setScreen, () => {});
  whenShown(shown, load);
  // The appearance settings, followed live (the compositor follows the same documents).
  const settings = db.live({ type: "Setting", where: { key: { $in: [THEME_KEY, MOTION_KEY] } } });
  const setting = (key: string) => settings().find((d) => d.data.key === key);
  const dark = () => setting(THEME_KEY)?.data.value === "dark";
  const still = () => setting(MOTION_KEY)?.data.value === true;
  const save = async (key: string, value: unknown) => {
    try {
      const doc = setting(key);
      if (doc) await db.update(doc.id, { value });
      else await db.put("Setting", { key, value });
      setMessage("");
    } catch (e) {
      setMessage(errorText(e));
    }
  };
  return vstack(
    { gap: 10, hidden: () => !shown() },
    heading("Display"),
    label({ text: () => (screen() ? `${screen()!.width} × ${screen()!.height} (logical pixels)` : "…") }),
    muted(() => (screen() ? `Workspace ${screen()!.workspace} of ${screen()!.workspaces}` : "")),
    toggle({ label: "Dark theme", checked: dark, onChange: (on) => void save(THEME_KEY, on ? "dark" : "light") }),
    muted("Every app follows the theme, apart from colours an app sets itself."),
    toggle({ label: "Reduce motion", checked: still, onChange: (on) => void save(MOTION_KEY, on) }),
    muted("Things change at once instead of sliding and fading."),
    label({ text: message, color: "$danger", size: 13, hidden: () => message() === "" }),
  );
}

function storageSection(shown: () => boolean): UiNode {
  const [stats, setStats] = signal<StatsResult | null>(null);
  const [snapshots, setSnapshots] = signal<SnapshotInfo[]>([]);
  const [message, setMessage] = signal("");
  const refresh = async () => {
    try {
      setStats(await request<StatsResult>("db", "stats", null));
      setSnapshots((await request<{ snapshots: SnapshotInfo[] }>("db", "snapshots", null)).snapshots);
    } catch (e) {
      setMessage(errorText(e));
    }
  };
  const s = () => stats();
  whenShown(shown, refresh);
  return vstack(
    { gap: 10, hidden: () => !shown() },
    heading("Storage"),
    label({ text: () => (s() ? `Database: ${bytes(s()!.db_bytes)}, ${s()!.docs} documents of ${s()!.types} types` : "…") }),
    muted(() => (s() ? `${s()!.blobs} blobs (${bytes(s()!.blob_bytes)}), ${s()!.change_sets} changes in history` : "")),
    muted(() => (s() ? `Snapshots: ${bytes(s()!.snapshot_bytes)}` : "")),
    button({
      label: "Take a snapshot now",
      onClick: async () => {
        setMessage("Taking a snapshot…");
        try {
          await request("db", "snapshot", null, { timeoutMs: 120_000 });
          setMessage("Done.");
        } catch (e) {
          setMessage(errorText(e));
        }
        void refresh();
      },
    }),
    muted(message),
    each(
      snapshots,
      (sn) => sn.name,
      (sn) => muted(() => `${since(sn().taken)}  ${bytes(sn().bytes)}  (change ${sn().change_set})`),
      { gap: 2 },
    ),
  );
}

// --- Backup ----------------------------------------------------------------------

function backupSection(shown: () => boolean): UiNode {
  const [status, setStatus] = signal<BackupStatus | null>(null);
  const [message, setMessage] = signal("");
  // The backups listed for a drive, once asked for.
  const [listed, setListed] = signal<{ drive: string; backups: BackupInfo[] } | null>(null);
  const [confirm, setConfirm] = signal<{ drive: string; backup: BackupInfo } | null>(null);
  stream<BackupStatus>("backup", "watch", null, setStatus, (e) => e && setMessage(`backupd: ${e.message}`));

  const hour = 60 * 60 * 1000;
  const act = async (doing: string, f: () => Promise<string>) => {
    setMessage(doing);
    try {
      setMessage(await f());
    } catch (e) {
      setMessage(errorText(e));
    }
  };
  const list = (drive: string) =>
    act("Reading the drive…", async () => {
      const r = await request<BackupsResult>("backup", "backups", { drive });
      setListed({ drive, backups: r.backups });
      return r.backups.length ? "" : "No backups on this drive yet.";
    });
  const backUp = (drive: string) =>
    act("Backing up…", async () => {
      const r = await request<BackedUp>("backup", "back_up", { drive }, { timeoutMs: hour });
      return `Backed up: ${r.blobs_copied} new files, ${bytes(r.bytes_copied)} copied.`;
    });
  const restore = (drive: string, b: BackupInfo) =>
    act("Restoring…", async () => {
      await request("backup", "restore", { drive, name: b.name }, { timeoutMs: hour });
      setListed(null);
      return `Restored the backup from ${since(b.taken)}. What was here before is kept as a snapshot (Storage).`;
    });
  const progress = () => {
    const b = status()?.busy;
    if (!b) return "";
    const pct = b.total ? Math.floor((100 * b.done) / b.total) : 0;
    return `${b.doing === "backing_up" ? "Backing up to" : "Restoring from"} ${b.drive}: ${pct}%`;
  };
  const describeDrive = (d: Drive) => {
    const size = `${bytes(d.bytes)}, ${bytes(d.free)} free`;
    if (!d.set_up) return `${d.id} · ${size} · not used for backups`;
    return `${d.id} · ${size} · ${d.last ? `last backup ${since(d.last.taken)}` : "no backup of this device yet"}`;
  };
  return vstack(
    { gap: 10, hidden: () => !shown() },
    heading("Backup"),
    muted("Your documents and files, copied to a USB drive formatted as exFAT. A backup drive gets a backup when it's plugged in and daily while it stays in. Secrets and Wi-Fi passwords aren't included, and backups aren't encrypted."),
    label({ text: () => (status() ? (status()!.drives.length ? "" : "Plug in a USB drive formatted as exFAT.") : "Asking backupd…"), hidden: () => (status()?.drives.length ?? 0) > 0 }),
    each(
      () => status()?.drives ?? [],
      (d) => d.id,
      (d) =>
        vstack(
          { gap: 6 },
          label({ text: () => describeDrive(d()) }),
          hstack(
            { gap: 8 },
            button({
              label: "Use for backups",
              hidden: () => d().set_up,
              onClick: () => void act("Setting up the drive…", async () => {
                await request("backup", "setup", { drive: d().id });
                return "This drive is now a backup drive.";
              }),
            }),
            button({ label: "Back up now", hidden: () => !d().set_up, onClick: () => void backUp(d().id) }),
            button({ label: "Show backups", hidden: () => !d().set_up, onClick: () => void list(d().id) }),
          ),
        ),
      { gap: 12 },
    ),
    label({ text: progress, color: "$muted", size: 13, hidden: () => !status()?.busy }),
    label({ text: () => status()?.error ?? "", color: "$danger", hidden: () => !status()?.error }),
    label({ text: message, color: "$muted", size: 13, hidden: () => message() === "" }),
    each(
      () => listed()?.backups ?? [],
      (b) => b.name,
      (b) =>
        hstack(
          { gap: 10, align: "center" },
          label({ text: () => since(b().taken), w: 140 }),
          muted(() => `${bytes(b().bytes)} · ${b().this_device ? "this device" : `device ${b().device.slice(0, 6)}`}`),
          button({ label: "Restore…", onClick: () => setConfirm({ drive: listed()!.drive, backup: b() }) }),
        ),
      { gap: 4 },
    ),
    dialog(
      { hidden: () => confirm() === null, onCancel: () => setConfirm(null), gap: 10, padding: 16 },
      label({ text: "Restore this backup?", size: 16, weight: "bold" }),
      muted(() => {
        const c = confirm();
        if (!c) return "";
        const other = c.backup.this_device ? "" : " It's of another device, which this one becomes.";
        return `Everything on this device is replaced with the backup from ${since(c.backup.taken)}.${other} What's here now is kept as a snapshot, so you can go back.`;
      }),
      hstack(
        { gap: 8 },
        button({ label: "Cancel", onClick: () => setConfirm(null) }),
        button({
          label: "Restore",
          onClick: () => {
            const c = confirm();
            setConfirm(null);
            if (c) void restore(c.drive, c.backup);
          },
        }),
      ),
    ),
  );
}

function aboutSection(shown: () => boolean): UiNode {
  const [info, setInfo] = signal<InfoResult | null>(null);
  const load = () => request<InfoResult>("session", "info", null).then(setInfo, () => {});
  const i = () => info();
  whenShown(shown, load);
  return vstack(
    { gap: 8, hidden: () => !shown() },
    heading("About"),
    label({ text: () => (i() ? `idealos ${i()!.version}` : "…"), size: 15 }),
    muted(() => (i() ? `${i()!.hostname} · ${i()!.kernel} · ${i()!.machine}` : "")),
    muted(() => (i() ? `Up ${Math.floor(i()!.uptime_secs / 3600)} h ${Math.floor((i()!.uptime_secs % 3600) / 60)} min` : "")),
    muted(() => (i() ? `Services: ${i()!.services.join(", ")}` : "")),
  );
}

await openWindow({ title: "Settings", width: 760, height: 520 }, () => {
  const [section, setSection] = signal<Section>("Network");
  const nav = vstack(
    { gap: 4, padding: 10 },
    ...SECTIONS.map((s) =>
      button({
        label: s,
        w: "fill",
        class: () => (section() === s ? "primary" : ""),
        onClick: () => setSection(s),
      }),
    ),
  );
  const pane = (s: Section) => () => section() === s;
  const content = scroll(
    { padding: 16, grow: 1 },
    vstack({ hidden: () => section() !== "Network" }, network()),
    permissionsSection(pane("Permissions")),
    displaySection(pane("Display")),
    storageSection(pane("Storage")),
    backupSection(pane("Backup")),
    aboutSection(pane("About")),
  );
  return split({ ratio: 0.22, grow: 1 }, nav, content);
});
