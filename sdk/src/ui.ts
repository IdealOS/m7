// Windows and their content. An app builds its UI once; props given as functions
// are reactive, and an effect sends a `set` op whenever one changes. Ops are queued
// and sent as one commit per microtask, so a burst of changes is one message.
//
//   openWindow({ title: "Counter" }, () => {
//     const [count, setCount] = signal(0);
//     return vstack({ padding: 16, gap: 12 },
//       label({ text: () => `Count: ${count()}`, size: 20 }),
//       button({ label: "Add one", onClick: () => setCount((n) => n + 1) }));
//   });
//
// A window lasts as long as its `open_window` request: closing it cancels the
// request, and the compositor removes the window when the app goes away.

import { request, stream } from "./bus.ts";
import { logError, queueMicrotask } from "./env.ts";
import type { NodeKind, SceneOp, WindowEvent } from "./protocol.ts";
import { effect, onCleanup, root, signal, untrack } from "./signals.ts";

/** A prop's value, or a function of signals that gives it (and keeps it current). */
export type Prop<T> = T | (() => T);

/** A node in a window's scene. */
export interface UiNode {
  readonly id: number;
}

/** The node id of a window's content area. */
const CONTENT = 1;

class Win {
  nextId = CONTENT + 1;
  ops: SceneOp[] = [];
  scheduled = false;
  closed = false;
  handlers = new Map<number, () => void>();
  readonly id: number;

  constructor(id: number) {
    this.id = id;
  }

  send(op: SceneOp) {
    if (this.closed) return;
    this.ops.push(op);
    if (!this.scheduled) {
      this.scheduled = true;
      queueMicrotask(() => this.flush());
    }
  }

  flush() {
    this.scheduled = false;
    if (this.closed || this.ops.length === 0) return;
    const ops = this.ops;
    this.ops = [];
    request("compositor", "commit", { window: this.id, ops }).catch((e: Error) =>
      logError(`commit to window ${this.id} failed: ${e.message}`),
    );
  }
}

/** The window whose content is being built. */
let building: Win | null = null;

function win(): Win {
  if (!building) throw new Error("UI nodes are made inside openWindow's build function (or an each() row)");
  return building;
}

/** Make a node of `kind`. Function-valued props are reactive; `on*` props are skipped. */
export function node(kind: NodeKind, props: Record<string, unknown> = {}, children: UiNode[] = []): UiNode {
  const w = win();
  const id = w.nextId++;
  const fixed: Record<string, unknown> = {};
  const reactive: [string, () => unknown][] = [];
  for (const [k, v] of Object.entries(props)) {
    if (k.startsWith("on") || v === undefined) continue;
    if (typeof v === "function") reactive.push([k, v as () => unknown]);
    else fixed[k] = v;
  }
  w.send({ op: "create", id, type: kind, props: fixed });
  for (const [k, fn] of reactive) {
    // Only send a value that differs from the last one sent.
    let last: unknown = undefined;
    let sent = false;
    effect(() => {
      const value = fn();
      const seen = typeof value === "object" && value !== null ? JSON.stringify(value) : value;
      if (sent && Object.is(seen, last)) return;
      last = seen;
      sent = true;
      w.send({ op: "set", id, props: { [k]: value } });
    });
  }
  children.forEach((child, index) => w.send({ op: "insert", parent: id, child: child.id, index }));
  return { id };
}

export interface StackProps {
  gap?: Prop<number>;
  padding?: Prop<number>;
  w?: Prop<number>;
  h?: Prop<number>;
  hidden?: Prop<boolean>;
}

/** Children in a column. */
export function vstack(props: StackProps, ...children: UiNode[]): UiNode {
  return node("vstack", { ...props }, children);
}

/** Children in a row. */
export function hstack(props: StackProps, ...children: UiNode[]): UiNode {
  return node("hstack", { ...props }, children);
}

export interface LabelProps {
  text: Prop<string>;
  size?: Prop<number>;
  color?: Prop<string>;
  weight?: Prop<"normal" | "bold">;
  align?: Prop<"left" | "center">;
  w?: Prop<number>;
  h?: Prop<number>;
  hidden?: Prop<boolean>;
}

export function label(props: LabelProps): UiNode {
  return node("label", { ...props });
}

export interface ButtonProps {
  label: Prop<string>;
  onClick?: () => void;
  w?: Prop<number>;
  h?: Prop<number>;
  size?: Prop<number>;
  hidden?: Prop<boolean>;
}

export function button(props: ButtonProps): UiNode {
  const w = win();
  const n = node("button", { ...props });
  if (props.onClick) {
    w.handlers.set(n.id, props.onClick);
    w.send({ op: "listen", id: n.id, events: ["click"] });
    onCleanup(() => w.handlers.delete(n.id));
  }
  return n;
}

export interface RectProps {
  w?: Prop<number>;
  h?: Prop<number>;
  color?: Prop<string>;
  radius?: Prop<number>;
  border_color?: Prop<string>;
  border_width?: Prop<number>;
  hidden?: Prop<boolean>;
}

export function rect(props: RectProps): UiNode {
  return node("rect", { ...props });
}

/**
 * A column with one row per item, kept in step with `items`: rows are matched by
 * `key`, so an item that stays keeps its row (its accessor gives the new value),
 * new items get new rows, and rows for items that left are destroyed along with
 * their effects.
 */
export function each<T>(
  items: () => T[],
  key: (item: T) => string,
  render: (item: () => T) => UiNode,
  props: StackProps = {},
): UiNode {
  const w = win();
  const list = vstack(props);
  type Row = { node: UiNode; set: (item: T) => void; dispose: () => void };
  const rows = new Map<string, Row>();
  let order: string[] = [];
  effect(() => {
    const now = items();
    untrack(() => {
      const previous = building;
      building = w;
      try {
        const keys = now.map(key);
        now.forEach((item, i) => {
          const row = rows.get(keys[i]);
          if (row) row.set(item);
          else
            rows.set(
              keys[i],
              root((dispose) => {
                const [get, set] = signal(item, () => false);
                return { node: render(get), set, dispose };
              }),
            );
        });
        const keep = new Set(keys);
        for (const [k, row] of rows) {
          if (keep.has(k)) continue;
          row.dispose();
          w.send({ op: "destroy", id: row.node.id });
          rows.delete(k);
        }
        // Insert every row in order when the order changed (insert moves a node).
        if (keys.length !== order.length || keys.some((k, i) => k !== order[i])) {
          keys.forEach((k, index) => w.send({ op: "insert", parent: list.id, child: rows.get(k)!.node.id, index }));
          order = keys;
        }
      } finally {
        building = previous;
      }
    });
  });
  return list;
}

export interface WindowOptions {
  title: string;
  /** Content size in logical pixels. */
  width?: number;
  height?: number;
  /** Called when the user clicks the close button; return false to keep the window. */
  onClose?: () => boolean | void;
}

export interface AppWindow {
  /** The compositor's id for the window. */
  readonly id: number;
  /** Close the window and stop its effects. */
  close(): void;
  /** Resolves when the window has closed, by either side. */
  readonly closed: Promise<void>;
}

/** Open a window and build its content with `build`, once. */
export function openWindow(options: WindowOptions, build: () => UiNode): Promise<AppWindow> {
  return new Promise((resolve, reject) => {
    let w: Win | undefined;
    let dispose = () => {};
    let ended = () => {};
    const closed = new Promise<void>((r) => (ended = r));
    const finish = () => {
      if (w) w.closed = true;
      dispose();
      ended();
    };
    const cancel = stream<WindowEvent>(
      "compositor",
      "open_window",
      { title: options.title, width: options.width, height: options.height },
      (event) => {
        if (event.kind === "opened") {
          const opened = new Win(event.window);
          w = opened;
          root((d) => {
            dispose = d;
            const previous = building;
            building = opened;
            try {
              const top = build();
              opened.send({ op: "insert", parent: CONTENT, child: top.id, index: 0 });
            } finally {
              building = previous;
            }
          });
          resolve({ id: event.window, close, closed });
        } else if (event.kind === "event" && event.event === "click") {
          try {
            w?.handlers.get(event.node)?.();
          } catch (e) {
            logError("a click handler threw:", e);
          }
        } else if (event.kind === "close") {
          if (options.onClose?.() !== false) close();
        }
      },
      (error) => {
        finish();
        if (!w) reject(error ?? new Error("the window closed before it opened"));
      },
    );
    function close() {
      finish();
      cancel();
    }
  });
}
