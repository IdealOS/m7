// The SDK against a fake app-host: signals, live queries and windows, checked by
// the requests the SDK sends and the stream parts it's given.

import assert from "node:assert/strict";
import { test } from "node:test";

import { applyWatch } from "../src/db.ts";
import type { Doc, SceneOp } from "../src/protocol.ts";
import {
  batch,
  button,
  checkbox,
  clipboard,
  db,
  each,
  effect,
  label,
  memo,
  menu,
  openWindow,
  root,
  setClipboard,
  signal,
  textField,
  virtualList,
  vstack,
} from "../src/index.ts";

type Stream = { to: string; method: string; params: unknown; part: (p: unknown) => void; end: (e?: Error) => void; cancelled: boolean };

/** Stands in for app-host's `__idealos`. */
class FakeHost {
  requests: { to: string; method: string; params: unknown }[] = [];
  streams: Stream[] = [];
  answer: (to: string, method: string, params: unknown) => unknown = () => null;

  request(to: string, method: string, params: unknown) {
    this.requests.push({ to, method, params });
    return Promise.resolve(this.answer(to, method, params));
  }
  stream(to: string, method: string, params: unknown, part: (p: unknown) => void, end: (e?: Error) => void) {
    const s: Stream = { to, method, params, part, end, cancelled: false };
    this.streams.push(s);
    return () => {
      s.cancelled = true;
    };
  }
  exit() {}
  /** The ops of every commit so far, in order. */
  commits(): SceneOp[][] {
    return this.requests.filter((r) => r.method === "commit").map((r) => (r.params as { ops: SceneOp[] }).ops);
  }
}

function fresh(): FakeHost {
  const host = new FakeHost();
  (globalThis as { __idealos?: unknown }).__idealos = host;
  return host;
}

const tick = () => new Promise((r) => setTimeout(r, 0));

const doc = (id: string, data: Record<string, unknown> = {}): Doc => ({ id, type: "Note", created: 1, updated: 1, data });

test("effects re-run when what they read changes, and track dependencies afresh", () => {
  const [a, setA] = signal(1);
  const [b, setB] = signal(10);
  const [useA, setUseA] = signal(true);
  const seen: number[] = [];
  effect(() => seen.push(useA() ? a() : b()));
  setA(2);
  setB(20); // not read yet: no run
  setUseA(false);
  setA(3); // no longer read: no run
  setB(30);
  assert.deepEqual(seen, [1, 2, 20, 30]);
  setB(30); // same value: no run
  assert.equal(seen.length, 4);
});

test("batch folds writes into one run, and memo follows its inputs", () => {
  const [x, setX] = signal(1);
  const [y, setY] = signal(2);
  const sum = memo(() => x() + y());
  const runs: number[] = [];
  effect(() => runs.push(sum()));
  batch(() => {
    setX(5);
    setY(6);
  });
  assert.deepEqual(runs, [3, 11]);
});

test("disposing a root stops its effects", () => {
  const [n, setN] = signal(0);
  const seen: number[] = [];
  const dispose = root((d) => {
    effect(() => seen.push(n()));
    return d;
  });
  setN(1);
  dispose();
  setN(2);
  assert.deepEqual(seen, [0, 1]);
});

test("watch parts are applied in the documented way", () => {
  let list = applyWatch([], { kind: "initial", docs: [doc("a"), doc("b"), doc("c")] });
  list = applyWatch(list, {
    kind: "diff",
    added: [doc("d")],
    changed: [doc("b", { v: 2 })],
    removed: ["a"],
    order: ["d", "c", "b"],
  });
  assert.deepEqual(list.map((d) => d.id), ["d", "c", "b"]);
  assert.deepEqual(list[2].data, { v: 2 });
  list = applyWatch(list, { kind: "diff", added: [], changed: [], removed: ["c"] });
  assert.deepEqual(list.map((d) => d.id), ["d", "b"]);
});

test("a live query follows its watch stream", () => {
  const host = fresh();
  const notes = db.live({ type: "Note" });
  const seen: string[][] = [];
  effect(() => seen.push(notes().map((d) => d.id)));
  const s = host.streams[0];
  assert.deepEqual([s.to, s.method, s.params], ["db", "watch", { type: "Note" }]);
  assert.equal(notes.ready(), false);
  s.part({ kind: "initial", docs: [doc("a")] });
  s.part({ kind: "diff", added: [doc("b")], changed: [], removed: [], order: ["a", "b"] });
  assert.equal(notes.ready(), true);
  assert.deepEqual(seen, [[], ["a"], ["a", "b"]]);
  notes.close();
  assert.ok(s.cancelled);
});

test("a window is built once, committed per microtask, and clicks reach handlers", async () => {
  const host = fresh();
  const opened = openWindow({ title: "Counter", width: 300 }, () => {
    const [count, setCount] = signal(0);
    return vstack(
      { padding: 16, gap: 12 },
      label({ text: () => `Count: ${count()}`, size: 20 }),
      button({ label: "Add one", onClick: () => setCount((n) => n + 1) }),
    );
  });
  const s = host.streams[0];
  assert.deepEqual([s.to, s.method, s.params], ["compositor", "open_window", { title: "Counter", width: 300, height: undefined }]);
  s.part({ kind: "opened", window: 4, width: 300, height: 240 });
  const win = await opened;
  assert.equal(win.id, 4);
  await tick();

  const [first] = host.commits();
  assert.equal(host.commits().length, 1, "the whole build is one commit");
  const creates = first.filter((op) => op.op === "create");
  assert.deepEqual(creates.map((op) => op.op === "create" && op.type), ["label", "button", "vstack"]);
  const labelId = (creates[0] as { id: number }).id;
  const buttonId = (creates[1] as { id: number }).id;
  const stackId = (creates[2] as { id: number }).id;
  assert.ok(first.some((op) => op.op === "set" && op.id === labelId && op.props.text === "Count: 0"));
  assert.ok(first.some((op) => op.op === "listen" && op.id === buttonId));
  assert.ok(first.some((op) => op.op === "insert" && op.parent === 1 && op.child === stackId));
  assert.ok(first.every((op) => !("id" in op) || (op as { id: number }).id > 1), "ids above the content area");

  // Two clicks before the microtask runs are one commit with the latest text.
  s.part({ kind: "event", node: buttonId, event: "click" });
  s.part({ kind: "event", node: buttonId, event: "click" });
  await tick();
  const second = host.commits()[1];
  assert.deepEqual(second.at(-1), { op: "set", id: labelId, props: { text: "Count: 2" } });

  // The close button closes the window: the request is cancelled.
  s.part({ kind: "close" });
  assert.ok(s.cancelled);
  await win.closed;
});

test("each keeps rows in step with a list: added, kept, reordered, removed", async () => {
  const host = fresh();
  const [items, setItems] = signal([{ id: "a", t: "A" }, { id: "b", t: "B" }]);
  const opened = openWindow({ title: "List" }, () =>
    each(items, (i) => i.id, (item) => label({ text: () => item().t })),
  );
  host.streams[0].part({ kind: "opened", window: 1, width: 360, height: 240 });
  await opened;
  await tick();
  const ops = () => host.commits().flat();
  const created = (text: string) =>
    ops().find((op) => op.op === "set" && op.props.text === text) as { id: number } | undefined;
  const aId = created("A")!.id;
  const bId = created("B")!.id;

  host.requests.length = 0;
  setItems([{ id: "b", t: "B2" }, { id: "c", t: "C" }]);
  await tick();
  const update = ops();
  assert.ok(update.some((op) => op.op === "destroy" && op.id === aId), "a's row is destroyed");
  assert.ok(update.some((op) => op.op === "set" && op.id === bId && op.props.text === "B2"), "b's row is kept and updated");
  assert.ok(update.some((op) => op.op === "create" && op.type === "label"), "c gets a new row");
  const inserts = update.filter((op) => op.op === "insert").map((op) => op.op === "insert" && op.index);
  assert.deepEqual(inserts, [0, 1]);

  // The same keys in the same order: only the changed text goes out.
  host.requests.length = 0;
  setItems([{ id: "b", t: "B3" }, { id: "c", t: "C" }]);
  await tick();
  assert.deepEqual(ops(), [{ op: "set", id: bId, props: { text: "B3" } }]);
});

test("widgets hand their values to handlers, and the window follows resizes", async () => {
  const host = fresh();
  const seen: unknown[] = [];
  const opened = openWindow({ title: "Form" }, () =>
    vstack(
      {},
      checkbox({ label: "On", onChange: (v) => seen.push(["check", v]) }),
      textField({ placeholder: "Name", bind: { doc: "d1", path: "name" }, onSubmit: (v) => seen.push(["submit", v]) }),
      menu({ label: "File", options: ["New", "Open"], onChoose: (v) => seen.push(["choose", v]) }),
    ),
  );
  host.streams[0].part({ kind: "opened", window: 2, width: 300, height: 200 });
  const win = await opened;
  await tick();
  const creates = host.commits()[0].filter((op) => op.op === "create") as { id: number; type: string; props: Record<string, unknown> }[];
  const check = creates.find((c) => c.type === "checkbox")!;
  const field = creates.find((c) => c.type === "text_field")!;
  const actions = creates.find((c) => c.type === "menu")!;
  assert.deepEqual(actions.props, { label: "File", options: ["New", "Open"] }, "handlers stay in the app");
  assert.deepEqual(field.props.bind, { doc: "d1", path: "name" }, "bind goes to the compositor as is");
  assert.ok(!host.commits()[0].some((op) => op.op === "listen" && op.id === check.id), "only clicks need listening");
  host.streams[0].part({ kind: "event", node: check.id, event: "change", value: true });
  host.streams[0].part({ kind: "event", node: field.id, event: "submit", value: "Ada" });
  host.streams[0].part({ kind: "event", node: actions.id, event: "choose", value: "Open" });
  assert.deepEqual(seen, [["check", true], ["submit", "Ada"], ["choose", "Open"]]);
  assert.deepEqual(win.size(), { width: 300, height: 200 });
  host.streams[0].part({ kind: "resized", width: 640, height: 480 });
  assert.deepEqual(win.size(), { width: 640, height: 480 });
});

test("a virtual list makes rows as they come into view and drops them as they leave", async () => {
  const host = fresh();
  const made: number[] = [];
  const opened = openWindow({ title: "Long" }, () =>
    virtualList({ count: 1000, rowHeight: 20, overscan: 1 }, (i) => {
      made.push(i);
      return label({ text: `Row ${i}` });
    }),
  );
  host.streams[0].part({ kind: "opened", window: 3, width: 300, height: 200 });
  await opened;
  await tick();
  const vlist = (host.commits()[0].find((op) => op.op === "create" && op.type === "vlist") as { id: number; props: Record<string, unknown> });
  assert.equal(vlist.props.row_height, 20);
  host.streams[0].part({ kind: "event", node: vlist.id, event: "range", value: [0, 4] });
  assert.deepEqual(made, [0, 1, 2, 3, 4, 5], "visible rows plus one");
  await tick();
  const indexes = host.commits().flat().filter((op) => op.op === "set" && "index" in op.props);
  assert.equal(indexes.length, 6, "each row says its index");
  host.requests.length = 0;
  host.streams[0].part({ kind: "event", node: vlist.id, event: "range", value: [10, 14] });
  await tick();
  assert.deepEqual(made.slice(6), [9, 10, 11, 12, 13, 14, 15]);
  assert.equal(host.commits().flat().filter((op) => op.op === "destroy").length, 6, "the first rows left");
});

test("the clipboard goes through the compositor", async () => {
  const host = fresh();
  host.answer = (_, method) => (method === "get_clipboard" ? { text: "hi" } : null);
  assert.deepEqual(await clipboard(), { text: "hi" });
  await setClipboard("note", "d2");
  assert.deepEqual(host.requests.at(-1), { to: "compositor", method: "set_clipboard", params: { text: "note", doc: "d2" } });
});
