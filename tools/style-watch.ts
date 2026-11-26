// Edit a style sheet live from this Mac: every time FILE is saved, send it to the
// device as TARGET's edits (`osctl style set TARGET -` over SSH), and the screen
// restyles at once. A sheet with a mistake isn't applied; its line and column are
// printed here instead, and the last good one stays on screen.
//
//   node tools/style-watch.ts HOST[:PORT] [TARGET] FILE
//   make style-watch HOST=idealos.local FILE=my.css [TARGET=app:org.idealos.notes]
//
// TARGET is `system` (the default) or `app:<app id>`. For the QEMU image, HOST is
// localhost:2222. `osctl style get builtin` on the device prints the built-in sheet
// to start from.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, watchFile } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
if (args.length < 2 || args.length > 3) {
  console.error("usage: node tools/style-watch.ts HOST[:PORT] [TARGET] FILE");
  process.exit(2);
}
const [hostPort, target, file] = args.length === 3 ? [args[0], args[1], args[2]] : [args[0], "system", args[1]];
if (!/^(system|app:[A-Za-z0-9.-]+)$/.test(target)) {
  console.error(`a target is system or app:ID, not ${target}`);
  process.exit(2);
}
const [host, port = "22"] = hostPort.split(":");
const key = join(REPO, "image/output/dev-key");
if (!existsSync(key)) {
  console.error(`No ${key}. tools/build-image makes it.`);
  process.exit(2);
}
// A QEMU guest gets a new host key with every fresh disk, so don't remember it.
const local = host === "localhost" || host === "127.0.0.1";
const known = local ? ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
  : ["-o", "StrictHostKeyChecking=accept-new", "-o", `UserKnownHostsFile=${process.env.KNOWN_HOSTS ?? join(REPO, "image/output/known_hosts")}`];
const ssh = ["-i", key, "-p", port, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ...known, `root@${host}`];

function send(): void {
  let css: string;
  try {
    css = readFileSync(file, "utf8");
  } catch (e) {
    console.error(`${file}: ${(e as Error).message}`);
    return;
  }
  const r = spawnSync("ssh", [...ssh, `osctl style set ${target} -`], { input: css, encoding: "utf8" });
  const time = new Date().toLocaleTimeString();
  const said = `${r.stdout ?? ""}${r.stderr ?? ""}`.trim().replace(/^osctl: /, "");
  console.log(r.status === 0 ? `${time}  ${said}` : `${time}  not applied: ${said || `ssh exited ${r.status}`}`);
}

console.log(`Watching ${file}; each save restyles ${target} on ${host}. Ctrl-C to stop.`);
send();
watchFile(file, { interval: 250 }, (now, before) => {
  if (now.mtimeMs !== before.mtimeMs) send();
});
