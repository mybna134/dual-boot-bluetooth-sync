# Dual Boot Bluetooth Sync

English | [简体中文](README.md)

`bls` synchronizes Bluetooth pairing keys and device information from a dual-boot Windows installation to BlueZ on Linux. It reads the unmounted Windows `SYSTEM` registry hive and writes the data to the matching BlueZ adapter directory.

## Features

### Multiple pairing methods

The following pairing keys can be synchronized:

- Bluetooth Classic link keys
  - Legacy
  - SSP
  - SSP Secure Connections
- BLE Long Term Keys (LTKs)
  - Legacy
  - Secure Connections
- Remote IRKs and the Windows host's local IRK

### IRK synchronization

- Writes each device's remote IRK to BlueZ so Linux can load it at startup for LE Privacy address resolution.
- Writes the Windows host's local IRK to the BlueZ adapter's `identity` file.
- Requires LE Privacy support from the Bluetooth controller and kernel.

### Device information synchronization

- Imports the device name, class (COD), appearance, VID, PID, device version, and service UUIDs saved by Windows.
- Invalid or missing Windows metadata does not overwrite existing BlueZ values.
- Preserves BlueZ aliases, trust settings, GATT caches, connection timestamps, and raw SDP caches.

## Quick start

### Before installing

- Disable Windows Fast Startup and hibernation so Linux reads a clean, unhibernated NTFS partition and registry hive.
- BlueZ and systemd are required. The program reads the Windows registry hive directly; building from source also requires a C compiler.
- The Windows partition is mounted read-only; `bls` does not modify the Windows registry hive.
- Configure the Windows partition before the first sync. You can then run `sudo systemctl enable --now bls.service` to enable and start the service immediately.

### Install from source

Example for Arch Linux:

```sh
sudo pacman -S --needed bluez bluez-utils rust base-devel
sudo ./scripts/install.sh
# The installer builds the Rust program, installs `bls` to `/usr/local/bin/bls`, and installs and enables `bls.service`.
sudo bls config --device /dev/nvme0n1p3
# Replace `/dev/nvme0n1p3` with the device path of your Windows system partition.
# `bls config --device` checks for `Windows/System32/config/SYSTEM` and saves the resolved device path.
sudo systemctl enable --now bls.service
# Start the service.
```

### Install a distribution package

Download and install a DEB, RPM, or AUR package from [Releases](../../releases), then configure the Windows partition and enable the service:

```sh
sudo bls config --device /dev/nvme0n1p3
# Replace `/dev/nvme0n1p3` with the device path of your Windows system partition.
# `bls config --device` checks for `Windows/System32/config/SYSTEM` and saves the resolved device path.
sudo systemctl enable --now bls.service
# Start the service.
```

- If you previously installed from source with `scripts/install.sh`, remove `/usr/local/bin/bls` and `/etc/systemd/system/bls.service` before installing a distribution package. Keep `/etc/bls.conf` to reuse your settings.

## Build

Build from the repository root:

```sh
cargo build --release
```

The resulting program is at `target/release/bls`. To build distribution packages locally, use `scripts/package.sh OWNER/REPO`.

## Usage

Run configuration changes, sync operations, and operations that read BlueZ storage as root. Pair the target device in Windows first, then boot Linux and sync.

### Configure

Set the Windows system partition:

```sh
sudo bls config --device /dev/nvme0n1p3
```

`/etc/bls.conf` stores the device path, mount point, and BlueZ storage root.

The configuration file must contain exactly three lines: the device path, mount point, and BlueZ storage root. Use a direct device path such as `/dev/nvme0n1p3`; the old two-line configuration format and `/dev/disk/by-uuid/...` paths are no longer supported.

#### Options

- `--device DEVICE`: Windows system partition block device. Configuration checks that the partition contains `Windows/System32/config/SYSTEM` and saves the resolved `/dev/...` path.
- `--mount-point PATH`: Mount point for the Windows partition; defaults to `/mnt/blsTemp`.
- `--bluez-root PATH`: BlueZ storage directory; defaults to `/var/lib/bluetooth`.

#### Notes

- You can update each setting separately. Run `sudo bls config` without options to view the current settings.
- If the partition is already mounted elsewhere, `bls` creates a read-only bind mount at its own mount point and removes only that bind mount when it finishes.

### Sync manually

```sh
sudo bls sync --dry-run
sudo bls sync
```

#### Option

- `--dry-run` still mounts and unmounts the Windows partition read-only and previews BlueZ changes, but does not write data or restart the Bluetooth service.

#### Note

- A normal sync reads Windows pairing data, updates BlueZ, unmounts the Windows partition, and restarts `bluetooth.service` so BlueZ reloads the keys and submits remote IRKs to the kernel/controller.

### Sync automatically

`bls.service` is a one-shot systemd service that runs at boot. The installation script installs and enables it automatically.

- Check service status: `systemctl status bls.service`
- View logs from the current boot: `journalctl -u bls.service -b`

#### Note

- The service does not start a sync until `/etc/bls.conf` exists. Configure the Windows partition with `bls config` first.

### View BlueZ device information

```sh
sudo bls info
```

#### Options

- `--adapter MAC` or `--device MAC` filters by adapter or device address.
- `--bluez-root PATH` temporarily selects a BlueZ storage directory without changing the configuration.

#### Notes

- Lists adapters and devices under the configured BlueZ storage root, including names, device IDs, services, key types, and whether pairing keys or remote IRKs are present.
- Does not print key material. Run as root if your user cannot read the BlueZ storage directory.

## Details

### Bluetooth device information synchronization

- Windows `Name` (or BLE `LEName`) is written to BlueZ `[General] Name`.
- `COD` is written to `Class`, and `LEAppearance` to `Appearance`.
- `VIDType`, `VID`, `PID`, and `Version` are written to `[DeviceID]`.
- Service UUIDs from `ServicesFor<adapter>` are merged into `[General] Services`, preserving existing UUIDs and excluding Windows's placeholder UUID.
- BlueZ aliases, trust settings, existing GATT caches, connection timestamps, and raw SDP caches are preserved.
- Invalid or missing Windows metadata does not overwrite existing BlueZ values.

### Bluetooth Classic

- A 16-byte Classic key saved by Windows does not, by itself, identify the pairing method.
- `SSP Paired` and `SSP MITM Protected` under `BTHPORT\\Parameters\\Devices\\<device>\\ServicesFor<adapter>` identify some cases:
  - When `SSP Paired=0`, Legacy is used (`Type=0`).
  - When `SSP Paired=1` and the remote `HostSupportedFeaturesMap` does not advertise Secure Connections, SSP is used; the MITM flag determines `Type=4` or `5`.
- `HostSupportedFeaturesMap` indicates that the device supports Secure Connections, but does not prove that the existing key was generated with it. The key type for an SC-capable device may therefore remain ambiguous.
- In that case, the program preserves the existing BlueZ `[LinkKey] Type` and `PINLength`. An existing LinkKey without `Type` is treated as the BlueZ default, `0`; a new Classic device defaults to `Type=4`.
- If you have confirmed the key type from pairing logs, specify `--classic-type AA:BB:CC:DD:EE:FF=sc` or an exact BlueZ type from `0..8`. You can also set the override in a systemd drop-in for future boot-time syncs.

### BLE and LE Privacy

- Windows `LTK`, `KeyLength`, `EDIV`, little-endian `ERand`, `IRK`, `Address`, and `AddressType` map to BlueZ `LongTermKey`, `IdentityResolvingKey`, and `[General] AddressType`.
- An SC-style LTK with zero `EDIV` and `ERand` is also written to BlueZ's peripheral-role groups; a Legacy LTK with nonzero values does not overwrite an existing distinct peripheral key.
- `CSRK` and `CSRKInbound` are imported when available.
- `Address` provides a stable identity MAC, even if Linux previously saw a different resolvable address.
- Windows `CentralIRK` (or `MasterIRK`) is written to the adapter's `identity` file as the **local host IRK**; each device's `IRK` is its **remote IRK**.
- At startup, BlueZ loads remote IRKs through the kernel management interface's `Load Identity Resolving Keys` operation; this requires LE Privacy support from the controller and kernel.
- If the Windows identity address differs from an old Linux device directory, the program creates a new directory for the identity address and preserves the old directory. The old directory's GATT cache is not copied automatically; the Windows name is imported when available.
- Windows registry fields do not fully describe the Bluetooth pairing mode or negotiated authentication properties. If a device refuses to connect, check the existing BlueZ key type and authentication properties.

## References

- [ArchWiki Bluetooth: Dual boot pairing](https://wiki.archlinux.org/title/Bluetooth#Dual_boot_pairing)
- [BlueZ settings storage](https://bluez.readthedocs.io/en/latest/settings-storage/)
- [BlueZ management protocol](https://github.com/bluez/bluez/blob/master/doc/mgmt-protocol.rst)
- [Bluetooth Core Specification: BR/EDR feature mask and pairing](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/br-edr-controller/link-manager-protocol-specification.html)
