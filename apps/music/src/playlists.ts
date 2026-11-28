// Playlists and ratings, apart from the widgets so they can be tested.
//
// A playlist is a `Playlist` document: a name and a list of links to `Track`s,
// in the listener's order. A smart playlist holds a rule instead (`smart`), and
// its tracks are a live query, so they keep themselves current as tracks are
// rated, played and imported. A few smart playlists come built in, made from
// the same rules but kept here rather than in the database.

import type { Doc, Query } from "@idealos/sdk";
import { fold, type Track } from "./library.ts";

export const PLAYLIST = "Playlist";
const DAY_MS = 24 * 60 * 60 * 1000;
/** The most tracks one query returns, which is the most a smart playlist holds. */
export const MAX_SMART = 10_000;
export const MAX_RATING = 5;

export type Played = "never" | "ever";
export type Order = "added" | "played" | "plays" | "rating" | "title";

/** Which tracks a smart playlist holds: all the rules given must hold. */
export interface Rule {
  minRating?: number;
  addedWithinDays?: number;
  played?: Played;
  /** An artist's name as the library has it; matches the track's artist or album artist. */
  artist?: string;
  genre?: string;
  order?: Order;
  limit?: number;
}

export interface Playlist {
  /** The document's id, or for a built-in smart playlist, `smart:` and a name. */
  id: string;
  name: string;
  /** Track ids in order, for a playlist that isn't smart. A track may be in it twice. */
  tracks: string[];
  smart?: Rule;
  /** Built in: can't be renamed, changed or deleted. */
  builtIn?: boolean;
}

export const BUILT_IN: Playlist[] = [
  { id: "smart:top", name: "Top rated", tracks: [], builtIn: true, smart: { minRating: 4, order: "rating" } },
  { id: "smart:added", name: "Recently added", tracks: [], builtIn: true, smart: { addedWithinDays: 30, order: "added" } },
  { id: "smart:never", name: "Never played", tracks: [], builtIn: true, smart: { played: "never", order: "title" } },
  { id: "smart:most", name: "Most played", tracks: [], builtIn: true, smart: { played: "ever", order: "plays", limit: 50 } },
  { id: "smart:lately", name: "Recently played", tracks: [], builtIn: true, smart: { played: "ever", order: "played", limit: 50 } },
];

const int = (v: unknown, min: number, max: number) =>
  typeof v === "number" && Number.isInteger(v) && v >= min && v <= max ? v : undefined;
const str = (v: unknown) => (typeof v === "string" && v.trim() !== "" ? v : undefined);
const oneOf = <T extends string>(v: unknown, options: readonly T[]) =>
  options.find((o) => o === v);

const ORDERS = ["added", "played", "plays", "rating", "title"] as const;

/** A smart playlist's rule from a document, keeping only well-formed rules. */
export function ruleOf(v: unknown): Rule {
  const d = typeof v === "object" && v !== null ? (v as Record<string, unknown>) : {};
  const fields: Rule = {
    minRating: int(d.minRating, 1, MAX_RATING),
    addedWithinDays: int(d.addedWithinDays, 1, 100_000),
    played: oneOf(d.played, ["never", "ever"] as const),
    artist: str(d.artist),
    genre: str(d.genre),
    order: oneOf(d.order, ORDERS),
    limit: int(d.limit, 1, MAX_SMART),
  };
  return Object.fromEntries(Object.entries(fields).filter(([, x]) => x !== undefined)) as Rule;
}

/** A Playlist document as a playlist, or null without a name. */
export function playlistOf(doc: Doc): Playlist | null {
  const name = str(doc.data.name);
  if (name === undefined) return null;
  const links = Array.isArray(doc.data.tracks) ? doc.data.tracks : [];
  const tracks = links
    .map((l) => (typeof l === "object" && l !== null ? (l as { $link?: unknown }).$link : undefined))
    .filter((id): id is string => typeof id === "string");
  const p: Playlist = { id: doc.id, name, tracks };
  if (typeof doc.data.smart === "object" && doc.data.smart !== null) p.smart = ruleOf(doc.data.smart);
  return p;
}

/** The listener's playlists, by name. */
export function playlistsOf(docs: Doc[]): Playlist[] {
  return docs
    .map(playlistOf)
    .filter((p): p is Playlist => p !== null)
    .sort((a, b) => fold(a.name).localeCompare(fold(b.name)) || (a.id < b.id ? -1 : 1));
}

/** The data of a playlist document: its name, and its tracks or its rule. */
export function playlistData(name: string, content: { tracks: string[] } | { smart: Rule }): Record<string, unknown> {
  if ("smart" in content) return { name, smart: content.smart };
  return { name, tracks: linksTo(content.tracks) };
}

/** Links to tracks, as a playlist document holds them. */
export function linksTo(ids: string[]): { $link: string }[] {
  return ids.map((id) => ({ $link: id }));
}

/** The query that finds a smart playlist's tracks, as of `now`. */
export function smartQuery(rule: Rule, now: number): Query {
  const all: Record<string, unknown>[] = [];
  if (rule.minRating !== undefined) all.push({ rating: { $gte: rule.minRating } });
  if (rule.addedWithinDays !== undefined) all.push({ $created: { $gte: now - rule.addedWithinDays * DAY_MS } });
  // Never played: no play count, or zero ($not counts a missing field as not matching).
  if (rule.played === "never") all.push({ $not: { playCount: { $gt: 0 } } });
  if (rule.played === "ever") all.push({ playCount: { $gt: 0 } });
  if (rule.artist !== undefined) all.push({ $or: [{ artist: rule.artist }, { albumArtist: rule.artist }] });
  if (rule.genre !== undefined) all.push({ genre: rule.genre });
  const q: Query = { type: "Track", limit: rule.limit ?? MAX_SMART };
  if (all.length === 1) q.where = all[0];
  else if (all.length > 1) q.where = { $and: all };
  // By title is sorted in the app, which folds case and accents as the library does.
  const orderBy: Record<Order, Query["orderBy"]> = {
    added: [["$created", "desc"]],
    played: [["lastPlayed", "desc"]],
    plays: [["playCount", "desc"], ["lastPlayed", "desc"]],
    rating: [["rating", "desc"], ["$created", "desc"]],
    title: undefined,
  };
  const order = orderBy[rule.order ?? "added"];
  if (order) q.orderBy = order;
  return q;
}

const WITHIN: [number, string][] = [
  [7, "in the last week"],
  [30, "in the last 30 days"],
  [90, "in the last 90 days"],
  [365, "in the last year"],
];

const ORDER_WORDS: Record<Order, string> = {
  added: "newest first",
  played: "last played first",
  plays: "most played first",
  rating: "highest rated first",
  title: "by title",
};

/** What a rule picks, in words: "Rated 4★ or more · never played · newest first". */
export function describe(rule: Rule): string {
  const parts: string[] = [];
  if (rule.minRating !== undefined)
    parts.push(rule.minRating === MAX_RATING ? `Rated ${MAX_RATING}★` : `Rated ${rule.minRating}★ or more`);
  if (rule.addedWithinDays !== undefined)
    parts.push(`added ${WITHIN.find(([d]) => d === rule.addedWithinDays)?.[1] ?? `in the last ${rule.addedWithinDays} days`}`);
  if (rule.played === "never") parts.push("never played");
  if (rule.played === "ever") parts.push("played");
  if (rule.artist !== undefined) parts.push(`by ${rule.artist}`);
  if (rule.genre !== undefined) parts.push(rule.genre);
  if (parts.length === 0) parts.push("Every song");
  parts.push(ORDER_WORDS[rule.order ?? "added"]);
  if (rule.limit !== undefined) parts.push(`at most ${rule.limit}`);
  parts[0] = parts[0][0].toUpperCase() + parts[0].slice(1);
  return parts.join(" · ");
}

/** The tracks of a playlist that are still in the library, each with its place
 * in the playlist (which removing and moving use). */
export function entriesOf(p: Playlist, byId: Map<string, Track>): { at: number; track: Track }[] {
  return p.tracks.flatMap((id, at) => {
    const track = byId.get(id);
    return track ? [{ at, track }] : [];
  });
}

/** `ids` without the one at `at`. */
export function removeAt(ids: string[], at: number): string[] {
  return ids.filter((_, i) => i !== at);
}

/** `ids` with the one at `from` moved to `to`. */
export function move(ids: string[], from: number, to: number): string[] {
  if (from < 0 || from >= ids.length || to < 0 || to >= ids.length || from === to) return ids;
  const next = [...ids];
  const [id] = next.splice(from, 1);
  next.splice(to, 0, id);
  return next;
}

/** A name for a new playlist that no other playlist has. */
export function freshName(base: string, taken: string[]): string {
  const names = new Set(taken.map(fold));
  if (!names.has(fold(base))) return base;
  for (let n = 2; ; n++) if (!names.has(fold(`${base} ${n}`))) return `${base} ${n}`;
}

/** The library's name for `typed` among `names`, ignoring case and accents, so a
 * rule matches the tags exactly. */
export function canonical(typed: string, names: string[]): string | undefined {
  const want = fold(typed);
  return want === "" ? undefined : names.find((n) => fold(n) === want);
}

/** A rating as stars: ★★★☆☆, or empty stars when unrated. */
export function stars(rating: number | undefined): string {
  const n = rating ?? 0;
  return "★".repeat(n) + "☆".repeat(MAX_RATING - n);
}

export const NO_RATING = "No rating";
/** What a rating menu offers: five stars down to one, then none. */
export const RATING_OPTIONS = [5, 4, 3, 2, 1].map((n) => "★".repeat(n)).concat(NO_RATING);

/** The rating a choice from `RATING_OPTIONS` gives: its stars, or null for none. */
export function ratingFrom(option: string): number | null {
  const n = Array.from(option).filter((c) => c === "★").length;
  return n >= 1 && n <= MAX_RATING ? n : null;
}
