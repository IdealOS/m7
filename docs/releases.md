# Releases for beta testers

A plan, not yet built. Written 2026-11-23, after M3's live queries; to be done
before the first beta tester gets a device, alongside or ahead of M8.

## Where releases live

GitHub Releases on IdealOS/m7, not Actions artifacts. Artifacts expire (90 days by
default) and need a GitHub login to download; a release asset on a public repo has
a stable public URL that a person, a script or a device can fetch. Each asset may
be up to 2 GiB.

Each release, tagged like `v0.3.0-beta.1`, carries:

| Asset | For |
| --- | --- |
| `idealos-rpi4-<version>.img.xz` | First install: flash an SD card. |
| `idealos-rpi4-<version>.raucb` | Updates: a signed RAUC bundle holding one root filesystem. |
| `manifest.json` | What devices read to find updates: version, bundle URL, SHA-256, minimum version it can update from. |
| `SHA256SUMS` | Checking downloads by hand. |

Betas are GitHub prereleases. The version goes into `/etc/os-release`, so bug
reports say which build they came from.

## First install

The raw `sdcard.img` is about 4.4 GB, but most of it is the empty 2 GiB data
partition and a second copy of the 1 GiB root, so it compresses to a few hundred
MB. Testers flash it with Raspberry Pi Imager or balenaEtcher. Pi Imager can also
read a custom OS list from a JSON URL, which could later list idealos in its menu.

## Updates

Reflashing wipes `/data`, so updates go through RAUC (planned for M8):

- The Pi image already has `root-a` and `root-b` partitions
  (`image/board/rpi4/genimage.cfg`). An update writes the bundle to the inactive
  slot, switches the boot slot and reboots. If the new slot doesn't boot and
  confirm itself healthy, the bootloader falls back to the old one.
- Devices find updates from a plain file, without the GitHub API's rate limit:
  `https://github.com/IdealOS/m7/releases/latest/download/manifest.json` always
  points at the newest stable release. "Latest" skips prereleases, so the beta
  channel needs its own pointer: a moving `beta` tag, or a JSON file on GitHub
  Pages.
- Bundles are signed; devices trust a certificate built into the image. The
  private key lives off GitHub (or in Actions secrets, once CI builds releases).

## Must be settled first

- **A release image, separate from the dev image.** Today's image puts
  `image/board/common/dev-key.pub` in root's SSH keys and has a writable
  `/opt/idealos` overlay. Shipped to testers, that is a back door. The release
  image is read-only, without the dev key or the overlay, and both share one
  Buildroot config.
- **Rollback and the database.** `db::Store` refuses a database whose schema is
  newer than its build (`SCHEMA_VERSION`). If an update migrates the database and
  then rolls back, the old slot's dbd can't open it. Options: snapshot the database
  before migrating (M3's snapshots make this cheap, though nothing takes one
  before a migration yet) and restore it on rollback, or allow rollback only
  until the new version has run cleanly once.
- **Who builds releases.** Start with a local `tools/release` script: build in the
  Lima VM, then `gh release create` with the assets. Later, GitHub's free ARM
  Linux runners for public repos can build it; a cold Buildroot build took about
  45 minutes here, within Actions' 6-hour limit, if the download and ccache
  directories are cached.
- **Private testers.** The repo is public, so its releases are too. A closed beta
  would need another place to host images and manifests.

## Open question

For the first beta, either testers reflash for each release (losing `/data`, and
needing only the release image and the release script), or RAUC comes forward
from M8 so their data survives updates.
