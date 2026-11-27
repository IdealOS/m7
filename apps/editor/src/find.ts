// The find and replace bar. Matches are found in the app (text.ts), and the
// body's text area is told what to select, so the match shows highlighted and
// scrolled into view. Replacing splices the document: the text area follows it,
// like any other change, and each replacement can be undone.

import { button, checkbox, hstack, label, memo, signal, textField, vstack, type TextControl, type UiNode } from "@idealos/sdk";
import { findAll, matchIndex, stepMatch, type Range } from "./text.ts";

export interface FindProps {
  hidden: () => boolean;
  /** The body as shown, and where its selection is. */
  text: () => string;
  selection: () => Range;
  /** The body's text area. */
  body: TextControl;
  /** Replace these ranges of the body (in order, not overlapping) with \`with\`;
   * resolves once the body shows the result. */
  replace: (ranges: readonly Range[], with_: string) => Promise<void>;
  onClose: () => void;
  /** Focuses the find field when the bar opens. */
  control: TextControl;
}

export function findBar(p: FindProps): UiNode {
  const [query, setQuery] = signal("");
  const [replacement, setReplacement] = signal("");
  const [matchCase, setMatchCase] = signal(false);
  const [wholeWord, setWholeWord] = signal(false);
  const matches = memo(() => findAll(p.text(), query(), { matchCase: matchCase(), wholeWord: wholeWord() }));
  const current = () => matchIndex(matches(), p.selection());

  const go = (back: boolean) => {
    const i = stepMatch(matches(), p.selection(), back);
    if (i >= 0) p.body.select(matches()[i][0], matches()[i][1]);
  };

  const replaceOne = async () => {
    const i = current();
    if (i < 0) return go(false);
    const [start, end] = matches()[i];
    await p.replace([[start, end]], replacement());
    // Then on to the next match after the replacement, in the new text.
    const after = start + [...replacement()].length;
    const next = matches().findIndex(([s]) => s >= after);
    const target = next >= 0 ? next : matches().length > 0 ? 0 : -1;
    if (target >= 0) p.body.select(matches()[target][0], matches()[target][1]);
    else p.body.select(after, after);
  };

  const status = () => {
    if (!query()) return "";
    const n = matches().length;
    if (n === 0) return "No matches";
    const i = current();
    return i >= 0 ? `${i + 1} of ${n}` : `${n} match${n === 1 ? "" : "es"}`;
  };

  return vstack(
    { gap: 6, hidden: p.hidden },
    hstack(
      { gap: 8, align: "center" },
      textField({ placeholder: "Find", value: query, onChange: setQuery, onSubmit: () => go(false), control: p.control, grow: 1 }),
      label({ text: status, color: "$muted", size: 13, w: 90 }),
      button({ label: "Previous", onClick: () => go(true) }),
      button({ label: "Next", onClick: () => go(false) }),
      button({ label: "Done", onClick: p.onClose }),
    ),
    hstack(
      { gap: 8, align: "center" },
      textField({ placeholder: "Replace with", value: replacement, onChange: setReplacement, onSubmit: () => void replaceOne(), grow: 1 }),
      checkbox({ label: "Match case", checked: matchCase, onChange: setMatchCase }),
      checkbox({ label: "Whole word", checked: wholeWord, onChange: setWholeWord }),
      button({ label: "Replace", onClick: () => void replaceOne() }),
      button({ label: "Replace all", onClick: () => void p.replace(matches(), replacement()) }),
    ),
  );
}
