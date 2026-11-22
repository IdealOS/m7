// node --test tools/
import { test } from "node:test";
import assert from "node:assert/strict";
import { CMDLINE, SSH_PORT, randomMac, utmConfig } from "./utm-bundle.ts";

const ids = { vm: "A", kernel: "k", disk: "d", mac: "02:00:00:00:00:01" };

test("the display device is the one UTM runs with Venus", () => {
  const config = utmConfig(ids) as { Display: { Hardware: string }[] };
  assert.equal(config.Display.length, 1);
  assert.match(config.Display[0].Hardware, /-gl-/);
});

test("input, kernel command line and SSH match tools/run-qemu", () => {
  const config = utmConfig(ids) as {
    QEMU: { AdditionalArguments: string[] };
    Input: { UsbBusSupport: string };
    Network: { PortForward: { HostPort: number; GuestPort: number }[] }[];
    Drive: { ImageType: string; Interface: string }[];
  };
  const args = config.QEMU.AdditionalArguments;
  assert.ok(args.includes("virtio-keyboard-pci") && args.includes("virtio-tablet-pci"));
  // Regression: UTM splits unquoted arguments on spaces, which broke -append.
  assert.equal(args[args.indexOf("-append") + 1], `"${CMDLINE}"`);
  assert.equal(config.Input.UsbBusSupport, "Disabled");
  assert.deepEqual(config.Network[0].PortForward[0], { Protocol: "TCP", HostAddress: "127.0.0.1", HostPort: SSH_PORT, GuestPort: 22 });
  // The root disk must be the first virtio disk, /dev/vda.
  assert.deepEqual(
    config.Drive.filter((d) => d.ImageType === "Disk").map((d) => d.Interface),
    ["VirtIO"],
  );
});

test("MAC addresses are locally administered unicast", () => {
  for (let i = 0; i < 50; i++) {
    const first = parseInt(randomMac().slice(0, 2), 16);
    assert.equal(first & 0x03, 0x02);
  }
});
