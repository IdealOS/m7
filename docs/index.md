# IdealOS

A research desktop OS for the Raspberry Pi, built around one question: what would a
desktop look like if a persistent, versioned, syncable database replaced the
filesystem? Every piece of data is a document in one database, and every process
talks through one message bus.

Phase 1 (milestones M0 to M7) is built: the bus, the database, the app runtime,
widgets and the window manager, permissions and sandboxes, and the first apps.
Phase 2 is under way: M8 (history compaction, and backups to a USB drive) is built,
and the text editor, the music player, imports from other computers and a web
browser (Servo, drawing pages into compositor windows) are working and being
finished. The compositor has style sheets with a dark theme, and transitions. It
runs hosted on macOS, in QEMU, and on a Raspberry Pi 4 and 3B+.

## Documentation

- **[Architecture](architecture.html)**: an interactive map of every component, what
  each talks to and what's inside it, with placeholders for what's still to come.
- **[API reference](api.md)**: every service on the bus, windows and widgets,
  style sheets, transitions, and the app SDK.
- **[Feature list](feature-list.md)**: the vision, and what the system should do.
- **[Implementation plan](implementation-plan.md)**: milestones M0 to M14, from first
  boot to a daily driver.
- **[Spikes](spikes.md)**: the experiments and their measurements, Servo on aarch64
  (Spike E) the latest.
- **[Releases](releases.md)**: how releases will reach beta testers (a plan).

The code, the build instructions and the status of each part are in the
[repository](https://github.com/IdealOS/m7).
