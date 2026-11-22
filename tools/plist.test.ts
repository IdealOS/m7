// node --test tools/
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { plist } from "./plist.ts";

test("values are written with sorted keys and escaped text", () => {
  const out = plist({ b: [1, true, "a<b&c"], a: {}, c: [] });
  assert.match(out, /<dict>\n\t<key>a<\/key>\n\t<dict\/>\n\t<key>b<\/key>\n\t<array>/);
  assert.match(out, /<integer>1<\/integer>\n\t\t<true\/>\n\t\t<string>a&lt;b&amp;c<\/string>/);
  assert.match(out, /<key>c<\/key>\n\t<array\/>/);
});

test("fractions are rejected", () => {
  assert.throws(() => plist({ x: 1.5 }), /only integers/);
});

test("macOS reads the output back", { skip: process.platform !== "darwin" }, () => {
  const file = join(mkdtempSync(join(tmpdir(), "plist-")), "t.plist");
  writeFileSync(file, plist({ Name: "idealos & co", Count: 4, On: false, List: ["x"] }));
  const json = JSON.parse(execFileSync("plutil", ["-convert", "json", "-o", "-", file], { encoding: "utf8" }));
  assert.deepEqual(json, { Name: "idealos & co", Count: 4, On: false, List: ["x"] });
});
