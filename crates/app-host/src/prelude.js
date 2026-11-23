// The globals every app sees, set up before its bundle runs. `__host` is the
// native side (app-host's Rust); it speaks only numbers and JSON strings, and
// answers through `__dispatch(id, kind, data)`. Everything here turns that into
// ordinary JavaScript: console, timers, and `__idealos`, the low-level bus API that
// @idealos/sdk wraps.
(() => {
  const host = globalThis.__host;
  const callbacks = new Map();
  let nextId = 1;

  globalThis.__dispatch = (id, kind, data) => {
    const cb = callbacks.get(id);
    if (!cb) return;
    // Timers and finished requests are done; stream parts keep their callback.
    if (kind !== "part") callbacks.delete(id);
    cb(kind, data === undefined || data === "" ? undefined : JSON.parse(data));
  };

  const text = (args) =>
    args
      .map((a) => {
        if (typeof a === "string") return a;
        if (a instanceof Error) return `${a.name}: ${a.message}\n${a.stack ?? ""}`;
        try {
          return JSON.stringify(a);
        } catch {
          return String(a);
        }
      })
      .join(" ");
  globalThis.console = {
    log: (...a) => host.log("info", text(a)),
    info: (...a) => host.log("info", text(a)),
    warn: (...a) => host.log("warn", text(a)),
    error: (...a) => host.log("error", text(a)),
    debug: (...a) => host.log("debug", text(a)),
  };

  const timer = (fn, ms, repeat, args) => {
    const id = nextId++;
    const delay = Math.max(0, Number(ms) || 0);
    const fire = () => {
      if (repeat) {
        callbacks.set(id, fire);
        host.setTimer(id, delay);
      }
      fn(...args);
    };
    callbacks.set(id, fire);
    host.setTimer(id, delay);
    return id;
  };
  const clear = (id) => {
    if (callbacks.delete(id)) host.clearTimer(id);
  };
  globalThis.setTimeout = (fn, ms, ...args) => timer(fn, ms, false, args);
  globalThis.setInterval = (fn, ms, ...args) => timer(fn, ms, true, args);
  globalThis.clearTimeout = clear;
  globalThis.clearInterval = clear;
  if (typeof globalThis.queueMicrotask !== "function") {
    globalThis.queueMicrotask = (fn) => Promise.resolve().then(fn);
  }

  const busError = (e) => Object.assign(new Error(e?.message ?? "bus error"), { code: e?.code ?? "unknown" });
  globalThis.__idealos = {
    /** A request; resolves with the result or rejects with an Error carrying `code`. */
    request(to, method, params) {
      return new Promise((resolve, reject) => {
        const id = nextId++;
        callbacks.set(id, (kind, data) => (kind === "ok" ? resolve(data) : reject(busError(data))));
        host.request(id, to, method, JSON.stringify(params ?? null));
      });
    },
    /** A streamed request: `onPart(data)` for each part, `onEnd(error?)` once. Returns a cancel function. */
    stream(to, method, params, onPart, onEnd) {
      const id = nextId++;
      let open = true;
      callbacks.set(id, (kind, data) => {
        if (kind === "part") return onPart(data);
        open = false;
        onEnd?.(kind === "err" ? busError(data) : undefined);
      });
      host.stream(id, to, method, JSON.stringify(params ?? null));
      return () => {
        if (open && callbacks.delete(id)) {
          open = false;
          host.cancel(id);
        }
      };
    },
    exit(code) {
      host.exit(code ?? 0);
    },
  };
})();
