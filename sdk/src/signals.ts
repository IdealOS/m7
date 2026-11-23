// Signals in the SolidJS style: UI code creates nodes once, and effects keep their
// props up to date by reading signals. Reading a signal inside an effect subscribes
// the effect; writing it re-runs the subscribers, with dependencies tracked afresh
// each run. `batch` holds re-runs until it ends, so several writes cost one run.
// `root` owns the effects created inside it, so a whole subtree (a list item, a
// window) can be disposed at once.

type Computation = {
  run: () => void;
  sources: Set<Source>;
  owner: Owner | null;
  disposed: boolean;
};

type Source = { observers: Set<Computation> };

type Owner = { children: Set<Computation | Owner>; cleanups: (() => void)[]; parent: Owner | null; disposed: boolean };

let observer: Computation | null = null;
let owner: Owner | null = null;
let batchDepth = 0;
const pending = new Set<Computation>();

/** A getter, which subscribes the running effect, and a setter. */
export type Signal<T> = [get: () => T, set: (value: T | ((previous: T) => T)) => void];

export function signal<T>(initial: T, equals: (a: T, b: T) => boolean = Object.is): Signal<T> {
  let value = initial;
  const source: Source = { observers: new Set() };
  const get = () => {
    if (observer && !observer.disposed) {
      source.observers.add(observer);
      observer.sources.add(source);
    }
    return value;
  };
  const set = (next: T | ((previous: T) => T)) => {
    const v = typeof next === "function" ? (next as (p: T) => T)(value) : next;
    if (equals(value, v)) return;
    value = v;
    for (const c of [...source.observers]) pending.add(c);
    if (batchDepth === 0) flush();
  };
  return [get, set];
}

function flush() {
  // Running an effect can write signals and queue more; keep going until quiet.
  let guard = 0;
  while (pending.size > 0) {
    if (++guard > 10_000) {
      pending.clear();
      throw new Error("signals: effects kept triggering each other (a cycle?)");
    }
    const next = [...pending];
    pending.clear();
    for (const c of next) if (!c.disposed) c.run();
  }
}

function unsubscribe(c: Computation) {
  for (const s of c.sources) s.observers.delete(c);
  c.sources.clear();
}

/** Run `fn` now and again whenever a signal it read changes. */
export function effect(fn: () => void): void {
  const c: Computation = {
    sources: new Set(),
    owner,
    disposed: false,
    run: () => {
      unsubscribe(c);
      const previous = observer;
      observer = c;
      try {
        fn();
      } finally {
        observer = previous;
      }
    },
  };
  owner?.children.add(c);
  c.run();
}

/** A derived value, recomputed when what it reads changes; reads like a signal. */
export function memo<T>(fn: () => T, equals?: (a: T, b: T) => boolean): () => T {
  let first = true;
  let set: Signal<T>[1] = () => {};
  let get: () => T = () => undefined as T;
  effect(() => {
    const v = fn();
    if (first) {
      [get, set] = signal(v, equals);
      first = false;
    } else {
      set(() => v);
    }
  });
  return () => get();
}

/** Run `fn` with re-runs held until it returns. */
export function batch<T>(fn: () => T): T {
  batchDepth++;
  try {
    return fn();
  } finally {
    if (--batchDepth === 0) flush();
  }
}

/** Run `fn` without subscribing the running effect to what it reads. */
export function untrack<T>(fn: () => T): T {
  const previous = observer;
  observer = null;
  try {
    return fn();
  } finally {
    observer = previous;
  }
}

/** Run `fn` as the owner of the effects it creates. The returned function stops them all. */
export function root<T>(fn: (dispose: () => void) => T): T {
  const r: Owner = { children: new Set(), cleanups: [], parent: owner, disposed: false };
  owner?.children.add(r);
  const dispose = () => disposeOwner(r);
  const previousOwner = owner;
  const previousObserver = observer;
  owner = r;
  observer = null;
  try {
    return fn(dispose);
  } finally {
    owner = previousOwner;
    observer = previousObserver;
  }
}

/** Call `fn` when the current root is disposed. */
export function onCleanup(fn: () => void): void {
  owner?.cleanups.push(fn);
}

function disposeOwner(o: Owner) {
  if (o.disposed) return;
  o.disposed = true;
  for (const child of o.children) {
    if ("run" in child) {
      child.disposed = true;
      unsubscribe(child);
      pending.delete(child);
    } else {
      disposeOwner(child);
    }
  }
  o.children.clear();
  for (const fn of o.cleanups.splice(0)) fn();
  o.parent?.children.delete(o);
}
