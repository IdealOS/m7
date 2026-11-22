// Build a UTM virtual machine that boots the QEMU image with GPU acceleration.
//
//   node tools/utm-bundle.ts [--force]
//   open image/output/qemu/idealos-venus.utm      # then press Play in UTM
//
// UTM's own QEMU runs virtio-gpu-gl-pci with Venus: the guest's Vulkan calls go to
// the Mac's GPU through virglrenderer and MoltenVK, so the compositor renders on the
// GPU instead of lavapipe. The display is still the copy path (render, read back,
// copy into a dumb buffer); only the render step moves. Homebrew's QEMU has no
// Venus, and UTM's QEMU only runs inside UTM (it is sandboxed), hence a bundle.
//
// The bundle holds clones of the kernel and disk image (APFS clones, so no extra
// space until the guest writes). The serial console is a TCP server on
// 127.0.0.1:4556 and SSH is forwarded from 127.0.0.1:2223. --force rewrites an
// existing bundle in place, including whatever the guest wrote to its disk, and
// keeps its IDs so UTM still recognises it.
import { execFileSync } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
import { constants, copyFileSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { type PlistValue, plist } from "./plist.ts";

export const SSH_PORT = 2223;
export const SERIAL_PORT = 4556;
/** The same command line as tools/run-qemu. */
export const CMDLINE = "root=/dev/vda1 rootfstype=ext4 rootwait ro console=tty0 console=ttyAMA0";

export interface Ids {
  vm: string;
  kernel: string;
  disk: string;
  mac: string;
}

/** A locally administered unicast MAC address. */
export function randomMac(): string {
  const b = randomBytes(6);
  b[0] = (b[0] & 0xfc) | 0x02;
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join(":");
}

/** UTM 5's config.plist (ConfigurationVersion 4) for the QEMU image. Every key
 * UTM decodes without a default is present, or UTM refuses to open the bundle. */
export function utmConfig(ids: Ids): PlistValue {
  return {
    Backend: "QEMU",
    ConfigurationVersion: 4,
    Information: { Name: "idealos (Venus)", IconCustom: false, UUID: ids.vm },
    System: {
      Architecture: "aarch64",
      Target: "virt",
      CPU: "host",
      CPUFlagsAdd: [],
      CPUFlagsRemove: [],
      CPUCount: 4,
      ForceMulticore: false,
      MemorySize: 4096,
      JITCacheSize: 0,
    },
    QEMU: {
      // Data/debug.log gets QEMU's command line and errors.
      DebugLog: true,
      UEFIBoot: false,
      RNGDevice: true,
      BalloonDevice: false,
      TPMDevice: false,
      Hypervisor: true,
      TSO: false,
      RTCLocalTime: false,
      PS2Controller: false,
      // The same virtio keyboard and tablet as tools/run-qemu (UTM's own input is USB,
      // turned off below), and the kernel command line, which UTM has no field for.
      // UTM splits every argument on spaces unless it is wrapped in double quotes.
      AdditionalArguments: ["-device", "virtio-keyboard-pci", "-device", "virtio-tablet-pci", "-append", `"${CMDLINE}"`],
    },
    Input: { UsbBusSupport: "Disabled", UsbSharing: false, MaximumUsbShare: 0 },
    Sharing: { DirectoryShareMode: "None", DirectoryShareReadOnly: false, ClipboardSharing: false },
    // A "-gl-" device is what makes UTM add hostmem=8G,blob=true,venus=true.
    Display: [
      {
        Hardware: "virtio-gpu-gl-pci",
        DynamicResolution: false,
        NativeResolution: false,
        UpscalingFilter: "Nearest",
        DownscalingFilter: "Linear",
      },
    ],
    Drive: [
      { ImageName: "Image", ImageType: "LinuxKernel", Interface: "None", Identifier: ids.kernel, ReadOnly: true },
      { ImageName: "disk.img", ImageType: "Disk", Interface: "VirtIO", Identifier: ids.disk, ReadOnly: false },
    ],
    Network: [
      {
        Mode: "Emulated",
        Hardware: "virtio-net-pci",
        MacAddress: ids.mac,
        IsolateFromHost: false,
        PortForward: [{ Protocol: "TCP", HostAddress: "127.0.0.1", HostPort: SSH_PORT, GuestPort: 22 }],
      },
    ],
    Serial: [{ Mode: "TcpServer", Target: "Auto", TcpPort: SERIAL_PORT, WaitForConnection: false, RemoteConnectionAllowed: false }],
    Sound: [],
  };
}

/** The IDs in an existing bundle's config.plist, so a rewrite keeps the VM's identity. */
function existingIds(config: string): Ids | undefined {
  try {
    const c = JSON.parse(execFileSync("plutil", ["-convert", "json", "-o", "-", config], { encoding: "utf8" }));
    const drive = (type: string) => c.Drive.find((d: { ImageType: string }) => d.ImageType === type).Identifier;
    return { vm: c.Information.UUID, kernel: drive("LinuxKernel"), disk: drive("Disk"), mac: c.Network[0].MacAddress };
  } catch {
    return undefined;
  }
}

function main(): void {
  const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const out = join(repo, "image/output/qemu");
  const bundle = join(out, "idealos-venus.utm");
  if (!existsSync(join(out, "Image")) || !existsSync(join(out, "disk.img"))) {
    console.error("No QEMU image. Run: make image-qemu");
    process.exit(2);
  }
  const config = join(bundle, "config.plist");
  if (existsSync(bundle) && !process.argv.includes("--force")) {
    console.error(`${bundle} exists; --force rewrites it, including its disk`);
    process.exit(2);
  }
  const ids = existingIds(config) ?? {
    vm: randomUUID().toUpperCase(),
    kernel: randomUUID(),
    disk: randomUUID(),
    mac: randomMac(),
  };
  const data = join(bundle, "Data");
  mkdirSync(data, { recursive: true });
  // Replace the files rather than the directory: UTM keeps a bookmark to the bundle.
  for (const name of ["Image", "disk.img"]) {
    rmSync(join(data, name), { force: true });
    copyFileSync(join(out, name), join(data, name), constants.COPYFILE_FICLONE);
  }
  writeFileSync(config, plist(utmConfig(ids)));
  console.log(`Wrote ${bundle}`);
  console.log(`Open it: open ${bundle}   (then press Play)`);
  console.log(`Serial: nc 127.0.0.1 ${SERIAL_PORT}   SSH: ssh -p ${SSH_PORT} -i image/output/dev-key root@127.0.0.1`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
