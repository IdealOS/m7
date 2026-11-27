// Files: browse a share on another computer and import from it, through importd.
//
// Type a share as smb://user@computer/Share and open it. If importd has no
// password for that user on that computer, the page asks for one once (importd
// keeps it in secretsd). Folders open with a click; tick files or folders and
// import them, or import the folder you're in. Progress shows as it goes, and a
// finished import can be taken back in one step: it's one change set.

import {
  button,
  checkbox,
  each,
  hstack,
  label,
  onCleanup,
  openWindow,
  request,
  scroll,
  signal,
  textField,
  vstack,
  type BrowseResult,
  type Entry,
  type ImportStatus,
  type Source,
} from "@idealos/sdk";
import { describe, inside, loginOf, parseSource, progress, trail, up } from "./places.ts";

const IMPORT = "import";
/** Opening a big folder or starting a big import can take a while over the network. */
const SLOW = { timeoutMs: 120_000 };

type Failure = Error & { code?: string };

await openWindow({ title: "Files", width: 760, height: 560 }, () => {
  const [address, setAddress] = signal("");
  const [source, setSource] = signal<Source | null>(null);
  const [path, setPath] = signal("");
  const [entries, setEntries] = signal<Entry[]>([]);
  const [chosen, setChosen] = signal<ReadonlySet<string>>(new Set());
  const [needsLogin, setNeedsLogin] = signal(false);
  const [password, setPassword] = signal("");
  const [message, setMessage] = signal("");
  const [status, setStatus] = signal<ImportStatus | null>(null);
  const [undone, setUndone] = signal(false);

  const open = async (src: Source, p: string) => {
    setMessage("Opening…");
    try {
      const r = await request<BrowseResult>(IMPORT, "browse", { source: src, path: p }, SLOW);
      setSource(src);
      setPath(p);
      setEntries(r.entries);
      setChosen(new Set<string>());
      setNeedsLogin(false);
      setMessage(r.entries.length === 0 ? "This folder is empty." : "");
    } catch (e) {
      const f = e as Failure;
      setNeedsLogin(f.code === "unreachable" && /log in|refused/.test(f.message));
      setMessage(f.message);
    }
  };

  const connect = () => {
    const s = parseSource(address());
    if (!s) return setMessage("Type a share as smb://you@computer/Share");
    void open(s, "");
  };

  const login = async () => {
    const s = parseSource(address());
    const who = s && loginOf(s);
    if (!s || !who) return;
    try {
      await request(IMPORT, "login", { ...who, password: password() });
      setPassword("");
      await open(s, "");
    } catch (e) {
      setMessage((e as Error).message);
    }
  };

  // Follow an import while it runs.
  let timer = -1;
  const follow = () => {
    clearInterval(timer);
    timer = setInterval(() => {
      request<ImportStatus>(IMPORT, "status").then((s) => {
        setStatus(s);
        if (!s.running) clearInterval(timer);
      }, (e: Error) => setMessage(e.message));
    }, 500);
  };
  onCleanup(() => clearInterval(timer));
  // One may be running already (started earlier, or from osctl).
  request<ImportStatus>(IMPORT, "status").then((s) => {
    setStatus(s);
    if (s.running) follow();
  }, (e: Error) => setMessage(e.message));

  const start = async () => {
    const src = source();
    if (!src) return;
    const picked = [...chosen()];
    const paths = picked.length > 0 ? picked.map((n) => inside(path(), n)) : [path()];
    try {
      setUndone(false);
      setStatus(await request<ImportStatus>(IMPORT, "import", { source: src, paths }, SLOW));
      follow();
    } catch (e) {
      setMessage((e as Error).message);
    }
  };

  const undo = async () => {
    const set = status()?.change_set;
    if (set === undefined) return;
    try {
      await request(IMPORT, "undo", { change_set: set });
      setUndone(true);
    } catch (e) {
      setMessage((e as Error).message);
    }
  };

  const toggle = (name: string, on: boolean) =>
    setChosen((c) => {
      const next = new Set(c);
      if (on) next.add(name);
      else next.delete(name);
      return next;
    });

  const top = () => {
    const s = source();
    return s === null ? "" : "share" in s ? `${s.share.share} on ${s.share.host}` : s.dir.path;
  };
  const running = () => status()?.running === true;

  return vstack(
    { padding: 12, gap: 8, grow: 1 },
    hstack(
      { gap: 8, align: "center" },
      textField({ placeholder: "smb://you@computer/Share", value: address, onChange: setAddress, onSubmit: connect, grow: 1 }),
      button({ label: "Open", class: "primary", onClick: connect }),
    ),
    hstack(
      { gap: 8, align: "center", hidden: () => !needsLogin() },
      label({
        text: () => {
          const s = parseSource(address());
          const who = s && loginOf(s);
          return who ? `Password for ${who.user} on ${who.host}` : "Password";
        },
      }),
      textField({ secret: true, value: password, onChange: setPassword, onSubmit: () => void login(), grow: 1 }),
      button({ label: "Log in", onClick: () => void login() }),
    ),
    label({ text: message, color: "$muted", hidden: () => !message() }),
    hstack(
      { gap: 8, align: "center", hidden: () => source() === null },
      button({ label: "Up", hidden: () => path() === "", onClick: () => void open(source()!, up(path())) }),
      label({ text: () => trail(path(), top()), grow: 1, weight: "bold" }),
      label({ text: () => `${entries().length} item${entries().length === 1 ? "" : "s"}`, color: "$muted", size: 12 }),
    ),
    scroll(
      { grow: 1, gap: 2 },
      each(
        entries,
        (e) => e.name,
        (e) =>
          hstack(
            { gap: 8, align: "center" },
            checkbox({ checked: () => chosen().has(e().name), onChange: (on) => toggle(e().name, on) }),
            e().kind === "folder"
              ? button({ label: () => e().name, onClick: () => void open(source()!, inside(path(), e().name)) })
              : label({ text: () => e().name }),
            label({ text: "", grow: 1 }),
            label({ text: () => describe(e()), color: "$muted", size: 12 }),
          ),
        { gap: 2 },
      ),
    ),
    hstack(
      { gap: 8, align: "center", hidden: () => source() === null },
      button({
        label: () => (running() ? "Importing…" : chosen().size > 0 ? `Import ${chosen().size} selected` : "Import this folder"),
        class: "primary",
        onClick: () => void (running() ? undefined : start()),
      }),
      label({ text: () => (undone() ? "Import undone." : status() ? progress(status()!) : ""), grow: 1, size: 13 }),
      button({
        label: "Undo import",
        hidden: () => running() || undone() || status()?.change_set === undefined,
        onClick: () => void undo(),
      }),
    ),
  );
});
