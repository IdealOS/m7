// What the listener keeps in the database besides the music: playlists, and
// each track's rating. Follows the Playlist documents with a live query and
// changes them through db, so other windows (and other devices, later) see the
// same thing. Made inside a window's build, so it stops when the window closes.

import { db, onCleanup, signal } from "@idealos/sdk";
import { move, PLAYLIST, playlistData, playlistsOf, removeAt, linksTo, type Playlist, type Rule } from "./playlists.ts";

/** Every rule, cleared: the base of a patch that replaces a smart playlist's rule. */
const CLEARED: Record<keyof Required<Rule>, null> = {
  minRating: null,
  addedWithinDays: null,
  played: null,
  artist: null,
  genre: null,
  order: null,
  limit: null,
};

export interface Collection {
  /** The listener's playlists (not the built-in ones), by name. */
  playlists(): Playlist[];
  /** Whether the playlists have arrived. */
  ready(): boolean;
  /** What last went wrong saving, if anything. */
  problem(): string;
  /** A playlist just deleted, which `restore` brings back. */
  deleted(): { name: string; changeSet: number } | null;
  rate(trackId: string, rating: number | null): void;
  /** Make a playlist; resolves to its id, or null if it failed. */
  create(name: string, content: { tracks: string[] } | { smart: Rule }): Promise<string | null>;
  rename(p: Playlist, name: string): void;
  setRule(p: Playlist, rule: Rule): void;
  add(p: Playlist, trackIds: string[]): void;
  removeAt(p: Playlist, at: number): void;
  move(p: Playlist, from: number, to: number): void;
  remove(p: Playlist): void;
  restore(): void;
}

export function collection(): Collection {
  const docs = db.live({ type: PLAYLIST, limit: 1000 });
  onCleanup(() => docs.close());
  const [problem, setProblem] = signal("");
  const [deleted, setDeleted] = signal<{ name: string; changeSet: number } | null>(null);
  const fail = (e: Error) => setProblem(e.message);
  const save = (id: string, patch: Record<string, unknown>) => {
    setProblem("");
    void db.update(id, patch).catch(fail);
  };
  const setTracks = (p: Playlist, ids: string[]) => save(p.id, { tracks: linksTo(ids) });

  return {
    playlists: () => playlistsOf(docs()),
    ready: docs.ready,
    problem,
    deleted,
    rate: (trackId, rating) => save(trackId, { rating }),
    create: async (name, content) => {
      setProblem("");
      try {
        return (await db.put(PLAYLIST, playlistData(name, content))).id;
      } catch (e) {
        fail(e as Error);
        return null;
      }
    },
    rename: (p, name) => save(p.id, { name }),
    // A merge patch keeps what it doesn't mention, so rules taken away are nulled.
    setRule: (p, rule) => save(p.id, { smart: { ...CLEARED, ...rule } }),
    add: (p, ids) => setTracks(p, [...p.tracks, ...ids]),
    removeAt: (p, at) => setTracks(p, removeAt(p.tracks, at)),
    move: (p, from, to) => setTracks(p, move(p.tracks, from, to)),
    remove: (p) => {
      setProblem("");
      db.remove(p.id).then((changeSet) => setDeleted({ name: p.name, changeSet }), fail);
    },
    restore: () => {
      const d = deleted();
      if (!d) return;
      setDeleted(null);
      void db.undo(d.changeSet).catch(fail);
    },
  };
}
