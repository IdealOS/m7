// The document list's logic, kept apart from the widgets so it can be tested:
// what a document is called, its format and tags, and which documents a search
// and the filters leave showing.

import type { Doc } from "@idealos/sdk";

export const TYPE = "TextDocument";

export type Format = "plain" | "markdown";

/** Each format and what it's called on screen. */
export const FORMATS: readonly (readonly [Format, string])[] = [
  ["plain", "Plain text"],
  ["markdown", "Markdown"],
];

export function formatName(f: Format): string {
  return FORMATS.find(([k]) => k === f)?.[1] ?? f;
}

/** The format called `name` on screen, if there is one. */
export function formatNamed(name: string): Format | undefined {
  return FORMATS.find(([, n]) => n === name)?.[0];
}

export function titleOf(d: Doc): string {
  const t = typeof d.data.title === "string" ? d.data.title.trim() : "";
  return t || "Untitled";
}

export function formatOf(d: Doc): Format {
  return d.data.format === "markdown" ? "markdown" : "plain";
}

export function tagsOf(d: Doc): string[] {
  const tags = d.data.tags;
  return Array.isArray(tags) ? tags.filter((t): t is string => typeof t === "string" && t !== "") : [];
}

/** Every tag any of `docs` has, sorted. */
export function allTags(docs: readonly Doc[]): string[] {
  return [...new Set(docs.flatMap(tagsOf))].sort((a, b) => a.localeCompare(b));
}

/** Tags typed as a list separated by commas: trimmed, without blanks or repeats. */
export function parseTags(text: string): string[] {
  return [...new Set(text.split(",").map((t) => t.trim()).filter((t) => t !== ""))];
}

export interface Filter {
  /** The ids a search found, or null when nothing is being searched for. */
  found: ReadonlySet<string> | null;
  format: Format | null;
  tag: string | null;
}

/** The documents left showing, in the order they came (most recent first). */
export function shown(docs: readonly Doc[], f: Filter): Doc[] {
  return docs.filter(
    (d) =>
      (f.found === null || f.found.has(d.id)) &&
      (f.format === null || formatOf(d) === f.format) &&
      (f.tag === null || tagsOf(d).includes(f.tag)),
  );
}

/** What to select once `id` is gone from `list`: the one after it, else before. */
export function nextAfter(list: readonly Doc[], id: string): string | null {
  const i = list.findIndex((d) => d.id === id);
  if (i < 0) return list[0]?.id ?? null;
  return (list[i + 1] ?? list[i - 1])?.id ?? null;
}
