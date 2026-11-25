// Pass/fail rules for tools/qemu-smoke.ts, kept free of I/O so they can be tested.
//
// The guest script prints one "SMOKE <key> <value>" line per fact it checked; the
// rules below turn the whole serial log into a list of results.

export interface Check {
  name: string;
  ok: boolean;
  detail: string;
}

/** Logged by the compositor's KMS backend once its first frame is on screen. */
export const FIRST_FRAME = /kms: first frame on screen/;

/** Shell lines the smoke test types into the session's console shell, once the
 * compositor has shown its first frame (so its startup log can't split a line). */
export const GUEST_SCRIPT = [
  "echo SMOKE hostname $(hostname)",
  'grep " /data " /proc/mounts | grep -q " rw," && echo SMOKE data rw',
  'grep -q "^overlay /opt/idealos overlay" /proc/mounts && echo SMOKE overlay ok',
  "[ -e /usr/lib/libvulkan.so.1 ] && echo SMOKE vulkan-loader ok",
  'ps | grep -q "[/]usr/bin/session" && echo SMOKE session ok',
  // Still up a few seconds after its first frame, so it didn't crash on the next ones.
  "sleep 3; echo SMOKE compositor-pid $(pidof compositor)",
  // The compositor turns the text console's keyboard off while it runs.
  "echo SMOKE console-keyboard $(kbd_mode -C /dev/tty0 2>&1)",
  // Mesa caches compiled shaders under XDG_CACHE_HOME, which start-session puts on /data.
  "echo SMOKE shader-cache-files $(find /data/cache -type f 2>/dev/null | wc -l)",
  // session starts busd; a service and a client talk through it.
  "echo SMOKE bus-socket $(ls /run/idealos/bus.sock 2>&1)",
  // busd inherited session's connection, and session claimed its name on it.
  "echo SMOKE bus-session $(osctl bus list 2>&1 | grep -c '^session ')",
  "osctl bus echo smoke-echo >/dev/null 2>&1 & sleep 0.5",
  `echo SMOKE bus-call $(osctl bus call smoke-echo ping '{"n":1}' 2>&1)`,
  "echo SMOKE bus-bench $(osctl bus bench --seconds 1 2>&1 | grep ^BENCH)",
  "kill $!",
  // session starts dbd on the bus; a document goes in and comes back out of a query.
  "echo SMOKE db-file $(ls /data/db/idealos.db 2>&1)",
  `echo SMOKE db-roundtrip $(osctl db create Smoke '{"n":7}' >/dev/null && osctl db query '{"type":"Smoke","where":{"n":7},"limit":1}' 2>&1 | grep -c '"n":7')`,
  // Undo: change a document, undo the change, and read the old value back.
  `SID=$(osctl db create SmokeUndo '{"v":1}' | sed 's/.*"id":"\\([^"]*\\)".*/\\1/'); osctl db update "$SID" '{"v":2}' >/dev/null`,
  `SSET=$(osctl db history "$SID" | grep -o '"change_set":[0-9]*' | tail -1 | cut -d: -f2); osctl db undo "$SSET" >/dev/null`,
  `echo SMOKE db-undo $(osctl db get "$SID" 2>&1 | grep -o '"v":[0-9]*')`,
  // A live query: its first part, then a diff once another client creates a match.
  `(sleep 1; osctl db create SmokeLive '{"n":1}' >/dev/null) &`,
  `echo SMOKE db-watch $(timeout 10 osctl db watch '{"type":"SmokeLive"}' --count 2 2>&1 | grep -o '"kind":"[a-z]*"' | tr '\\n' ' ')`,
  // A snapshot lands beside the database on /data.
  `SN=$(osctl db snapshot 2>&1 | grep -o 'snapshot-[0-9]*-[0-9]*\\.db' | head -1); echo SMOKE db-snapshot $(ls /data/db/snapshots/$SN 2>&1)`,
  // A link, followed back from its target.
  `LA=$(osctl db create Contact '{"name":"Smoke"}' | sed 's/.*"id":"\\([^"]*\\)".*/\\1/'); LB=$(osctl db create Note '{}' | sed 's/.*"id":"\\([^"]*\\)".*/\\1/'); osctl db link "$LB" about "$LA" >/dev/null`,
  `echo SMOKE db-links $(osctl db links "$LA" 2>&1 | grep -c '"path":"about"')`,
  // A blob goes in from a file and comes back out through a passed descriptor.
  `printf smoke-blob > /run/smoke-blob; SH=$(osctl db put-blob /run/smoke-blob | grep -o '"hash":"[0-9a-f]*"' | cut -d'"' -f4)`,
  `echo SMOKE db-blob $(osctl db get-blob "$SH" 2>&1) $(ls /data/db/blobs/$(echo "$SH" | cut -c1-2) 2>&1 | wc -l)`,
  // System schemas are enforced.
  `echo SMOKE db-schema $(osctl db create Contact '{}' 2>&1 | grep -c 'name is required')`,
  // session runs the bundled apps (installed at boot by app-host --install).
  `n=0; for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do n=$(osctl app list 2>/dev/null | grep -c running); [ "$n" -ge 2 ] && break; sleep 1; done; echo SMOKE apps-running $n`,
  // The widget gallery ships too, but waits to be launched from the Apps menu.
  "echo SMOKE app-gallery $(osctl app list 2>/dev/null | grep org.idealos.gallery | awk '{print $NF}')",
  // M6: permd and secretsd run; installing notes granted what its manifest asks for;
  // and the notes app runs sandboxed: its own network namespace and a seccomp filter.
  "echo SMOKE perm-services $(osctl bus list 2>&1 | grep -cE '^(perm|secrets) ')",
  `echo SMOKE notes-grant $(osctl perm list org.idealos.notes 2>&1 | grep -c '"type":"Note"')`,
  `P=$(ps | grep "[a]pp-host --app org.idealos.notes" | awk '{print $1}' | head -1); echo SMOKE app-sandbox $([ -n "$P" ] && [ "$(readlink /proc/$P/ns/net)" != "$(readlink /proc/1/ns/net)" ] && echo netns) $(grep -o 'Seccomp:[[:space:]]*[0-9]' /proc/$P/status 2>/dev/null | tr -d '[:space:]')`,
  // M7: ptyd and netd run; netd sees QEMU's wired network through NetworkManager
  // (give DHCP a moment); a shell session opens; the new apps are installed.
  "echo SMOKE m7-services $(osctl bus list 2>&1 | grep -cE '^(pty|net) ')",
  `n=0; for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do n=$(osctl bus call net status 2>/dev/null | grep -c '"kind":"ethernet"'); [ "$n" -ge 1 ] && break; sleep 1; done; echo SMOKE net-wired $n`,
  `echo SMOKE pty-open $(timeout 5 osctl bus stream pty open '{"cols":20,"rows":5}' --count 1 2>&1 | grep -c '"event":"opened"')`,
  "echo SMOKE m7-apps $(osctl app list 2>/dev/null | grep -cE 'org.idealos.(settings|browser|terminal)')",
  "echo SMOKE end",
];

/** Facts the guest reported, keyed by name. Only lines at the start of a line count,
 * so the echo of the typed command itself is never mistaken for a result. */
export function facts(log: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const raw of log.split("\n")) {
    const m = /^SMOKE (\S+) ?(.*)$/.exec(raw.replace(/\r$/, ""));
    if (m) out.set(m[1], m[2]);
  }
  return out;
}

export function evaluate(log: string): Check[] {
  const f = facts(log);
  const fact = (key: string, want: string, name: string): Check => ({
    name,
    ok: f.get(key) === want,
    detail: f.get(key) ?? "not reported",
  });
  const s05 = log.split("\n").find((l) => l.startsWith("S05data:"));
  const kms = /kms: (.* on .* @ \d+ Hz).*adapter ([^(]+)/.exec(log);
  const exited = /session: compositor exited.*$/m.exec(log);
  const keyboard = f.get("console-keyboard");
  const cacheFiles = Number(f.get("shader-cache-files") ?? 0);
  // Mesa: "Failed to create //.cache for shader cache (Read-only file system)---disabling."
  const cacheOff = /^.*for shader cache.*$/m.exec(log);
  // wgpu's OpenGL backend probing EGL; the renderer only asks for Vulkan.
  const egl = /^.*libEGL warning.*$/m.exec(log);
  const bench = f.get("bus-bench") ?? "";
  const rps = Number(/requests_per_s=(\d+)/.exec(bench)?.[1] ?? 0);
  const rtt = /rtt_p50_us=([\d.]+)/.exec(bench)?.[1];
  return [
    {
      name: "session started its shell",
      ok: /session: started shell/.test(log),
      detail: /session: started shell/.test(log) ? "yes" : "not seen",
    },
    fact("hostname", "idealos", "hostname is idealos"),
    { name: "S05data ran cleanly", ok: !s05, detail: s05?.trim() ?? "no errors" },
    fact("data", "rw", "/data mounted read-write"),
    fact("overlay", "ok", "/opt/idealos dev overlay"),
    fact("vulkan-loader", "ok", "Vulkan loader installed"),
    fact("session", "ok", "session process running"),
    {
      name: "compositor showed a frame",
      ok: FIRST_FRAME.test(log),
      detail: kms ? `${kms[1]}, ${kms[2].trim()}` : FIRST_FRAME.test(log) ? "yes" : "not seen",
    },
    {
      name: "compositor still running",
      ok: /^\d+$/.test(f.get("compositor-pid") ?? "") && !exited,
      detail: exited?.[0] ?? (f.get("compositor-pid") ? `pid ${f.get("compositor-pid")}` : "not running"),
    },
    {
      name: "text console's keyboard off under the compositor",
      ok: /in off mode/.test(keyboard ?? ""),
      detail: keyboard ?? "not reported",
    },
    {
      name: "compositor didn't start OpenGL",
      ok: !egl,
      detail: egl?.[0].trim() ?? "no EGL messages",
    },
    {
      name: "Mesa shader cache on /data",
      ok: cacheFiles > 0 && !cacheOff,
      detail: cacheOff?.[0].trim() ?? `${cacheFiles} file(s) in /data/cache`,
    },
    {
      // busd's own line, not session's "started bus": the console shell's messages
      // can land in the middle of session's lines on the shared serial console.
      name: "session started busd on /run/idealos/bus.sock",
      ok: /busd: listening on \/run\/idealos\/bus\.sock/.test(log) && f.get("bus-socket") === "/run/idealos/bus.sock",
      detail: f.get("bus-socket") ?? "not reported",
    },
    fact("bus-session", "1", "session has its own connection to busd"),
    fact("bus-call", '{"n":1}', "a request went through the bus and back"),
    fact("db-file", "/data/db/idealos.db", "dbd keeps the database on /data"),
    fact("db-roundtrip", "1", "a document went into the database and came back from a query"),
    fact("db-undo", '"v":1', "undoing a change brought the old value back"),
    fact("db-watch", '"kind":"initial" "kind":"diff"', "a live query reported a new document"),
    {
      name: "a snapshot was written to /data/db/snapshots",
      ok: /^\/data\/db\/snapshots\/snapshot-\d+-\d+\.db$/.test(f.get("db-snapshot") ?? ""),
      detail: f.get("db-snapshot") ?? "not reported",
    },
    fact("db-links", "1", "a link was followed back from its target"),
    fact("db-blob", "smoke-blob 1", "a blob went in from a file and came back through a descriptor"),
    fact("db-schema", "1", "the Contact schema refused a contact without a name"),
    fact("apps-running", "2", "session runs the two bundled apps"),
    fact("app-gallery", "installed", "the widget gallery is installed but not started"),
    fact("perm-services", "2", "permd and secretsd are on the bus"),
    fact("notes-grant", "1", "installing notes granted the permissions its manifest lists"),
    fact("app-sandbox", "netns Seccomp:2", "the notes app runs in its own network namespace under seccomp"),
    fact("m7-services", "2", "ptyd and netd are on the bus"),
    fact("net-wired", "1", "netd reports the wired connection NetworkManager made"),
    fact("pty-open", "1", "ptyd opened a shell session"),
    fact("m7-apps", "3", "Settings, Documents and Terminal are installed"),
    {
      name: "the apps opened their windows under their own ids",
      ok: ["org.idealos.counter", "org.idealos.notes"].every((id) =>
        new RegExp(`apps: opened a window for App \\{ app: "${id.replaceAll(".", "\\.")}"`).test(log),
      ),
      detail: [...log.matchAll(/apps: opened a window for App \{ app: "([^"]+)"/g)].map((m) => m[1]).join(", ") || "none",
    },
    {
      name: "bus benchmark ran",
      ok: rps > 0,
      detail: rps ? `${rps} requests/s, round trip p50 ${rtt} µs` : bench || "not reported",
    },
  ];
}

// --- Input ---------------------------------------------------------------------
//
// QEMU's virtio tablet and keyboard are driven over QMP (input-send-event). The plan
// drags the topmost demo window by its title bar, then presses Alt+Tab; the
// compositor logs one "wm:" line per finished action.

/** Screen size from the compositor's "kms: ... at WxH @ ..." line. */
export function screenSize(log: string): { w: number; h: number } | undefined {
  const m = /kms: .* at (\d+)x(\d+) @/.exec(log);
  return m ? { w: Number(m[1]), h: Number(m[2]) } : undefined;
}

export interface InputPlan {
  /** QMP commands for the title-bar drag, sent in order. */
  drag: object[];
  /** QMP commands for Alt+Tab. Send them only once the drop is logged: libinput
   * doesn't order events across devices, so keys sent in the same burst as the
   * button release can arrive mid-drag, when the window manager ignores Alt+Tab. */
  keys: object[];
  /** The dragged window and where it must land. */
  drop: { id: number; x: number; y: number };
  /** The window Alt+Tab must raise. */
  raised: number;
}

/** virtio-tablet axes run 0..32767; libinput maps them to `value * size / 32768`. */
const ABS_RANGE = 32768;
const DRAG = { dx: 100, dy: 80, steps: 5 };

/** The input plan for the default 20-window demo scene on a `w` by `h` screen.
 * Window placement mirrors `add_window` in crates/compositor/src/demo.rs. */
export function inputPlan(w: number, h: number): InputPlan {
  const windows = 20;
  const last = windows - 1;
  const [ww, wh] = [380, 260];
  const col = Math.floor(last / 10);
  const row = last % 10;
  const x = Math.min(48 + col * 440 + row * 34, Math.max(w - ww, 0));
  const y = Math.min(52 + row * 30, Math.max(h - wh, 30));
  const id = 1000 + last * 100;
  const grab = { x: x + 150, y: y + 18 }; // inside the 36 px title bar

  const abs = (px: number, py: number) => ({
    execute: "input-send-event",
    arguments: {
      events: [
        { type: "abs", data: { axis: "x", value: Math.round((px * ABS_RANGE) / w) } },
        { type: "abs", data: { axis: "y", value: Math.round((py * ABS_RANGE) / h) } },
      ],
    },
  });
  const btn = (down: boolean) => ({
    execute: "input-send-event",
    arguments: { events: [{ type: "btn", data: { down, button: "left" } }] },
  });
  const key = (qcode: string, down: boolean) => ({
    execute: "input-send-event",
    arguments: { events: [{ type: "key", data: { down, key: { type: "qcode", data: qcode } } }] },
  });

  const drag: object[] = [abs(grab.x, grab.y), btn(true)];
  for (let i = 1; i <= DRAG.steps; i++) {
    drag.push(abs(grab.x + (DRAG.dx * i) / DRAG.steps, grab.y + (DRAG.dy * i) / DRAG.steps));
  }
  drag.push(btn(false));
  const keys = [key("alt", true), key("tab", true), key("tab", false), key("alt", false)];
  // Alt+Tab raises the bottom-most window: the first one the demo created.
  return { drag, keys, drop: { id, x: x + DRAG.dx, y: y + DRAG.dy }, raised: 1000 };
}

export function evaluateInput(log: string, plan: InputPlan | undefined): Check[] {
  const added = [...log.matchAll(/input: added (.*) \((.*)\)/g)];
  const has = (cap: string) => added.find((m) => m[2].split(", ").includes(cap));
  const keyboard = has("keyboard");
  const pointer = has("pointer");
  const dropped = /wm: dropped window (\d+) at \((-?[\d.]+), (-?[\d.]+)\)/.exec(log);
  const raised = /wm: raised window (\d+) from the keyboard/.exec(log);
  // libinput's tablet scaling can land a pixel off.
  const near = (a: string, b: number) => Math.abs(Number(a) - b) <= 2;
  const dropOk =
    plan !== undefined &&
    dropped !== null &&
    Number(dropped[1]) === plan.drop.id &&
    near(dropped[2], plan.drop.x) &&
    near(dropped[3], plan.drop.y);
  return [
    { name: "keyboard found", ok: !!keyboard, detail: keyboard?.[1] ?? "no keyboard device" },
    { name: "pointer found", ok: !!pointer, detail: pointer?.[1] ?? "no pointer device" },
    {
      name: "title-bar drag moved the window",
      ok: dropOk,
      detail: dropped
        ? `window ${dropped[1]} at (${dropped[2]}, ${dropped[3]})` +
          (plan ? `, wanted ${plan.drop.id} at (${plan.drop.x}, ${plan.drop.y})` : "")
        : plan
          ? "no drop logged"
          : "screen size not logged",
    },
    {
      name: "Alt+Tab raised the next window",
      ok: plan !== undefined && raised !== null && Number(raised[1]) === plan.raised,
      detail: raised ? `window ${raised[1]}` : "no raise logged",
    },
  ];
}

// --- Hardware report -------------------------------------------------------------
//
// tools/pi-report run against the QEMU image, so the script the Pi's first boot
// depends on has already run end to end once.

/** Shell command run over SSH after the report: it must have put the session back. */
export const REPORT_PROBE = "[ -e /data/session.conf ] && echo session-conf present || echo session-conf absent";

/** `report` is pi-report's output, `exit` its exit code, `log` the serial log,
 * `framesBefore` the compositor's first-frame count before the report started, and
 * `probe` the output of REPORT_PROBE. */
export function evaluateReport(
  report: string,
  exit: number | null,
  log: string,
  framesBefore: number,
  probe: string,
): Check[] {
  const has = (re: RegExp) => re.test(report);
  const errors = report.split("\n").filter((l) => /^(kms-probe|db-bench|store-bench|js-bench|osctl|dev-exec): /.test(l));
  const sections = ["Board", "Storage", "Display", "Input devices", "Spike A/B", "Spike C", "Database", "Spike D", "Bus"];
  const missing = sections.filter((name) => !report.includes(`## ${name}`));
  const frames = log.split("\n").filter((l) => FIRST_FRAME.test(l)).length;
  const conf = /session-conf (\w+)/.exec(probe)?.[1];
  return [
    { name: "report exited 0", ok: exit === 0, detail: `exit ${exit}` },
    { name: "report has every section", ok: missing.length === 0, detail: missing.length ? `missing ${missing.join(", ")}` : "yes" },
    { name: "report tools all ran", ok: errors.length === 0, detail: errors[0] ?? "no errors" },
    {
      name: "kms-probe ran with 2 and 3 buffers",
      ok: has(/2 scanout buffers/) && has(/3 scanout buffers/),
      detail: `${has(/2 scanout buffers/) ? "2" : "-"} / ${has(/3 scanout buffers/) ? "3" : "-"}`,
    },
    {
      name: "report's compositor run found input",
      ok: has(/kms: .* adapter/) && has(/input: added .*\(.*keyboard/),
      detail: has(/input: added/) ? "yes" : has(/kms: /) ? "no input devices" : "compositor did not start",
    },
    {
      name: "benchmarks produced tables",
      ok:
        has(/\| Operation \| Count \| p50/) &&
        has(/\| Store operation \| Count \| p50/) &&
        has(/\| Process \| Ready in/) &&
        has(/^BENCH size=4096 /m),
      detail: `db-bench ${has(/\| Operation \|/) ? "yes" : "no"}, store-bench ${has(/\| Store operation \|/) ? "yes" : "no"}, js-bench ${has(/\| Process \|/) ? "yes" : "no"}, bus ${
        has(/^BENCH size=64 /m) && has(/^BENCH size=4096 /m) ? "yes" : "no"
      }`,
    },
    {
      // hw-report stops the desktop's compositor by restarting session, and bounds its
      // own compositor run with timeout; both send SIGTERM.
      name: "compositor stopped cleanly and gave the console back",
      ok: /kms: stopping on SIGTERM/.test(log) && /compositor stopped: The keyboard is in (?!off)/.test(report),
      detail: `${/kms: stopping on SIGTERM/.test(log) ? "clean exit" : "no clean exit logged"}, ${
        /compositor stopped: (.*)$/m.exec(report)?.[1] ?? "console keyboard not reported"
      }`,
    },
    {
      name: "desktop restored after the report",
      ok: frames > framesBefore && conf === "absent",
      detail: `${frames - framesBefore} new first frame(s), /data/session.conf ${conf ?? "not checked"}`,
    },
  ];
}

// session and the console shell share the serial console, so session's messages can
// follow a shell prompt on the same line ("# session: ..."). Don't anchor them to the
// start of a line.

/** Pids of every shell session has started, in order ("session: started shell (pid N)"). */
export function shellPids(log: string): number[] {
  return [...log.matchAll(/session: started shell \(pid (\d+)\)/g)].map((m) => Number(m[1]));
}

/** Shell command run over SSH after a deploy. `oldShell` is the pid of the shell the
 * first session started, which must be gone once session restarts. */
export function deployProbe(oldShell: number): string {
  return [
    "echo exe $(readlink /proc/$(pidof session)/exe)",
    "echo compositor-exe $(readlink /proc/$(pidof compositor)/exe)",
    `[ -d /proc/${oldShell} ] && echo old-shell alive || echo old-shell gone`,
  ].join("; ");
}

/** Results of tools/deploy-to-pi against the running image: `log` is the whole serial
 * log, `exit` the deploy script's exit code, `probe` the output of deployProbe. */
export function evaluateDeploy(log: string, exit: number | null, probe: string): Check[] {
  const f = facts(probe.split("\n").map((l) => `SMOKE ${l}`).join("\n"));
  const shells = shellPids(log).length;
  const stopped = /session: SIGTERM, stopping services/.test(log);
  const frames = log.split("\n").filter((l) => FIRST_FRAME.test(l)).length;
  return [
    { name: "deploy script exited 0", ok: exit === 0, detail: `exit ${exit}` },
    {
      name: "session restarted",
      ok: stopped && shells >= 2,
      detail: `${stopped ? "stopped on SIGTERM" : "no SIGTERM stop logged"}, ${shells} shell start(s)`,
    },
    {
      name: "runs the deployed session",
      ok: f.get("exe") === "/opt/idealos/bin/session",
      detail: f.get("exe") || "not reported",
    },
    { name: "old shell stopped", ok: f.get("old-shell") === "gone", detail: f.get("old-shell") ?? "not reported" },
    {
      name: "compositor restarted",
      ok: frames >= 2 && f.get("compositor-exe") === "/opt/idealos/bin/compositor",
      detail: `${frames} first frame(s), running ${f.get("compositor-exe") || "nothing"}`,
    },
  ];
}
