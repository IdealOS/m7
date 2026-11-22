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
