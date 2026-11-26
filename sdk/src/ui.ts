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
// Widgets run in the compositor: typing, sliders, scrolling and menus happen
// there without waiting for the app, which hears the results through `on*`
// handlers (`onChange` with the new value, `onSubmit`, `onCancel`, `onRange`).
// A window lasts as long as its `open_window` request: closing it cancels the
// request, and the compositor removes the window when the app goes away.

import { appId, request, stream } from "./bus.ts";
import { logError, queueMicrotask } from "./env.ts";
import type { NodeKind, SceneOp, WindowEvent } from "./protocol.ts";
import { effect, onCleanup, root, signal, untrack } from "./signals.ts";

/** A prop's value, or a function of signals that gives it (and keeps it current). */
export type Prop<T> = T | (() => T);

/** A node in a window's scene. */
export interface UiNode {
  readonly id: number;
}

/** A size: pixels, a percentage of the parent (`"50%"`), or `"fill"` for the free space. */
export type Size = number | `${number}%` | "fill";

/** The node id of a window's content area. */
const CONTENT = 1;

/** Which handler prop each compositor event calls. */
const HANDLERS: Record<string, string> = {
  onClick: "click",
  onChange: "change",
  onSubmit: "submit",
  onCancel: "cancel",
  onRange: "range",
  onChoose: "choose",
};

type Handler = (value: unknown) => void;

class Win {
  nextId = CONTENT + 1;
  ops: SceneOp[] = [];
  scheduled = false;
  closed = false;
  handlers = new Map<number, Map<string, Handler>>();
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

  on(id: number, event: string, handler: Handler) {
    let byEvent = this.handlers.get(id);
    if (!byEvent) this.handlers.set(id, (byEvent = new Map()));
    byEvent.set(event, handler);
  }
}

/** The window whose content is being built. */
let building: Win | null = null;

function win(): Win {
  if (!building) throw new Error("UI nodes are made inside openWindow's build function (or an each() row)");
  return building;
}

/** Make a node of `kind`. Function-valued props are reactive; `on*` props are handlers. */
export function node(kind: NodeKind, props: Record<string, unknown> = {}, children: UiNode[] = []): UiNode {
  const w = win();
  const id = w.nextId++;
  const fixed: Record<string, unknown> = {};
  const reactive: [string, () => unknown][] = [];
  const handlers: [string, Handler][] = [];
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined) continue;
    if (k in HANDLERS) {
      if (typeof v === "function") handlers.push([HANDLERS[k], v as Handler]);
      continue;
    }
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
  for (const [event, handler] of handlers) {
    w.on(id, event, handler);
    // Buttons only report clicks to apps that listen; other widgets always report.
    if (event === "click") w.send({ op: "listen", id, events: ["click"] });
  }
  if (handlers.length > 0) onCleanup(() => w.handlers.delete(id));
  children.forEach((child, index) => w.send({ op: "insert", parent: id, child: child.id, index }));
  return { id };
}

/** Layout props every node takes. */
export interface Common {
  /** Position in the parent, for a node not placed by a stack, grid or split. */
  x?: Prop<number>;
  y?: Prop<number>;
  w?: Prop<Size>;
  h?: Prop<Size>;
  grow?: Prop<number>;
  min_w?: Prop<number>;
  min_h?: Prop<number>;
  max_w?: Prop<number>;
  max_h?: Prop<number>;
  hidden?: Prop<boolean>;
  /** Style classes, separated by spaces, for the app's style sheet (`.primary`). */
  class?: Prop<string>;
  /**
   * Props the compositor moves to their new value instead of jumping, with how long
   * in milliseconds and, optionally, an easing: `{ x: 200, color: [150, "ease-in-out"] }`.
   * `x` and `y` (for nodes placed by them, not by a stack) and the colour props.
   */
  transition?: Prop<Partial<Record<"x" | "y" | "color" | "text_color" | "border_color", Transition>>>;
}

/** A transition's duration in milliseconds, or that and an easing. */
export type Transition = number | [number, "linear" | "ease-in" | "ease-out" | "ease-in-out"];

export interface StackProps extends Common {
  gap?: Prop<number>;
  /** One number, `[vertical, horizontal]`, or `[top, right, bottom, left]`. */
  padding?: Prop<number | number[]>;
  align?: Prop<"start" | "center" | "end" | "stretch">;
  justify?: Prop<"start" | "center" | "end" | "between" | "around">;
}

/** Children in a column. */
export function vstack(props: StackProps, ...children: UiNode[]): UiNode {
  return node("vstack", { ...props }, children);
}

/** Children in a row. */
export function hstack(props: StackProps, ...children: UiNode[]): UiNode {
  return node("hstack", { ...props }, children);
}

/** A row of controls on a bar. */
export function toolbar(props: StackProps, ...children: UiNode[]): UiNode {
  return node("toolbar", { ...props }, children);
}

/** Children in `columns` equal columns, filling row by row. */
export function grid(props: StackProps & { columns: Prop<number> }, ...children: UiNode[]): UiNode {
  return node("grid", { ...props }, children);
}

/** A column that scrolls (wheel, or drag with momentum) when its content is taller. */
export function scroll(props: StackProps, ...children: UiNode[]): UiNode {
  return node("scroll", { ...props }, children);
}

export interface SplitProps extends Common {
  /** The first pane's share of the space, 0 to 1 (default 0.5). */
  ratio?: Prop<number>;
  /** Panes above and below each other rather than side by side. */
  vertical?: Prop<boolean>;
  /** The user dragged the divider. */
  onChange?: (ratio: number) => void;
}

/** Two panes with a divider the user can drag. */
export function split(props: SplitProps, first: UiNode, second: UiNode): UiNode {
  return node("split", { ...props }, [first, second]);
}

export interface DialogProps extends StackProps {
  /** Escape was pressed. */
  onCancel?: () => void;
}

/** A panel over its window, which takes all of the window's input while shown. */
export function dialog(props: DialogProps, ...children: UiNode[]): UiNode {
  return node("dialog", { ...props }, children);
}

export interface LabelProps extends Common {
  text: Prop<string>;
  size?: Prop<number>;
  /** A color (`#rrggbb`) or a theme token (`$muted`, `$accent`, ...). */
  color?: Prop<string>;
  weight?: Prop<"normal" | "bold">;
  align?: Prop<"left" | "center">;
}

export function label(props: LabelProps): UiNode {
  return node("label", { ...props });
}

export interface ButtonProps extends Common {
  label: Prop<string>;
  onClick?: () => void;
  size?: Prop<number>;
  color?: Prop<string>;
  text_color?: Prop<string>;
}

export function button(props: ButtonProps): UiNode {
  return node("button", { ...props });
}

export interface CheckProps extends Common {
  label?: Prop<string>;
  checked?: Prop<boolean>;
  onChange?: (checked: boolean) => void;
}

export function checkbox(props: CheckProps): UiNode {
  return node("checkbox", { ...props });
}

export function toggle(props: CheckProps): UiNode {
  return node("toggle", { ...props });
}

export interface SliderProps extends Common {
  value?: Prop<number>;
  min?: Prop<number>;
  max?: Prop<number>;
  step?: Prop<number>;
  onChange?: (value: number) => void;
}

export function slider(props: SliderProps): UiNode {
  return node("slider", { ...props });
}

export interface SelectProps extends Common {
  options: Prop<string[]>;
  value?: Prop<string>;
  onChange?: (value: string) => void;
}

/** A button that opens a menu of `options`. */
export function select(props: SelectProps): UiNode {
  return node("select", { ...props });
}

export interface MenuProps extends Common {
  label: Prop<string>;
  options: Prop<string[]>;
  /** Called with the option chosen. */
  onChoose?: (option: string) => void;
}

/** A button labelled `label` that opens a menu of actions (`options`). Unlike
 * a select it keeps no value: each choice is an action, sent to `onChoose`. */
export function menu(props: MenuProps): UiNode {
  return node("menu", { ...props });
}

export interface TextProps extends Common {
  /** The text. Setting it replaces what the user typed. */
  value?: Prop<string>;
  placeholder?: Prop<string>;
  /** Edit a text field of a document directly: typing goes straight to the
   * database (it's collaborative text there), with no round trip to the app.
   * The field follows the document, so changes made elsewhere show too. */
  bind?: { doc: string; path: string };
  size?: Prop<number>;
  onChange?: (value: string) => void;
}

/**
 * One line of text. `secret` shows bullets instead (a password) and keeps it off
 * the clipboard; `onChange` still hears the text.
 */
export function textField(props: TextProps & { onSubmit?: (value: string) => void; secret?: boolean }): UiNode {
  return node("text_field", { ...props });
}

/** Several lines of editable text. */
export function textArea(props: TextProps): UiNode {
  return node("text_area", { ...props });
}

export interface RectProps extends Common {
  color?: Prop<string>;
  radius?: Prop<number>;
  border_color?: Prop<string>;
  border_width?: Prop<number>;
}

export function rect(props: RectProps): UiNode {
  return node("rect", { ...props });
}

export interface ImageProps extends Common {
  /** The blob holding the image (a PNG), by its SHA-256. */
  blob: Prop<string>;
  radius?: Prop<number>;
}

/** An image from a blob; its natural size unless given `w` and `h`. */
export function image(props: ImageProps): UiNode {
  return node("image", { ...props });
}

export interface TerminalProps extends Common {
  /** The pty session to show, from `pty.open`. */
  pty: string;
  /** Type size of the cells (default 14). */
  size?: number;
}

/**
 * A terminal showing a pty session the app opened. The compositor draws its
 * screen and sends it what's typed; the app never sees the output.
 */
export function terminal(props: TerminalProps): UiNode {
  return node("terminal", { ...props });
}

export interface VirtualListProps extends Common {
  count: Prop<number>;
  rowHeight: number;
  /** Rows to keep beyond the visible ones, so scrolling doesn't show gaps. */
  overscan?: number;
}

/**
 * A long list that only has nodes for the rows on screen: the compositor says
 * which rows are visible as the user scrolls, and `render(index)` makes each one
 * as it appears; rows that scroll away are destroyed.
 */
export function virtualList(props: VirtualListProps, render: (index: number) => UiNode): UiNode {
  const w = win();
  const { rowHeight, overscan = 3, ...rest } = props;
  type Row = { node: UiNode; dispose: () => void };
  const rows = new Map<number, Row>();
  const list = node("vlist", {
    ...rest,
    row_height: rowHeight,
    onRange: (value: unknown) => {
      const [first, last] = value as [number, number];
      const count = untrack(() => (typeof props.count === "function" ? props.count() : props.count));
      const from = Math.max(0, first - overscan);
      const to = Math.min(count - 1, last + overscan);
      const previous = building;
      building = w;
      try {
        for (const [i, row] of rows) {
          if (i < from || i > to) {
            row.dispose();
            w.send({ op: "destroy", id: row.node.id });
            rows.delete(i);
          }
        }
        for (let i = from; i <= to; i++) {
          if (rows.has(i)) continue;
          const row = root((dispose) => ({ node: render(i), dispose }));
          w.send({ op: "set", id: row.node.id, props: { index: i } });
          w.send({ op: "insert", parent: list.id, child: row.node.id, index: 0 });
          rows.set(i, row);
        }
      } finally {
        building = previous;
      }
    },
  });
  return list;
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
  /**
   * The app's style sheet: CSS-like rules for colours and fonts, over the system's,
   * inside this window. Import it from a file: `import style from "./style.css"`.
   * A mistake in it fails openWindow, saying where.
   */
  style?: string;
  /** Called when the user clicks the close button; return false to keep the window. */
  onClose?: () => boolean | void;
}

export interface AppWindow {
  /** The compositor's id for the window. */
  readonly id: number;
  /** The content area's size, following resizes. */
  size(): { width: number; height: number };
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
    const [size, setSize] = signal({ width: options.width ?? 0, height: options.height ?? 0 });
    const finish = () => {
      if (w) w.closed = true;
      dispose();
      ended();
    };
    const cancel = stream<WindowEvent>(
      "compositor",
      "open_window",
      { title: options.title, width: options.width, height: options.height, ...(options.style === undefined ? {} : { style: options.style }), ...(appId() === undefined ? {} : { app: appId() }) },
      (event) => {
        if (event.kind === "opened") {
          const opened = new Win(event.window);
          w = opened;
          setSize({ width: event.width, height: event.height });
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
          resolve({ id: event.window, size, close, closed });
        } else if (event.kind === "event") {
          try {
            w?.handlers.get(event.node)?.get(event.event)?.(event.value);
          } catch (e) {
            logError(`a ${event.event} handler threw:`, e);
          }
        } else if (event.kind === "resized") {
          setSize({ width: event.width, height: event.height });
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

/** What's on the compositor's clipboard: text, and the document it came from, if any. */
export function clipboard(): Promise<{ text: string; doc?: string }> {
  return request("compositor", "get_clipboard");
}

export function setClipboard(text: string, doc?: string): Promise<void> {
  return request("compositor", "set_clipboard", doc ? { text, doc } : { text }).then(() => undefined);
}
