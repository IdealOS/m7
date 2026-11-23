// Notes: a live list of Note documents. Adding a note writes to the database, and
// the list updates from the live query, as it does for notes added anywhere else
// (another app, `osctl db create Note ...`).

import { button, db, each, hstack, label, memo, openWindow, vstack } from "@idealos/sdk";

const notes = db.live({ type: "Note", orderBy: [["created", "desc"]], limit: 12 });
let added = 0;

const win = await openWindow({ title: "Notes", width: 340, height: 300 }, () => {
  const count = memo(() => notes().length);
  return vstack(
    { padding: 14, gap: 10 },
    hstack(
      { gap: 10, h: 32 },
      button({
        label: "Add note",
        w: 110,
        onClick: () => {
          added += 1;
          db.put("Note", { title: `Note ${added}`, created: Date.now() }).catch((e: Error) =>
            console.error(`couldn't add a note: ${e.message}`),
          );
        },
      }),
      label({ text: () => (notes.ready() ? `${count()} shown` : "Loading…"), color: "#4b5563", h: 32 }),
    ),
    each(
      notes,
      (doc) => doc.id,
      (doc) => label({ text: () => String(doc().data.title ?? "(untitled)"), size: 15 }),
      { gap: 6 },
    ),
  );
});

await win.closed;
notes.close();
