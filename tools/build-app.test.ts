import assert from "node:assert/strict";
import { test } from "node:test";

import { esbuildArgs, manifest } from "./build-app.ts";

test("the manifest comes from package.json's idealos section", () => {
  const m = manifest(JSON.stringify({ version: "1.2.0", idealos: { id: "org.idealos.notes", name: "Notes", autostart: true } }));
  assert.deepEqual(m, { id: "org.idealos.notes", name: "Notes", version: "1.2.0", autostart: true });
  assert.equal(manifest(JSON.stringify({ idealos: { id: "a.b", name: "X" } })).autostart, false);
});

test("bad manifests are refused", () => {
  assert.throws(() => manifest("{}"), /idealos/);
  assert.throws(() => manifest(JSON.stringify({ idealos: { id: "notes", name: "N" } })), /reverse-DNS/);
  assert.throws(() => manifest(JSON.stringify({ idealos: { id: "a.b c", name: "N" } })), /reverse-DNS/);
});

test("esbuild bundles main.ts and the SDK into one ES module", () => {
  const args = esbuildArgs("/r/apps/x", "/r/sdk/src/index.ts");
  assert.equal(args[0], "/r/apps/x/src/main.ts");
  assert.ok(args.includes("--bundle") && args.includes("--format=esm"));
  assert.ok(args.includes("--alias:@idealos/sdk=/r/sdk/src/index.ts"));
  assert.ok(args.includes("--outfile=/r/apps/x/dist/app.js"));
});
