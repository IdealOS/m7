// Playlists, smart playlists' queries, and ratings.

import assert from "node:assert/strict";
import { test } from "node:test";

import type { Doc } from "@idealos/sdk";
import { trackOf, type Track } from "../src/library.ts";
import {
  BUILT_IN,
  canonical,
  describe,
  entriesOf,
  freshName,
  move,
  NO_RATING,
  playlistData,
  playlistOf,
  playlistsOf,
  RATING_OPTIONS,
  ratingFrom,
  removeAt,
  ruleOf,
  smartQuery,
  stars,
  storedMove,
} from "../src/playlists.ts";

let next = 0;
function doc(type: string, data: Record<string, unknown>): Doc {
  next++;
  return { id: `id-${next}`, type, created: next, updated: next, data };
}

test("a Playlist document becomes a playlist of track ids", () => {
  const p = playlistOf(doc("Playlist", { name: "Road", tracks: [{ $link: "a" }, { $link: "b" }, "junk", { $link: "a" }] }));
  assert.deepEqual(p?.tracks, ["a", "b", "a"], "links only, in order, repeats kept");
  assert.equal(p?.smart, undefined);
  assert.equal(playlistOf(doc("Playlist", { tracks: [] })), null, "no name");
  assert.deepEqual(playlistOf(doc("Playlist", { name: "Empty" }))?.tracks, []);
});

test("playlists sort by name, ignoring case and accents", () => {
  const names = playlistsOf([
    doc("Playlist", { name: "zebra" }),
    doc("Playlist", { name: "Évening" }),
    doc("Playlist", { name: "apple" }),
  ]).map((p) => p.name);
  assert.deepEqual(names, ["apple", "Évening", "zebra"]);
});

test("a smart playlist keeps only well-formed rules", () => {
  const p = playlistOf(
    doc("Playlist", { name: "S", smart: { minRating: 9, addedWithinDays: 30, played: "sometimes", artist: " ", order: "plays", limit: 50 } }),
  );
  assert.deepEqual(p?.smart, { addedWithinDays: 30, order: "plays", limit: 50 });
  assert.deepEqual(ruleOf("nonsense"), {});
});

test("what a playlist document holds", () => {
  assert.deepEqual(playlistData("Road", { tracks: ["a", "b"] }), { name: "Road", tracks: [{ $link: "a" }, { $link: "b" }] });
  assert.deepEqual(playlistData("Best", { smart: { minRating: 4 } }), { name: "Best", smart: { minRating: 4 } });
});

test("smart playlists become queries", () => {
  const now = 1_000 * 24 * 60 * 60 * 1000;
  assert.deepEqual(smartQuery({ minRating: 4, order: "rating" }, now), {
    type: "Track",
    limit: 10_000,
    where: { rating: { $gte: 4 } },
    orderBy: [["rating", "desc"], ["$created", "desc"]],
  });
  assert.deepEqual(smartQuery({ addedWithinDays: 30 }, now).where, { $created: { $gte: now - 30 * 24 * 60 * 60 * 1000 } });
  assert.deepEqual(smartQuery({ played: "never" }, now).where, { $not: { playCount: { $gt: 0 } } });
  const q = smartQuery({ played: "ever", artist: "Enya", genre: "New Age", order: "title", limit: 50 }, now);
  assert.deepEqual(q.where, {
    $and: [{ playCount: { $gt: 0 } }, { $or: [{ artist: "Enya" }, { albumArtist: "Enya" }] }, { genre: "New Age" }],
  });
  assert.equal(q.orderBy, undefined, "by title is sorted in the app");
  assert.equal(q.limit, 50);
  assert.equal(smartQuery({}, now).where, undefined, "no rules: every track");
});

test("the built-in smart playlists all make queries", () => {
  for (const p of BUILT_IN) {
    assert.ok(p.smart && p.builtIn, p.name);
    assert.equal(smartQuery(p.smart, 0).type, "Track");
  }
});

test("rules in words", () => {
  assert.equal(describe({ minRating: 4, order: "rating" }), "Rated 4★ or more · highest rated first");
  assert.equal(describe({ minRating: 5 }), "Rated 5★ · newest first");
  assert.equal(describe({ played: "never", addedWithinDays: 30 }), "Added in the last 30 days · never played · newest first");
  assert.equal(describe({ artist: "Enya", order: "plays", limit: 50 }), "By Enya · most played first · at most 50");
  assert.equal(describe({}), "Every song · newest first");
});

test("a playlist's entries skip tracks no longer in the library, keeping their places", () => {
  const track = (id: string): Track => ({ id, title: id });
  const byId = new Map([["a", track("a")], ["c", track("c")]]);
  const entries = entriesOf({ id: "p", name: "P", tracks: ["a", "b", "c", "a"] }, byId);
  assert.deepEqual(
    entries.map((e) => [e.at, e.track.id]),
    [[0, "a"], [2, "c"], [3, "a"]],
  );
});

test("removing and moving tracks in a playlist", () => {
  assert.deepEqual(removeAt(["a", "b", "a"], 2), ["a", "b"], "only that one of a repeated track");
  assert.deepEqual(move(["a", "b", "c"], 0, 2), ["b", "c", "a"]);
  assert.deepEqual(move(["a", "b", "c"], 2, 1), ["a", "c", "b"]);
  const same = ["a", "b"];
  assert.equal(move(same, 0, 5), same, "out of range: unchanged");
  assert.equal(move(same, 1, 1), same);
});

test("moving a shown entry moves its stored track past ones not shown", () => {
  // "b" is gone from the library: the list shows a, c, d.
  const tracks = ["a", "b", "c", "d"];
  const entries = [{ at: 0 }, { at: 2 }, { at: 3 }];
  const shown = (ids: string[]) => ids.filter((id) => id !== "b");
  const moved = (from: number, to: number) => {
    const m = storedMove(entries, from, to);
    return m ? move(tracks, m[0], m[1]) : tracks;
  };
  assert.deepEqual(shown(moved(0, 2)), ["c", "d", "a"], "first to last");
  assert.deepEqual(shown(moved(2, 0)), ["d", "a", "c"], "last to first");
  assert.deepEqual(shown(moved(1, 2)), ["a", "d", "c"], "down one");
  assert.deepEqual(shown(moved(1, 0)), ["c", "a", "d"], "up one, past the hidden track");
  assert.equal(storedMove(entries, 1, 1), null);
  assert.equal(storedMove(entries, 0, 3), null, "no such entry");
  assert.equal(storedMove(entries, -1, 0), null);
});

test("new playlists get a name of their own", () => {
  assert.equal(freshName("New playlist", []), "New playlist");
  assert.equal(freshName("New playlist", ["new playlist", "New playlist 2"]), "New playlist 3");
});

test("a typed artist matches the library's spelling", () => {
  assert.equal(canonical("beyonce", ["Enya", "Beyoncé"]), "Beyoncé");
  assert.equal(canonical("nobody", ["Enya"]), undefined);
  assert.equal(canonical("  ", ["Enya"]), undefined);
});

test("ratings", () => {
  assert.equal(stars(3), "★★★☆☆");
  assert.equal(stars(undefined), "☆☆☆☆☆");
  assert.deepEqual(RATING_OPTIONS.map(ratingFrom), [5, 4, 3, 2, 1, null]);
  assert.equal(ratingFrom(NO_RATING), null);
  assert.equal(trackOf(doc("Track", { title: "x", rating: 4 }))?.rating, 4);
  assert.equal(trackOf(doc("Track", { title: "x", rating: 7 }))?.rating, undefined, "out of range");
  assert.equal(trackOf(doc("Track", { title: "x", rating: 2.5 }))?.rating, undefined);
});
