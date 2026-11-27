// Counting, finding and diffing text, in characters (code points).

import assert from "node:assert/strict";
import { test } from "node:test";

import { around, changeBetween, counts, findAll, lineCol, matchIndex, onOwnLine, spliced, stepMatch } from "../src/text.ts";

test("words and characters are counted, an emoji as one character", () => {
  assert.deepEqual(counts(""), { words: 0, chars: 0 });
  assert.deepEqual(counts("  two  words\n"), { words: 2, chars: 13 });
  assert.deepEqual(counts("hi 👋"), { words: 2, chars: 4 });
});

test("the caret's line and column start at 1", () => {
  const t = "one\ntwo 👋 x\nthree";
  assert.deepEqual(lineCol(t, 0), { line: 1, col: 1 });
  assert.deepEqual(lineCol(t, 3), { line: 1, col: 4 });
  assert.deepEqual(lineCol(t, 4), { line: 2, col: 1 });
  assert.deepEqual(lineCol(t, 10), { line: 2, col: 7 }, "past the emoji, counted as one");
  assert.deepEqual(lineCol(t, 99), { line: 3, col: 6 });
});

test("find honours match case and whole word, and counts characters", () => {
  const t = "Cat cat 👋 cat-like scatter CAT";
  const any = { matchCase: false, wholeWord: false };
  assert.deepEqual(findAll(t, "cat", any).length, 5);
  assert.deepEqual(findAll(t, "cat", { matchCase: true, wholeWord: false }), [[4, 7], [10, 13], [20, 23]]);
  assert.deepEqual(findAll(t, "cat", { matchCase: false, wholeWord: true }), [[0, 3], [4, 7], [10, 13], [27, 30]]);
  assert.deepEqual(findAll(t, "", any), []);
  assert.deepEqual(findAll("a.b axb", ".", any), [[1, 2]], "the query is text, not a pattern");
});

test("stepping through matches goes from the selection and wraps round", () => {
  const m = [[0, 3], [10, 13], [20, 23]] as const;
  assert.equal(matchIndex(m, [13, 10]), 1, "either direction");
  assert.equal(stepMatch(m, [10, 13], false), 2);
  assert.equal(stepMatch(m, [20, 23], false), 0, "wraps");
  assert.equal(stepMatch(m, [0, 3], true), 2);
  assert.equal(stepMatch(m, [5, 5], false), 1, "the first after the caret");
  assert.equal(stepMatch(m, [15, 15], true), 1, "the last before it");
  assert.equal(stepMatch(m, [25, 25], false), 0);
  assert.equal(stepMatch([], [0, 0], false), -1);
});

test("the change between two texts is one splice that makes the second", () => {
  const c = changeBetween("the quick fox", "the slow fox");
  assert.deepEqual(c, { at: 4, delete: 5, insert: "slow", removed: "quick" });
  assert.equal(spliced("the quick fox", c.at, c.delete, c.insert), "the slow fox");
  assert.deepEqual(changeBetween("aaa", "aaaa"), { at: 3, delete: 0, insert: "a", removed: "" });
  assert.deepEqual(changeBetween("👋 hi", "👋 ho"), { at: 3, delete: 1, insert: "o", removed: "i" });
  assert.deepEqual(around("0123456789", 4, 6, 2), { before: "…23", after: "67…" });
});

test("an image goes in on a line of its own", () => {
  assert.equal(onOwnLine("ab", 0, 0, "X"), "X\n", "before text on the line");
  assert.equal(onOwnLine("ab", 1, 1, "X"), "\nX\n", "mid-line");
  assert.equal(onOwnLine("ab", 2, 2, "X"), "\nX", "at the end");
  assert.equal(onOwnLine("a\n\nb", 2, 2, "X"), "X", "on an empty line already");
  assert.equal(onOwnLine("", 0, 0, "X"), "X");
});
