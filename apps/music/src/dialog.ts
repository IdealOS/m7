// The dialog that makes a playlist or a smart playlist, renames one, or changes
// a smart playlist's rules. It edits a draft; nothing is saved until Save.

import { button, dialog, each, hstack, label, select, textControl, textField, vstack, type UiNode } from "@idealos/sdk";
import { canonical, type Order, type Played, type Rule } from "./playlists.ts";

/** What's being edited: a new playlist (no `id`) or an existing one. */
export interface Draft {
  id?: string;
  smart: boolean;
  name: string;
  rule: Rule;
  /** The artist and genre as typed, matched to the library's spelling on Save. */
  artist: string;
  genre: string;
}

/** A select's choices: what it shows, and the value it stands for. */
type Choices<T> = [string, T | undefined][];

const RATINGS: Choices<number> = [
  ["Any rating", undefined],
  ["1★ or more", 1],
  ["2★ or more", 2],
  ["3★ or more", 3],
  ["4★ or more", 4],
  ["5★", 5],
];
const ADDED: Choices<number> = [
  ["Any time", undefined],
  ["In the last week", 7],
  ["In the last 30 days", 30],
  ["In the last 90 days", 90],
  ["In the last year", 365],
];
const PLAYED: Choices<Played> = [
  ["Played or not", undefined],
  ["Never played", "never"],
  ["Played", "ever"],
];
const ORDERS: Choices<Order> = [
  ["Newest first", "added"],
  ["Last played first", "played"],
  ["Most played first", "plays"],
  ["Highest rated first", "rating"],
  ["By title", "title"],
];
const LIMITS: Choices<number> = [
  ["No limit", undefined],
  ["25 songs", 25],
  ["50 songs", 50],
  ["100 songs", 100],
  ["250 songs", 250],
];

const shown = <T,>(choices: Choices<T>, v: T | undefined) => (choices.find(([, x]) => x === v) ?? choices[0])[0];
const valueOf = <T,>(choices: Choices<T>, name: string) => choices.find(([n]) => n === name)?.[1];

export interface DialogProps {
  draft: () => Draft | null;
  setDraft: (d: Draft | null) => void;
  /** The library's artists and genres, which typed ones are matched to. */
  artists: () => string[];
  genres: () => string[];
  save: (d: Draft) => void;
}

/** The rule a draft saves: the typed artist and genre spelled as the library has them. */
export function finish(d: Draft, artists: string[], genres: string[]): Draft {
  const rule: Rule = { ...d.rule };
  delete rule.artist;
  delete rule.genre;
  const artist = canonical(d.artist, artists) ?? (d.artist.trim() || undefined);
  const genre = canonical(d.genre, genres) ?? (d.genre.trim() || undefined);
  if (artist) rule.artist = artist;
  if (genre) rule.genre = genre;
  return { ...d, name: d.name.trim(), rule };
}

export function playlistDialog(props: DialogProps): UiNode {
  const { draft, setDraft } = props;
  const close = () => setDraft(null);
  return dialog(
    { hidden: () => draft() === null, onCancel: close, gap: 12, padding: 20, w: 460 },
    // Built afresh each time it opens, for a playlist or a smart one.
    each(
      () => {
        const d = draft();
        return d ? [`${d.id ?? "new"}:${d.smart}`] : [];
      },
      (k) => k,
      () => form(props, close),
    ),
  );
}

function form(props: DialogProps, close: () => void): UiNode {
  const { draft, setDraft } = props;
  const d = () => draft();
  const edit = (patch: Partial<Draft>) => {
    const now = d();
    if (now) setDraft({ ...now, ...patch });
  };
  const setRule = (patch: Partial<Rule>) => {
    const now = d();
    if (!now) return;
    const rule: Rule = { ...now.rule, ...patch };
    for (const k of Object.keys(rule) as (keyof Rule)[]) if (rule[k] === undefined) delete rule[k];
    setDraft({ ...now, rule });
  };
  const save = () => {
    const now = d();
    if (!now || now.name.trim() === "") return;
    props.save(finish(now, props.artists(), props.genres()));
  };
  const smart = d()?.smart ?? false;
  const title = () => {
    const now = d();
    if (!now) return "";
    if (now.id) return now.smart ? "Edit smart playlist" : "Rename playlist";
    return now.smart ? "New smart playlist" : "New playlist";
  };
  // The name starts selected, so typing replaces it.
  const name = textControl();
  setTimeout(() => name.select(0, Array.from(d()?.name ?? "").length), 0);

  const row = (text: string, control: UiNode) =>
    hstack({ gap: 10, align: "center" }, label({ text, w: 90, color: "$muted" }), control);
  const choice = <T,>(text: string, choices: Choices<T>, get: (r: Rule) => T | undefined, set: (v: T | undefined) => void) =>
    row(
      text,
      select({
        options: choices.map(([n]) => n),
        value: () => shown(choices, get(d()?.rule ?? {})),
        onChange: (v) => set(valueOf(choices, v)),
        w: 220,
      }),
    );
  /** Says when a typed name isn't in the library, since the rule would match nothing. */
  const unknown = (typed: () => string, names: () => string[], what: string) => () => {
    const t = typed().trim();
    return t === "" || canonical(t, names()) ? "" : `No ${what} called “${t}” in the library yet.`;
  };
  const artistNote = unknown(() => d()?.artist ?? "", props.artists, "artist");
  const genreNote = unknown(() => d()?.genre ?? "", props.genres, "genre");

  return vstack(
    { gap: 12 },
    label({ text: title, size: 18, weight: "bold" }),
    row(
      "Name",
      textField({ value: () => d()?.name ?? "", onChange: (name) => edit({ name }), onSubmit: save, control: name, w: 300 }),
    ),
    ...(smart
      ? [
          choice("Rating", RATINGS, (r) => r.minRating, (minRating) => setRule({ minRating })),
          choice("Added", ADDED, (r) => r.addedWithinDays, (addedWithinDays) => setRule({ addedWithinDays })),
          choice("Played", PLAYED, (r) => r.played, (played) => setRule({ played })),
          row("Artist", textField({ placeholder: "Any artist", value: () => d()?.artist ?? "", onChange: (artist) => edit({ artist }), w: 300 })),
          label({ text: artistNote, color: "$muted", size: 12, hidden: () => artistNote() === "" }),
          row("Genre", textField({ placeholder: "Any genre", value: () => d()?.genre ?? "", onChange: (genre) => edit({ genre }), w: 300 })),
          label({ text: genreNote, color: "$muted", size: 12, hidden: () => genreNote() === "" }),
          choice("Order", ORDERS, (r) => r.order, (order) => setRule({ order })),
          choice("At most", LIMITS, (r) => r.limit, (limit) => setRule({ limit })),
        ]
      : []),
    hstack(
      { gap: 8, justify: "end", padding: [8, 0, 0, 0] },
      button({ label: "Cancel", onClick: close }),
      button({
        label: () => (d()?.id ? "Save" : "Create"),
        class: "primary",
        onClick: save,
      }),
    ),
  );
}
