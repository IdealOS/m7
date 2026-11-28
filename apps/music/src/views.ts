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
import { coverColor, initials, queueFrom, type Album, type Artist, type Track } from "./library.ts";
import type { Player } from "./player.ts";

export const TILE = 150;
export const GAP = 18;
const ROW = 34;
const PLAY_NEXT = "Play next";
const ADD = "Add to Up Next";

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
  t: () => Track | undefined,
  index: () => number,
  ids: () => string[],
  opts: { number?: boolean; artist?: (t: Track) => string | undefined; album?: boolean },
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
    label({ text: () => (opts.artist && t() ? (opts.artist(t()!) ?? "") : ""), color: "$muted", size: 13, w: 200 }),
    label({ text: () => (opts.album ? (t()?.album ?? "") : ""), color: "$muted", size: 13, w: 220, hidden: !opts.album }),
    menu({
      label: "⋯",
      w: 56,
      options: [PLAY_NEXT, ADD],
      onChoose: (o) => {
        const id = t()?.id;
        if (id) p.enqueue([id], o === PLAY_NEXT);
      },
    }),
    label({ text: () => clock(t()?.duration), w: 48, color: "$muted", size: 13 }),
  );
}

/** An album: its cover, what it is, and its tracks. */
export function albumPage(p: Player, album: () => Album | undefined): UiNode {
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
        ),
      ),
    ),
    virtualList({ count: () => album()?.tracks.length ?? 0, rowHeight: ROW, grow: 1 }, (i) =>
      trackRow(p, () => album()?.tracks[i], () => i, ids, {
        number: true,
        // A track's artist only when it isn't the album's.
        artist: (t) => (t.artist && t.artist !== album()?.artist ? t.artist : undefined),
      }),
    ),
  );
}

/** Every song, as a list made as it scrolls into view. */
export function songList(p: Player, songs: () => Track[]): UiNode {
  const ids = () => songs().map((t) => t.id);
  return virtualList({ count: () => songs().length, rowHeight: ROW, grow: 1 }, (i) =>
    trackRow(p, () => songs()[i], () => i, ids, { artist: (t) => t.artist, album: true }),
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
export function nowPlaying(p: Player, albumOf: (id: string) => Album | undefined, open: (a: Album) => void): UiNode {
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
