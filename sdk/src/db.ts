// The database, through the `db` service. Documents are JSON objects of a named
// type; queries filter, order and limit them (see protocol.ts `Query`).

import { request, stream } from "./bus.ts";
import type { Doc, DocsResult, Query, UndoResult, WatchEvent } from "./protocol.ts";
import { signal } from "./signals.ts";

export type Data = Record<string, unknown>;

export function query(q: Query): Promise<Doc[]> {
  return request<DocsResult>("db", "query", q).then((r) => r.docs);
}

export function get(id: string): Promise<Doc> {
  return request<Doc>("db", "get", { id });
}

/** Create a document of `type`. */
export function put(type: string, data: Data = {}): Promise<Doc> {
  return request<Doc>("db", "create", { type, data });
}

/** Apply a JSON merge patch (a `null` value deletes the field). */
export function update(id: string, patch: Data): Promise<Doc> {
  return request<Doc>("db", "update", { id, patch });
}

/** Delete a document. Its history stays: the change set returned undoes it. */
export function remove(id: string): Promise<number> {
  return request<UndoResult>("db", "delete", { id }).then((r) => r.change_set);
}

/** Revert one of this app's change sets (a delete, say) with a new change set,
 * which is returned and can itself be undone. */
export function undo(changeSet: number): Promise<number> {
  return request<UndoResult>("db", "undo", { change_set: changeSet }).then((r) => r.change_set);
}

export function search(text: string, options: { type?: string; limit?: number } = {}): Promise<Doc[]> {
  return request<DocsResult>("db", "search", { text, ...options }).then((r) => r.docs);
}

/** A live query's current result: call it (inside an effect, to follow changes). */
export interface Live {
  (): Doc[];
  /** Whether the first result has arrived. */
  ready(): boolean;
  /** The error that ended it, if one did. */
  error(): Error | undefined;
  /** Stop following the query. */
  close(): void;
}

/** Follow `q`: the result updates as matching documents change. */
export function live(q: Query): Live {
  const [docs, setDocs] = signal<Doc[]>([]);
  const [ready, setReady] = signal(false);
  const [error, setError] = signal<Error | undefined>(undefined);
  const close = stream<WatchEvent>(
    "db",
    "watch",
    q,
    (part) => {
      setDocs((current) => applyWatch(current, part));
      setReady(true);
    },
    (e) => e && setError(e),
  );
  return Object.assign(() => docs(), { ready, error, close });
}

/** Apply one `watch` part to a result, as WatchEvent's docs describe. */
export function applyWatch(current: Doc[], part: WatchEvent): Doc[] {
  if (part.kind === "initial") return part.docs;
  const removed = new Set(part.removed);
  const changed = new Map(part.changed.map((d) => [d.id, d]));
  const next = current.filter((d) => !removed.has(d.id)).map((d) => changed.get(d.id) ?? d);
  next.push(...part.added);
  if (part.order) {
    const at = new Map(part.order.map((id, i) => [id, i]));
    next.sort((a, b) => (at.get(a.id) ?? 0) - (at.get(b.id) ?? 0));
  }
  return next;
}
