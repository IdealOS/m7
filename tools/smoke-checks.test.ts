// node --test tools/
import { test } from "node:test";
import assert from "node:assert/strict";
import { evaluate, facts } from "./smoke-checks.ts";

const GOOD = [
  "Starting syslogd: OK",
  "session: started shell (pid 183)",
  "# echo SMOKE hostname $(hostname)",
  "SMOKE hostname idealos\r",
  "SMOKE data rw",
  "SMOKE overlay ok",
  "SMOKE vulkan-loader ok",
  "SMOKE session ok",
  "SMOKE holding",
  "SMOKE probe-exit 0",
  "SMOKE fps 79.2",
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

test("a missing Vulkan loader and a failed probe are reported", () => {
  const log = GOOD.replace("SMOKE vulkan-loader ok\n", "")
    .replace("SMOKE probe-exit 0", "SMOKE probe-exit 1")
    .replace("SMOKE fps 79.2\n", "");
  const failed = evaluate(log).filter((c) => !c.ok).map((c) => c.name);
  assert.deepEqual(failed, ["Vulkan loader installed", "kms-probe exited 0", "kms-probe frame rate"]);
});

test("an unset hostname fails", () => {
  const log = GOOD.replace("SMOKE hostname idealos", "SMOKE hostname (none)");
  const check = evaluate(log).find((c) => c.name === "hostname is idealos");
  assert.equal(check?.ok, false);
  assert.equal(check?.detail, "(none)");
});
