// Text logic for the editor, apart from the widgets so it can be tested: counts,
// the caret's line and column, finding matches, and the change between two texts.
//
// Positions are character indexes counted in code points, as the compositor's
// text widgets and db's text count them (JavaScript strings count UTF-16 units,
// so "é" is one either way but an emoji is two units and one character).

/** Words (runs of non-space) and characters in `text`. */
export function counts(text: string): { words: number; chars: number } {
  return { words: text.match(/\S+/gu)?.length ?? 0, chars: [...text].length };
}

/** The 1-based line and column of character `at` in `text`. */
export function lineCol(text: string, at: number): { line: number; col: number } {
  const before = [...text].slice(0, at);
  const lastBreak = before.lastIndexOf("\n");
  const line = before.filter((c) => c === "\n").length + 1;
  return { line, col: before.length - lastBreak };
}

/** A stretch of text, [start, end) in characters. */
export type Range = readonly [number, number];

export interface FindOptions {
  matchCase: boolean;
  wholeWord: boolean;
}

function escape(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Every match of `query` in `text`, in order, as character ranges. */
export function findAll(text: string, query: string, o: FindOptions): Range[] {
  if (query === "") return [];
  const edge = "[\\p{L}\\p{N}_]";
  const body = escape(query);
  const pattern = o.wholeWord ? `(?<!${edge})${body}(?!${edge})` : body;
  const re = new RegExp(pattern, o.matchCase ? "gu" : "giu");
  const found: Range[] = [];
  // Matches come in order, so code points are counted as the search goes.
  let unit = 0;
  let chars = 0;
  const charsTo = (target: number) => {
    while (unit < target) {
      const cp = text.codePointAt(unit)!;
      unit += cp > 0xffff ? 2 : 1;
      chars += 1;
    }
    return chars;
  };
  for (const m of text.matchAll(re)) {
    const start = charsTo(m.index);
    const end = charsTo(m.index + m[0].length);
    found.push([start, end]);
  }
  return found;
}

/** The index of the match that is exactly `selection`, or -1. */
export function matchIndex(matches: readonly Range[], selection: Range): number {
  const [a, b] = [Math.min(...selection), Math.max(...selection)];
  return matches.findIndex(([s, e]) => s === a && e === b);
}

/** The match to go to from `selection`: the next one (wrapping round), or with
 * `back` the one before. From a selected match, its neighbour; otherwise the
 * first match after the selection (or the last before it). -1 when there are none. */
export function stepMatch(matches: readonly Range[], selection: Range, back: boolean): number {
  const n = matches.length;
  if (n === 0) return -1;
  const at = matchIndex(matches, selection);
  if (at >= 0) return back ? (at + n - 1) % n : (at + 1) % n;
  const [a, b] = [Math.min(...selection), Math.max(...selection)];
  if (back) {
    for (let i = n - 1; i >= 0; i--) if (matches[i][1] <= a) return i;
    return n - 1;
  }
  const i = matches.findIndex(([s]) => s >= b);
  return i >= 0 ? i : 0;
}

/** The single splice that turns `from` into `to`: the stretch between their
 * longest common prefix and suffix. `at` and `delete` count characters. */
export function changeBetween(from: string, to: string): { at: number; delete: number; insert: string; removed: string } {
  const a = [...from];
  const b = [...to];
  let prefix = 0;
  while (prefix < a.length && prefix < b.length && a[prefix] === b[prefix]) prefix++;
  let suffix = 0;
  while (suffix < a.length - prefix && suffix < b.length - prefix && a[a.length - 1 - suffix] === b[b.length - 1 - suffix]) suffix++;
  return {
    at: prefix,
    delete: a.length - prefix - suffix,
    insert: b.slice(prefix, b.length - suffix).join(""),
    removed: a.slice(prefix, a.length - suffix).join(""),
  };
}

/** `text` with characters [at, at + del) replaced by `insert`. */
export function spliced(text: string, at: number, del: number, insert: string): string {
  const chars = [...text];
  return chars.slice(0, at).join("") + insert + chars.slice(at + del).join("");
}

/** A short piece of `text` around characters [start, end), for showing a change in context. */
export function around(text: string, start: number, end: number, room = 24): { before: string; after: string } {
  const chars = [...text];
  const before = chars.slice(Math.max(0, start - room), start).join("");
  const after = chars.slice(end, end + room).join("");
  return { before: (start > room ? "…" : "") + before, after: after + (end + room < chars.length ? "…" : "") };
}

/** \`insert\`, with line breaks added so it sits on a line of its own when it goes
 * in over characters [start, end) of \`text\` (an image, which Markdown shows only
 * on its own line). */
export function onOwnLine(text: string, start: number, end: number, insert: string): string {
  const chars = [...text];
  const before = start > 0 && chars[start - 1] !== "\n" ? "\n" : "";
  const after = end < chars.length && chars[end] !== "\n" ? "\n" : "";
  return before + insert + after;
}
