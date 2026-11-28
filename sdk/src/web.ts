// Web pages, through the `web` service (it needs the `service:web` grant). Open
// a page, then show it with a `surface` node: browser-host draws it and the
// compositor sends it the pointer and keys, so the app only steers it (where to
// go, back, forward) and hears where it went.

import { request, stream } from "./bus.ts";
import type { RequestError } from "./bus.ts";
import type { PageEvent } from "./protocol.ts";

/** What a page tells its app. Each is optional. */
export interface PageListener {
  /** The page is at `url` now. */
  onUrl?: (url: string) => void;
  onTitle?: (title: string | undefined) => void;
  /** Loading started, the head was read, or it all loaded. */
  onLoad?: (load: "started" | "parsed" | "complete") => void;
  /** Whether there's somewhere to go back or forward to. */
  onHistory?: (back: boolean, forward: boolean) => void;
}

export interface Page {
  /** The page's id, for a `surface` node (`service: "web"`). */
  readonly id: string;
  /** Resolves when the page closes: closed by the app, or crashed (with why). */
  readonly closed: Promise<string | undefined>;
  /** Go to what was typed (an address without a scheme gets `https://`). */
  navigate(url: string): Promise<void>;
  back(): Promise<void>;
  forward(): Promise<void>;
  reload(): Promise<void>;
  close(): void;
}

/** Open a page at `url` (a blank one if none). */
export function open(url?: string, listener: PageListener = {}): Promise<Page> {
  return new Promise((resolve, reject) => {
    let opened = false;
    let done = (_why?: string) => {};
    const closed = new Promise<string | undefined>((r) => (done = r));
    const close = stream<PageEvent>(
      "web",
      "open",
      url === undefined ? {} : { url },
      (event) => {
        switch (event.event) {
          case "opened": {
            opened = true;
            const id = event.id;
            const call = (method: string, extra: object = {}) =>
              request<null>("web", method, { id, ...extra }).then(() => undefined);
            resolve({
              id,
              closed,
              navigate: (to) => call("navigate", { url: to }),
              back: () => call("back"),
              forward: () => call("forward"),
              reload: () => call("reload"),
              close: () => {
                close();
                done();
              },
            });
            break;
          }
          case "url":
            listener.onUrl?.(event.url);
            break;
          case "title":
            listener.onTitle?.(event.title ?? undefined);
            break;
          case "load":
            listener.onLoad?.(event.load);
            break;
          case "history":
            listener.onHistory?.(event.back, event.forward);
            break;
          case "crashed":
            done(event.reason);
            break;
        }
      },
      (error?: RequestError) => {
        if (!opened) reject(error ?? new Error("the page closed before it opened"));
        done(error?.message);
      },
    );
  });
}
