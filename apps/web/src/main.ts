// Web: pages drawn by Servo. The app opens a page with the `web` service and
// shows it in a `surface` node; browser-host draws it and the compositor sends it
// the pointer and keys, so the app only has the address bar and the buttons.

import { button, openWindow, signal, surface, textField, toolbar, vstack, web } from "@idealos/sdk";

const START = "https://servo.org";

const [address, setAddress] = signal(START);
const [loading, setLoading] = signal(true);

const page = await web.open(START, {
  onUrl: setAddress,
  onLoad: (load) => setLoading(load !== "complete"),
});

const win = await openWindow({ title: "Web", width: 1024, height: 720 }, () =>
  vstack(
    { grow: 1 },
    toolbar(
      {},
      button({ label: "‹", onClick: () => void page.back() }),
      button({ label: "›", onClick: () => void page.forward() }),
      button({ label: () => (loading() ? "…" : "↻"), onClick: () => void page.reload() }),
      textField({
        value: address,
        placeholder: "Type an address",
        grow: 1,
        onSubmit: (typed) => void page.navigate(typed),
      }),
    ),
    surface({ service: "web", id: page.id, grow: 1 }),
  ),
);

// Whichever ends first ends both: closing the window closes the page, and a page
// that crashed closes the window.
await Promise.race([win.closed, page.closed]);
page.close();
win.close();
