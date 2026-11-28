// Times, for people, and where playback is now.

import type { AudioStatus } from "@idealos/sdk";

/** Seconds as m:ss, or h:mm:ss from an hour. */
export function clock(seconds: number | undefined | null): string {
  if (seconds === undefined || seconds === null || !Number.isFinite(seconds)) return "–:––";
  const s = Math.max(0, Math.floor(seconds));
  const [h, m, r] = [Math.floor(s / 3600), Math.floor((s % 3600) / 60), s % 60];
  const ss = String(r).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

/** "12 songs, 48 min", or "1 song, 1 hr 3 min". */
export function totals(durations: (number | undefined)[]): string {
  const n = durations.length;
  const songs = `${n} song${n === 1 ? "" : "s"}`;
  const secs = durations.reduce<number>((sum, d) => sum + (d ?? 0), 0);
  if (secs <= 0) return songs;
  const mins = Math.round(secs / 60);
  const length = mins >= 60 ? `${Math.floor(mins / 60)} hr ${mins % 60} min` : `${Math.max(1, mins)} min`;
  return `${songs}, ${length}`;
}

/**
 * Where the current track is at `now` (ms since the epoch): audiod says where it
 * was at `at`, and while playing it has moved on since. Never past the end.
 */
export function positionAt(s: Pick<AudioStatus, "state" | "position" | "at" | "duration">, now: number): number {
  const moved = s.state === "playing" ? Math.max(0, now - s.at) / 1000 : 0;
  const p = s.position + moved;
  return s.duration !== undefined && s.duration !== null ? Math.min(p, s.duration) : p;
}
