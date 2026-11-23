// What the SDK uses from the JavaScript environment app-host sets up (and Node
// has too, for tests), reached through globalThis so the SDK declares no globals.

type Env = {
  queueMicrotask(fn: () => void): void;
  console: { error(...args: unknown[]): void; warn(...args: unknown[]): void };
};

const env = globalThis as unknown as Env;

export function queueMicrotask(fn: () => void): void {
  env.queueMicrotask(fn);
}

export function logError(...args: unknown[]): void {
  env.console.error(...args);
}
