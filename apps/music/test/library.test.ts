// The library: tracks from documents, albums, artists, songs, search, and times.

import assert from "node:assert/strict";
import { test } from "node:test";

import type { AudioStatus, Doc } from "@idealos/sdk";
import { clock, positionAt, totals } from "../src/clock.ts";
import {
  albumKey,
  albumMatches,
  albumsOf,
  artistsOf,
  coverColor,
  fold,
  genresOf,
  initials,
  matches,
  queueFrom,
  shelfMatches,
  songsOf,
  sortName,
  trackOf,
  tracksOf,
  UNKNOWN_ALBUM,
  VARIOUS,
  words,
  yearsOf,
  type Track,
} from "../src/library.ts";

let next = 0;
function doc(data: Record<string, unknown>): Doc {
  next++;
  return { id: `id-${String(next).padStart(3, "0")}`, type: "Track", created: next, updated: next, data };
}

function t(fields: Partial<Track> & { title: string }): Track {
  return { id: `t-${fields.title}-${fields.artist ?? ""}-${fields.trackNumber ?? ""}`, ...fields };
}

test("a document becomes a track, keeping only well-formed fields", () => {
  const d = doc({
    title: "Caribbean Blue",
    artist: "Enya",
    album: "Shepherd Moons",
    trackNumber: 2,
    duration: 236.1,
    year: "1991", // not a number: dropped
    audio: { $blob: "aa" },
    artwork: { $blob: "cc" },
    source: { from: "dir:/Music", path: "Enya/Shepherd Moons/02 Caribbean Blue.m4a" },
  });
  assert.deepEqual(trackOf(d), {
    id: d.id,
    title: "Caribbean Blue",
    artist: "Enya",
    album: "Shepherd Moons",
    trackNumber: 2,
    duration: 236.1,
    artwork: "cc",
    folder: "dir:/Music/Enya/Shepherd Moons",
  });
  assert.equal(trackOf(doc({ artist: "No title" })), null);
  assert.equal(trackOf(doc({ title: "  " })), null);
  assert.equal(tracksOf([doc({ title: "a" }), doc({})]).length, 1);
});

test("an album's tracks come in disc, then track order", () => {
  const [a] = albumsOf([
    t({ title: "B", album: "X", artist: "P", discNumber: 2, trackNumber: 1 }),
    t({ title: "A", album: "X", artist: "P", discNumber: 1, trackNumber: 2 }),
    t({ title: "C", album: "X", artist: "P", discNumber: 1, trackNumber: 1 }),
    t({ title: "D", album: "X", artist: "P" }), // no number: after the numbered ones
  ]);
  assert.deepEqual(
    a.tracks.map((x) => x.title),
    ["C", "A", "D", "B"],
  );
});

test("a soundtrack with no album artist stays one album, by various artists", () => {
  // As imported from a folder: the files share it, though their artists differ.
  const folder = "dir:/Music/Compilations/Garden State";
  const albums = albumsOf([
    t({ title: "The Only Living Boy in New York", artist: "Simon & Garfunkel", album: "Garden State", folder }),
    t({ title: "Let Go", artist: "Frou Frou", album: "Garden State", folder }),
    // Another album of the same name, from elsewhere, stays apart.
    t({ title: "Other", artist: "Someone", album: "Garden State", folder: "dir:/Music/Someone/Garden State" }),
  ]);
  assert.equal(albums.length, 2);
  const soundtrack = albums.find((a) => a.tracks.length === 2)!;
  assert.equal(soundtrack.artist, VARIOUS);
  // The album artist, when tagged, says whose album it is wherever the files were.
  const tagged = albumsOf([
    t({ title: "a", artist: "X", albumArtist: "Various", album: "Hits", folder: "f1" }),
    t({ title: "b", artist: "Y", albumArtist: "various", album: "hits", folder: "f2" }),
  ]);
  assert.equal(tagged.length, 1);
  assert.equal(tagged[0].artist, "Various");
});

test("tracks with no album go together under the artist's unknown album", () => {
  const albums = albumsOf([t({ title: "Lord of the Ring", artist: "Enya" }), t({ title: "x", artist: "Enya" })]);
  assert.equal(albums.length, 1);
  assert.equal(albums[0].title, UNKNOWN_ALBUM);
  assert.equal(albumKey(t({ title: "y", artist: "Enya" })), albums[0].key);
});

test("albums sort by artist (ignoring The), then year, then title", () => {
  const albums = albumsOf([
    t({ title: "1", album: "Tusk", artist: "Fleetwood Mac", year: 1979 }),
    t({ title: "2", album: "Rumours", artist: "Fleetwood Mac", year: 1977 }),
    t({ title: "3", album: "Abbey Road", artist: "The Beatles", year: 1969 }),
    t({ title: "4", album: "Tidal", artist: "Fiona Apple", year: 1996 }),
  ]);
  assert.deepEqual(
    albums.map((a) => a.title),
    ["Abbey Road", "Tidal", "Rumours", "Tusk"],
  );
  assert.equal(sortName("The Beatles"), "beatles");
  assert.equal(sortName("A-ha"), "a-ha");
});

test("artists group their albums; various artists come last", () => {
  const albums = albumsOf([
    t({ title: "a", album: "Tidal", artist: "Fiona Apple" }),
    t({ title: "b", album: "When the Pawn", artist: "fiona apple" }),
    t({ title: "c", album: "Mix", artist: "A", folder: "f" }),
    t({ title: "d", album: "Mix", artist: "B", folder: "f" }),
    t({ title: "e", album: "Demon Days", artist: "Gorillaz" }),
  ]);
  const artists = artistsOf(albums);
  assert.deepEqual(
    artists.map((a) => [a.name, a.albums.length]),
    [
      ["Fiona Apple", 2],
      ["Gorillaz", 1],
      [VARIOUS, 1],
    ],
  );
});

test("genres hold the albums with a track in them, one genre however it's spelled", () => {
  const albums = albumsOf([
    t({ title: "Orinoco Flow", artist: "Enya", album: "Watermark", genre: "New Age" }),
    t({ title: "Storms in Africa", artist: "Enya", album: "Watermark", genre: "new age" }),
    t({ title: "Criminal", artist: "Fiona Apple", album: "Tidal", genre: "Rock" }),
    t({ title: "Shadowboxer", artist: "Fiona Apple", album: "Tidal", genre: "Pop" }),
    t({ title: "Untagged", artist: "Nobody", album: "Nothing" }),
    t({ title: "Électro", artist: "Air", album: "Moon Safari", genre: " Électronique " }),
  ]);
  const genres = genresOf(albums);
  assert.deepEqual(
    genres.map((g) => [g.name, g.albums.map((a) => a.title)]),
    [
      ["Électronique", ["Moon Safari"]],
      ["New Age", ["Watermark"]],
      ["Pop", ["Tidal"]],
      ["Rock", ["Tidal"]],
    ],
    "by name without accents; an album once per genre, and on each of its genres; untagged on none",
  );
});

test("years hold their albums, newest first", () => {
  const albums = albumsOf([
    t({ title: "a", artist: "Enya", album: "Shepherd Moons", year: 1991 }),
    t({ title: "b", artist: "Enya", album: "Watermark", year: 1988 }),
    t({ title: "c", artist: "Fiona Apple", album: "When the Pawn", year: 1999 }),
    t({ title: "d", artist: "Air", album: "Premiers Symptômes", year: 1999 }),
    t({ title: "e", artist: "Nobody", album: "Undated" }),
  ]);
  assert.deepEqual(
    yearsOf(albums).map((y) => [y.name, y.albums.map((a) => a.title)]),
    [
      ["1999", ["Premiers Symptômes", "When the Pawn"]],
      ["1991", ["Shepherd Moons"]],
      ["1988", ["Watermark"]],
    ],
    "albums in a year keep the library's order; undated ones are on no year",
  );
});

test("a search finds a shelf by its name or by an album on it", () => {
  const [newAge] = genresOf(albumsOf([t({ title: "Orinoco Flow", artist: "Enya", album: "Watermark", genre: "New Age" })]));
  assert.equal(shelfMatches(newAge, words("new age")), true, "its name");
  assert.equal(shelfMatches(newAge, words("orinoco")), true, "a track on one of its albums");
  assert.equal(shelfMatches(newAge, words("rock")), false);
  assert.equal(shelfMatches(newAge, []), true);
});

test("search matches every word, without case or accents", () => {
  const lothlorien = t({ title: "Lothlórien", artist: "Enya", album: "Shepherd Moons" });
  assert.equal(matches(lothlorien, words("lothlorien")), true);
  assert.equal(matches(lothlorien, words("  ENYA  moons ")), true);
  assert.equal(matches(lothlorien, words("enya tidal")), false);
  assert.equal(matches(lothlorien, []), true);
  assert.equal(fold("Exposé"), "expose");
  const [album] = albumsOf([lothlorien]);
  assert.equal(albumMatches(album, words("shepherd")), true);
  assert.equal(albumMatches(album, words("lothlorien")), true); // through a track
  assert.equal(albumMatches(album, words("demon")), false);
});

test("songs sort by title, then artist", () => {
  const songs = songsOf([
    t({ title: "the Chain", artist: "Fleetwood Mac" }),
    t({ title: "Angeles", artist: "Enya" }),
    t({ title: "Angeles", artist: "Elliott Smith" }),
  ]);
  assert.deepEqual(
    songs.map((s) => `${s.title}/${s.artist}`),
    ["Angeles/Elliott Smith", "Angeles/Enya", "the Chain/Fleetwood Mac"],
  );
});

test("a queue longer than audiod takes starts at the song chosen", () => {
  const ids = ["a", "b", "c", "d", "e"];
  assert.deepEqual(queueFrom(ids, 2, 10), { tracks: ids, start: 2 });
  assert.deepEqual(queueFrom(ids, 3, 3), { tracks: ["d", "e", "a"], start: 0 });
});

test("covers without pictures get initials and a steady colour", () => {
  assert.equal(initials("Shepherd Moons"), "SM");
  assert.equal(initials("When The Pawn..."), "WT");
  assert.equal(initials("(What's the Story) Morning Glory?"), "WT");
  assert.equal(initials("!!!"), "♪");
  assert.match(coverColor("x"), /^#[0-9a-f]{6}$/);
  assert.equal(coverColor("Demon Days"), coverColor("Demon Days"));
  // Dark enough for white letters: no channel very bright.
  for (const k of ["a", "b", "Tidal", "Rumours", "Holst"]) {
    const c = coverColor(k);
    const [r, g, b] = [1, 3, 5].map((i) => parseInt(c.slice(i, i + 2), 16));
    assert.ok(0.2126 * r + 0.7152 * g + 0.0722 * b < 140, `${k}: ${c}`);
  }
});

test("times read as people write them", () => {
  assert.equal(clock(0), "0:00");
  assert.equal(clock(236.9), "3:56");
  assert.equal(clock(3723), "1:02:03");
  assert.equal(clock(undefined), "–:––");
  assert.equal(totals([236, 200, undefined]), "3 songs, 7 min");
  assert.equal(totals([3780]), "1 song, 1 hr 3 min");
  assert.equal(totals([]), "0 songs");
});

test("the position moves on while playing, and never past the end", () => {
  const s: Pick<AudioStatus, "state" | "position" | "at" | "duration"> = { state: "playing", position: 10, at: 1000, duration: 12 };
  assert.equal(positionAt(s, 2500), 11.5);
  assert.equal(positionAt(s, 99_000), 12);
  assert.equal(positionAt({ ...s, state: "paused" }, 2500), 10);
  assert.equal(positionAt({ ...s, duration: null }, 99_000), 108);
  assert.equal(positionAt(s, 0), 10); // a clock behind audiod's never goes back
});
