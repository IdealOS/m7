// Minimal QMP client: QEMU's JSON control protocol over a Unix socket.
import { createConnection } from "node:net";
import { resolve } from "node:path";

interface QmpReply {
  QMP?: unknown;
  return?: unknown;
  error?: { desc: string };
  event?: string;
}

/** Run QMP commands in order; resolves once every one has succeeded. */
export function qmp(socketPath: string, commands: object[]): Promise<void> {
  const queue = [{ execute: "qmp_capabilities" }, ...commands];
  return new Promise((done, fail) => {
    const sock = createConnection(socketPath);
    let buffered = "";
    let sent = 0;
    sock.on("data", (chunk: Buffer) => {
      buffered += chunk.toString();
      let newline: number;
      while ((newline = buffered.indexOf("\n")) >= 0) {
        const line = buffered.slice(0, newline).trim();
        buffered = buffered.slice(newline + 1);
        if (!line) continue;
        const msg = JSON.parse(line) as QmpReply;
        if (msg.error) {
          sock.destroy();
          fail(new Error(`qmp: ${msg.error.desc}`));
          return;
        }
        if (msg.event) continue; // asynchronous events are not replies
        if (msg.QMP || msg.return !== undefined) {
          if (sent < queue.length) {
            sock.write(JSON.stringify(queue[sent++]) + "\n");
          } else {
            sock.end();
            done();
          }
        }
      }
    });
    sock.on("error", (e: Error) => fail(new Error(`qmp: ${e.message}`)));
  });
}

/** Save what the guest's display scanout shows to a PNG. */
export function screendump(socketPath: string, outFile: string): Promise<void> {
  return qmp(socketPath, [
    { execute: "screendump", arguments: { filename: resolve(outFile), format: "png" } },
  ]);
}
