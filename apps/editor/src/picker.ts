// A dialog to choose one of a list (a document to link to, an image to show),
// narrowed by typing. Escape or Cancel closes it.

import { button, dialog, each, hstack, label, memo, scroll, signal, textField, type UiNode } from "@idealos/sdk";

export interface Choice {
  key: string;
  label: string;
}

export interface PickerProps {
  title: () => string;
  shown: () => boolean;
  choices: () => Choice[];
  /** What to say when there's nothing to choose from. */
  empty: () => string;
  onPick: (key: string) => void;
  onCancel: () => void;
}

export function picker(p: PickerProps): UiNode {
  const [filter, setFilter] = signal("");
  const shown = memo(() => {
    const f = filter().trim().toLowerCase();
    return p.choices().filter((c) => !f || c.label.toLowerCase().includes(f));
  });
  const close = () => {
    setFilter("");
    p.onCancel();
  };
  return dialog(
    { hidden: () => !p.shown(), onCancel: close, gap: 10, padding: 16, w: 420 },
    label({ text: p.title, size: 16, weight: "bold" }),
    textField({ placeholder: "Filter", value: filter, onChange: setFilter }),
    scroll(
      { h: 260, gap: 4 },
      each(
        shown,
        (c) => c.key,
        (c) =>
          button({
            label: () => c().label,
            w: "fill",
            onClick: () => {
              setFilter("");
              p.onPick(c().key);
            },
          }),
        { gap: 4 },
      ),
      label({ text: () => (p.choices().length === 0 ? p.empty() : "Nothing matches."), color: "$muted", hidden: () => shown().length > 0 }),
    ),
    hstack({ justify: "end" }, button({ label: "Cancel", onClick: close })),
  );
}
