// A Markdown document's preview, beside its text: each block as widgets, with
// the documents it links to as buttons to open them, and the documents that link
// to it ("Linked from") at the end. Inline marks are shown as plain text until
// text widgets can mix styles in a run.

import { button, db, each, effect, hstack, image, label, onCleanup, rect, scroll, signal, vstack, type Doc, type UiNode } from "@idealos/sdk";
import { parse, type Block, type Link } from "./markdown.ts";
import { titleOf, TYPE } from "./library.ts";

export interface PreviewProps {
  id: string;
  hidden: () => boolean;
  text: () => string;
  doc: () => Doc | undefined;
  docs: () => Doc[];
  open: (id: string) => void;
  fail: (e: Error) => void;
}

const SIZES = [26, 22, 19, 17, 15, 14];

export function preview(p: PreviewProps): UiNode {
  // Parse a moment after typing settles, not on every key.
  const [blocks, setBlocks] = signal<Block[]>([]);
  let timer = -1;
  effect(() => {
    const text = p.text();
    if (p.hidden()) return;
    clearTimeout(timer);
    timer = setTimeout(() => setBlocks(parse(text)), blocks().length === 0 ? 0 : 250);
  });
  onCleanup(() => clearTimeout(timer));

  // What links here, asked again when this document changes.
  const [backlinks, setBacklinks] = signal<string[]>([]);
  effect(() => {
    if (p.hidden()) return;
    p.doc()?.updated;
    db.links(p.id).then((l) => setBacklinks([...new Set(l.incoming.filter((b) => b.type === TYPE).map((b) => b.id))]), p.fail);
  });

  const titleFor = (id: string) => {
    const d = p.docs().find((x) => x.id === id);
    return d ? titleOf(d) : "a document that's gone";
  };
  const docLink = (l: Link) => (l.target.startsWith("doc:") ? l.target.slice(4) : null);

  const linkButtons = (links: () => Link[]) =>
    each(
      () => links().flatMap((l) => (docLink(l) ? [docLink(l)!] : [])),
      (id) => id,
      (id) => button({ label: () => `→ ${titleFor(id())}`, onClick: () => p.open(id()) }),
      { gap: 4 },
    );

  const render = (b: () => Block): UiNode => {
    const first = b();
    const text = () => ("text" in b() ? (b() as { text: string }).text : "");
    const links = () => ("links" in b() ? (b() as { links: Link[] }).links : []);
    let main: UiNode;
    switch (first.kind) {
      case "heading":
        main = label({ text, weight: "bold", size: () => SIZES[(b() as { level: number }).level - 1] ?? 14 });
        break;
      case "paragraph":
        main = label({ text, weight: () => ((b() as { strong: boolean }).strong ? "bold" : "normal") });
        break;
      case "item":
        main = hstack(
          { gap: 6 },
          rect({ w: () => (b() as { depth: number }).depth * 18, h: 1 }),
          label({ text: () => (b() as { marker: string }).marker, color: "$muted" }),
          label({ text, grow: 1 }),
        );
        break;
      case "quote":
        main = hstack({ gap: 10 }, rect({ w: 3, h: "fill", color: "$muted", radius: 1 }), label({ text, color: "$muted", grow: 1 }));
        break;
      case "code":
        main = label({ text, class: "mono" });
        break;
      case "image":
        main = image({ blob: () => (b() as { blob: string }).blob, radius: 4 });
        break;
      case "rule":
        main = rect({ h: 1, w: "fill", color: "$muted" });
        break;
    }
    return vstack({ gap: 4 }, main, linkButtons(links));
  };

  return scroll(
    { padding: 16, gap: 10, w: 380, hidden: p.hidden },
    // Rows by position and kind: an edit inside a block updates its row in place.
    each(() => blocks().map((b, i) => ({ key: `${i}:${b.kind}`, b })), (r) => r.key, (r) => render(() => r().b), { gap: 10 }),
    label({ text: "Nothing to preview yet.", color: "$muted", hidden: () => blocks().length > 0 }),
    vstack(
      { gap: 4, hidden: () => backlinks().length === 0 },
      label({ text: "Linked from", size: 12, color: "$muted" }),
      each(backlinks, (id) => id, (id) => button({ label: () => `← ${titleFor(id())}`, onClick: () => p.open(id()) }), { gap: 4 }),
    ),
  );
}
