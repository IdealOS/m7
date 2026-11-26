// Widget gallery: every widget the compositor draws, to try them by hand.
//
// The text area is bound to a "Gallery scratchpad" note: typing goes straight to
// the database, so it's still there after the app (or the device) restarts.

import style from "./style.css";
import {
  button,
  checkbox,
  db,
  dialog,
  grid,
  hstack,
  image,
  label,
  menu,
  rect,
  scroll,
  select,
  signal,
  slider,
  split,
  textArea,
  textField,
  toggle,
  toolbar,
  virtualList,
  vstack,
  node,
  openWindow,
  type UiNode,
} from "@idealos/sdk";

const SCRATCHPAD = "Gallery scratchpad";

async function scratchpad(): Promise<string> {
  const found = await db.query({ type: "Note", where: { title: SCRATCHPAD }, limit: 1 });
  if (found.length > 0) return found[0].id;
  return (await db.put("Note", { title: SCRATCHPAD, body: "Type here. It's saved as you go." })).id;
}

/** A picture to show: the first stored Image document's blob, if there is one. */
async function picture(): Promise<string | undefined> {
  const found = await db.query({ type: "Image", limit: 1 });
  const blob = found[0]?.data.blob as { $blob?: string } | undefined;
  return blob?.$blob;
}

function section(title: string, ...children: UiNode[]): UiNode {
  return vstack({ gap: 8 }, label({ text: title, size: 13, weight: "bold", color: "$muted" }), ...children);
}

const note = await scratchpad();
const blob = await picture();

await openWindow({ title: "Widget Gallery", width: 520, height: 620, style }, () => {
  const [clicks, setClicks] = signal(0);
  const [wifi, setWifi] = signal(true);
  const [dark, setDark] = signal(false);
  const [volume, setVolume] = signal(40);
  const [fruit, setFruit] = signal("Apple");
  const [action, setAction] = signal("");
  const [name, setName] = signal("");
  const [submitted, setSubmitted] = signal("");
  const [showDialog, setShowDialog] = signal(false);
  const [ratio, setRatio] = signal(0.4);
  const [moved, setMoved] = signal(false);
  const colors = ["$accent", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#06b6d4"];

  return vstack(
    {},
    toolbar(
      {},
      button({ label: "Open dialog", onClick: () => setShowDialog(true) }),
      label({ text: () => `${clicks()} clicks`, color: "$muted" }),
    ),
    scroll(
      { padding: 16, gap: 18, h: "fill" },
      section(
        "Buttons",
        hstack(
          { gap: 8 },
          button({ label: "Click me", onClick: () => setClicks((n) => n + 1) }),
          button({ label: "Accent", class: "primary", onClick: () => setClicks((n) => n + 1) }),
        ),
      ),
      section(
        "Style (the gallery's own style.css)",
        label({ text: "A serif heading", class: "serif" }),
        label({ text: "let x = 42; // monospace", class: "code" }),
        label({ text: "An italic note, in the muted colour.", class: "note" }),
      ),
      section(
        "Transitions (the compositor moves it; the app only sets where to)",
        hstack(
          { gap: 12, align: "center" },
          button({ label: "Move", onClick: () => setMoved((m) => !m) }),
          node("group", { w: 260, h: 28 }, [
            rect({
              x: () => (moved() ? 220 : 0),
              y: 0,
              w: 40,
              h: 28,
              radius: 6,
              color: () => (moved() ? "#10b981" : "$accent"),
              transition: { x: [400, "ease-in-out"], color: 400 },
            }),
          ]),
        ),
      ),
      section(
        "Checkbox and toggle",
        checkbox({ label: "Wi-Fi", checked: wifi, onChange: setWifi }),
        toggle({ label: () => (dark() ? "Dark mode on" : "Dark mode off"), checked: dark, onChange: setDark }),
      ),
      section(
        "Slider",
        hstack({ gap: 12 }, slider({ value: volume, min: 0, max: 100, step: 5, w: 220, onChange: setVolume }), label({ text: () => `Volume ${volume()}` })),
      ),
      section(
        "Select",
        hstack({ gap: 12 }, select({ options: ["Apple", "Banana", "Cherry"], value: fruit, onChange: setFruit }), label({ text: () => `You picked ${fruit()}` })),
      ),
      section(
        "Menu",
        hstack(
          { gap: 12 },
          menu({ label: "Edit", options: ["Cut", "Copy", "Paste"], onChoose: setAction }),
          label({ text: () => (action() ? `Last action: ${action()}` : "Choose an action") }),
        ),
      ),
      section(
        "Text field",
        textField({ placeholder: "Your name", value: name, onChange: setName, onSubmit: (v) => {
          setSubmitted(v);
          setName("");
        } }),
        label({ text: () => (submitted() ? `Hello, ${submitted()}!` : "Press Enter to submit"), color: "$muted" }),
      ),
      section("Text area (saved to the database)", textArea({ bind: { doc: note, path: "body" }, h: 100 })),
      section(
        "Grid",
        grid({ columns: 3, gap: 8 }, ...colors.map((color) => rect({ color, h: 28, radius: 6 }))),
      ),
      section(
        "Split view (drag the divider)",
        split(
          { ratio, onChange: setRatio, h: 80 },
          vstack({ padding: 8 }, label({ text: () => `Left ${Math.round(ratio() * 100)}%` })),
          vstack({ padding: 8 }, label({ text: "Right" })),
        ),
      ),
      section(
        "Image",
        blob ? image({ blob, w: 160, h: 100, radius: 8 }) : label({ text: "Store an Image document to see one here.", color: "$muted" }),
      ),
      section(
        "Virtual list (1,000 rows; only the visible ones exist)",
        virtualList({ count: 1000, rowHeight: 24, h: 140 }, (i) => label({ text: `Row ${i + 1}` })),
      ),
    ),
    dialog(
      { hidden: () => !showDialog(), onCancel: () => setShowDialog(false) },
      label({ text: "A dialog", size: 18, weight: "bold" }),
      label({ text: "It takes the window's input until it's closed. Escape closes it too." }),
      button({ label: "Close", onClick: () => setShowDialog(false) }),
    ),
  );
});
