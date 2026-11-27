// Version history: every change db kept, on a timeline to scrub through. Each
// version shows what that change did, next to the text as it was then. Restoring
// a version is a new change (splices from the current text to that one), so it
// shows in the history too and can be undone. Pinned versions, named, are kept
// through history compaction.

import { button, db, each, effect, hstack, label, onCleanup, scroll, signal, slider, textField, untrack, vstack, type Doc, type UiNode, type VersionInfo } from "@idealos/sdk";
import { around, changeBetween } from "./text.ts";
import { titleOf } from "./library.ts";

export interface HistoryProps {
  id: string;
  hidden: () => boolean;
  doc: () => Doc | undefined;
  /** Make the document what \`data\` says (a version's data); the change sets made. */
  restore: (data: Record<string, unknown>) => Promise<number[]>;
  fail: (e: Error) => void;
}

const body = (d: Record<string, unknown> | undefined) => (typeof d?.body === "string" ? d.body : "");

function when(ms: number): string {
  const d = new Date(ms);
  const two = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())} ${two(d.getHours())}:${two(d.getMinutes())}:${two(d.getSeconds())}`;
}

/** The app or service that made a change, briefly: org.idealos.editor is "editor". */
function who(origin: string): string {
  return origin.split(".").at(-1) ?? origin;
}

export function historyPanel(p: HistoryProps): UiNode {
  const [versions, setVersions] = signal<VersionInfo[]>([]);
  const [index, setIndex] = signal(-1);
  /** The chosen version's data and the one before's. */
  const [shown, setShown] = signal<{ data: Record<string, unknown>; before: Record<string, unknown> | undefined } | null>(null);
  const [pinName, setPinName] = signal("");
  const [restored, setRestored] = signal<number[]>([]);
  const latest = () => index() < 0 || index() >= versions().length - 1;

  // Load the history when the panel shows, and again (a second after typing
  // settles) as the document changes. Staying on the newest version follows it.
  let timer = -1;
  const load = () =>
    db.history(p.id).then((v) => {
      const wasLatest = untrack(latest);
      setVersions(v);
      if (wasLatest) setIndex(v.length - 1);
    }, p.fail);
  effect(() => {
    if (p.hidden()) return;
    p.doc()?.updated;
    clearTimeout(timer);
    timer = setTimeout(() => void load(), versions().length === 0 ? 0 : 1000);
  });
  onCleanup(() => clearTimeout(timer));

  // Read the chosen version and the one before it.
  let asked = 0;
  effect(() => {
    const v = versions();
    const i = index();
    if (p.hidden() || i < 0 || i >= v.length) return;
    const mine = ++asked;
    const prev = i > 0 ? db.at(p.id, v[i - 1].change_set) : Promise.resolve(undefined);
    Promise.all([db.at(p.id, v[i].change_set), prev]).then(([data, before]) => {
      if (mine === asked) setShown({ data, before });
    }, p.fail);
  });

  const version = () => versions()[index()];
  const pins = () => (p.doc()?.data.pins ?? {}) as Record<string, string>;
  const nameOf = (v: VersionInfo) => pins()[String(v.change_set)] ?? when(v.at);

  const describe = () => {
    const s = shown();
    if (!s) return { title: "", removed: "", added: "", before: "", after: "" };
    const was = body(s.before);
    const now = body(s.data);
    const c = changeBetween(was, now);
    const ctx = around(now, c.at, c.at + [...c.insert].length);
    const oldTitle = s.before ? titleOf({ data: s.before } as Doc) : "";
    const newTitle = titleOf({ data: s.data } as Doc);
    return {
      title: s.before && oldTitle !== newTitle ? `Title: ${oldTitle} → ${newTitle}` : "",
      removed: c.removed,
      added: c.insert,
      before: ctx.before,
      after: ctx.after,
    };
  };

  const pin = async () => {
    const v = version();
    if (!v) return;
    await db.pin(p.id, v.change_set, true);
    await db.update(p.id, { pins: { [String(v.change_set)]: pinName().trim() || when(v.at) } });
    setPinName("");
  };
  const unpin = async () => {
    const v = version();
    if (!v) return;
    await db.pin(p.id, v.change_set, false);
    await db.update(p.id, { pins: { [String(v.change_set)]: null } });
  };

  return scroll(
    { gap: 8, padding: 12, w: 380, hidden: p.hidden },
    label({ text: "History", size: 16, weight: "bold" }),
    slider({
      value: () => Math.max(0, index()),
      min: 0,
      max: () => Math.max(0, versions().length - 1),
      step: 1,
      w: "fill",
      onChange: (v) => setIndex(Math.round(v)),
    }),
    hstack(
      { gap: 6, align: "center" },
      button({ label: "‹", onClick: () => setIndex((i) => Math.max(0, i - 1)) }),
      button({ label: "›", onClick: () => setIndex((i) => Math.min(versions().length - 1, i + 1)) }),
      label({
        text: () => {
          const v = version();
          return v ? `${index() + 1} of ${versions().length} · ${when(v.at)} · ${who(v.origin)}${v.kind === "update" ? "" : ` · ${v.kind}`}` : "Loading…";
        },
        size: 12,
        color: "$muted",
        grow: 1,
      }),
    ),
    label({ text: () => describe().title, hidden: () => !describe().title, size: 13 }),
    label({ text: () => describe().before, hidden: () => !describe().before, size: 13, color: "$muted" }),
    label({ text: () => `− ${describe().removed}`, hidden: () => !describe().removed, size: 13, class: "removed" }),
    label({ text: () => `+ ${describe().added}`, hidden: () => !describe().added, size: 13, class: "added" }),
    label({ text: () => describe().after, hidden: () => !describe().after, size: 13, color: "$muted" }),
    label({ text: "The text then:", size: 12, color: "$muted" }),
    scroll({ h: 220, padding: 4 }, label({ text: () => body(shown()?.data) || "(empty)", class: "version" })),
    hstack(
      { gap: 8 },
      button({
        label: "Restore this version",
        class: "primary",
        hidden: latest,
        onClick: () => {
          const s = shown();
          if (s) void p.restore(s.data).then(setRestored, p.fail);
        },
      }),
      button({
        label: "Undo the restore",
        hidden: () => restored().length === 0,
        onClick: async () => {
          for (const set of [...restored()].reverse()) await db.undo(set).catch(p.fail);
          setRestored([]);
        },
      }),
    ),
    hstack(
      { gap: 8, align: "center" },
      textField({ placeholder: "Name to pin it by (draft sent)", value: pinName, onChange: setPinName, onSubmit: () => void pin().catch(p.fail), grow: 1 }),
      button({ label: () => (version()?.pinned ? "Unpin" : "Pin"), onClick: () => void (version()?.pinned ? unpin() : pin()).catch(p.fail) }),
    ),
    label({ text: "Pinned", size: 12, color: "$muted", hidden: () => !versions().some((v) => v.pinned) }),
    pinnedList(versions, nameOf, setIndex),
  );
}

function pinnedList(versions: () => VersionInfo[], nameOf: (v: VersionInfo) => string, choose: (i: number) => void): UiNode {
  return each(
    () => versions().map((v, i) => ({ v, i })).filter(({ v }) => v.pinned),
    ({ v }) => String(v.change_set),
    (row) => button({ label: () => nameOf(row().v), w: "fill", onClick: () => choose(row().i) }),
    { gap: 4 },
  );
}
