// node --test tools/
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  deployProbe,
  evaluate,
  evaluateDeploy,
  evaluateInput,
  evaluateReport,
  facts,
  inputPlan,
  screenSize,
  shellPids,
} from "./smoke-checks.ts";

const GOOD = [
  "Starting syslogd: OK",
  "session: started shell (pid 183)",
  "session: started compositor (pid 184)",
  "[2026-11-21T10:00:00Z INFO  compositor::backend::kms::run] kms: virtio_gpu (virtio GPU) on Virtual-1 at 1280x800 @ 75 Hz; adapter llvmpipe (LLVM 22.1.8, 128 bits) (Vulkan)",
  "[2026-11-21T10:00:00Z INFO  compositor::backend::kms::run] kms: first frame on screen",
  "# echo SMOKE hostname $(hostname)",
  "SMOKE hostname idealos\r",
  "SMOKE data rw",
  "SMOKE overlay ok",
  "SMOKE vulkan-loader ok",
  "SMOKE session ok",
  "SMOKE compositor-pid 184",
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
    .replace("SMOKE compositor-pid 184", "SMOKE compositor-pid ");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["Vulkan loader installed", "compositor showed a frame", "compositor still running"]);
});

test("a compositor that crashes after its first frame is reported", () => {
  // session restarts it, so a pid may still be there; the exit line gives it away.
  const log = GOOD.replace(
    "SMOKE compositor-pid 184",
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
const DEPLOY_PROBE = "exe /opt/idealos/bin/session\ncompositor-exe /opt/idealos/bin/compositor\nold-shell gone\n";

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
  assert.deepEqual(failed, ["deploy script exited 0", "runs the deployed session", "compositor restarted"]);
});

test("the deploy probe checks the old shell's pid", () => {
  assert.match(deployProbe(183), /\/proc\/183 /);
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
  "Scene: 20 windows, 800 nodes; 2 scanout buffers",
  "Scene: 20 windows, 800 nodes; 3 scanout buffers",
  "[2026-11-22T10:00:00Z INFO  compositor::backend::kms::run] kms: virtio_gpu (virtio GPU) on Virtual-1 at 1280x800 @ 75 Hz; adapter llvmpipe (Vulkan)",
  "[2026-11-22T10:00:00Z INFO  compositor::backend::kms::input] input: added QEMU Virtio Keyboard (keyboard)",
  "## Spike C: database (10,000 documents on /data)",
  "| Operation | Count | p50 | p99 | Throughput |",
  "## Spike D: QuickJS app processes",
  "| Process | Ready in | RSS | QuickJS heap |",
].join("\n");
const RESTORED = `${GOOD}\n[2026-11-22T10:01:00Z INFO  compositor::backend::kms::run] kms: first frame on screen`;

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
  const left = evaluateReport(REPORT, 0, GOOD, 1, "session-conf present").filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(left, ["desktop restored after the report"]);
});
