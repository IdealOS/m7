// Writes sdk/src/protocol.ts from crates/protocol/schema.json (see protocol-types.ts).
// `make protocol` runs this after exporting the schema.
//
//   node tools/gen-protocol.ts           write sdk/src/protocol.ts
//   node tools/gen-protocol.ts --check   exit 1 if it is out of date

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { generate } from "./protocol-types.ts";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SCHEMA = "crates/protocol/schema.json";
const OUT = "sdk/src/protocol.ts";

const text = generate(JSON.parse(readFileSync(resolve(REPO, SCHEMA), "utf8")), SCHEMA);
if (process.argv.includes("--check")) {
  const current = readFileSync(resolve(REPO, OUT), "utf8");
  if (current !== text) {
    console.error(`${OUT} is out of date with ${SCHEMA}; run \`make protocol\``);
    process.exit(1);
  }
} else {
  writeFileSync(resolve(REPO, OUT), text);
  console.log(`wrote ${OUT}`);
}
