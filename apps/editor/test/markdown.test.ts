// The preview's Markdown: blocks, inline marks taken out, links kept.

import assert from "node:assert/strict";
import { test } from "node:test";

import { blobRefs, docLinks, inline, parse } from "../src/markdown.ts";

const ID = "01a21a7a-007e-74b5-9e52-44e947d4b90a";
const HASH = "a".repeat(64);

test("inline marks come out and links are listed", () => {
  assert.deepEqual(inline("some **bold**, _soft_ and `code`"), { text: "some bold, soft and code", links: [], strong: false });
  assert.deepEqual(inline(`see [the plan](doc:${ID}) or [web](https://x.org)`).links, [
    { text: "the plan", target: `doc:${ID}` },
    { text: "web", target: "https://x.org" },
  ]);
  assert.equal(inline("**all of it**").strong, true);
  assert.equal(inline("snake_case_name stays").text, "snake_case_name stays");
});

test("blocks: headings, paragraphs, lists, quotes, code, rules, images", () => {
  const md = [
    "# Title #",
    "",
    "A paragraph",
    "over two lines.",
    "",
    "- one",
    "  - nested",
    "2) two",
    "> quoted",
    "> more",
    "",
    "```",
    "let x = 1;",
    "  # not a heading",
    "```",
    "---",
    `![a cat](blob:${HASH})`,
  ].join("\n");
  assert.deepEqual(parse(md), [
    { kind: "heading", level: 1, text: "Title", links: [] },
    { kind: "paragraph", text: "A paragraph over two lines.", links: [], strong: false },
    { kind: "item", marker: "•", depth: 0, text: "one", links: [] },
    { kind: "item", marker: "•", depth: 1, text: "nested", links: [] },
    { kind: "item", marker: "2.", depth: 0, text: "two", links: [] },
    { kind: "quote", text: "quoted more", links: [] },
    { kind: "code", text: "let x = 1;\n  # not a heading" },
    { kind: "rule" },
    { kind: "image", alt: "a cat", blob: HASH },
  ]);
  assert.deepEqual(parse(""), []);
});

test("document links and images are found once each", () => {
  const md = `[a](doc:${ID}) and [b](doc:${ID}) ![x](blob:${HASH}) [c](doc:nope)`;
  assert.deepEqual(docLinks(md), [ID]);
  assert.deepEqual(blobRefs(md), [HASH]);
});
