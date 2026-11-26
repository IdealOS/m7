// The bus, as app-host's prelude provides it (`__idealos`). Every app process has
// one connection, made by session, which identifies the app to the broker.

/** An error from the broker or a service, with its code (such as `not_found`). */
export interface RequestError extends Error {
  code: string;
}

export interface RequestOptions {
  /** How long the broker waits for an answer, instead of its default (30 s). */
  timeoutMs?: number;
}

/** What app-host's prelude puts on `globalThis`. */
export interface Host {
  request(to: string, method: string, params: unknown, options?: RequestOptions): Promise<unknown>;
  stream(
    to: string,
    method: string,
    params: unknown,
    onPart: (part: unknown) => void,
    onEnd: (error?: RequestError) => void,
  ): () => void;
  exit(code?: number): void;
  /** Which app this is, as app-host says. */
  app?: string;
}

/** This app's id, if app-host said. */
export function appId(): string | undefined {
  return (globalThis as { __idealos?: Host }).__idealos?.app;
}

function host(): Host {
  const h = (globalThis as { __idealos?: Host }).__idealos;
  if (!h) throw new Error("@idealos/sdk runs inside app-host (no __idealos here)");
  return h;
}

/** Send a request and wait for its answer. */
export function request<T = unknown>(to: string, method: string, params?: unknown, options?: RequestOptions): Promise<T> {
  const h = host();
  return (options ? h.request(to, method, params ?? null, options) : h.request(to, method, params ?? null)) as Promise<T>;
}

/** A streamed request: `onPart` for each part, `onEnd` once. Returns a function that cancels it. */
export function stream<T = unknown>(
  to: string,
  method: string,
  params: unknown,
  onPart: (part: T) => void,
  onEnd?: (error?: RequestError) => void,
): () => void {
  return host().stream(to, method, params, onPart as (p: unknown) => void, onEnd ?? (() => {}));
}

/** End the app's process. */
export function exit(code = 0): void {
  host().exit(code);
}
