// Boot the QEMU image headless and check that it came up with the compositor on its
// display, and that it takes input: a title-bar drag and Alt+Tab, sent through QEMU's
// virtio tablet and keyboard. Exits 1 if any check fails.
//
//   node tools/qemu-smoke.ts [--screenshot docs/screenshots/phase0-qemu-image.png] [--deploy]
//
// --deploy then pushes target/device (from tools/build-device-bins) with
// tools/deploy-to-pi over SSH and checks that session restarted from the overlay.
// --report then runs tools/pi-report, the Pi's first-boot report, against the image.
//
// The serial console is driven over a Unix socket and the display is captured
// over QMP, so no window opens. Boots a scratch copy of the disk image.
import { spawn, spawnSync } from "node:child_process";
import { constants, copyFileSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { connect, createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { qmp, screendump } from "./qmp.ts";
import {
  type Check,
  FIRST_FRAME,
  GUEST_SCRIPT,
  deployProbe,
  type InputPlan,
  REPORT_PROBE,
  evaluate,
  evaluateDeploy,
  evaluateInput,
  evaluateReport,
  inputPlan,
  screenSize,
  shellPids,
} from "./smoke-checks.ts";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(REPO, "image/output/qemu");
const TIMEOUT_MS = 120_000;
/** The report runs kms-probe twice, the compositor for 10 s and both benchmarks. */
const REPORT_TIMEOUT_MS = 240_000;

const args = process.argv.slice(2);
const shotIndex = args.indexOf("--screenshot");
const screenshot = shotIndex >= 0 ? resolve(args[shotIndex + 1] ?? "") : undefined;
if (shotIndex >= 0 && !args[shotIndex + 1]) {
  console.error("--screenshot needs a path");
  process.exit(2);
}
const deploy = args.includes("--deploy");
const report = args.includes("--report");
if (!existsSync(join(OUT, "Image")) || !existsSync(join(OUT, "disk.img"))) {
  console.error("No QEMU image. Run: make image-qemu");
  process.exit(2);
}
if (deploy && !existsSync(join(REPO, "target/device/session"))) {
  console.error("No device binaries. Run: tools/build-device-bins");
  process.exit(2);
}

/** A free TCP port on 127.0.0.1 for the guest's SSH, so a running `make qemu` on
 * 2222 doesn't collide. */
async function freePort(): Promise<number> {
  const srv = createServer();
  await new Promise<void>((done) => srv.listen(0, "127.0.0.1", done));
  const addr = srv.address();
  await new Promise((done) => srv.close(done));
  if (addr === null || typeof addr === "string") throw new Error("no port");
  return addr.port;
}
const sshPort = await freePort();

// Unix socket paths must be under 104 bytes, so keep everything in a short temp dir.
const work = mkdtempSync(join(tmpdir(), "iq-"));
const disk = join(work, "disk.img");
copyFileSync(join(OUT, "disk.img"), disk, constants.COPYFILE_FICLONE);

const qemu = spawn(
  "qemu-system-aarch64",
  [
    ...["-M", "virt", "-accel", "hvf", "-cpu", "host", "-smp", "4", "-m", "4G"],
    ...["-kernel", join(OUT, "Image")],
    ...["-append", "root=/dev/vda1 rootfstype=ext4 rootwait ro console=tty0 console=ttyAMA0"],
    ...["-drive", `file=${disk},if=virtio,format=raw`],
    ...["-device", "virtio-gpu-pci", "-display", "none"],
    ...["-device", "virtio-keyboard-pci", "-device", "virtio-tablet-pci"],
    ...["-netdev", `user,id=net0,hostfwd=tcp:127.0.0.1:${sshPort}-:22`, "-device", "virtio-net-pci,netdev=net0"],
    ...["-chardev", `socket,id=ser,path=${join(work, "serial")},server=on,wait=on`],
    ...["-serial", "chardev:ser"],
    ...["-qmp", `unix:${join(work, "qmp")},server=on,wait=off`],
  ],
  { stdio: ["ignore", "ignore", "pipe"] },
);

// QEMU prints informational lines on stderr; show them only if something fails.
let qemuStderr = "";
qemu.stderr?.on("data", (chunk: Buffer) => (qemuStderr += chunk.toString()));

let log = "";
const waiters: { pattern: RegExp; resolve: () => void }[] = [];
function onOutput(chunk: Buffer) {
  log += chunk.toString().replaceAll("\r", "");
  for (const w of waiters.splice(0)) {
    if (w.pattern.test(log)) w.resolve();
    else waiters.push(w);
  }
}
function waitFor(pattern: RegExp): Promise<void> {
  return new Promise((done) => {
    if (pattern.test(log)) done();
    else waiters.push({ pattern, resolve: done });
  });
}

/** waitFor, but give up after `ms` and return false. */
function waitAtMost(pattern: RegExp, ms: number): Promise<boolean> {
  return Promise.race([
    waitFor(pattern).then(() => true),
    new Promise<boolean>((done) => setTimeout(() => done(false), ms).unref()),
  ]);
}

let deployResults: Check[] = [];
let reportResults: Check[] = [];
let plan: InputPlan | undefined;

/** Drag the top window and press Alt+Tab, then wait for the compositor to log both. */
async function runInput(): Promise<void> {
  const size = screenSize(log);
  if (!size) return; // evaluateInput reports it
  plan = inputPlan(size.w, size.h);
  await qmp(join(work, "qmp"), plan.drag);
  if (!(await waitAtMost(/wm: dropped window/, 10_000))) return;
  await qmp(join(work, "qmp"), plan.keys);
  await waitAtMost(/wm: raised window/, 10_000);
}

/** Push target/device with tools/deploy-to-pi and check the restarted session. */
async function runDeploy(): Promise<void> {
  const oldShell = shellPids(log)[0] ?? -1;
  // The guest's host key is new on every scratch disk, so use a throwaway known_hosts.
  const env = { ...process.env, NO_BUILD: "1", KNOWN_HOSTS: join(work, "known_hosts") };
  const deployed = spawnSync(join(REPO, "tools/deploy-to-pi"), ["127.0.0.1", String(sshPort)], {
    env,
    stdio: "inherit",
  });
  // A second "started shell" line means init respawned session and it started again.
  await waitFor(/session: started shell[\s\S]*session: started shell/);
  await waitAtMost(new RegExp(`${FIRST_FRAME.source}[\\s\\S]*${FIRST_FRAME.source}`), 20_000);
  const probe = spawnSync("ssh", [...sshArgs(), deployProbe(oldShell)], { encoding: "utf8" });
  deployResults = evaluateDeploy(log, deployed.status, probe.stdout ?? "");
}

function sshArgs(): string[] {
  return [
    ...["-i", join(OUT, "../dev-key"), "-p", String(sshPort), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes"],
    ...["-o", "StrictHostKeyChecking=accept-new", "-o", `UserKnownHostsFile=${join(work, "known_hosts")}`],
    "root@127.0.0.1",
  ];
}

/** Run tools/pi-report against the guest and check it put the desktop back. */
async function runReport(): Promise<void> {
  const framesBefore = log.split("\n").filter((l) => FIRST_FRAME.test(l)).length;
  const env = { ...process.env, KNOWN_HOSTS: join(work, "known_hosts") };
  // Async, so the serial console keeps being read while the report runs.
  const child = spawn(join(REPO, "tools/pi-report"), ["127.0.0.1", String(sshPort)], { env });
  let out = "";
  child.stdout.on("data", (c: Buffer) => (out += c.toString()));
  child.stderr.on("data", (c: Buffer) => (out += c.toString()));
  const exit = await new Promise<number | null>((done) => child.on("exit", done));
  const again = new RegExp(Array(framesBefore + 1).fill(FIRST_FRAME.source).join("[\\s\\S]*"));
  await waitAtMost(again, 20_000);
  const probe = spawnSync("ssh", [...sshArgs(), REPORT_PROBE], { encoding: "utf8" });
  writeFileSync(join(REPO, "image/output/qemu/smoke-report.md"), out);
  reportResults = evaluateReport(out, exit, log, framesBefore, probe.stdout ?? "");
}

async function connectSerial(): Promise<Socket> {
  const path = join(work, "serial");
  for (let i = 0; i < 100; i++) {
    if (existsSync(path)) {
      const sock = connect(path);
      await new Promise<void>((done, fail) => sock.once("connect", done).once("error", fail));
      return sock;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("QEMU did not open its serial socket");
}

async function run(): Promise<void> {
  const serial = await connectSerial();
  serial.on("data", onOutput);
  await waitFor(/session: started shell/);
  // If the compositor never draws, carry on so the other checks still report.
  if ((await waitAtMost(FIRST_FRAME, 20_000)) && screenshot) {
    await screendump(join(work, "qmp"), screenshot);
    console.log(`Screenshot: ${screenshot}`);
  }
  // Before the guest script, so the compositor's "wm:" lines can't split its output.
  if (FIRST_FRAME.test(log)) await runInput();
  // Type the script into a file, then run it. Typed straight into the shell, its
  // lines arrive while earlier ones run, and the console echoes the typed-ahead text
  // in the middle of their output ("ecrets string:802-SMOKE hostname idealos"), so
  // results stop starting their lines. Nothing runs while a here-document is typed.
  serial.write(`cat > /tmp/smoke.sh <<'SMOKE_EOF'\n${GUEST_SCRIPT.join("\n")}\nSMOKE_EOF\nsh /tmp/smoke.sh\n`);
  await waitFor(/^SMOKE end$/m);
  if (deploy) await runDeploy();
  if (report) await runReport();
  serial.destroy();
}

let failed = true;
const limit = report ? REPORT_TIMEOUT_MS : TIMEOUT_MS;
const timer = setTimeout(() => {
  console.error(`Timed out after ${limit / 1000} s`);
  finish();
}, limit);

function finish(): never {
  clearTimeout(timer);
  qemu.kill();
  writeFileSync(join(REPO, "image/output/qemu/smoke-serial.log"), log);
  rmSync(work, { recursive: true, force: true });
  const results = [...evaluate(log), ...evaluateInput(log, plan), ...deployResults, ...reportResults];
  if (deploy && deployResults.length === 0) {
    results.push({ name: "deploy", ok: false, detail: "did not finish" });
  }
  if (report && reportResults.length === 0) {
    results.push({ name: "hardware report", ok: false, detail: "did not finish" });
  }
  for (const c of results) console.log(`${c.ok ? "ok  " : "FAIL"}  ${c.name}: ${c.detail}`);
  failed = failed || results.some((c) => !c.ok);
  if (failed && qemuStderr) console.error(qemuStderr.trimEnd());
  console.log(failed ? "Smoke test failed (log: image/output/qemu/smoke-serial.log)" : "Smoke test passed");
  process.exit(failed ? 1 : 0);
}

qemu.on("exit", (code) => {
  if (failed) console.error(`QEMU exited early (code ${code})`);
});

try {
  await run();
  failed = false;
} catch (e) {
  console.error((e as Error).message);
}
finish();
