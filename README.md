# Dual Boot Bluetooth Sync

[English](README.en.md) | 简体中文

`bls` 用于将 Windows 双系统中的蓝牙配对密钥和设备信息同步到 Linux 的 BlueZ。它读取未挂载的 Windows `SYSTEM` 注册表配置单元，并将数据写入对应的 BlueZ 适配器目录。

## 功能

### 支持多种配对方式

支持同步以下配对密钥：

- Bluetooth Classic 链路密钥
  - Legacy
  - SSP
  - SSP Secure Connections
- BLE 长期密钥（LTK）
  - Legacy
  - Secure Connections
- 远端 IRK，以及 Windows 主机的本地 IRK

### IRK 同步

- 将设备的远端 IRK 写入 BlueZ，供 Linux 在启动时加载，用于 LE Privacy 地址解析。
- 将 Windows 主机的本地 IRK 写入 BlueZ 适配器的 `identity` 文件。
- 需要蓝牙控制器和内核支持 LE Privacy。

### 设备信息同步

- 导入 Windows 保存的设备名称、类别（COD）、外观（Appearance）、VID、PID、设备版本和服务 UUID。
- 无效或缺失的 Windows 元数据不会覆盖 BlueZ 中已有的值。
- 保留 BlueZ 的 Alias、信任设置、GATT 缓存、连接时间戳和原始 SDP 缓存。

## 快速上手

### 注意

- 安装前请关闭 Windows 快速启动和休眠，确保 Linux 读取到干净、未休眠的 NTFS 分区及注册表配置单元。
- 需要 BlueZ 和 systemd。程序直接读取 Windows 注册表配置单元；从源码构建还需要 C 编译器
- Windows 分区只会以只读方式挂载；`bls` 不会修改 Windows 注册表配置单元。
- 首次同步前必须配置 Windows 分区。完成后可以执行 `sudo systemctl enable --now bls.service` 立即启用服务。
### 从源码安装

Arch Linux 示例：

```sh
sudo pacman -S --needed bluez bluez-utils rust base-devel
sudo ./scripts/install.sh
# - 安装脚本会构建 Rust 程序，将 `bls` 安装到 `/usr/local/bin/bls`，安装并启用 `bls.service`。
sudo bls config --device /dev/nvme0n1p3
# 将 `/dev/nvme0n1p3` 替换成 Windows 系统分区对应的设备路径。
# `bls config --device` 会检查分区中是否存在 `Windows/System32/config/SYSTEM`，并保存解析后的设备路径。
sudo systemctl enable --now bls.service
# 启动服务
```

### 从发行包安装
从releases下载并安装 DEB、RPM 或 AUR 包后，配置 Windows 分区并启用服务：

```sh
sudo bls config --device /dev/nvme0n1p3
# 将 `/dev/nvme0n1p3` 替换成 Windows 系统分区对应的设备路径。
# `bls config --device` 会检查分区中是否存在 `Windows/System32/config/SYSTEM`，并保存解析后的设备路径。
sudo systemctl enable --now bls.service
# 启动服务
```

- 若此前使用 `scripts/install.sh`编译安装，请在安装发行包前删除 `/usr/local/bin/bls` 和 `/etc/systemd/system/bls.service` ；保留 `/etc/bls.conf` 可继续使用已有设置。

## 构建

在仓库根目录构建：

```sh
cargo build --release
```

生成的程序位于 `target/release/bls`。如需在本机生成发行包，运行 `scripts/package.sh OWNER/REPO`。

## 使用

所有配置修改、同步操作以及读取 BlueZ 存储的操作都应以 root 身份运行。先在 Windows 中与目标设备完成配对，再启动 Linux 并运行同步。

### 配置

设置 Windows 系统分区：

```sh
sudo bls config --device /dev/nvme0n1p3
```

`/etc/bls.conf` 为配置文件，保存了设备路径、挂载点和 BlueZ 存储根目录

配置文件必须恰好包含三行：设备路径、挂载点和 BlueZ 存储根目录。设备路径请使用 `/dev/nvme0n1p3` 这类直接设备路径；旧的两行配置格式和 `/dev/disk/by-uuid/...` 路径不再支持。
#### 参数
- `--device DEVICE`：Windows 系统分区块设备。配置时会验证分区包含 `Windows/System32/config/SYSTEM`，并保存解析后的 `/dev/...` 路径。
- `--mount-point PATH`：Windows 分区的挂载点，默认 `/mnt/blsTemp`。
- `--bluez-root PATH`：BlueZ 存储目录，默认 `/var/lib/bluetooth`。
#### 注意
- 各选项可以单独更新；不带选项运行 `sudo bls config` 可查看当前设置。
- 分区已在其他位置挂载时，`bls` 会在自己的挂载点建立只读 bind mount，并在结束后只移除该 bind mount。

### 手动同步

```sh
sudo bls sync --dry-run
sudo bls sync
```
#### 参数
- `--dry-run` 会照常只读挂载并卸载 Windows 分区，预览 BlueZ 变更，但不会写入数据，也不会重启 Bluetooth 服务。
#### 注意
- 正常同步会读取 Windows 配对数据、更新 BlueZ，然后卸载 Windows 分区并重启 `bluetooth.service`，使 BlueZ 重新加载密钥并将远端 IRK 提交给内核/控制器。

### 自动同步

`bls.service` 是开机时运行的一次性 systemd 服务。安装脚本会自动安装并启用它
- 查看服务状态：`systemctl status bls.service`
- 查看本次启动日志：`journalctl -u bls.service -b`

#### 注意
- 尚未创建 `/etc/bls.conf` 时服务不会启动同步，应该先使用 `bls config` 配置 Windows 分区。
### 查看 BlueZ 设备信息

```sh
sudo bls info
```
#### 参数
- `--adapter MAC` 或 `--device MAC` 可按适配器或设备地址筛选。
 - `--bluez-root PATH`可临时指定 BlueZ 存储目录，不修改配置。
#### 注意
- 列出配置的 BlueZ 存储根目录中的适配器和设备，包括名称、设备 ID、服务、密钥类型，以及是否存在配对密钥或远端 IRK。
- 不会输出密钥内容。若当前用户无权读取 BlueZ 存储目录，请以 root 身份运行。

## 细节

### 蓝牙设备信息同步

- Windows `Name`（或 BLE `LEName`）写入 BlueZ `[General] Name`。
- `COD` 写入 `Class`，`LEAppearance` 写入 `Appearance`。
- `VIDType`、`VID`、`PID`、`Version` 写入 `[DeviceID]`。
- `ServicesFor<adapter>` 中的服务 UUID 合并到 `[General] Services`，保留现有 UUID，并排除 Windows 的占位 UUID。
- BlueZ 的 `Alias`、信任设置、现有 GATT 缓存、连接时间戳和原始 SDP 缓存都会保留。
- 无效或缺失的 Windows 元数据不会覆盖已有 BlueZ 值。

### Bluetooth Classic

- Windows 保存的 16 字节 Classic 密钥本身无法标明配对时使用的方法。
- `BTHPORT\Parameters\Devices\<device>\ServicesFor<adapter>` 中的 `SSP Paired` 和 `SSP MITM Protected` 可用于识别部分情况：
  - `SSP Paired=0` 时使用 Legacy（`Type=0`）。
  - `SSP Paired=1` 且远端 `HostSupportedFeaturesMap` 未声明 Secure Connections 时，使用 SSP；MITM 标志决定 `Type=4` 或 `5`。
- `HostSupportedFeaturesMap` 表示设备具备 Secure Connections 能力，不代表现有密钥一定通过该方式生成。因此，支持 SC 的设备其密钥类型仍可能无法判断。
- 对于这种情况，程序保留 BlueZ 现有的 `[LinkKey] Type` 和 `PINLength`。已有 LinkKey 但缺少 `Type` 时按 BlueZ 默认值 `0` 处理；全新的 Classic 设备默认使用 `Type=4`。
- 若通过配对日志确认了密钥类型，可用 `--classic-type AA:BB:CC:DD:EE:FF=sc` 指定，或传入准确的 BlueZ 类型 `0..8`。也可以通过 systemd drop-in 设置覆盖项，以供之后开机同步使用。

### BLE 与 LE Privacy

- Windows 的 `LTK`、`KeyLength`、`EDIV`、小端序 `ERand`、`IRK`、`Address` 和 `AddressType` 会映射到 BlueZ 的 `LongTermKey`、`IdentityResolvingKey` 和 `[General] AddressType`。
- `EDIV` 与 `ERand` 均为零的 SC 类型 LTK 也会写入 BlueZ 的外围设备角色分组；非零值的 Legacy LTK 不会覆盖已有的独立外围密钥。
- 如果存在 `CSRK` 和 `CSRKInbound`，也会一并导入。
- `Address` 提供稳定的身份 MAC，即使 Linux 之前见到的是不同的可解析地址。
- Windows `CentralIRK`（或 `MasterIRK`）作为**本地主机 IRK**写入适配器的 `identity` 文件；每台设备的 `IRK` 是**远端 IRK**。
- BlueZ 在启动时通过内核管理接口的 `Load Identity Resolving Keys` 操作加载远端 IRK；这要求控制器和内核支持 LE Privacy。
- 若 Windows 身份地址与旧 Linux 设备目录不同，程序会创建身份地址对应的新目录，并保留旧目录。旧目录中的 GATT 缓存不会自动复制；若 Windows 中有名称，则会导入该名称。
- Windows 注册表字段无法完整描述蓝牙配对模式和协商出的认证属性。若设备拒绝连接，请检查 BlueZ 中已有的密钥类型和认证属性。

## 参考资料

- [ArchWiki 蓝牙：双系统配对](https://wiki.archlinux.org/title/Bluetooth#Dual_boot_pairing)
- [BlueZ 设置存储格式](https://bluez.readthedocs.io/en/latest/settings-storage/)
- [BlueZ 管理协议](https://github.com/bluez/bluez/blob/master/doc/mgmt-protocol.rst)
- [蓝牙核心规范：BR/EDR 功能掩码与配对](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/br-edr-controller/link-manager-protocol-specification.html)
