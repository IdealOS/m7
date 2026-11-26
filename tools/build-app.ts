// Bundle an app for app-host: `node tools/build-app.ts apps/counter [...]`.
//
// esbuild (run through npx, pinned, so nothing is installed in the repo) bundles
// src/main.ts and @idealos/sdk into one ES module, dist/app.js; dist/manifest.json
// is the `idealos` section of the app's package.json plus its version. That pair is
// what `osctl app install` and `app-host --install` take.

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const ESBUILD = "esbuild@0.28.2";
const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export interface Manifest {
  id: string;
  name: string;
  version?: string;
  autostart?: boolean;
  /** Permission rules, granted when the app is installed (see protocol::perm). */
  permissions?: object[];
}

/** The manifest from a package.json's text, checked. */
export function manifest(packageJson: string): Manifest {
  const pkg = JSON.parse(packageJson) as { version?: string; idealos?: Partial<Manifest> };
  const app = pkg.idealos;
  if (!app || typeof app.id !== "string" || typeof app.name !== "string") {
    throw new Error('package.json needs "idealos": {"id": "org.example.app", "name": "App"}');
  }
  const segments = app.id.split(".");
  if (segments.length < 2 || !segments.every((s) => /^[A-Za-z0-9-]+$/.test(s))) {
    throw new Error(`"${app.id}" is not a reverse-DNS app id`);
  }
  const m: Manifest = { id: app.id, name: app.name, version: app.version ?? pkg.version, autostart: app.autostart ?? false };
  if (app.permissions !== undefined) {
    if (!Array.isArray(app.permissions) || !app.permissions.every((p) => typeof p === "object" && p !== null && !Array.isArray(p))) {
      throw new Error('"permissions" is a list of rules such as {"type": "Note", "access": ["read"]}');
    }
    m.permissions = app.permissions;
  }
  return m;
}

/** esbuild's arguments for the app in `dir`. */
export function esbuildArgs(dir: string, sdk: string): string[] {
  return [
    join(dir, "src", "main.ts"),
    "--bundle",
    "--format=esm",
    // QuickJS (in app-host) supports ES2023.
    "--target=es2022",
    `--alias:@idealos/sdk=${sdk}`,
    // `import style from "./style.css"` gives the sheet's text, for openWindow.
    "--loader:.css=text",
    `--outfile=${join(dir, "dist", "app.js")}`,
    "--log-level=warning",
  ];
}

export function build(dir: string): Manifest {
  const m = manifest(readFileSync(join(dir, "package.json"), "utf8"));
  mkdirSync(join(dir, "dist"), { recursive: true });
  execFileSync("npx", ["--yes", ESBUILD, ...esbuildArgs(dir, join(REPO, "sdk", "src", "index.ts"))], {
    stdio: "inherit",
  });
  writeFileSync(join(dir, "dist", "manifest.json"), JSON.stringify(m, null, 2) + "\n");
  return m;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const dirs = process.argv.slice(2);
  if (dirs.length === 0) {
    console.error("usage: node tools/build-app.ts APP_DIR...");
    process.exit(2);
  }
  for (const dir of dirs) {
    const m = build(resolve(dir));
    console.log(`built ${m.id} -> ${relative(process.cwd(), join(resolve(dir), "dist"))}/app.js`);
  }
}
