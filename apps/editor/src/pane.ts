// One open document: a toolbar, its title, format and tags, the find bar, the
// body (bound to the document), a preview or history beside it, and a status line
// with counts and the caret's line and column. Focus mode hides all but the body
// and the status line.

import { button, db, effect, hstack, label, menu, onCleanup, select, signal, textArea, textControl, textField, untrack, vstack, type Doc, type UiNode } from "@idealos/sdk";
import { findBar } from "./find.ts";
import { historyPanel } from "./history.ts";
import { formatName, formatNamed, formatOf, FORMATS, parseTags, tagsOf, titleOf } from "./library.ts";
import { blobRefs, docLinks } from "./markdown.ts";
import { picker, type Choice } from "./picker.ts";
import { preview } from "./preview.ts";
import { changeBetween, counts, lineCol, onOwnLine, spliced, type Range } from "./text.ts";

export type Mode = "sans" | "serif" | "mono";
export type Panel = "none" | "preview" | "history";

const MODES: readonly (readonly [Mode, string])[] = [
  ["sans", "Sans"],
  ["serif", "Serif"],
  ["mono", "Monospace"],
];
const LINK = "Link to a document…";
const IMAGE = "Image…";

export interface PaneProps {
  id: string;
  doc: () => Doc | undefined;
  docs: () => Doc[];
  mode: () => Mode;
  setMode: (m: Mode) => void;
  panel: () => Panel;
  setPanel: (p: Panel) => void;
  focusMode: () => boolean;
  setFocusMode: (on: boolean) => void;
  remove: (id: string) => void;
  open: (id: string) => void;
  fail: (e: Error) => void;
}

const str = (v: unknown) => (typeof v === "string" ? v : "");
const same = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x, i) => x === b[i]);

export function documentPane(p: PaneProps): UiNode {
  const { id, fail } = p;
  const [text, setText] = signal(str(untrack(p.doc)?.data.body));
  const [selection, setSelection] = signal<Range>([0, 0]);
  const [finding, setFinding] = signal(false);
  const [picking, setPicking] = signal<"link" | "image" | null>(null);
  const [images, setImages] = signal<Choice[]>([]);
  const body = textControl();
  const findField = textControl();
  const markdown = () => {
    const d = p.doc();
    return d !== undefined && formatOf(d) === "markdown";
  };
  const plain = () => p.focusMode();

  // The body as shown: its text area reports each change, its own and others'.
  // Changes the app makes wait for the text area to show them.
  const waiting: { want: string; done: () => void }[] = [];
  const onBody = (t: string) => {
    setText(t);
    for (const w of waiting.filter((w) => w.want === t)) {
      waiting.splice(waiting.indexOf(w), 1);
      w.done();
    }
  };
  const shows = (want: string) =>
    new Promise<void>((resolve) => {
      if (untrack(text) === want) return resolve();
      waiting.push({ want, done: resolve });
      setTimeout(() => resolve(), 2000); // someone else changed it meanwhile
    });

  /** Replace ranges of the body, last first so earlier positions hold. One
   * Command-Z undoes them all. */
  const replace = async (ranges: readonly Range[], with_: string) => {
    let want = untrack(text);
    const sets: number[] = [];
    for (const [s, e] of [...ranges].reverse()) {
      sets.push(await db.splice(id, "body", s, e - s, with_));
      want = spliced(want, s, e - s, with_);
    }
    body.addUndo(sets);
    await shows(want);
  };

  /** Type \`s\` over the body's selection (on a line of its own with \`ownLine\`),
   * then go back to the body. */
  const insert = async (piece: string, ownLine = false) => {
    const [a, b] = selection();
    const [start, end] = [Math.min(a, b), Math.max(a, b)];
    const s = ownLine ? onOwnLine(untrack(text), start, end, piece) : piece;
    body.addUndo([await db.splice(id, "body", start, end - start, s)]);
    await shows(spliced(untrack(text), start, end - start, s));
    const after = start + [...s].length;
    body.select(after, after);
    body.focus();
  };

  /** Make the document what a version's data says, as splices (undoable). */
  const restore = async (data: Record<string, unknown>) => {
    const now = (await db.get(id)).data;
    const sets: number[] = [];
    for (const path of ["title", "body"]) {
      const c = changeBetween(str(now[path]), str(data[path]));
      if (c.delete > 0 || c.insert !== "") sets.push(await db.splice(id, path, c.at, c.delete, c.insert));
    }
    const patch: Record<string, unknown> = {};
    if ((data.format ?? "plain") !== (now.format ?? "plain")) patch.format = data.format ?? "plain";
    if (JSON.stringify(data.tags ?? []) !== JSON.stringify(now.tags ?? [])) patch.tags = data.tags ?? [];
    if (Object.keys(patch).length > 0) await db.update(id, patch);
    // Command-Z in the body undoes the whole restore, title included.
    body.addUndo(sets);
    return sets;
  };

  // Keep the document's links and images in step with what the body refers to,
  // a moment after typing settles, so backlinks work and the blobs are kept.
  let refsTimer = -1;
  effect(() => {
    const t = text();
    clearTimeout(refsTimer);
    refsTimer = setTimeout(() => {
      const d = untrack(p.doc);
      if (!d) return;
      const links = docLinks(t).filter((l) => l !== id);
      const blobs = blobRefs(t);
      const had = (key: string, inner: string) =>
        (Array.isArray(d.data[key]) ? (d.data[key] as Record<string, unknown>[]) : []).map((x) => str(x?.[inner]));
      if (same(links, had("links", "$link")) && same(blobs, had("images", "$blob"))) return;
      db.update(id, { links: links.map((l) => ({ $link: l })), images: blobs.map((h) => ({ $blob: h })) }).catch(fail);
    }, 800);
  });
  onCleanup(() => clearTimeout(refsTimer));

  // Counts a moment after typing settles; the caret's place at once.
  const [count, setCount] = signal(counts(untrack(text)));
  let countTimer = -1;
  effect(() => {
    const t = text();
    clearTimeout(countTimer);
    countTimer = setTimeout(() => setCount(counts(t)), 150);
  });
  onCleanup(() => clearTimeout(countTimer));
  const status = () => {
    const { words, chars } = count();
    const { line, col } = lineCol(text(), selection()[1]);
    const [a, b] = selection();
    const picked = Math.abs(b - a);
    const s = (n: number, one: string) => `${n.toLocaleString()} ${one}${n === 1 ? "" : "s"}`;
    return `${s(words, "word")} · ${s(chars, "character")} · Line ${line}, Column ${col}${picked ? ` · ${s(picked, "character")} selected` : ""}`;
  };

  const togglePanel = (which: Panel) => p.setPanel(p.panel() === which ? "none" : which);
  const openPicker = (which: "link" | "image") => {
    setPicking(which);
    if (which === "image")
      db.query({ type: "Image", limit: 500 }).then(
        (found) =>
          setImages(
            found.flatMap((d) => {
              const hash = (d.data.blob as { $blob?: string } | undefined)?.$blob;
              const w = d.data.width;
              const caption = str(d.data.caption) || (typeof w === "number" ? `Image, ${w} × ${d.data.height}` : `Image ${hash?.slice(0, 8)}`);
              return hash ? [{ key: `${hash} ${caption}`, label: caption }] : [];
            }),
          ),
        fail,
      );
  };

  const toolbar = hstack(
    { gap: 8, align: "center", hidden: plain },
    button({
      label: "Find",
      onClick: () => {
        setFinding(true);
        findField.focus();
      },
    }),
    button({ label: "Preview", hidden: () => !markdown(), class: () => (p.panel() === "preview" ? "primary" : ""), onClick: () => togglePanel("preview") }),
    button({ label: "History", class: () => (p.panel() === "history" ? "primary" : ""), onClick: () => togglePanel("history") }),
    menu({ label: "Insert", options: [LINK, IMAGE], hidden: () => !markdown(), onChoose: (o) => openPicker(o === LINK ? "link" : "image") }),
    select({
      options: MODES.map(([, n]) => n),
      value: () => MODES.find(([m]) => m === p.mode())?.[1] ?? "Sans",
      onChange: (v) => p.setMode(MODES.find(([, n]) => n === v)?.[0] ?? "sans"),
    }),
    label({ text: "", grow: 1 }),
    button({ label: "Focus", onClick: () => p.setFocusMode(true) }),
  );

  const heading = hstack(
    { gap: 8, align: "center", hidden: plain },
    textField({ bind: { doc: id, path: "title" }, placeholder: "Untitled", size: 18, grow: 1 }),
    select({
      options: FORMATS.map(([, name]) => name),
      value: () => {
        const d = p.doc();
        return formatName(d ? formatOf(d) : "plain");
      },
      onChange: (v) => {
        const f = formatNamed(v);
        if (f) void db.update(id, { format: f }).catch(fail);
      },
    }),
    button({ label: "Delete", onClick: () => p.remove(id) }),
  );

  const tags = textField({
    placeholder: "Tags, separated by commas (Enter sets them)",
    hidden: plain,
    value: () => {
      const d = p.doc();
      return d ? tagsOf(d).join(", ") : "";
    },
    onSubmit: (v) => void db.update(id, { tags: parseTags(v) }).catch(fail),
  });

  const find = findBar({
    hidden: () => !finding(),
    text,
    selection,
    body,
    replace,
    control: findField,
    onClose: () => {
      setFinding(false);
      body.focus();
    },
  });

  const editor = hstack(
    { grow: 1, justify: "center", align: "stretch" },
    textArea({
      bind: { doc: id, path: "body" },
      placeholder: "Write here. It's saved as you go.",
      class: () => `write-${p.mode()}${p.focusMode() ? "-big" : ""}`,
      h: "fill",
      grow: 1,
      max_w: () => (p.focusMode() ? 760 : 100000),
      control: body,
      onChange: onBody,
      onSelect: setSelection,
    }),
  );

  const docChoices = () =>
    p.docs().filter((d) => d.id !== id).map((d) => ({ key: d.id, label: titleOf(d) }));

  return vstack(
    { gap: 8, padding: 12, grow: 1 },
    toolbar,
    heading,
    tags,
    find,
    hstack(
      { grow: 1, gap: 12, align: "stretch" },
      editor,
      preview({ id, hidden: () => plain() || !markdown() || p.panel() !== "preview", text, doc: p.doc, docs: p.docs, open: p.open, fail }),
      historyPanel({ id, hidden: () => plain() || p.panel() !== "history", doc: p.doc, restore, fail }),
    ),
    hstack(
      { gap: 8, align: "center" },
      label({ text: status, size: 12, color: "$muted", grow: 1 }),
      button({ label: "Leave focus mode", hidden: () => !p.focusMode(), onClick: () => p.setFocusMode(false) }),
    ),
    picker({
      title: () => (picking() === "image" ? "Show an image" : "Link to a document"),
      shown: () => picking() !== null,
      choices: () => (picking() === "image" ? images() : docChoices()),
      empty: () => (picking() === "image" ? "No images stored yet." : "No other documents yet."),
      onCancel: () => setPicking(null),
      onPick: (key) => {
        const which = picking();
        setPicking(null);
        if (which === "link") {
          const d = p.docs().find((x) => x.id === key);
          void insert(`[${d ? titleOf(d) : "a document"}](doc:${key})`).catch(fail);
        } else {
          const [hash, ...caption] = key.split(" ");
          void insert(`![${caption.join(" ")}](blob:${hash})`, true).catch(fail);
        }
      },
    }),
  );
}
