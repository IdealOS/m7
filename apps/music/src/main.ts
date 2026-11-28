// Music: the library of Track documents, and playback through audiod.
//
// On the left, a search and the ways in: albums, artists, genres, years, songs,
// and Now Playing (the track playing, large, with Up Next beside it), then
// playlists: smart ones built in, and the listener's own. Opening an album shows
// its tracks; playing one plays the album from there. audiod owns the queue, so
// music keeps playing when this window closes,
// and the bar along the bottom follows it whoever changes it (the top bar,
// `osctl audio`, another window).

import style from "./style.css";
import { batch, button, db, each, effect, hstack, label, memo, onCleanup, openWindow, scroll, signal, split, textField, vstack } from "@idealos/sdk";
import { collection } from "./collection.ts";
import { playlistDialog, type Draft } from "./dialog.ts";
import {
  albumMatches,
  albumsOf,
  artistsOf,
  genresOf,
  matches,
  shelfMatches,
  songsOf,
  tracksOf,
  TYPE,
  words,
  yearsOf,
  type Album,
  type Shelf,
} from "./library.ts";
import { player } from "./player.ts";
import { BUILT_IN, describe, entriesOf, freshName, smartQuery, type Playlist, type Rule } from "./playlists.ts";
import { albumGrid, albumPage, GAP, nowPlaying, nowPlayingPage, playlistPage, shelfList, smartPage, songList, TILE } from "./views.ts";

type Section = "albums" | "artists" | "genres" | "years" | "songs" | "playing";

/** Where the main area is: a section, or something opened from one. */
type Route =
  | { kind: Section }
  | { kind: "album"; key: string }
  | { kind: "artist"; name: string }
  | { kind: "genre"; name: string }
  | { kind: "year"; name: string }
  | { kind: "playlist"; id: string };

const SECTIONS: [Section, string][] = [
  ["albums", "Albums"],
  ["artists", "Artists"],
  ["genres", "Genres"],
  ["years", "Years"],
  ["songs", "Songs"],
  ["playing", "Now Playing"],
];

/** The pages a section opens onto, whose albums a grid shows. */
type ShelfKind = "artist" | "genre" | "year";

const WIDTH = 1120;
const [width, setWidth] = signal(WIDTH);
const [ratio, setRatio] = signal(0.2);

const win = await openWindow({ title: "Music", width: WIDTH, height: 740, style }, () => {
  const docs = db.live({ type: TYPE, limit: 10_000 });
  const p = player();
  const c = collection();
  const [draft, setDraft] = signal<Draft | null>(null);
  const [routes, setRoutes] = signal<Route[]>([{ kind: "albums" }]);
  const [search, setSearch] = signal("");

  const route = () => routes()[routes().length - 1];
  // The section the first route is (pages opened from it are pushed on top).
  const section = (): Section | null => SECTIONS.find(([s]) => s === routes()[0].kind)?.[0] ?? null;
  const go = (s: Section) => setRoutes([{ kind: s }]);
  const push = (r: Route) => setRoutes((rs) => [...rs, r]);
  const back = () => setRoutes((rs) => (rs.length > 1 ? rs.slice(0, -1) : rs));

  const tracks = memo(() => tracksOf(docs()));
  const albums = memo(() => albumsOf(tracks()));
  const artists = memo(() => artistsOf(albums()));
  const ws = memo(() => words(search()), (a, b) => a.join(" ") === b.join(" "));
  const shownAlbums = memo(() => albums().filter((a) => albumMatches(a, ws())));
  const genres = memo(() => genresOf(albums()));
  const years = memo(() => yearsOf(albums()));
  const shown = (shelves: () => Shelf[]) => memo(() => (ws().length === 0 ? shelves() : shelves().filter((s) => shelfMatches(s, ws()))));
  const shownArtists = shown(artists);
  const shownGenres = shown(genres);
  const shownYears = shown(years);
  const songs = memo(() => songsOf(tracks().filter((t) => matches(t, ws()))));
  const openAlbum = (a: Album) => {
    const r = route();
    if (r.kind !== "album" || r.key !== a.key) push({ kind: "album", key: a.key });
  };
  const openShelf = (kind: ShelfKind) => (s: Shelf) => push({ kind, name: s.name });
  const shelfAt = (kind: ShelfKind, name: string) =>
    ({ artist: artists, genre: genres, year: years })[kind]().find((s) => s.name === name);
  const albumAt = (key: string) => albums().find((a) => a.key === key);
  const byTrack = memo(() => new Map(albums().flatMap((a) => a.tracks.map((t) => [t.id, a] as const))));
  const trackById = memo(() => new Map(tracks().map((t) => [t.id, t] as const)));

  const allPlaylists = memo(() => [...BUILT_IN, ...c.playlists()]);
  const playlistAt = (id: string) => allPlaylists().find((pl) => pl.id === id);
  const openPlaylist = (id: string) => setRoutes([{ kind: "playlist", id }]);
  const newPlaylist = (smart: boolean) =>
    setDraft({
      smart,
      name: freshName(smart ? "New smart playlist" : "New playlist", c.playlists().map((pl) => pl.name)),
      rule: smart ? { order: "added" } : {},
      artist: "",
      genre: "",
    });
  const editPlaylist = (pl: Playlist) =>
    setDraft({ id: pl.id, smart: pl.smart !== undefined, name: pl.name, rule: pl.smart ?? {}, artist: pl.smart?.artist ?? "", genre: pl.smart?.genre ?? "" });
  const saveDraft = (d: Draft) => {
    setDraft(null);
    const pl = d.id ? c.playlists().find((x) => x.id === d.id) : undefined;
    if (pl) {
      if (pl.name !== d.name) c.rename(pl, d.name);
      if (d.smart) c.setRule(pl, d.rule);
      return;
    }
    void c.create(d.name, d.smart ? { smart: d.rule } : { tracks: [] }).then((id) => id && setOpening(id));
  };
  // A new playlist opens once the live query has it, so its page has something to show.
  const [opening, setOpening] = signal<string | null>(null);
  effect(() => {
    const id = opening();
    if (id && playlistAt(id))
      batch(() => {
        setOpening(null);
        openPlaylist(id);
      });
  });
  const deletePlaylist = (pl: Playlist) => {
    c.remove(pl);
    go("albums");
  };
  // A playlist that went away (deleted in another window, say) leaves its page.
  effect(() => {
    const r = route();
    if (r.kind === "playlist" && c.ready() && !playlistAt(r.id)) batch(() => go("albums"));
  });

  // Searching shows songs from an album or artist page, where it would filter nothing.
  effect(() => {
    if (ws().length > 0) {
      const r = route();
      if (r.kind === "album" || r.kind === "artist" || r.kind === "genre" || r.kind === "year") batch(() => go("songs"));
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
          return s === "playing" && n > 0 ? `${name} (${n})` : name;
        },
        w: "fill",
        class: () => (section() === s ? "primary" : ""),
        onClick: () => go(s),
      }),
    ),
    label({ text: "Playlists", size: 12, color: "$muted", weight: "bold" }),
    scroll(
      { grow: 1, gap: 2 },
      each(
        allPlaylists,
        (pl) => pl.id,
        (pl) =>
          button({
            label: () => `${pl().smart ? "◇ " : ""}${pl().name}`,
            w: "fill",
            class: () => {
              const r = route();
              return r.kind === "playlist" && r.id === pl().id ? "primary" : "flat";
            },
            onClick: () => openPlaylist(pl().id),
          }),
        { gap: 2 },
      ),
    ),
    hstack(
      { gap: 6 },
      button({ label: "New playlist", grow: 1, onClick: () => newPlaylist(false) }),
      button({ label: "Smart", onClick: () => newPlaylist(true) }),
    ),
    label({ text: counts, size: 12, color: "$muted" }),
  );

  const heading = () => {
    const r = route();
    switch (r.kind) {
      case "albums":
        return ws().length ? `Albums matching “${search().trim()}”` : "Albums";
      case "artists":
      case "genres":
      case "years": {
        const name = SECTIONS.find(([s]) => s === r.kind)?.[1] ?? "";
        return ws().length ? `${name} matching “${search().trim()}”` : name;
      }
      case "songs":
        return ws().length ? `Songs matching “${search().trim()}”` : "Songs";
      case "album":
      case "playlist":
      case "playing":
        return "";
      case "artist":
      case "genre":
      case "year":
        return r.name;
    }
  };

  /** What's empty, when a view has nothing to show. */
  const emptiness = () => {
    if (!docs.ready()) return "";
    if (tracks().length === 0 && route().kind !== "playing") return "No music yet. Import some with Files.";
    switch (route().kind) {
      case "albums":
        return shownAlbums().length === 0 ? "No albums match." : "";
      case "artists":
        return shownArtists().length === 0 ? "No artists match." : "";
      case "genres":
        return shownGenres().length === 0 ? (ws().length ? "No genres match." : "No song says its genre.") : "";
      case "years":
        return shownYears().length === 0 ? (ws().length ? "No years match." : "No album says its year.") : "";
      case "songs":
        return songs().length === 0 ? "No songs match." : "";
      default:
        return "";
    }
  };

  // Rebuilt when the route (or, for grids, the number of columns) changes.
  const viewKey = () => {
    const r = route();
    if (r.kind === "playlist") {
      // A smart playlist's page queries afresh when its rule changes.
      const rule = playlistAt(r.id)?.smart;
      return `playlist:${r.id}${rule ? JSON.stringify(rule) : ""}`;
    }
    if (r.kind === "album") return `album:${r.key}`;
    if (r.kind === "artist" || r.kind === "genre" || r.kind === "year") return `${r.kind}:${r.name}@${cols()}`;
    return r.kind === "albums" ? `albums@${cols()}` : r.kind;
  };

  const main = vstack(
    { grow: 1 },
    hstack(
      { gap: 10, align: "center", padding: [10, 16], hidden: () => route().kind === "playlist" || route().kind === "playing" },
      button({ label: "Back", hidden: () => routes().length < 2, onClick: back }),
      label({ text: heading, size: 20, weight: "bold", grow: 1 }),
    ),
    label({
      text: () => p.problem() || p.status()?.error || c.problem(),
      class: "err",
      hidden: () => !(p.problem() || p.status()?.error || c.problem()),
    }),
    hstack(
      { gap: 10, align: "center", padding: [0, 16], hidden: () => c.deleted() === null },
      label({ text: () => `Deleted “${c.deleted()?.name ?? ""}”.` }),
      button({ label: "Undo", onClick: c.restore }),
    ),
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
            return albumPage(p, c, () => albumAt(r.key));
          case "artists":
            return shelfList(shownArtists, openShelf("artist"));
          case "genres":
            return shelfList(shownGenres, openShelf("genre"));
          case "years":
            return shelfList(shownYears, openShelf("year"));
          case "artist":
          case "genre":
          case "year":
            return albumGrid(() => shelfAt(r.kind, r.name)?.albums ?? [], cols(), openAlbum);
          case "songs":
            return songList(p, c, songs);
          case "playing":
            return nowPlayingPage(p, c, (id) => trackById().get(id), (id) => byTrack().get(id), openAlbum);
          case "playlist":
            return playlistView(r.id);
        }
      },
      { grow: 1 },
    ),
  );

  return vstack(
    { grow: 1 },
    split({ ratio, grow: 1, onChange: setRatio }, sidebar, main),
    nowPlaying(p, c, (id) => trackById().get(id), (id) => byTrack().get(id), () => go("playing")),
    playlistDialog({ draft, setDraft, artists: () => artists().map((a) => a.name), genres: () => genres().map((g) => g.name), save: saveDraft }),
  );

  /** A playlist's page: its own tracks, or what a smart playlist's rule finds. */
  function playlistView(id: string) {
    const list = () => playlistAt(id);
    const edit = () => {
      const pl = list();
      if (pl) editPlaylist(pl);
    };
    const remove = () => {
      const pl = list();
      if (pl) deletePlaylist(pl);
    };
    const rule: Rule | undefined = list()?.smart;
    if (rule === undefined) {
      return playlistPage(p, c, list, () => entriesOf(list() ?? { id, name: "", tracks: [] }, trackById()), edit, remove);
    }
    // As of now: "added in the last 30 days" counts back from when the page opened.
    const found = db.live(smartQuery(rule, Date.now()));
    onCleanup(() => found.close());
    const shown = memo(() => {
      const ts = tracksOf(found());
      return rule.order === "title" ? songsOf(ts) : ts;
    });
    return smartPage(p, c, list, shown, () => describe(rule), found.ready, edit, remove);
  }
});

effect(() => setWidth(win.size().width));
