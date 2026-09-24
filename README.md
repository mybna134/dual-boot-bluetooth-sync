# Linux Bluetooth Sync (`bls`)

English | [简体中文](README.zh-CN.md)

`bls` copies pairing secrets and device metadata from an offline Windows `SYSTEM` registry hive to BlueZ on Linux. It supports Bluetooth Classic link keys (Legacy, SSP, Secure Connections), BLE legacy and Secure Connections LTKs, remote IRKs, and the Windows host's local IRK. Every sync mounts the configured Windows partition read-only and unmounts it afterward. Every successful real sync, whether manual or from `bls.service` at boot, restarts `bluetooth.service` so BlueZ reloads the keys and submits remote IRKs to the kernel/controller for LE Privacy address resolution.

## Install on this dual-boot machine

Requirements: Linux with BlueZ and systemd; `reged` from the `chntpw` package; an unhibernated Windows NTFS system partition.

```sh
sudo pacman -S --needed bluez bluez-utils chntpw rust
sudo ./scripts/install.sh
sudo bls config --device /dev/nvme0n1p3
sudo bls sync --dry-run
sudo bls info
```

Replace `/dev/nvme0n1p3` with your Windows system partition. `bls config --device DEVICE` verifies that it contains `Windows/System32/config/SYSTEM`, then saves the resolved `/dev/...` device path, mount point, and BlueZ root in `/etc/bls.conf`. The default mount point is `/mnt/blsTemp`; use `bls config --mount-point PATH` to change it. The BlueZ root defaults to `/var/lib/bluetooth`; use `bls config --bluez-root PATH` to change it. Each setting can be updated separately, and `bls config` without options prints all saved values. The Windows partition must be configured before sync. Existing two-line configuration files and older UUID device paths remain readable. The installer builds the Rust binary, installs `bls.service`, and enables it for each boot. The Windows hive is never modified. If the same partition is already mounted elsewhere, `bls` creates a read-only bind mount at its own mount point and removes only that bind mount afterward.

On the next boot, `bls.service` starts `bls sync`, which mounts the Windows partition read-only, syncs, unmounts, and restarts Bluetooth. Check with `systemctl status bls.service` and `journalctl -u bls.service -b`. For an immediate sync, run `sudo bls sync`. `--dry-run` also mounts and unmounts, but only previews BlueZ changes and does not restart Bluetooth.

## Build packages

Use Rust, `dpkg-deb`, and `rpmbuild` to build the distribution artifacts locally. If `makepkg` is available, the script uses it to generate `.SRCINFO`; otherwise it writes the equivalent metadata directly. Pass the actual GitHub repository name because the AUR `-bin` recipe downloads the release archive from that repository:

```sh
scripts/package.sh OWNER/REPO
```

All package formats use the same `YYYYMMDDHHMMSS.BUILD_NUMBER` version in UTC, such as `20260924190000.42`. Local builds default to build number `1`; set `BLS_BUILD_NUMBER` to choose another. For a repeatable build, set `BLS_PACKAGE_VERSION` to the complete version or set `BLS_BUILD_TIME` and `BLS_BUILD_NUMBER` separately. `Cargo.toml` keeps the source crate version.

The script writes a DEB, an RPM, a prebuilt Linux tar archive, and `linux-bluetooth-sync-bin-*-aur.tar.gz` into `dist/`. The AUR archive contains `PKGBUILD` and `.SRCINFO`; it is a recipe for AUR submission, not a package to upload to AUR as a binary. The DEB, RPM, and AUR package install `bls` to `/usr/bin` and `bls.service` to `/usr/lib/systemd/system`. After installing a package, configure the Windows partition and enable the service:

```sh
sudo bls config --device /dev/nvme0n1p3
sudo systemctl enable --now bls.service
```

If you previously used `scripts/install.sh`, remove its `/usr/local/bin/bls` and `/etc/systemd/system/bls.service` copies before switching to a distribution package; those paths take precedence over package files. Keep `/etc/bls.conf` so the package can reuse the existing settings.

The GitHub workflow runs tests and builds packages for pull requests. Each push to `main` (or manual workflow run on `main`) also creates a GitHub Release tagged `vYYYYMMDDHHMMSS.BUILD_NUMBER`, using the UTC build time and workflow run number. Its DEB, RPM, prebuilt tar archive, and AUR recipe are available under [Releases](../../releases). The workflow deletes its temporary Actions artifact after publishing; the Release assets remain available. Publish the recipe's `PKGBUILD` and `.SRCINFO` to your AUR repository separately; the workflow does not require AUR credentials.

## Commands

```text
Usage:
  bls --help
  bls config [OPTIONS]
  bls info [OPTIONS]
  bls sync [OPTIONS]

Commands:
  config         Show or change saved settings
  info           List BlueZ devices and key status
  sync           Copy Windows pairing data to BlueZ

Run 'bls COMMAND --help' for command options.

Config options:
  --device DEVICE                 Windows block device
  --mount-point PATH              Mount point for the Windows partition
  --bluez-root PATH               BlueZ storage directory
  --help                          Show config help

Info options:
  --bluez-root PATH               Override the BlueZ storage directory
  --adapter MAC                   Filter by adapter address
  --device MAC                    Filter by device address
  --help                          Show info help

Sync options:
  --dry-run                       Preview changes without writing
  --bluez-root PATH               Override the BlueZ storage directory
  --classic-type MAC=TYPE         Override one Classic device's key type
  --default-classic-type TYPE     Fallback for new Classic devices
  --help                          Show sync help

TYPE: legacy, ssp, sc, or 0-8. MAC: Bluetooth address (AA:BB:CC:DD:EE:FF).
```

`bls info` lists current BlueZ devices from the configured BlueZ root, including names, device IDs, services, key types, and whether bond keys or remote IRKs are present. It does not print key material. Use `--adapter MAC` or `--device MAC` to filter the list. `bls sync --bluez-root PATH` and `bls info --bluez-root PATH` temporarily override the configured value. Run `info` as root when BlueZ storage is not readable by your user.

The tool reads `SYSTEM\Select\Current` to select the active Windows control set, then `Services\BTHPORT\Parameters\Keys` and `Devices`. It only writes under a BlueZ adapter directory matching the Windows adapter MAC. New device files are mode `0600`. Runs are idempotent. Pair a device in Windows first.

### Device metadata

For paired devices, `bls` imports Windows `Name` (or BLE `LEName`) into BlueZ `[General] Name`, `COD` into `Class`, `LEAppearance` into `Appearance`, and `VIDType`/`VID`/`PID`/`Version` into `[DeviceID]`. Service UUIDs from `ServicesFor<adapter>` are merged into `[General] Services`, preserving existing UUIDs and excluding Windows's placeholder UUID. BlueZ `Alias`, trust settings, existing GATT caches, connection timestamps, and raw SDP caches are preserved. Invalid or absent Windows metadata leaves existing BlueZ values intact.

### Classic key types

The 16-byte Windows Classic key value alone does **not** identify the pairing method. However, Windows also stores `SSP Paired` and `SSP MITM Protected` under `BTHPORT\Parameters\Devices\<device>\ServicesFor<adapter>`. If `SSP Paired=0`, `bls` uses Legacy (`Type=0`). If it is `1` and the remote `HostSupportedFeaturesMap` does not advertise Secure Connections, `bls` uses SSP (`Type=4` or `5` according to the MITM flag). That feature bit indicates **capability**, not the pairing method used to make an existing key, so SC-capable devices remain ambiguous. In that case `bls` retains BlueZ's existing `[LinkKey] Type` and `PINLength`. An existing LinkKey without `Type` means BlueZ's default `0`; for a wholly new Classic device, the fallback is `4`. Set `--classic-type AA:BB:CC:DD:EE:FF=sc` (or an exact BlueZ type `0..8`) when verified by a pairing trace. Overrides can be placed in a systemd drop-in for future boots.

### BLE and privacy

Windows `LTK`, `KeyLength`, `EDIV`, little-endian `ERand`, `IRK`, `Address`, and `AddressType` map to BlueZ `LongTermKey`, `IdentityResolvingKey`, and `[General] AddressType`. An SC-style LTK with zero `EDIV` and `ERand` is also written to BlueZ's peripheral-role groups; a legacy LTK with nonzero values leaves any distinct peripheral key intact. `CSRK` and `CSRKInbound` are imported when available. `Address` provides the stable identity MAC, even when Linux previously saw a different resolvable address. Windows `CentralIRK` (or `MasterIRK`) is written to the adapter's `identity` file as the **local** IRK; the per-device `IRK` is the **remote** IRK. BlueZ loads remote IRKs through the kernel management `Load Identity Resolving Keys` operation at startup. This requires controller/kernel LE Privacy support.

If the Windows identity address differs from an old Linux directory, `bls` creates the identity-address directory and leaves the old directory in place. Its old GATT cache is not copied automatically; the Windows name is imported when available. Bluetooth pairing modes and negotiated authentication are not fully described by Windows's registry fields; if a device rejects a connection, inspect the existing BlueZ key type and authentication properties.

Disable Windows Fast Startup/hibernation so the SYSTEM hive and NTFS volume are clean when Linux reads them. Keep `/var/lib/bluetooth` and the registry hive private: both contain reusable pairing secrets.

## References

- [ArchWiki Bluetooth: Dual boot pairing](https://wiki.archlinux.org/title/Bluetooth#Dual_boot_pairing)
- [BlueZ settings storage](https://bluez.readthedocs.io/en/latest/settings-storage/)
- [BlueZ management protocol](https://github.com/bluez/bluez/blob/master/doc/mgmt-protocol.rst)
- [Bluetooth Core: BR/EDR feature mask and pairing](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/br-edr-controller/link-manager-protocol-specification.html)
