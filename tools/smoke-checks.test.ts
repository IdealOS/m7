// node --test tools/
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  DEPLOYED_SERVICES,
  deployProbe,
  evaluate,
  evaluateDeploy,
  evaluateInput,
  evaluateRecording,
  evaluateReport,
  facts,
  inputPlan,
  screenSize,
  shellPids,
} from "./smoke-checks.ts";

const GOOD = [
  "Starting syslogd: OK",
  "session: started shell (pid 183)",
  "session: started bus (pid 184)",
  "session: started compositor (pid 185)",
  "[2026-11-21T10:00:00Z INFO  busd] busd: listening on /run/idealos/bus.sock",
  "[2026-11-21T10:00:00Z INFO  compositor::backend::kms::run] kms: virtio_gpu (virtio GPU) on Virtual-1 at 1280x800 @ 75 Hz; adapter llvmpipe (LLVM 22.1.8, 128 bits) (Vulkan)",
  "[2026-11-21T10:00:00Z INFO  compositor::backend::kms::run] kms: first frame on screen",
  "# echo SMOKE hostname $(hostname)",
  "SMOKE hostname idealos\r",
  "SMOKE data rw",
  "SMOKE overlay ok",
  "SMOKE vulkan-loader ok",
  "SMOKE session ok",
  "SMOKE compositor-pid 185",
  "SMOKE console-keyboard The keyboard is in off mode",
  "SMOKE shader-cache-files 3",
  "SMOKE bus-socket /run/idealos/bus.sock",
  "SMOKE bus-session 1",
  'SMOKE bus-call {"n":1}',
  "SMOKE bus-bench BENCH size=64 rtt_p50_us=180.2 rtt_p99_us=410.0 requests_per_s=21000 publishes_per_s=90000",
  "SMOKE db-file /data/db/idealos.db",
  "SMOKE db-roundtrip 1",
  'SMOKE db-undo "v":1',
  'SMOKE db-watch "kind":"initial" "kind":"diff"',
  "SMOKE db-snapshot /data/db/snapshots/snapshot-1795000000000-12.db",
  "SMOKE db-links 1",
  "SMOKE db-blob smoke-blob 1",
  "SMOKE db-schema 1",
  "SMOKE apps-running 2",
  "SMOKE app-gallery installed",
  "SMOKE perm-services 2",
  "SMOKE notes-grant 1",
  "SMOKE app-sandbox netns Seccomp:2",
  "SMOKE m7-services 2",
  "SMOKE net-wired 1",
  "SMOKE pty-open 1",
  "SMOKE m7-apps 3",
  "SMOKE wifi-preset ok",
  "SMOKE backup-drive 1",
  "SMOKE backup-now 1",
  'SMOKE backup-restore "k":"kept"',
  "SMOKE backup-unmounted 0",
  "SMOKE import-service 1",
  "SMOKE cifs 1",
  "SMOKE audio-service 1",
  "SMOKE sound-card 1",
  "SMOKE audio-output 1",
  'SMOKE audio-played "playCount":1',
  "SMOKE session-log ok",
  "SMOKE console-tty ok",
  '[2026-11-21T10:00:05Z INFO  compositor::desktop] apps: opened a window for App { app: "org.idealos.counter", conn: 9 }: "Counter"',
  '[2026-11-21T10:00:05Z INFO  compositor::desktop] apps: opened a window for App { app: "org.idealos.notes", conn: 10 }: "Notes"',
  "SMOKE end",
].join("\n");

test("a healthy boot passes every check", () => {
  const failed = evaluate(GOOD).filter((c) => !c.ok);
  assert.deepEqual(failed, []);
});

test("the echoed command line is not read as a result", () => {
  // The shell echoes "# echo SMOKE hostname $(hostname)"; only real output counts.
  assert.equal(facts(GOOD).get("hostname"), "idealos");
  assert.equal(facts("# echo SMOKE data rw\n").has("data"), false);
});

test("the original findfs failure is caught", () => {
  // Regression: BusyBox shipped without findfs, so /data never mounted.
  const log = GOOD.replace("SMOKE data rw\n", "").replace(
    "Starting syslogd: OK",
    "/etc/init.d/S05data: line 7: findfs: not found\nS05data: no partition labelled data",
  );
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["S05data ran cleanly", "/data mounted read-write"]);
});

test("a missing Vulkan loader and a compositor that never draws are reported", () => {
  const log = GOOD.replace("SMOKE vulkan-loader ok\n", "")
    .replace(/^.*kms: .*$\n/gm, "")
    .replace("SMOKE compositor-pid 185", "SMOKE compositor-pid ");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["Vulkan loader installed", "compositor showed a frame", "compositor still running"]);
});

test("a compositor that crashes after its first frame is reported", () => {
  // session restarts it, so a pid may still be there; the exit line gives it away.
  const log = GOOD.replace(
    "SMOKE compositor-pid 185",
    "session: compositor exited (signal: 11 (SIGSEGV)) after 1.2s\nSMOKE compositor-pid 190",
  );
  const failed = evaluate(log).filter((c) => !c.ok);
  assert.deepEqual(failed.map((c) => c.name), ["compositor still running"]);
  assert.match(failed[0].detail, /SIGSEGV/);
});

test("the compositor's adapter and mode are reported", () => {
  const shown = evaluate(GOOD).find((c) => c.name === "compositor showed a frame");
  assert.equal(shown?.detail, "virtio_gpu (virtio GPU) on Virtual-1 at 1280x800 @ 75 Hz, llvmpipe");
});

test("an unset hostname fails", () => {
  const log = GOOD.replace("SMOKE hostname idealos", "SMOKE hostname (none)");
  const check = evaluate(log).find((c) => c.name === "hostname is idealos");
  assert.equal(check?.ok, false);
  assert.equal(check?.detail, "(none)");
});

const DEPLOYED = [
  GOOD,
  "session: SIGTERM, stopping services",
  "session: killing shell",
  "session: started shell (pid 241)",
  "session: started compositor (pid 242)",
  "[2026-11-21T10:01:00Z INFO  compositor::backend::kms::run] kms: first frame on screen",
].join("\n");
const DEPLOY_PROBE = [
  ...DEPLOYED_SERVICES.map((b) => `exe-${b} /opt/idealos/bin/${b}`),
  "old-shell gone",
  "apps-running 2",
  "apps-overlay 6/6",
  "",
].join("\n");

test("a deploy that restarts session from the overlay passes", () => {
  assert.deepEqual(shellPids(DEPLOYED), [183, 241]);
  const failed = evaluateDeploy(DEPLOYED, 0, DEPLOY_PROBE).filter((c) => !c.ok);
  assert.deepEqual(failed, []);
});

test("an orphaned old shell fails the deploy", () => {
  // Regression: session died on SIGTERM without stopping its shell, leaving it on the
  // console next to the new session's shell.
  const log = DEPLOYED.replace("session: SIGTERM, stopping services\nsession: killing shell\n", "");
  const failed = evaluateDeploy(log, 0, DEPLOY_PROBE.replace("gone", "alive"))
    .filter((c) => !c.ok)
    .map((c) => c.name);
  assert.deepEqual(failed, ["session restarted", "old shell stopped"]);
});

test("still running the image's session fails the deploy", () => {
  const probe = DEPLOY_PROBE.replaceAll("/opt/idealos/bin/", "/usr/bin/");
  const failed = evaluateDeploy(DEPLOYED, 1, probe)
    .filter((c) => !c.ok)
    .map((c) => c.name);
  assert.deepEqual(failed, [
    "deploy script exited 0",
    "runs the deployed session",
    "services run from the overlay",
    "compositor restarted",
  ]);
});

test("a service still running the image's copy fails the deploy", () => {
  // Regression: deploy-to-pi pushed only the M1 binaries, so permd, secretsd, ptyd
  // and netd kept running the flashed versions next to a new session.
  const probe = DEPLOY_PROBE.replace("exe-ptyd /opt/idealos/bin/ptyd", "exe-ptyd /usr/bin/ptyd").replace(
    "exe-netd /opt/idealos/bin/netd\n",
    "exe-netd\n",
  );
  const checks = evaluateDeploy(DEPLOYED, 0, probe).filter((c) => !c.ok);
  assert.deepEqual(
    checks.map((c) => c.name),
    ["services run from the overlay"],
  );
  assert.equal(checks[0]?.detail, "ptyd: /usr/bin/ptyd, netd: not running");
});

test("apps installed from the image fail the deploy", () => {
  // Regression: deploy-to-pi didn't push apps, and boot reinstalled the image's.
  for (const overlay of ["apps-overlay 0/6", "apps-overlay 0/0", ""]) {
    const probe = DEPLOY_PROBE.replace("apps-overlay 6/6", overlay);
    const failed = evaluateDeploy(DEPLOYED, 0, probe)
      .filter((c) => !c.ok)
      .map((c) => c.name);
    assert.deepEqual(failed, ["apps installed from the overlay"], overlay);
  }
  const failed = evaluateDeploy(DEPLOYED, 0, DEPLOY_PROBE.replace("apps-running 2", "apps-running 0"))
    .filter((c) => !c.ok)
    .map((c) => c.name);
  assert.deepEqual(failed, ["apps running after the restart"]);
});

test("the deploy probe checks the old shell's pid and every service", () => {
  assert.match(deployProbe(183), /\/proc\/183 /);
  for (const b of ["permd", "secretsd", "ptyd", "netd"]) assert.match(deployProbe(183), new RegExp(` ${b}[ ;]`));
});

test("session's messages count after a shell prompt on the same line", () => {
  // Regression: the first QEMU deploy run logged "# session: SIGTERM, stopping
  // services" and the check, anchored to the line start, missed it.
  const log = DEPLOYED.replace("session: SIGTERM", "# session: SIGTERM");
  const failed = evaluateDeploy(log, 0, DEPLOY_PROBE).filter((c) => !c.ok);
  assert.deepEqual(failed, []);
});

// --- Input ---

const INFO = "[2026-11-21T10:00:01Z INFO  compositor::backend::kms::input]";
const INPUT_LOG = [
  GOOD,
  `${INFO} input: added QEMU Virtio Keyboard (keyboard)`,
  `${INFO} input: added QEMU Virtio Tablet (pointer)`,
  "[2026-11-21T10:00:02Z INFO  compositor::backend::kms::run] wm: dropped window 2900 at (894, 402)",
  "[2026-11-21T10:00:02Z INFO  compositor::backend::kms::run] wm: raised window 1000 from the keyboard",
].join("\n");

test("the screen size comes from the compositor's mode line", () => {
  assert.deepEqual(screenSize(GOOD), { w: 1280, h: 800 });
  assert.equal(screenSize("no compositor here"), undefined);
});

test("the input plan grabs the top demo window's title bar", () => {
  const plan = inputPlan(1280, 800);
  // demo.rs: window 20 is column 1, row 9 -> (794, 322); dragged by (100, 80).
  assert.deepEqual(plan.drop, { id: 2900, x: 894, y: 402 });
  assert.equal(plan.raised, 1000);
  // The first command moves to the grab point (944, 340) on the 0..32767 tablet axes.
  const first = JSON.stringify(plan.drag[0]);
  assert.match(first, /"axis":"x","value":24166/);
  assert.match(first, /"axis":"y","value":13926/);
  // The drag ends with the button release; the keys are a separate phase.
  assert.match(JSON.stringify(plan.drag.at(-1)), /"btn".*"down":false/);
  assert.equal(JSON.stringify(plan.keys).match(/"qcode"/g)?.length, 4);
});

test("the input plan keeps windows on a small screen", () => {
  // At 1024x600 the cascade is clamped to the screen edge, as demo.rs does.
  assert.deepEqual(inputPlan(1024, 600).drop, { id: 2900, x: 744, y: 402 });
});

test("a drag and Alt+Tab that both land pass", () => {
  const failed = evaluateInput(INPUT_LOG, inputPlan(1280, 800)).filter((c) => !c.ok);
  assert.deepEqual(failed, []);
});

test("a drop a pixel off still passes, a wrong window fails", () => {
  const plan = inputPlan(1280, 800);
  const offByOne = INPUT_LOG.replace("at (894, 402)", "at (895, 401)");
  assert.ok(evaluateInput(offByOne, plan).every((c) => c.ok));
  const wrong = INPUT_LOG.replace("dropped window 2900", "dropped window 2800");
  const failed = evaluateInput(wrong, plan).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["title-bar drag moved the window"]);
});

test("no input devices and no input actions are each reported", () => {
  const failed = evaluateInput(GOOD, inputPlan(1280, 800)).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, [
    "keyboard found",
    "pointer found",
    "title-bar drag moved the window",
    "Alt+Tab raised the next window",
  ]);
});

// --- Hardware report ---

const REPORT = [
  "# idealos hardware report",
  "## Board", "model:    linux,dummy-virt",
  "## Storage", "## Display", "## Input devices",
  "## Spike A/B: KMS scanout (compositor stopped)",
  "Text console keyboard with the compositor stopped: The keyboard is in Unicode (UTF-8) mode",
  "Scene: 20 windows, 800 nodes; 2 scanout buffers",
  "Scene: 20 windows, 800 nodes; 3 scanout buffers",
  "[2026-11-22T10:00:00Z INFO  compositor::backend::kms::run] kms: virtio_gpu (virtio GPU) on Virtual-1 at 1280x800 @ 75 Hz; adapter llvmpipe (Vulkan)",
  "[2026-11-22T10:00:00Z INFO  compositor::backend::kms::input] input: added QEMU Virtio Keyboard (keyboard)",
  "## Spike C: database (10,000 documents on /data)",
  "| Operation | Count | p50 | p99 | Throughput |",
  "## Database: the store (10,000 emails on /data)",
  "| Store operation | Count | p50 | p99 | Throughput |",
  "## Spike D: QuickJS app processes",
  "| Process | Ready in | RSS | QuickJS heap |",
  "## Bus: round trip and throughput",
  "BENCH size=64 rtt_p50_us=180.2 rtt_p99_us=410.0 requests_per_s=21000 publishes_per_s=90000",
  "BENCH size=4096 rtt_p50_us=260.0 rtt_p99_us=600.0 requests_per_s=12000 publishes_per_s=40000",
].join("\n");
const RESTORED = [
  GOOD,
  "[2026-11-22T10:00:30Z INFO  compositor::backend::kms::run] kms: stopping on SIGTERM",
  "[2026-11-22T10:01:00Z INFO  compositor::backend::kms::run] kms: first frame on screen",
].join("\n");

test("a full report that restores the desktop passes", () => {
  const failed = evaluateReport(REPORT, 0, RESTORED, 1, "session-conf absent").filter((c) => !c.ok);
  assert.deepEqual(failed, []);
});

test("a missing benchmark binary and a missing section are reported", () => {
  const report = REPORT.replace("| Process | Ready in | RSS | QuickJS heap |", "dev-exec: no js-bench binary found").replace(
    "## Storage\n",
    "",
  );
  const failed = evaluateReport(report, 0, RESTORED, 1, "session-conf absent").filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["report has every section", "report tools all ran", "benchmarks produced tables"]);
});

test("a report that leaves the compositor off fails", () => {
  // Regression guard: the session config written to stop the compositor must go away.
  const stopped = `${GOOD}\n[2026-11-22T10:00:30Z INFO  compositor::backend::kms::run] kms: stopping on SIGTERM`;
  const left = evaluateReport(REPORT, 0, stopped, 1, "session-conf present").filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(left, ["desktop restored after the report"]);
});

// --- Bus ---

test("a missing broker is reported", () => {
  const log = GOOD.replace("[2026-11-21T10:00:00Z INFO  busd] busd: listening on /run/idealos/bus.sock\n", "")
    .replace("SMOKE bus-socket /run/idealos/bus.sock", "SMOKE bus-socket ls: /run/idealos/bus.sock: No such file or directory")
    .replace('SMOKE bus-call {"n":1}', "SMOKE bus-call osctl: connect to the bus at /run/idealos/bus.sock: No such file or directory")
    .replace(/SMOKE bus-bench .*/, "SMOKE bus-bench ")
    .replace("SMOKE bus-session 1", "SMOKE bus-session 0");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, [
    "session started busd on /run/idealos/bus.sock",
    "session has its own connection to busd",
    "a request went through the bus and back",
    "bus benchmark ran",
  ]);
});

test("a missing database service is reported", () => {
  const log = GOOD.replace("SMOKE db-file /data/db/idealos.db", "SMOKE db-file ls: /data/db/idealos.db: No such file or directory")
    .replace("SMOKE db-roundtrip 1", "SMOKE db-roundtrip 0")
    .replace('SMOKE db-undo "v":1', "SMOKE db-undo")
    .replace('SMOKE db-watch "kind":"initial" "kind":"diff"', 'SMOKE db-watch "kind":"initial"')
    .replace("SMOKE db-snapshot /data/db/snapshots/snapshot-1795000000000-12.db", "SMOKE db-snapshot osctl: no_such_service: no service named db")
    .replace("SMOKE db-links 1", "SMOKE db-links 0")
    .replace("SMOKE db-blob smoke-blob 1", "SMOKE db-blob osctl: no_such_service: no service named db 0")
    .replace("SMOKE db-schema 1", "SMOKE db-schema 0")
    .replace("SMOKE apps-running 2", "SMOKE apps-running 0")
    .replace(/.*apps: opened a window.*\n/g, "");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, [
    "dbd keeps the database on /data",
    "a document went into the database and came back from a query",
    "undoing a change brought the old value back",
    "a live query reported a new document",
    "a snapshot was written to /data/db/snapshots",
    "a link was followed back from its target",
    "a blob went in from a file and came back through a descriptor",
    "the Contact schema refused a contact without a name",
    "session runs the two bundled apps",
    "the apps opened their windows under their own ids",
  ]);
});

test("busd counts even when the shell's message splits session's line", () => {
  // Seen on the QEMU serial console: "session: started " + the shell's warning.
  const log = GOOD.replace("session: started bus (pid 184)", "/bin/sh: can't access tty; job control turned offsession: started ");
  assert.deepEqual(evaluate(log).filter((c) => !c.ok), []);
});

test("the bus benchmark's numbers are shown", () => {
  const bench = evaluate(GOOD).find((c) => c.name === "bus benchmark ran");
  assert.equal(bench?.detail, "21000 requests/s, round trip p50 180.2 µs");
});

test("a report without the bus benchmark is reported", () => {
  const report = REPORT.replace(/BENCH size=4096 .*/, "osctl: connect to the bus at /run/idealos/bus.sock: Connection refused");
  const failed = evaluateReport(report, 0, RESTORED, 1, "session-conf absent").filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["report tools all ran", "benchmarks produced tables"]);
});

// --- Console and shader cache ---

test("a console keyboard left on is reported", () => {
  // Regression guard: keys typed into the desktop reached the console (on the Pi, a root shell).
  const log = GOOD.replace("in off mode", "in Unicode (UTF-8) mode");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["text console's keyboard off under the compositor"]);
});

test("Mesa failing to create its shader cache is reported", () => {
  const log = GOOD.replace("SMOKE shader-cache-files 3", "SMOKE shader-cache-files 0").replace(
    "Starting syslogd: OK",
    "Starting syslogd: OK\nFailed to create //.cache for shader cache (Read-only file system)---disabling.",
  );
  const cache = evaluate(log).find((c) => c.name === "Mesa shader cache on /data");
  assert.equal(cache?.ok, false);
  assert.match(cache?.detail ?? "", /Read-only file system/);
});

test("a compositor killed without cleanup fails the report", () => {
  // No clean-exit line, and the console keyboard is still off once it has stopped.
  const report = REPORT.replace("Unicode (UTF-8)", "off");
  const log = RESTORED.replace(/^.*stopping on SIGTERM.*$/m, "");
  const failed = evaluateReport(report, 0, log, 1, "session-conf absent").filter((c) => !c.ok);
  assert.deepEqual(failed.map((c) => c.name), ["compositor stopped cleanly and gave the console back"]);
  assert.equal(failed[0].detail, "no clean exit logged, The keyboard is in off mode");
});

test("wgpu starting OpenGL is reported", () => {
  // Regression guard: the GL backend's teardown aborted the compositor under UTM.
  const log = GOOD.replace("Starting syslogd: OK", "Starting syslogd: OK\nlibEGL warning: DRI2: failed to create screen");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["compositor didn't start OpenGL"]);
});

test("the recording must hold sound, not silence", () => {
  const wav = (samples: number[]) => {
    const b = Buffer.alloc(44 + samples.length * 2);
    b.write("RIFF", 0);
    b.write("WAVEfmt ", 8);
    b.write("data", 36); // sizes left at 0, as when QEMU is killed
    samples.forEach((s, i) => b.writeInt16LE(s, 44 + i * 2));
    return b;
  };
  assert.equal(evaluateRecording(undefined).ok, false);
  assert.equal(evaluateRecording(wav(Array(50_000).fill(0))).ok, false);
  assert.equal(evaluateRecording(wav(Array(50_000).fill(3))).ok, false); // dither, not sound
  const noise = wav(Array.from({ length: 50_000 }, (_, i) => (i % 2 ? 9000 : -9000)));
  assert.deepEqual(evaluateRecording(noise), { name: "the sound card played sound", ok: true, detail: "50000 loud samples" });
});
