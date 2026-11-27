// The Files app's logic, apart from the widgets so it can be tested: sources
// written as text, paths inside them, and how entries and progress read.

import type { Entry, ImportStatus, Source } from "@idealos/sdk";

/**
 * A source from what was typed: `smb://user@host[:port]/share` (as importd's
 * own names are written) or `dir:/path` (a folder importd allows, on a
 * development machine). Null when it's neither.
 */
export function parseSource(text: string): Source | null {
  const t = text.trim();
  if (t.startsWith("dir:")) return t.length > 4 ? { dir: { path: t.slice(4) } } : null;
  const m = /^smb:\/\/([^@/]+)@(\[[^\]]+\]|[^:/]+)(?::(\d+))?\/([^/]+)\/?$/.exec(t);
  if (!m) return null;
  const [, user, host, port, share] = m;
  const source: Source = { share: { host: host.replace(/^\[|\]$/g, ""), share, user } };
  if (port) {
    const p = Number(port);
    if (!(p > 0 && p < 65536)) return null;
    source.share.port = p;
  }
  return source;
}

/** The computer and user a share's password is kept for. */
export function loginOf(source: Source): { host: string; user: string } | null {
  return "share" in source ? { host: source.share.host, user: source.share.user } : null;
}

/** Folder `name` inside `path` ("" is the top). */
export function inside(path: string, name: string): string {
  return path ? `${path}/${name}` : name;
}

/** The folder holding `path`. */
export function up(path: string): string {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i);
}

/** Where `path` is, for people: "Music › Enya", or the share's top. */
export function trail(path: string, top: string): string {
  return path ? [top, ...path.split("/")].join(" › ") : top;
}

export function size(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

/** What an entry is, in a word, and whether importing it makes anything. */
export function describe(e: Entry): string {
  switch (e.kind) {
    case "folder":
      return "Folder";
    case "text":
      return `Text · ${size(e.size)}`;
    case "image":
      return `Picture · ${size(e.size)}`;
    case "audio":
      return `Music · ${size(e.size)}`;
    default:
      return `${size(e.size)} · not imported`;
  }
}

/** An import's progress in a line. */
export function progress(s: ImportStatus): string {
  if (s.total === 0 && !s.running) return "";
  const parts = [`${s.imported} imported`];
  if (s.skipped) parts.push(`${s.skipped} already here`);
  if (s.failed) parts.push(`${s.failed} failed`);
  if (s.ignored) parts.push(`${s.ignored} not imported`);
  const head = s.running ? `Importing ${s.done} of ${s.total}` : `Done: ${s.total} file${s.total === 1 ? "" : "s"}`;
  return `${head} · ${parts.join(", ")}`;
}
