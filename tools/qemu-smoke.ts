// Boot the QEMU image headless and check that it came up with the compositor on its
// display. Exits 1 if any check fails.
//
//   node tools/qemu-smoke.ts [--screenshot docs/screenshots/phase0-qemu-image.png] [--deploy]
//
// --deploy then pushes target/device (from tools/build-device-bins) with
// tools/deploy-to-pi over SSH and checks that session restarted from the overlay.
//
// The serial console is driven over a Unix socket and the display is captured
// over QMP, so no window opens. Boots a scratch copy of the disk image.
import { spawn, spawnSync } from "node:child_process";
import { constants, copyFileSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { connect, createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { screendump } from "./qmp.ts";
import {
  type Check,
  FIRST_FRAME,
  GUEST_SCRIPT,
  deployProbe,
  evaluate,
  evaluateDeploy,
  shellPids,
} from "./smoke-checks.ts";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(REPO, "image/output/qemu");
const TIMEOUT_MS = 120_000;

const args = process.argv.slice(2);
const shotIndex = args.indexOf("--screenshot");
const screenshot = shotIndex >= 0 ? resolve(args[shotIndex + 1] ?? "") : undefined;
if (shotIndex >= 0 && !args[shotIndex + 1]) {
  console.error("--screenshot needs a path");
  process.exit(2);
}
const deploy = args.includes("--deploy");
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
  const probe = spawnSync(
    "ssh",
    [
      ...["-i", join(OUT, "../dev-key"), "-p", String(sshPort), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes"],
      ...["-o", "StrictHostKeyChecking=accept-new", "-o", `UserKnownHostsFile=${join(work, "known_hosts")}`],
      ...["root@127.0.0.1", deployProbe(oldShell)],
    ],
    { encoding: "utf8" },
  );
  deployResults = evaluateDeploy(log, deployed.status, probe.stdout ?? "");
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
  serial.write(GUEST_SCRIPT.join("\n") + "\n");
  await waitFor(/^SMOKE end$/m);
  if (deploy) await runDeploy();
  serial.destroy();
}

let failed = true;
const timer = setTimeout(() => {
  console.error(`Timed out after ${TIMEOUT_MS / 1000} s`);
  finish();
}, TIMEOUT_MS);

function finish(): never {
  clearTimeout(timer);
  qemu.kill();
  writeFileSync(join(REPO, "image/output/qemu/smoke-serial.log"), log);
  rmSync(work, { recursive: true, force: true });
  const results = [...evaluate(log), ...deployResults];
  if (deploy && deployResults.length === 0) {
    results.push({ name: "deploy", ok: false, detail: "did not finish" });
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
