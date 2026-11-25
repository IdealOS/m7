// Terminal: a shell, drawn by the compositor. The app opens a pty session and
// shows it in a `terminal` node; what the shell prints and what's typed go
// between ptyd and the compositor without passing through here.

import { openWindow, pty, terminal, vstack } from "@idealos/sdk";

const session = await pty.open(80, 24);
const win = await openWindow({ title: "Terminal", width: 720, height: 440 }, () =>
  vstack({ grow: 1, padding: 6 }, terminal({ pty: session.id, grow: 1 })),
);

// Whichever ends first ends both: closing the window hangs up the shell, and
// the shell exiting closes the window.
await Promise.race([win.closed, session.exited]);
session.close();
win.close();
