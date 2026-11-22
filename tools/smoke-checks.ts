// Pass/fail rules for tools/qemu-smoke.ts, kept free of I/O so they can be tested.
//
// The guest script prints one "SMOKE <key> <value>" line per fact it checked; the
// rules below turn the whole serial log into a list of results.

export interface Check {
  name: string;
  ok: boolean;
  detail: string;
}

/** Shell lines the smoke test types into the session's console shell. */
export const GUEST_SCRIPT = [
  "echo SMOKE hostname $(hostname)",
  'grep " /data " /proc/mounts | grep -q " rw," && echo SMOKE data rw',
  'grep -q "^overlay /opt/idealos overlay" /proc/mounts && echo SMOKE overlay ok',
  "[ -e /usr/lib/libvulkan.so.1 ] && echo SMOKE vulkan-loader ok",
  'ps | grep -q "[/]usr/bin/session" && echo SMOKE session ok',
  // Run the probe in the background so the host can capture the held frame.
  "kms-probe --hold 3 >/data/smoke-probe.out 2>/data/smoke-probe.err & p=$!",
  'while kill -0 $p 2>/dev/null && ! grep -q "holding the last frame" /data/smoke-probe.err; do sleep 0.2; done',
  "echo SMOKE holding",
  "wait $p; echo SMOKE probe-exit $?",
  "sed -n 's/.* = \\([0-9.]*\\) fps$/SMOKE fps \\1/p' /data/smoke-probe.out",
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
  const fps = Number(f.get("fps"));
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
    fact("probe-exit", "0", "kms-probe exited 0"),
    { name: "kms-probe frame rate", ok: fps > 0, detail: f.has("fps") ? `${fps} fps` : "not reported" },
  ];
}
