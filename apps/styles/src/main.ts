// Style Editor: edit a style sheet live. The text area is bound to the sheet's
// StyleSheet document, so each keystroke goes straight to the database and the
// compositor, which follows those documents, restyles the screen at once (the
// app's code isn't in that path). The status line says whether the sheet parses
// and where any mistake is; a sheet with a mistake isn't applied, and the last
// good one stays. Edits to the system sheet show in this window's preview too;
// edits to an app's sheet show in that app's windows.

import style from "./style.css";
import {
  button,
  checkbox,
  db,
  each,
  hstack,
  label,
  openWindow,
  request,
  scroll,
  select,
  signal,
  slider,
  split,
  textArea,
  textField,
  toggle,
  vstack,
  effect,
  type CheckStyleResult,
  type Doc,
  type SystemStyleResult,
} from "@idealos/sdk";

const TYPE = "StyleSheet";
const STARTER = `/* Edits to the system style. They come after the built-in sheet, so they win
   where they match the same things. Every change applies as you type.
   Try:   :root { --accent: #d9480f; }
   or:    button { background: #fef3c7; }
   "Show built-in" lists every variable and rule there is to change. */
`;

/** The newest document of each target. */
function newest(docs: Doc[]): Map<string, Doc> {
  const out = new Map<string, Doc>();
  for (const d of docs) {
    const t = String(d.data.target ?? "");
    const had = out.get(t);
    if (!had || had.updated < d.updated) out.set(t, d);
  }
  return out;
}

const apps = await db.query({ type: "App", limit: 500 });
const targets = ["system", ...apps.map((a) => `app:${String(a.data.id)}`).sort()];
const name = (t: string) => (t === "system" ? "System" : (apps.find((a) => `app:${a.data.id}` === t)?.data.name as string | undefined) ?? t);

await openWindow({ title: "Style Editor", width: 900, height: 600, style }, () => {
  const sheets = db.live({ type: TYPE, limit: 200 });
  const [target, setTarget] = signal("system");
  const [status, setStatus] = signal("");
  const [ok, setOk] = signal(true);
  const [builtin, setBuiltin] = signal("");
  const current = () => newest(sheets()).get(target());

  // Make sure the chosen target has a document to bind to.
  effect(() => {
    const t = target();
    if (!sheets.ready() || current()) return;
    void db.put(TYPE, { target: t, css: t === "system" ? STARTER : "" }).catch((e: Error) => setStatus(e.message));
  });

  // Check the sheet whenever it changes, as the compositor will parse it.
  effect(() => {
    const css = String(current()?.data.css ?? "");
    void request<CheckStyleResult>("compositor", "check_style", { css }).then(
      (r) => {
        setOk(!r.error);
        setStatus(r.error ? `Not applied: line ${r.error.line}, column ${r.error.col}: ${r.error.message}` : "Applied");
      },
      (e: Error) => setStatus(e.message),
    );
  });

  const editor = vstack(
    { gap: 8, padding: 12, grow: 1 },
    hstack(
      { gap: 8, align: "center" },
      label({ text: "Sheet" }),
      select({ options: targets.map(name), value: () => name(target()), onChange: (v) => setTarget(targets.find((t) => name(t) === v) ?? "system") }),
      button({
        label: "Reset",
        onClick: async () => {
          const d = current();
          if (d) await db.update(d.id, { css: "" });
        },
      }),
      button({
        label: () => (builtin() ? "Hide built-in" : "Show built-in"),
        onClick: async () =>
          setBuiltin(builtin() ? "" : (await request<SystemStyleResult>("compositor", "system_style", null)).css),
      }),
    ),
    // The text area follows its document, so changes made elsewhere (osctl
    // style set, make style-watch, Reset) show in it as they happen.
    each(
      () => {
        const d = current();
        return d ? [d.id] : [];
      },
      (id) => id,
      (id) => textArea({ bind: { doc: id(), path: "css" }, class: "code", h: "fill" }),
      { grow: 1 },
    ),
    label({ text: status, class: () => (ok() ? "ok" : "err"), size: 13 }),
    scroll({ h: 160, hidden: () => !builtin() }, label({ text: builtin, class: "code" })),
  );

  // A little of everything, to see a sheet's effect.
  const [on, setOn] = signal(true);
  const [level, setLevel] = signal(60);
  const preview = scroll(
    { padding: 16, gap: 12 },
    label({ text: "Preview", size: 16, weight: "bold" }),
    label({ text: "Body text, and a muted line below it." }),
    label({ text: "Secondary text", color: "$muted" }),
    hstack({ gap: 8 }, button({ label: "Button" }), button({ label: "Primary", class: "primary" })),
    checkbox({ label: "Checkbox", checked: on, onChange: setOn }),
    toggle({ label: "Toggle", checked: on, onChange: setOn }),
    slider({ value: level, min: 0, max: 100, w: 200, onChange: setLevel }),
    select({ options: ["One", "Two", "Three"], value: "One" }),
    textField({ placeholder: "A text field" }),
  );
  return split({ ratio: 0.62, grow: 1 }, editor, preview);
});
