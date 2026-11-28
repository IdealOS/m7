// The Music app's library, apart from the widgets so it can be tested: Track
// documents read into tracks, grouped into albums and artists, sorted the way
// people expect, and searched.

import type { Doc } from "@idealos/sdk";

export const TYPE = "Track";
/** What a track without an album, or an album without an artist, shows. */
export const UNKNOWN_ALBUM = "Unknown album";
export const UNKNOWN_ARTIST = "Unknown artist";
/** An album whose tracks are by different artists, with no album artist to say. */
export const VARIOUS = "Various artists";

export interface Track {
  id: string;
  title: string;
  artist?: string;
  album?: string;
  albumArtist?: string;
  trackNumber?: number;
  discNumber?: number;
  year?: number;
  duration?: number;
  genre?: string;
  /** Stars, 1 to 5; missing when unrated. */
  rating?: number;
  /** The cover's blob hash. */
  artwork?: string;
  /** The folder the file was imported from, which groups an album's tracks when
   * their tags don't say whose album it is. */
  folder?: string;
}

export interface Album {
  /** Unique: which tracks make one album (see `albumKey`). */
  key: string;
  title: string;
  artist: string;
  year?: number;
  artwork?: string;
  /** In disc and track order. */
  tracks: Track[];
}

export interface Artist extends Shelf {
  /** In year, then title order. */
  albums: Album[];
}

const str = (v: unknown) => (typeof v === "string" && v.trim() !== "" ? v : undefined);
const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : undefined);
const blob = (v: unknown) =>
  typeof v === "object" && v !== null && typeof (v as { $blob?: unknown }).$blob === "string"
    ? (v as { $blob: string }).$blob
    : undefined;

/** A Track document as a track, or null when it has no title. */
export function trackOf(doc: Doc): Track | null {
  const d = doc.data;
  const title = str(d.title);
  if (title === undefined) return null;
  const source = typeof d.source === "object" && d.source !== null ? (d.source as Record<string, unknown>) : {};
  const path = str(source.path);
  const t: Track = { id: doc.id, title };
  const fields: Partial<Track> = {
    artist: str(d.artist),
    album: str(d.album),
    albumArtist: str(d.albumArtist),
    trackNumber: num(d.trackNumber),
    discNumber: num(d.discNumber),
    year: num(d.year),
    duration: num(d.duration),
    genre: str(d.genre),
    rating: typeof d.rating === "number" && Number.isInteger(d.rating) && d.rating >= 1 && d.rating <= 5 ? d.rating : undefined,
    artwork: blob(d.artwork),
    folder: path?.includes("/") ? `${str(source.from) ?? ""}/${path.slice(0, path.lastIndexOf("/"))}` : undefined,
  };
  for (const [k, v] of Object.entries(fields)) if (v !== undefined) (t as unknown as Record<string, unknown>)[k] = v;
  return t;
}

export function tracksOf(docs: Doc[]): Track[] {
  return docs.map(trackOf).filter((t): t is Track => t !== null);
}

/**
 * Which album a track belongs to. The album artist says whose album it is when
 * it's tagged; otherwise the folder the files came from (a soundtrack's tracks
 * share one, though their artists differ), and failing that the track's artist.
 */
export function albumKey(t: Track): string {
  const title = fold(t.album ?? UNKNOWN_ALBUM);
  const whose = t.albumArtist ? `a:${fold(t.albumArtist)}` : t.folder ? `f:${t.folder}` : `t:${fold(t.artist ?? "")}`;
  return `${title}\n${whose}`;
}

/** Text for comparing and matching: lower case, without accents. */
export function fold(text: string): string {
  return text.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase().trim();
}

/** How a name sorts: folded, and without a leading "The" or "A". */
export function sortName(text: string): string {
  return fold(text).replace(/^(the|a|an)\s+/, "");
}

const byName = (a: string, b: string) => {
  const [x, y] = [sortName(a), sortName(b)];
  return x < y ? -1 : x > y ? 1 : 0;
};

function inAlbumOrder(a: Track, b: Track): number {
  return (
    (a.discNumber ?? 1) - (b.discNumber ?? 1) ||
    (a.trackNumber ?? Infinity) - (b.trackNumber ?? Infinity) ||
    byName(a.title, b.title) ||
    (a.id < b.id ? -1 : 1)
  );
}

/** The library's albums, by artist, then year, then title. */
export function albumsOf(tracks: Track[]): Album[] {
  const groups = new Map<string, Track[]>();
  for (const t of tracks) {
    const k = albumKey(t);
    const g = groups.get(k);
    if (g) g.push(t);
    else groups.set(k, [t]);
  }
  const albums: Album[] = [];
  for (const [key, ts] of groups) {
    ts.sort(inAlbumOrder);
    const first = ts[0];
    const artists = new Set(ts.map((t) => t.artist).filter((a) => a !== undefined));
    const album: Album = {
      key,
      title: first.album ?? UNKNOWN_ALBUM,
      artist:
        ts.find((t) => t.albumArtist)?.albumArtist ??
        (artists.size > 1 ? VARIOUS : (first.artist ?? UNKNOWN_ARTIST)),
      tracks: ts,
    };
    const year = ts.map((t) => t.year).find((y) => y !== undefined);
    if (year !== undefined) album.year = year;
    const artwork = ts.map((t) => t.artwork).find((a) => a !== undefined);
    if (artwork !== undefined) album.artwork = artwork;
    albums.push(album);
  }
  return albums.sort(
    (a, b) => byName(a.artist, b.artist) || (a.year ?? 0) - (b.year ?? 0) || byName(a.title, b.title) || (a.key < b.key ? -1 : 1),
  );
}

/** The artists whose albums these are, by name. Various-artist albums come last. */
export function artistsOf(albums: Album[]): Artist[] {
  const byArtist = new Map<string, Artist>();
  for (const al of albums) {
    const k = fold(al.artist);
    const a = byArtist.get(k);
    if (a) a.albums.push(al);
    else byArtist.set(k, { name: al.artist, albums: [al] });
  }
  return [...byArtist.values()].sort((a, b) =>
    a.name === VARIOUS ? 1 : b.name === VARIOUS ? -1 : byName(a.name, b.name),
  );
}

/** Albums gathered under a name: an artist's, a genre's or a year's. */
export interface Shelf {
  name: string;
  albums: Album[];
}

/**
 * The genres of the library's tracks, by name, each with the albums that have a
 * track in it (an album of several genres is on each). Spellings that differ only
 * in case or accents are one genre, named as it's first spelled.
 */
export function genresOf(albums: Album[]): Shelf[] {
  const byGenre = new Map<string, Shelf>();
  for (const al of albums) {
    const seen = new Set<string>();
    for (const t of al.tracks) {
      if (t.genre === undefined) continue;
      const k = fold(t.genre);
      if (seen.has(k)) continue;
      seen.add(k);
      const g = byGenre.get(k);
      if (g) g.albums.push(al);
      else byGenre.set(k, { name: t.genre.trim(), albums: [al] });
    }
  }
  return [...byGenre.values()].sort((a, b) => byName(a.name, b.name));
}

/** The years albums came out, newest first. Albums with no year aren't on one. */
export function yearsOf(albums: Album[]): Shelf[] {
  const byYear = new Map<number, Album[]>();
  for (const al of albums) {
    if (al.year === undefined) continue;
    const y = byYear.get(al.year);
    if (y) y.push(al);
    else byYear.set(al.year, [al]);
  }
  return [...byYear.entries()].sort(([a], [b]) => b - a).map(([year, albums]) => ({ name: String(year), albums }));
}

/** Whether every word is in the shelf's name, or one of its albums matches. */
export function shelfMatches(s: Shelf, ws: string[]): boolean {
  if (ws.length === 0) return true;
  const name = fold(s.name);
  return ws.every((w) => name.includes(w)) || s.albums.some((al) => albumMatches(al, ws));
}

/** Every track, by title, then artist. */
export function songsOf(tracks: Track[]): Track[] {
  return [...tracks].sort(
    (a, b) => byName(a.title, b.title) || byName(a.artist ?? "", b.artist ?? "") || (a.id < b.id ? -1 : 1),
  );
}

/** The words of a search, folded; empty matches everything. */
export function words(search: string): string[] {
  return fold(search).split(/\s+/).filter(Boolean);
}

/** Whether every word is in the track's title, artist or album. */
export function matches(t: Track, ws: string[]): boolean {
  if (ws.length === 0) return true;
  const text = fold([t.title, t.artist, t.albumArtist, t.album].filter(Boolean).join(" "));
  return ws.every((w) => text.includes(w));
}

/** Whether every word is in the album's title or artist, or one of its tracks matches. */
export function albumMatches(a: Album, ws: string[]): boolean {
  if (ws.length === 0) return true;
  const text = fold(`${a.title} ${a.artist}`);
  return ws.every((w) => text.includes(w)) || a.tracks.some((t) => matches(t, ws));
}

/** Up to two letters for a cover that has no picture. */
export function initials(title: string): string {
  const letters = title
    .split(/\s+/)
    .map((w) => Array.from(w.replace(/^[^\p{L}\p{N}]+/u, ""))[0])
    .filter((c): c is string => c !== undefined);
  return letters.slice(0, 2).join("").toUpperCase() || "♪";
}

/** A colour for a cover with no picture: steady for an album, and dark enough
 * for white letters on it. */
export function coverColor(key: string): string {
  let h = 0;
  for (const c of key) h = (h * 31 + (c.codePointAt(0) ?? 0)) >>> 0;
  const hue = h % 360;
  return hsl(hue, 0.45, 0.38);
}

function hsl(h: number, s: number, l: number): string {
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) => {
    const k = (n + h / 30) % 12;
    const v = l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
    return Math.round(v * 255)
      .toString(16)
      .padStart(2, "0");
  };
  return `#${f(0)}${f(8)}${f(4)}`;
}

/**
 * What to queue to play `ids` from `index`, within audiod's limit on a queue:
 * all of them if they fit, or else `max` of them starting there (wrapping round
 * to the beginning), so the one chosen always plays first.
 */
export function queueFrom(ids: string[], index: number, max: number): { tracks: string[]; start: number } {
  if (ids.length <= max) return { tracks: ids, start: index };
  const tracks = Array.from({ length: max }, (_, i) => ids[(index + i) % ids.length]);
  return { tracks, start: 0 };
}
