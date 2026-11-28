// The Music app's views: what's in the library (albums, an album, artists,
// songs), what's coming up, and the bar that shows what's playing. Each reads
// the library and the player through accessors, so it follows both.

import {
  button,
  each,
  hstack,
  image,
  label,
  MAX_QUEUE,
  menu,
  select,
  slider,
  toggle,
  toolbar,
  virtualList,
  vstack,
  type Repeat,
  type UiNode,
} from "@idealos/sdk";
import { clock, totals } from "./clock.ts";
import type { Collection } from "./collection.ts";
import { coverColor, initials, queueFrom, type Album, type Artist, type Track } from "./library.ts";
import type { Player } from "./player.ts";
import { RATING_OPTIONS, ratingFrom, stars, type Playlist } from "./playlists.ts";

export const TILE = 150;
export const GAP = 18;
const ROW = 34;
const PLAY_NEXT = "Play next";
const ADD = "Add to Up Next";
/** An action in a track's ⋯ menu: what it says, and what it does. */
type RowAction = [string, () => void];
const MOVE_UP = "Move up";
const MOVE_DOWN = "Move down";
const REMOVE = "Remove from playlist";
const addTo = (name: string) => `Add to “${name}”`;

/** The playlists tracks can be added to: the listener's, not smart ones. */
const addable = (c: Collection) => c.playlists().filter((pl) => !pl.smart);

/** Add `ids` to the playlist whose "Add to" option `option` is. Whether it was one. */
function addToChosen(c: Collection, option: string, ids: string[]): boolean {
  const pl = addable(c).find((x) => addTo(x.name) === option);
  if (pl) c.add(pl, ids);
  return pl !== undefined;
}

/** A track's rating, as stars that open a menu to change it. */
export function ratingMenu(c: Collection, t: () => Track | undefined): UiNode {
  return menu({
    label: () => stars(t()?.rating),
    class: "flat stars",
    w: 120,
    hidden: () => t() === undefined,
    options: RATING_OPTIONS,
    onChoose: (o) => {
      const id = t()?.id;
      if (id) c.rate(id, ratingFrom(o));
    },
  });
}

/** Whether `id` is the track playing (or paused) now. */
function isCurrent(p: Player, id: string | undefined): boolean {
  const s = p.status();
  return id !== undefined && s?.current !== undefined && s?.current !== null && s.queue[s.current]?.id === id;
}

/** What marks the track playing in a list: ▶ playing, ‖ paused. */
function marker(p: Player): string {
  return p.status()?.state === "playing" ? "▶" : "‖";
}

/** A cover: the album's picture, or its initials on a colour of its own. A
 * cover without a picture is a button that opens the album. */
function cover(album: () => Album | undefined, size: number, open?: () => void): UiNode {
  return vstack(
    {},
    each(
      () => {
        const art = album()?.artwork;
        return art ? [art] : [];
      },
      (a) => a,
      (a) => image({ blob: a(), w: size, h: size, radius: 6 }),
    ),
    button({
      label: () => initials(album()?.title ?? ""),
      color: () => coverColor(album()?.key ?? ""),
      text_color: "#ffffff",
      size: Math.round(size / 4),
      w: size,
      h: size,
      hidden: () => album()?.artwork !== undefined,
      onClick: () => open?.(),
    }),
  );
}

/** Albums in a grid of `cols` columns, made as they scroll into view. */
export function albumGrid(albums: () => Album[], cols: number, open: (a: Album) => void): UiNode {
  return virtualList(
    { count: () => Math.ceil(albums().length / cols), rowHeight: TILE + 64, grow: 1 },
    (row) =>
      hstack(
        { gap: GAP, padding: [8, 16, 0, 16] },
        ...Array.from({ length: cols }, (_, col) => {
          const album = () => albums()[row * cols + col];
          const go = () => {
            const a = album();
            if (a) open(a);
          };
          return vstack(
            { w: TILE, gap: 2, hidden: () => album() === undefined },
            cover(album, TILE, go),
            button({ label: () => album()?.title ?? "", class: "flat", max_w: TILE, size: 13, onClick: go }),
            label({ text: () => album()?.artist ?? "", color: "$muted", size: 12, w: TILE, align: "center" }),
          );
        }),
      ),
  );
}

/** A track in a list: playing it plays `ids` from `index`. */
function trackRow(
  p: Player,
  c: Collection,
  t: () => Track | undefined,
  index: () => number,
  ids: () => string[],
  opts: {
    number?: boolean;
    artist?: (t: Track) => string | undefined;
    album?: boolean;
    /** More of the ⋯ menu's actions, for this row. */
    actions?: () => RowAction[];
  },
): UiNode {
  const current = () => isCurrent(p, t()?.id);
  const play = () => {
    const q = queueFrom(ids(), index(), MAX_QUEUE);
    p.play(q.tracks, q.start);
  };
  return hstack(
    { gap: 12, align: "center", padding: [0, 16], h: ROW, hidden: () => t() === undefined },
    label({
      text: () => (current() ? marker(p) : opts.number ? String(t()?.trackNumber ?? "") : ""),
      w: 28,
      color: () => (current() ? "$accent" : "$muted"),
    }),
    button({ label: () => t()?.title ?? "", class: () => (current() ? "flat current" : "flat"), onClick: play }),
    label({ text: "", grow: 1 }),
    // Fixed widths, so artists and albums line up in columns.
    label({ text: () => (opts.artist && t() ? (opts.artist(t()!) ?? "") : ""), color: "$muted", size: 13, w: 170 }),
    label({ text: () => (opts.album ? (t()?.album ?? "") : ""), color: "$muted", size: 13, w: 180, hidden: !opts.album }),
    ratingMenu(c, t),
    menu({
      label: "⋯",
      w: 56,
      options: () => [...(opts.actions?.() ?? []).map(([name]) => name), PLAY_NEXT, ADD, ...addable(c).map((pl) => addTo(pl.name))],
      onChoose: (o) => {
        const id = t()?.id;
        const action = opts.actions?.().find(([name]) => name === o);
        if (action) return action[1]();
        if (!id || addToChosen(c, o, [id])) return;
        p.enqueue([id], o === PLAY_NEXT);
      },
    }),
    label({ text: () => clock(t()?.duration), w: 48, color: "$muted", size: 13 }),
  );
}

/** An album: its cover, what it is, and its tracks. */
export function albumPage(p: Player, c: Collection, album: () => Album | undefined): UiNode {
  const ids = () => album()?.tracks.map((t) => t.id) ?? [];
  const shuffle = () => {
    const n = ids().length;
    if (n === 0) return;
    p.setShuffle(true);
    p.play(ids(), Math.floor(Math.random() * n));
  };
  return vstack(
    { grow: 1, gap: 12 },
    hstack(
      { gap: 20, align: "end", padding: [4, 16] },
      cover(album, 180),
      vstack(
        { gap: 6, grow: 1 },
        label({ text: () => album()?.title ?? "", class: "album-title" }),
        label({ text: () => album()?.artist ?? "", size: 17 }),
        label({
          text: () => {
            const a = album();
            if (!a) return "";
            return [a.year, totals(a.tracks.map((t) => t.duration))].filter(Boolean).join(" · ");
          },
          color: "$muted",
          size: 13,
        }),
        hstack(
          { gap: 8, padding: [6, 0, 0, 0] },
          button({ label: "Play", class: "primary", onClick: () => p.play(ids(), 0) }),
          button({ label: "Shuffle", onClick: shuffle }),
          button({ label: "Play next", onClick: () => p.enqueue(ids(), true) }),
          menu({
            label: "Add to playlist",
            hidden: () => addable(c).length === 0,
            options: () => addable(c).map((pl) => addTo(pl.name)),
            onChoose: (o) => void addToChosen(c, o, ids()),
          }),
        ),
      ),
    ),
    virtualList({ count: () => album()?.tracks.length ?? 0, rowHeight: ROW, grow: 1 }, (i) =>
      trackRow(p, c, () => album()?.tracks[i], () => i, ids, {
        number: true,
        // A track's artist only when it isn't the album's.
        artist: (t) => (t.artist && t.artist !== album()?.artist ? t.artist : undefined),
      }),
    ),
  );
}

/** Every song, as a list made as it scrolls into view. */
export function songList(p: Player, c: Collection, songs: () => Track[]): UiNode {
  const ids = () => songs().map((t) => t.id);
  return virtualList({ count: () => songs().length, rowHeight: ROW, grow: 1 }, (i) =>
    trackRow(p, c, () => songs()[i], () => i, ids, { artist: (t) => t.artist, album: true }),
  );
}

/** A playlist's heading: its name, what it holds, and what can be done with it. */
function playlistHeading(
  p: Player,
  list: () => Playlist | undefined,
  ids: () => string[],
  about: () => string,
  edit: () => void,
  remove: () => void,
): UiNode {
  const shuffle = () => {
    const n = ids().length;
    if (n === 0) return;
    p.setShuffle(true);
    p.play(ids(), Math.floor(Math.random() * n));
  };
  const play = () => {
    const q = queueFrom(ids(), 0, MAX_QUEUE);
    p.play(q.tracks, q.start);
  };
  const fixed = () => list()?.builtIn === true;
  return vstack(
    { gap: 6, padding: [14, 16, 4, 16] },
    label({ text: () => list()?.name ?? "", class: "album-title" }),
    label({ text: about, color: "$muted", size: 13 }),
    hstack(
      { gap: 8, padding: [6, 0, 0, 0] },
      button({ label: "Play", class: "primary", onClick: play }),
      button({ label: "Shuffle", onClick: shuffle }),
      label({ text: "", grow: 1 }),
      button({ label: () => (list()?.smart ? "Edit rules" : "Rename"), hidden: fixed, onClick: edit }),
      button({ label: "Delete", hidden: fixed, onClick: remove }),
    ),
  );
}

/** Songs, with a total time: "12 songs, 48 min". */
const songCount = (tracks: Track[]) => totals(tracks.map((t) => t.duration));

/** A playlist the listener made: its tracks in their order, to play, reorder and remove. */
export function playlistPage(
  p: Player,
  c: Collection,
  list: () => Playlist | undefined,
  entries: () => { at: number; track: Track }[],
  edit: () => void,
  remove: () => void,
): UiNode {
  const ids = () => entries().map((e) => e.track.id);
  const about = () => (entries().length === 0 ? "No songs yet. Add them from a song's ⋯ menu." : songCount(entries().map((e) => e.track)));
  return vstack(
    { grow: 1, gap: 12 },
    playlistHeading(p, list, ids, about, edit, remove),
    virtualList({ count: () => entries().length, rowHeight: ROW, grow: 1 }, (i) => {
      const entry = () => entries()[i];
      const change = (fn: (pl: Playlist, at: number) => void) => () => {
        const pl = list();
        const e = entry();
        if (pl && e) fn(pl, e.at);
      };
      // Moves past tracks no longer in the library, so a move is always visible.
      const neighbour = (d: number) => entries()[i + d]?.at;
      return trackRow(p, c, () => entry()?.track, () => i, ids, {
        artist: (t) => t.artist,
        album: true,
        actions: () => {
          const actions: RowAction[] = [];
          if (i > 0) actions.push([MOVE_UP, change((pl, at) => c.move(pl, at, neighbour(-1) ?? at))]);
          if (i < entries().length - 1) actions.push([MOVE_DOWN, change((pl, at) => c.move(pl, at, neighbour(1) ?? at))]);
          actions.push([REMOVE, change((pl, at) => c.removeAt(pl, at))]);
          return actions;
        },
      });
    }),
  );
}

/** A smart playlist: what its rule finds, kept current by a live query. */
export function smartPage(
  p: Player,
  c: Collection,
  list: () => Playlist | undefined,
  tracks: () => Track[],
  rule: () => string,
  ready: () => boolean,
  edit: () => void,
  remove: () => void,
): UiNode {
  const ids = () => tracks().map((t) => t.id);
  const about = () => [rule(), ready() ? (tracks().length === 0 ? "nothing matches yet" : songCount(tracks())) : ""].filter(Boolean).join(" — ");
  return vstack(
    { grow: 1, gap: 12 },
    playlistHeading(p, list, ids, about, edit, remove),
    virtualList({ count: () => tracks().length, rowHeight: ROW, grow: 1 }, (i) =>
      trackRow(p, c, () => tracks()[i], () => i, ids, { artist: (t) => t.artist, album: true }),
    ),
  );
}

/** The artists, each opening their albums. */
export function artistList(artists: () => Artist[], open: (a: Artist) => void): UiNode {
  return virtualList({ count: () => artists().length, rowHeight: ROW, grow: 1 }, (i) => {
    const artist = () => artists()[i];
    return hstack(
      { gap: 12, align: "center", padding: [0, 16], h: ROW, hidden: () => artist() === undefined },
      button({ label: () => artist()?.name ?? "", class: "flat", onClick: () => artist() && open(artist()!) }),
      label({ text: "", grow: 1 }),
      label({
        text: () => {
          const n = artist()?.albums.length ?? 0;
          return `${n} album${n === 1 ? "" : "s"}`;
        },
        color: "$muted",
        size: 13,
      }),
    );
  });
}

/** What's playing and what comes after, from audiod's queue. */
export function upNext(p: Player): UiNode {
  const queue = () => p.status()?.queue ?? [];
  return virtualList({ count: () => queue().length, rowHeight: ROW, grow: 1 }, (i) => {
    const item = () => queue()[i];
    const current = () => p.status()?.current === i;
    return hstack(
      { gap: 12, align: "center", padding: [0, 16], h: ROW, hidden: () => item() === undefined },
      label({ text: () => (current() ? marker(p) : String(i + 1)), w: 36, color: () => (current() ? "$accent" : "$muted") }),
      button({ label: () => item()?.title ?? "", class: () => (current() ? "flat current" : "flat"), onClick: () => p.jump(i) }),
      label({ text: "", grow: 1 }),
      label({ text: () => [item()?.artist, item()?.album].filter(Boolean).join(" — "), color: "$muted", size: 13, max_w: 360 }),
      label({ text: () => clock(item()?.duration), w: 48, color: "$muted", size: 13 }),
      button({ label: "Remove", onClick: () => p.remove(i) }),
    );
  });
}

const REPEATS: [Repeat, string][] = [
  ["off", "Repeat off"],
  ["all", "Repeat all"],
  ["one", "Repeat one"],
];

/** The bar along the bottom: what's playing, where it is, and the controls.
 * `albumOf` finds a track's album in the library, to show and open. */
export function nowPlaying(
  p: Player,
  c: Collection,
  trackOf: (id: string) => Track | undefined,
  albumOf: (id: string) => Album | undefined,
  open: (a: Album) => void,
): UiNode {
  const item = () => {
    const s = p.status();
    return s?.current !== undefined && s?.current !== null ? s.queue[s.current] : undefined;
  };
  const duration = () => p.status()?.duration ?? item()?.duration ?? 0;
  const idle = () => item() === undefined;
  const album = () => {
    const it = item();
    return it ? albumOf(it.id) : undefined;
  };
  return toolbar(
    { padding: [8, 16], gap: 16, align: "center" },
    cover(album, 52, () => {
      const a = album();
      if (a) open(a);
    }),
    vstack(
      { gap: 2, w: 230 },
      label({ text: () => item()?.title ?? "Not playing", weight: "bold", max_w: 230 }),
      label({
        text: () => [item()?.artist, item()?.album].filter(Boolean).join(" — "),
        color: "$muted",
        size: 12,
        max_w: 230,
      }),
      ratingMenu(c, () => {
        const it = item();
        return it ? trackOf(it.id) : undefined;
      }),
    ),
    vstack(
      { gap: 4, grow: 1 },
      hstack(
        { gap: 8, justify: "center" },
        button({ label: "Previous", onClick: p.previous }),
        button({ label: () => (p.status()?.state === "playing" ? "Pause" : "Play"), class: "primary", w: 80, onClick: p.toggle }),
        button({ label: "Next", onClick: p.next }),
      ),
      hstack(
        { gap: 8, align: "center" },
        label({ text: () => (idle() ? "" : clock(p.position())), w: 52, color: "$muted", size: 12 }),
        slider({ value: p.position, min: 0, max: () => Math.max(1, duration()), step: 1, grow: 1, onChange: p.seek }),
        label({ text: () => (idle() ? "" : clock(duration())), w: 52, color: "$muted", size: 12 }),
      ),
    ),
    vstack(
      { gap: 6 },
      hstack(
        { gap: 8, align: "center" },
        toggle({ label: "Shuffle", checked: () => p.status()?.shuffle ?? false, onChange: p.setShuffle }),
        select({
          options: REPEATS.map(([, name]) => name),
          value: () => REPEATS.find(([r]) => r === p.status()?.repeat)?.[1] ?? REPEATS[0][1],
          onChange: (v) => p.setRepeat(REPEATS.find(([, name]) => name === v)?.[0] ?? "off"),
        }),
      ),
      hstack(
        { gap: 8, align: "center" },
        label({ text: "Volume", color: "$muted", size: 12 }),
        slider({ value: p.volume, min: 0, max: 1, step: 0.01, w: 130, onChange: p.setVolume }),
      ),
    ),
  );
}
