// Shells, through the `pty` service (it needs the `service:pty` grant). Open a
// session, then show it with a `terminal` node: the compositor draws the screen
// and does the typing, so the app only hears about the session.

import { stream } from "./bus.ts";
import type { RequestError } from "./bus.ts";
import type { SessionEvent } from "./protocol.ts";

export interface Session {
  /** The session's id, for a `terminal` node. */
  readonly id: string;
  /** Resolves with the shell's exit status (negative: the signal that ended it). */
  readonly exited: Promise<number>;
  /** Hang up: end the shell. */
  close(): void;
}

/** Start a shell on a `cols` by `rows` terminal. `onTitle` hears titles it sets. */
export function open(cols = 80, rows = 24, onTitle?: (title: string) => void): Promise<Session> {
  return new Promise((resolve, reject) => {
    let opened = false;
    let exit = (_code: number) => {};
    const exited = new Promise<number>((r) => (exit = r));
    const close = stream<SessionEvent>(
      "pty",
      "open",
      { cols, rows },
      (event) => {
        if (event.event === "opened") {
          opened = true;
          resolve({ id: event.id, exited, close });
        } else if (event.event === "title") {
          onTitle?.(event.title);
        } else {
          exit(event.code);
        }
      },
      (error?: RequestError) => {
        if (!opened) reject(error ?? new Error("the session ended before it opened"));
        exit(-1);
      },
    );
  });
}
