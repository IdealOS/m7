// Counter: the smallest app with a window. A button adds one; the label follows.

import { button, label, openWindow, signal, vstack } from "@idealos/sdk";

await openWindow({ title: "Counter", width: 260, height: 130 }, () => {
  const [count, setCount] = signal(0);
  return vstack(
    { padding: 18, gap: 14 },
    label({ text: () => `Count: ${count()}`, size: 22, weight: "bold" }),
    button({ label: "Add one", onClick: () => setCount((n) => n + 1) }),
  );
});
