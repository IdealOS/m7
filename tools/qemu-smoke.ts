// Boot the QEMU image headless, check that it came up, and run kms-probe on its
// display. Exits 1 if any check fails.
//
//   node tools/qemu-smoke.ts [--screenshot docs/screenshots/phase0-qemu-image.png]
//
// The serial console is driven over a Unix socket and the display is captured
// over QMP, so no window opens. Boots a scratch copy of the disk image.
import { spawn } from "node:child_process";
import { constants, copyFileSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { connect, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { screendump } from "./qmp.ts";
import { GUEST_SCRIPT, evaluate } from "./smoke-checks.ts";

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
if (!existsSync(join(OUT, "Image")) || !existsSync(join(OUT, "disk.img"))) {
  console.error("No QEMU image. Run: make image-qemu");
  process.exit(2);
}

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
    ...["-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0"],
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
  serial.write(GUEST_SCRIPT.join("\n") + "\n");
  await waitFor(/^SMOKE holding$/m);
  if (screenshot) {
    await screendump(join(work, "qmp"), screenshot);
    console.log(`Screenshot: ${screenshot}`);
  }
  await waitFor(/^SMOKE end$/m);
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
  const results = evaluate(log);
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
