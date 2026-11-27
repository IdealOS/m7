// Editor: a place to write. Documents are TextDocument records, so there are no
// files and no Save button: the title and body are bound to the document, so each
// keystroke goes straight to the database (as collaborative text), its history
// keeps every version, and another window on the same document follows along.
//
// On the left, the documents, most recently changed first, with a search (every
// word must match, anywhere in the title or body) and filters by format and tag.
// On the right, the one chosen (pane.ts): find and replace, counts, version
// history, a Markdown preview, writing modes and a focus mode. Deleting can be
// undone: db keeps the document's history, and undoing the delete's change set
// brings it back.

import style from "./style.css";
import { button, db, each, effect, hstack, label, memo, openWindow, scroll, select, signal, split, textField, vstack } from "@idealos/sdk";
import { allTags, formatName, formatNamed, FORMATS, nextAfter, shown, titleOf, TYPE, type Format } from "./library.ts";
import { documentPane, type Mode, type Panel } from "./pane.ts";

const ANY_FORMAT = "Any format";
const ANY_TAG = "Any tag";

await openWindow({ title: "Editor", width: 1100, height: 700, style }, () => {
  const docs = db.live({ type: TYPE, orderBy: [["$updated", "desc"]], limit: 1000 });
  const [selected, setSelected] = signal<string | null>(null);
  const [words, setWords] = signal("");
  const [found, setFound] = signal<ReadonlySet<string> | null>(null);
  const [format, setFormat] = signal<Format | null>(null);
  const [tag, setTag] = signal<string | null>(null);
  const [deleted, setDeleted] = signal<{ title: string; id: string; changeSet: number } | null>(null);
  const [problem, setProblem] = signal("");
  // How the editor looks, kept as documents change.
  const [mode, setMode] = signal<Mode>("sans");
  const [panel, setPanel] = signal<Panel>("none");
  const [focusMode, setFocusMode] = signal(false);

  const visible = memo(() => shown(docs(), { found: found(), format: format(), tag: tag() }));
  const current = () => docs().find((d) => d.id === selected());
  const fail = (e: Error) => setProblem(e.message);

  // Open the most recent document once the list first arrives.
  let opened = false;
  effect(() => {
    if (opened || !docs.ready()) return;
    opened = true;
    setSelected(docs()[0]?.id ?? null);
  });

  // Search again when the words or the documents change; only the newest answer counts.
  let asked = 0;
  effect(() => {
    const text = words().trim();
    docs();
    const mine = ++asked;
    if (!text) {
      setFound(null);
      return;
    }
    db.search(text, { type: TYPE, limit: 1000 }).then(
      (r) => mine === asked && setFound(new Set(r.map((d) => d.id))),
      (e: Error) => mine === asked && fail(e),
    );
  });

  const create = async () => {
    // In the format being shown, if one is; then clear the filters so it shows.
    const d = await db.put(TYPE, { title: "", body: "", format: format() ?? "plain", tags: [] });
    setWords("");
    setFormat(null);
    setTag(null);
    setSelected(d.id);
  };

  const remove = async (id: string) => {
    const d = docs().find((x) => x.id === id);
    const next = nextAfter(visible(), id);
    const changeSet = await db.remove(id);
    setDeleted({ title: d ? titleOf(d) : "Untitled", id, changeSet });
    setSelected(next);
  };

  const restore = async () => {
    const d = deleted();
    if (!d) return;
    await db.undo(d.changeSet);
    setDeleted(null);
    setSelected(d.id);
  };

  const sidebar = vstack(
    { gap: 8, padding: 10, hidden: focusMode },
    hstack(
      { gap: 8 },
      button({ label: "New", class: "primary", onClick: () => void create().catch(fail) }),
      textField({ placeholder: "Search", value: words, onChange: setWords, grow: 1 }),
    ),
    hstack(
      { gap: 8 },
      select({
        options: [ANY_FORMAT, ...FORMATS.map(([, name]) => name)],
        value: () => {
          const f = format();
          return f ? formatName(f) : ANY_FORMAT;
        },
        onChange: (v) => setFormat(formatNamed(v) ?? null),
      }),
      select({
        options: () => [ANY_TAG, ...allTags(docs())],
        value: () => tag() ?? ANY_TAG,
        onChange: (v) => setTag(v === ANY_TAG ? null : v),
      }),
    ),
    scroll(
      { grow: 1 },
      each(
        visible,
        (d) => d.id,
        (d) =>
          button({
            label: () => titleOf(d()),
            w: "fill",
            class: () => (selected() === d().id ? "primary" : ""),
            onClick: () => setSelected(d().id),
          }),
        { gap: 2 },
      ),
      label({
        text: () => (!docs.ready() ? "Loading…" : docs().length === 0 ? "No documents yet. New makes one." : "Nothing matches."),
        color: "$muted",
        hidden: () => docs.ready() && visible().length > 0,
      }),
    ),
    label({
      text: () => {
        const n = docs().length;
        const v = visible().length;
        return v === n ? `${n} document${n === 1 ? "" : "s"}` : `${v} of ${n} shown`;
      },
      size: 12,
      color: "$muted",
    }),
  );

  const main = vstack(
    { grow: 1 },
    hstack(
      { gap: 8, align: "center", padding: [6, 12], hidden: () => !deleted() || focusMode() },
      label({ text: () => `Deleted “${deleted()?.title ?? ""}”`, grow: 1 }),
      button({ label: "Undo", onClick: () => void restore().catch(fail) }),
      button({ label: "Dismiss", onClick: () => setDeleted(null) }),
    ),
    label({ text: problem, class: "err", hidden: () => !problem() }),
    each(
      () => {
        const d = current();
        return d ? [d.id] : [];
      },
      (id) => id,
      (id) =>
        documentPane({
          id: id(),
          doc: current,
          docs,
          mode,
          setMode,
          panel,
          setPanel,
          focusMode,
          setFocusMode,
          remove: (doc) => void remove(doc).catch(fail),
          open: setSelected,
          fail,
        }),
      { grow: 1 },
    ),
    label({
      text: () => (docs().length === 0 ? "New makes a document." : "Choose a document on the left."),
      color: "$muted",
      hidden: () => !docs.ready() || current() !== undefined,
    }),
  );

  return split({ ratio: 0.3, grow: 1 }, sidebar, main);
});
