// Playback, through audiod. audiod owns the queue and keeps playing with no app
// open; this follows its `watch` stream and sends it commands, keeping no copy of
// its own. Made inside a window's build, so it stops when the window closes.

import {
  AUDIO_SERVICE,
  onCleanup,
  request,
  signal,
  stream,
  type AudioStatus,
  type Repeat,
  type RequestError,
} from "@idealos/sdk";
import { positionAt } from "./clock.ts";

/** How often the shown position moves on while playing. */
const TICK_MS = 250;
/** A drag of the position slider seeks once it rests this long. */
const SEEK_AFTER_MS = 150;
/** Following audiod again after it went away (restarted, say). */
const RETRY_MS = 1000;

export interface Player {
  /** What audiod last said; null until it has. */
  status(): AudioStatus | null;
  /** The current track's position in seconds, moving on while playing. */
  position(): number;
  /** The volume to show: a drag's, until audiod has it. */
  volume(): number;
  /** What last went wrong talking to audiod, if anything. */
  problem(): string;
  play(ids: string[], start?: number): void;
  enqueue(ids: string[], next: boolean): void;
  toggle(): void;
  next(): void;
  previous(): void;
  jump(index: number): void;
  remove(index: number): void;
  clear(): void;
  /** Seek, once a drag rests; the position shows the new place at once. */
  seek(seconds: number): void;
  setVolume(volume: number): void;
  setShuffle(on: boolean): void;
  setRepeat(repeat: Repeat): void;
}

export function player(): Player {
  const [status, setStatus] = signal<AudioStatus | null>(null);
  const [now, setNow] = signal(Date.now());
  const [scrub, setScrub] = signal<number | null>(null);
  const [dragVolume, setDragVolume] = signal<number | null>(null);
  const [problem, setProblem] = signal("");
  let stopped = false;
  let ticker = -1;
  let seekTimer = -1;
  let retryTimer = -1;
  let seeking = false;
  let cancel = () => {};

  // Tick only while playing, so a paused player costs nothing.
  const follow = (s: AudioStatus) => {
    setStatus(s);
    setNow(Date.now());
    const v = dragVolume();
    if (v !== null && Math.abs(v - s.volume) < 0.001) setDragVolume(null);
    // A seek's place shows until audiod reports being there.
    const at = scrub();
    if (at !== null && !seeking && Math.abs(s.position - at) < 1) setScrub(null);
    clearInterval(ticker);
    if (s.state === "playing") ticker = setInterval(() => setNow(Date.now()), TICK_MS);
  };

  const watch = () => {
    cancel = stream<AudioStatus>(AUDIO_SERVICE, "watch", {}, (s) => {
      setProblem("");
      follow(s);
    }, (e?: RequestError) => {
      if (stopped) return;
      setProblem(e ? `Can't reach the music service: ${e.message}` : "The music service went away.");
      clearInterval(ticker);
      retryTimer = setTimeout(watch, RETRY_MS);
    });
  };
  watch();
  onCleanup(() => {
    stopped = true;
    cancel();
    clearInterval(ticker);
    clearTimeout(seekTimer);
    clearTimeout(retryTimer);
  });

  const send = (method: string, params: unknown = {}) =>
    void request(AUDIO_SERVICE, method, params).catch((e: Error) => setProblem(e.message));

  return {
    status,
    position: () => {
      const at = scrub();
      if (at !== null) return at;
      const s = status();
      return s && s.current !== undefined && s.current !== null ? positionAt(s, now()) : 0;
    },
    volume: () => dragVolume() ?? status()?.volume ?? 0,
    problem,
    play: (ids, start = 0) => send("play", { tracks: ids, start }),
    enqueue: (ids, next) => send("enqueue", { tracks: ids, next }),
    toggle: () => send("toggle"),
    next: () => send("next"),
    previous: () => send("previous"),
    jump: (index) => send("jump", { index }),
    remove: (index) => send("remove", { index }),
    clear: () => send("clear"),
    seek: (seconds) => {
      setScrub(seconds);
      clearTimeout(seekTimer);
      seeking = true;
      seekTimer = setTimeout(() => {
        seeking = false;
        // Stop showing the drag even if no report comes (stopped, say).
        const give = () => setTimeout(() => setScrub((at) => (at === seconds ? null : at)), 1000);
        request(AUDIO_SERVICE, "seek", { position: seconds }).then(give, (e: Error) => {
          setScrub(null);
          setProblem(e.message);
        });
      }, SEEK_AFTER_MS);
    },
    setVolume: (volume) => {
      setDragVolume(volume);
      send("volume", { volume });
    },
    setShuffle: (on) => send("shuffle", { on }),
    setRepeat: (repeat) => send("repeat", { repeat }),
  };
}
