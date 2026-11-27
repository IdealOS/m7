// Sources typed as text, paths, and how entries and progress read.

import assert from "node:assert/strict";
import { test } from "node:test";

import type { ImportStatus } from "@idealos/sdk";
import { describe, inside, loginOf, parseSource, progress, trail, up } from "../src/places.ts";

test("sources parse as importd writes them", () => {
  assert.deepEqual(parseSource(" smb://Joshua Marinacci@10.0.2.2/Music/ "), {
    share: { host: "10.0.2.2", share: "Music", user: "Joshua Marinacci" },
  });
  assert.deepEqual(parseSource("smb://me@[::1]:1445/x"), { share: { host: "::1", share: "x", user: "me", port: 1445 } });
  assert.deepEqual(parseSource("dir:/data/inbox"), { dir: { path: "/data/inbox" } });
  for (const bad of ["", "smb://host/share", "smb://me@host", "smb://me@host/a/b", "smb://me@host:99999/x", "dir:"]) {
    assert.equal(parseSource(bad), null, bad);
  }
  assert.deepEqual(loginOf(parseSource("smb://me@mac/M")!), { host: "mac", user: "me" });
  assert.equal(loginOf({ dir: { path: "/x" } }), null);
});

test("paths go in and up, and read as a trail", () => {
  assert.equal(inside("", "Music"), "Music");
  assert.equal(inside("Music", "Enya"), "Music/Enya");
  assert.equal(up("Music/Enya"), "Music");
  assert.equal(up("Music"), "");
  assert.equal(trail("Music/Enya", "Music share"), "Music share › Music › Enya");
  assert.equal(trail("", "Music share"), "Music share");
});

test("entries and progress read plainly", () => {
  assert.equal(describe({ name: "a.mp3", kind: "audio", size: 4_500_000 }), "Music · 4.3 MB");
  assert.equal(describe({ name: "x.zip", kind: "other", size: 10 }), "10 B · not imported");
  const s: ImportStatus = { running: true, total: 30, done: 10, imported: 7, skipped: 3, failed: 0, ignored: 0 };
  assert.equal(progress(s), "Importing 10 of 30 · 7 imported, 3 already here");
  assert.equal(progress({ ...s, running: false, done: 30, imported: 27, failed: 1 }), "Done: 30 files · 27 imported, 3 already here, 1 failed");
  assert.equal(progress({ running: false, total: 0, done: 0, imported: 0, skipped: 0, failed: 0, ignored: 0 }), "");
});
