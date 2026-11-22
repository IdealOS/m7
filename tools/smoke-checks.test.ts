// node --test tools/
import { test } from "node:test";
import assert from "node:assert/strict";
import { deployProbe, evaluate, evaluateDeploy, facts, shellPids } from "./smoke-checks.ts";

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
