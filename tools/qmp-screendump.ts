// Capture a QEMU VM's display scanout to a PNG through its QMP socket.
//
//   node tools/qmp-screendump.ts ~/.lima/idealos-spike/qmp.sock out.png
//
// This is what the guest's KMS output actually shows, so it verifies scanout end to
// end. Run with Node 23.6+ (TypeScript types are stripped natively).
import { resolve } from "node:path";
import { screendump } from "./qmp.ts";

const [socketPath, outFile] = process.argv.slice(2);
if (!socketPath || !outFile) {
  console.error("usage: qmp-screendump.ts QMP_SOCKET OUT.png");
  process.exit(2);
}

try {
  await screendump(socketPath, outFile);
  console.log(`wrote ${resolve(outFile)}`);
} catch (e) {
  console.error((e as Error).message);
  process.exit(1);
}
