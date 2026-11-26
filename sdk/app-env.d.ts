// The globals app-host gives every app (see crates/app-host/src/prelude.js), for
// type-checking app code. Apps' tsconfig.json includes this file.

declare const console: {
  log(...args: unknown[]): void;
  info(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
  debug(...args: unknown[]): void;
};
declare function setTimeout(fn: (...args: unknown[]) => void, ms?: number, ...args: unknown[]): number;
declare function setInterval(fn: (...args: unknown[]) => void, ms?: number, ...args: unknown[]): number;
declare function clearTimeout(id: number): void;
declare function clearInterval(id: number): void;
declare function queueMicrotask(fn: () => void): void;

// An app's style sheet, imported as its text (tools/build-app.ts bundles it so),
// for openWindow({ style }).
declare module "*.css" {
  const text: string;
  export default text;
}
