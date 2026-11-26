// Documents: browse the database. Pick a type, search, narrow with filters, and
// inspect a document: its fields, its links (followable both ways) and its
// version history (any version can be shown as it was).
//
// It reads every type (its one grant) and changes nothing.

import {
  button,
  db,
  each,
  effect,
  hstack,
  label,
  openWindow,
  request,
  scroll,
  select,
  signal,
  split,
  textField,
  untrack,
  vstack,
  type Doc,
  type HistoryResult,
  type LinksResult,
  type TypesResult,
  type UiNode,
} from "@idealos/sdk";

type Op = "=" | "≠" | ">" | "<" | "contains" | "exists";
const OPS: Op[] = ["=", "≠", ">", "<", "contains", "exists"];
interface Filter {
  field: string;
  op: Op;
  value: string;
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Typed text as a JSON value: a number, true, false or null if it reads as one. */
function parseValue(text: string): unknown {
  const t = text.trim();
  if (t === "true") return true;
  if (t === "false") return false;
  if (t === "null") return null;
  if (t !== "" && !Number.isNaN(Number(t))) return Number(t);
  return text;
}

/** The query language's form of a filter row. */
function condition(f: Filter): Record<string, unknown> {
  const v = parseValue(f.value);
  switch (f.op) {
    case "=":
      return { [f.field]: v };
    case "≠":
      return { [f.field]: { $ne: v } };
    case ">":
      return { [f.field]: { $gt: v } };
    case "<":
      return { [f.field]: { $lt: v } };
    case "contains":
      return { [f.field]: { $contains: v } };
    case "exists":
      return { [f.field]: { $exists: v !== false } };
  }
}

/** A short title for a document: its first short string field, or its id. */
function title(d: Doc): string {
  for (const key of ["title", "name", "subject", "caption"]) {
    const v = d.data[key];
    if (typeof v === "string" && v) return v;
  }
  const first = Object.values(d.data).find((v) => typeof v === "string" && v.length > 0 && v.length < 80);
  return typeof first === "string" ? first : d.id;
}

function show(v: unknown): string {
  const text = typeof v === "string" ? v : JSON.stringify(v);
  return text.length > 300 ? `${text.slice(0, 300)}…` : text;
}

function when(ms: number): string {
  return new Date(ms).toISOString().replace("T", " ").slice(0, 19);
}

function small(text: string | (() => string)): UiNode {
  return label({ text, size: 12, color: "$muted" });
}

await openWindow({ title: "Documents", width: 980, height: 620 }, () => {
  const [types, setTypes] = signal<[string, number][]>([]);
  const [type, setType] = signal<string | null>(null);
  const [search, setSearch] = signal("");
  const [filters, setFilters] = signal<Filter[]>([]);
  const [draft, setDraft] = signal<Filter>({ field: "", op: "=", value: "" });
  const [results, setResults] = signal<Doc[]>([]);
  const [message, setMessage] = signal("");
  const [doc, setDoc] = signal<Doc | null>(null);
  const [shownData, setShownData] = signal<Record<string, unknown> | null>(null);
  const [version, setVersion] = signal<number | null>(null);
  const [links, setLinks] = signal<LinksResult | null>(null);
  const [history, setHistory] = signal<HistoryResult["versions"]>([]);

  const loadTypes = async () => {
    try {
      const t = await request<TypesResult>("db", "types", null);
      setTypes(Object.entries(t.types).sort((a, b) => a[0].localeCompare(b[0])));
    } catch (e) {
      setMessage(errorText(e));
    }
  };

  const run = async () => {
    const t = type();
    const words = search().trim();
    try {
      let docs: Doc[];
      if (words) {
        docs = await db.search(words, t ? { type: t, limit: 100 } : { limit: 100 });
        // Filters narrow a search too, by checking each result.
        const fs = filters();
        if (fs.length > 0 && t) {
          const allowed = new Set((await db.query({ type: t, where: { $and: fs.map(condition) }, limit: 10000 })).map((d) => d.id));
          docs = docs.filter((d) => allowed.has(d.id));
        }
      } else if (t) {
        const where = filters().length > 0 ? { $and: filters().map(condition) } : {};
        docs = await db.query({ type: t, where, orderBy: [["updated", "desc"]], limit: 100 });
      } else {
        docs = [];
      }
      setResults(docs);
      setMessage(docs.length === 100 ? "Showing the first 100." : `${docs.length} found.`);
    } catch (e) {
      setMessage(errorText(e));
    }
  };

  const open = async (id: string) => {
    try {
      const d = await db.get(id);
      setDoc(d);
      setShownData(d.data);
      setVersion(null);
      setLinks(await request<LinksResult>("db", "links", { id }));
      setHistory((await request<HistoryResult>("db", "history", { id })).versions);
    } catch (e) {
      setMessage(errorText(e));
    }
  };

  const showVersion = async (changeSet: number) => {
    const d = doc();
    if (!d) return;
    try {
      const at = await request<{ data: Record<string, unknown> }>("db", "at", { id: d.id, change_set: changeSet });
      setShownData(at.data);
      setVersion(changeSet);
    } catch (e) {
      setMessage(errorText(e));
    }
  };

  effect(() => {
    type();
    filters();
    untrack(() => void run());
  });
  void loadTypes();

  // Types, with counts.
  const typeList = scroll(
    { padding: 10, gap: 4 },
    label({ text: "Types", weight: "bold", size: 13, color: "$muted" }),
    each(
      types,
      ([t]) => t,
      (entry) =>
        button({
          label: () => `${entry()[0]}  ${entry()[1]}`,
          w: "fill",
          color: () => (type() === entry()[0] ? "$accent" : "$control"),
          text_color: () => (type() === entry()[0] ? "$accent-text" : "$text"),
          onClick: () => {
            setType(entry()[0]);
            setFilters([]);
          },
        }),
      { gap: 4 },
    ),
    button({ label: "Refresh", onClick: () => void loadTypes() }),
  );

  // Search, filters, results.
  const middle = vstack(
    { padding: 10, gap: 8, grow: 1 },
    hstack(
      { gap: 8 },
      textField({ placeholder: "Search every document", value: search, onChange: setSearch, onSubmit: () => void run(), grow: 1 }),
      button({ label: "Search", onClick: () => void run() }),
    ),
    hstack(
      { gap: 6, align: "center", hidden: () => type() === null },
      textField({ placeholder: "field", w: 110, value: () => draft().field, onChange: (v) => setDraft({ ...draft(), field: v }) }),
      select({ options: OPS, value: () => draft().op, onChange: (v) => setDraft({ ...draft(), op: v as Op }) }),
      textField({ placeholder: "value", w: 110, value: () => draft().value, onChange: (v) => setDraft({ ...draft(), value: v }) }),
      button({
        label: "Add filter",
        onClick: () => {
          if (draft().field.trim()) {
            setFilters([...filters(), { ...draft(), field: draft().field.trim() }]);
            setDraft({ field: "", op: "=", value: "" });
          }
        },
      }),
    ),
    each(
      () => filters().map((f, i) => ({ f, i })),
      ({ f, i }) => `${i}:${f.field}${f.op}${f.value}`,
      (item) =>
        hstack(
          { gap: 6, align: "center" },
          small(() => `${item().f.field} ${item().f.op} ${item().f.op === "exists" ? "" : item().f.value}`),
          button({ label: "Remove", onClick: () => setFilters(filters().filter((_, j) => j !== item().i)) }),
        ),
      { gap: 4 },
    ),
    small(message),
    scroll(
      { grow: 1, gap: 4 },
      each(
        results,
        (d) => d.id,
        (d) =>
          button({
            label: () => `${d().type}: ${title(d())}`,
            w: "fill",
            color: () => (doc()?.id === d().id ? "$selection" : "$control"),
            onClick: () => void open(d().id),
          }),
        { gap: 4 },
      ),
    ),
  );

  // The inspector.
  const fields = () => Object.entries(shownData() ?? {});
  const inspector = scroll(
    { padding: 10, gap: 8 },
    label({ text: () => (doc() ? `${doc()!.type} ${doc()!.id}` : "Pick a document"), weight: "bold" }),
    small(() => (doc() ? `Created ${when(doc()!.created)}, changed ${when(doc()!.updated)}` : "")),
    small(() => (version() !== null ? `As it was after change ${version()}` : "")),
    button({
      label: "Back to now",
      hidden: () => version() === null,
      onClick: () => {
        const d = doc();
        if (d) {
          setShownData(d.data);
          setVersion(null);
        }
      },
    }),
    each(
      fields,
      ([k]) => k,
      (kv) =>
        vstack(
          { gap: 2 },
          label({ text: () => kv()[0], size: 12, weight: "bold" }),
          label({ text: () => show(kv()[1]) }),
        ),
      { gap: 6 },
    ),
    label({ text: "Links", weight: "bold", size: 13, color: "$muted", hidden: () => !links() }),
    each(
      () => links()?.outgoing ?? [],
      (l) => `out:${l.path}`,
      (l) => button({ label: () => `${l().path} → ${l().id}${l().exists ? "" : " (gone)"}`, onClick: () => l().exists && void open(l().id) }),
      { gap: 4 },
    ),
    each(
      () => links()?.incoming ?? [],
      (b) => `in:${b.id}:${b.path}`,
      (b) => button({ label: () => `${b().type} ${b().id} (${b().path}) → here`, onClick: () => void open(b().id) }),
      { gap: 4 },
    ),
    label({ text: "History", weight: "bold", size: 13, color: "$muted", hidden: () => history().length === 0 }),
    each(
      history,
      (v) => String(v.change_set),
      (v) =>
        button({
          label: () => `${when(v().at)}  ${v().kind} by ${v().origin}`,
          onClick: () => void showVersion(v().change_set),
        }),
      { gap: 4 },
    ),
  );

  return split({ ratio: 0.2, grow: 1 }, typeList, split({ ratio: 0.5 }, middle, inspector));
});
