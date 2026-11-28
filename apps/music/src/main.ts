// Music: the library of Track documents, and playback through audiod.
//
// On the left, a search and the ways in: albums, artists, songs, and what's up
// next. Opening an album shows its tracks; playing one plays the album from
// there. audiod owns the queue, so music keeps playing when this window closes,
// and the bar along the bottom follows it whoever changes it (the top bar,
// `osctl audio`, another window).

import style from "./style.css";
import { batch, button, db, each, effect, hstack, label, memo, openWindow, signal, split, textField, vstack } from "@idealos/sdk";
import {
  albumMatches,
  albumsOf,
  artistsOf,
  matches,
  songsOf,
  tracksOf,
  TYPE,
  words,
  type Album,
  type Artist,
} from "./library.ts";
import { player } from "./player.ts";
import { albumGrid, albumPage, artistList, GAP, nowPlaying, songList, TILE, upNext } from "./views.ts";

type Section = "albums" | "artists" | "songs" | "queue";

/** Where the main area is: a section, or something opened from one. */
type Route = { kind: Section } | { kind: "album"; key: string } | { kind: "artist"; name: string };

const SECTIONS: [Section, string][] = [
  ["albums", "Albums"],
  ["artists", "Artists"],
  ["songs", "Songs"],
  ["queue", "Up Next"],
];

const WIDTH = 1120;
const [width, setWidth] = signal(WIDTH);
const [ratio, setRatio] = signal(0.2);

const win = await openWindow({ title: "Music", width: WIDTH, height: 740, style }, () => {
  const docs = db.live({ type: TYPE, limit: 10_000 });
  const p = player();
  const [routes, setRoutes] = signal<Route[]>([{ kind: "albums" }]);
  const [search, setSearch] = signal("");

  const route = () => routes()[routes().length - 1];
  const section = (): Section => {
    const first = routes()[0];
    return first.kind === "album" || first.kind === "artist" ? "albums" : first.kind;
  };
  const go = (s: Section) => setRoutes([{ kind: s }]);
  const push = (r: Route) => setRoutes((rs) => [...rs, r]);
  const back = () => setRoutes((rs) => (rs.length > 1 ? rs.slice(0, -1) : rs));

  const tracks = memo(() => tracksOf(docs()));
  const albums = memo(() => albumsOf(tracks()));
  const artists = memo(() => artistsOf(albums()));
  const ws = memo(() => words(search()), (a, b) => a.join(" ") === b.join(" "));
  const shownAlbums = memo(() => albums().filter((a) => albumMatches(a, ws())));
  const shownArtists = memo(() =>
    ws().length === 0 ? artists() : artists().filter((a) => a.albums.some((al) => albumMatches(al, ws()))),
  );
  const songs = memo(() => songsOf(tracks().filter((t) => matches(t, ws()))));
  const openAlbum = (a: Album) => {
    const r = route();
    if (r.kind !== "album" || r.key !== a.key) push({ kind: "album", key: a.key });
  };
  const openArtist = (a: Artist) => push({ kind: "artist", name: a.name });
  const albumAt = (key: string) => albums().find((a) => a.key === key);
  const byTrack = memo(() => new Map(albums().flatMap((a) => a.tracks.map((t) => [t.id, a] as const))));
  const artistAt = (name: string) => artists().find((a) => a.name === name);

  // Searching shows songs from an album or artist page, where it would filter nothing.
  effect(() => {
    if (ws().length > 0) {
      const r = route();
      if (r.kind === "album" || r.kind === "artist") batch(() => go("songs"));
    }
  });

  // As many columns of covers as fit beside the sidebar.
  const cols = memo(() => Math.max(1, Math.floor((width() * (1 - ratio()) - 32 + GAP) / (TILE + GAP))));

  const counts = () => {
    if (!docs.ready()) return "Loading…";
    const n = tracks().length;
    return `${albums().length} album${albums().length === 1 ? "" : "s"}, ${n} song${n === 1 ? "" : "s"}`;
  };

  const sidebar = vstack(
    { gap: 6, padding: 10 },
    textField({ placeholder: "Search", value: search, onChange: setSearch }),
    ...SECTIONS.map(([s, name]) =>
      button({
        label: () => {
          const n = p.status()?.queue.length ?? 0;
          return s === "queue" && n > 0 ? `${name} (${n})` : name;
        },
        w: "fill",
        class: () => (section() === s ? "primary" : ""),
        onClick: () => go(s),
      }),
    ),
    label({ text: "", grow: 1 }),
    label({ text: counts, size: 12, color: "$muted" }),
  );

  const heading = () => {
    const r = route();
    switch (r.kind) {
      case "albums":
        return ws().length ? `Albums matching “${search().trim()}”` : "Albums";
      case "artists":
        return "Artists";
      case "songs":
        return ws().length ? `Songs matching “${search().trim()}”` : "Songs";
      case "queue":
        return "Up Next";
      case "album":
        return "";
      case "artist":
        return r.name;
    }
  };

  /** What's empty, when a view has nothing to show. */
  const emptiness = () => {
    if (!docs.ready()) return "";
    if (tracks().length === 0 && route().kind !== "queue") return "No music yet. Import some with Files.";
    switch (route().kind) {
      case "albums":
        return shownAlbums().length === 0 ? "No albums match." : "";
      case "artists":
        return shownArtists().length === 0 ? "No artists match." : "";
      case "songs":
        return songs().length === 0 ? "No songs match." : "";
      case "queue":
        return (p.status()?.queue.length ?? 0) === 0 ? "Nothing up next. Play an album or a song." : "";
      default:
        return "";
    }
  };

  // Rebuilt when the route (or, for grids, the number of columns) changes.
  const viewKey = () => {
    const r = route();
    const key = r.kind === "album" ? `album:${r.key}` : r.kind === "artist" ? `artist:${r.name}` : r.kind;
    return r.kind === "albums" || r.kind === "artist" ? `${key}@${cols()}` : key;
  };

  const main = vstack(
    { grow: 1 },
    hstack(
      { gap: 10, align: "center", padding: [10, 16] },
      button({ label: "Back", hidden: () => routes().length < 2, onClick: back }),
      label({ text: heading, size: 20, weight: "bold", grow: 1 }),
      button({ label: "Clear", hidden: () => route().kind !== "queue" || (p.status()?.queue.length ?? 0) === 0, onClick: p.clear }),
    ),
    label({ text: () => p.problem() || p.status()?.error || "", class: "err", hidden: () => !(p.problem() || p.status()?.error) }),
    label({ text: emptiness, color: "$muted", hidden: () => emptiness() === "" }),
    each(
      () => [viewKey()],
      (k) => k,
      () => {
        const r = route();
        switch (r.kind) {
          case "albums":
            return albumGrid(shownAlbums, cols(), openAlbum);
          case "album":
            return albumPage(p, () => albumAt(r.key));
          case "artists":
            return artistList(shownArtists, openArtist);
          case "artist":
            return albumGrid(() => artistAt(r.name)?.albums ?? [], cols(), openAlbum);
          case "songs":
            return songList(p, songs);
          case "queue":
            return upNext(p);
        }
      },
      { grow: 1 },
    ),
  );

  return vstack({ grow: 1 }, split({ ratio, grow: 1, onChange: setRatio }, sidebar, main), nowPlaying(p, (id) => byTrack().get(id), openAlbum));
});

effect(() => setWidth(win.size().width));
