// The document list's logic: names, tags, and what a search and filters show.

import assert from "node:assert/strict";
import { test } from "node:test";

import type { Doc } from "@idealos/sdk";
import { allTags, formatNamed, formatOf, nextAfter, parseTags, shown, tagsOf, titleOf } from "../src/library.ts";

const doc = (id: string, data: Record<string, unknown> = {}): Doc => ({ id, type: "TextDocument", created: 1, updated: 1, data });

test("documents without a title, format or tags get sensible ones", () => {
  assert.equal(titleOf(doc("a")), "Untitled");
  assert.equal(titleOf(doc("a", { title: "   " })), "Untitled");
  assert.equal(titleOf(doc("a", { title: " Letters " })), "Letters");
  assert.equal(formatOf(doc("a")), "plain");
  assert.equal(formatOf(doc("a", { format: "markdown" })), "markdown");
  assert.equal(formatOf(doc("a", { format: "rtf" })), "plain");
  assert.deepEqual(tagsOf(doc("a", { tags: ["x", 3, "", "y"] })), ["x", "y"]);
  assert.deepEqual(tagsOf(doc("a", { tags: "x" })), []);
  assert.equal(formatNamed("Markdown"), "markdown");
  assert.equal(formatNamed("Any format"), undefined);
});

test("typed tags are trimmed, without blanks or repeats; the list's tags are sorted", () => {
  assert.deepEqual(parseTags(" draft, letters,,draft , "), ["draft", "letters"]);
  assert.deepEqual(parseTags(""), []);
  assert.deepEqual(allTags([doc("a", { tags: ["b", "a"] }), doc("b", { tags: ["c", "a"] })]), ["a", "b", "c"]);
});

test("search results, format and tag all narrow the list, which keeps its order", () => {
  const docs = [
    doc("1", { format: "markdown", tags: ["draft"] }),
    doc("2", { tags: ["draft"] }),
    doc("3", { format: "markdown" }),
  ];
  const ids = (f: Parameters<typeof shown>[1]) => shown(docs, f).map((d) => d.id);
  assert.deepEqual(ids({ found: null, format: null, tag: null }), ["1", "2", "3"]);
  assert.deepEqual(ids({ found: new Set(["3", "1"]), format: null, tag: null }), ["1", "3"]);
  assert.deepEqual(ids({ found: null, format: "markdown", tag: null }), ["1", "3"]);
  assert.deepEqual(ids({ found: null, format: "plain", tag: "draft" }), ["2"]);
  assert.deepEqual(ids({ found: new Set(["2"]), format: "markdown", tag: null }), []);
});

test("after a delete, the next document is chosen, else the one before", () => {
  const list = [doc("a"), doc("b"), doc("c")];
  assert.equal(nextAfter(list, "b"), "c");
  assert.equal(nextAfter(list, "c"), "b");
  assert.equal(nextAfter([doc("a")], "a"), null);
  assert.equal(nextAfter(list, "gone"), "a");
});
