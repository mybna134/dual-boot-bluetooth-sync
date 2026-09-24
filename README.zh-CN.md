# Linux Bluetooth Sync (`bls`)

[English](README.md) | 简体中文

`bls` 将离线 Windows `SYSTEM` 注册表配置单元中的蓝牙配对密钥和设备信息同步到 Linux 的 BlueZ。它支持经典蓝牙链接密钥（Legacy、SSP、Secure Connections）、BLE Legacy 和 Secure Connections LTK、远端 IRK，以及 Windows 主机的本地 IRK。每次同步都会以只读方式挂载配置的 Windows 分区，并在结束后卸载。每次成功的实际同步（手动执行或由开机时的 `bls.service` 执行）都会重启 `bluetooth.service`，让 BlueZ 重新加载密钥，并将远端 IRK 提交给内核和控制器，以解析 LE Privacy 地址。

## 在这台双系统电脑上安装

要求：使用 BlueZ 和 systemd 的 Linux；`chntpw` 软件包中的 `reged`；未处于休眠状态的 Windows NTFS 系统分区。

```sh
sudo pacman -S --needed bluez bluez-utils chntpw rust
sudo ./scripts/install.sh
sudo bls config --device /dev/nvme0n1p3
sudo bls sync --dry-run
sudo bls info
```

将 `/dev/nvme0n1p3` 换成你的 Windows 系统分区。`bls config --device DEVICE` 会检查其中是否存在 `Windows/System32/config/SYSTEM`，然后将解析后的 `/dev/...` 设备路径、挂载点和 BlueZ 根目录保存到 `/etc/bls.conf`。默认挂载点是 `/mnt/blsTemp`，可用 `bls config --mount-point PATH` 修改。BlueZ 根目录默认为 `/var/lib/bluetooth`，可用 `bls config --bluez-root PATH` 修改。各项设置可以单独更新；不带选项运行 `bls config` 会显示所有已保存的设置。同步前必须配置 Windows 分区。旧版双行配置文件及使用 UUID 的设备路径仍可读取。安装脚本会编译 Rust 程序、安装 `bls.service`，并启用开机运行。程序不会修改 Windows 注册表配置单元。如果同一分区已挂载在其他位置，`bls` 会在自己的挂载点创建只读绑定挂载，结束时仅移除该绑定挂载。

下次启动时，`bls.service` 会执行 `bls sync`：只读挂载 Windows 分区、同步、卸载，然后重启蓝牙服务。可用 `systemctl status bls.service` 和 `journalctl -u bls.service -b` 检查。要立即同步，运行 `sudo bls sync`。`--dry-run` 同样会挂载和卸载，但只预览对 BlueZ 的改动，不会重启蓝牙服务。

## 构建软件包

本地构建需要 Rust、`dpkg-deb` 和 `rpmbuild`。如果安装了 `makepkg`，脚本会用它生成 `.SRCINFO`；否则会直接生成等效的元数据。请传入实际的 GitHub 仓库名，因为 AUR `-bin` 配方会从该仓库下载 Release 压缩包：

```sh
scripts/package.sh OWNER/REPO
```

所有格式共用 UTC 时间的 `YYYYMMDDHHMMSS.BUILD_NUMBER` 版本号，例如 `20260924190000.42`。本地构建默认使用构建序号 `1`；可设置 `BLS_BUILD_NUMBER` 指定序号。要固定版本，可以设置完整的 `BLS_PACKAGE_VERSION`，或分别设置 `BLS_BUILD_TIME` 和 `BLS_BUILD_NUMBER`。`Cargo.toml` 保留源码 crate 的版本号。

脚本会在 `dist/` 中生成 DEB、RPM、预编译 Linux tar 压缩包，以及 `linux-bluetooth-sync-bin-*-aur.tar.gz`。AUR 压缩包包含 `PKGBUILD` 和 `.SRCINFO`，供提交到 AUR 使用，不是要上传到 AUR 的二进制软件包。DEB、RPM 和 AUR 软件包会把 `bls` 安装到 `/usr/bin`，把 `bls.service` 安装到 `/usr/lib/systemd/system`。安装软件包后，配置 Windows 分区并启用服务：

```sh
sudo bls config --device /dev/nvme0n1p3
sudo systemctl enable --now bls.service
```

如果之前用过 `scripts/install.sh`，改用发行版软件包前请移除它安装的 `/usr/local/bin/bls` 和 `/etc/systemd/system/bls.service`；这些路径的优先级高于软件包中的文件。保留 `/etc/bls.conf`，软件包可以继续使用原有设置。

GitHub 工作流会对拉取请求运行测试并构建软件包。每次推送到 `main`（或在 `main` 上手动运行工作流）还会创建 GitHub Release，标签格式为 `vYYYYMMDDHHMMSS.BUILD_NUMBER`，使用 UTC 构建时间和工作流运行序号。DEB、RPM、预编译 tar 压缩包和 AUR 配方可在 [Releases](../../releases) 下载。发布后，工作流会删除临时的 Actions artifact；Release 附件会保留。请另行将配方中的 `PKGBUILD` 和 `.SRCINFO` 发布到你的 AUR 仓库；工作流不需要 AUR 凭据。

## 命令

```text
用法：
  bls --help
  bls config [OPTIONS]
  bls info [OPTIONS]
  bls sync [OPTIONS]

命令：
  config         显示或修改保存的设置
  info           列出 BlueZ 设备和密钥状态
  sync           将 Windows 配对数据复制到 BlueZ

运行 'bls COMMAND --help' 查看各命令的选项。

config 选项：
  --device DEVICE                 Windows 块设备
  --mount-point PATH              Windows 分区的挂载点
  --bluez-root PATH               BlueZ 数据目录
  --help                          显示 config 帮助

info 选项：
  --bluez-root PATH               临时覆盖 BlueZ 数据目录
  --adapter MAC                   按适配器地址筛选
  --device MAC                    按设备地址筛选
  --help                          显示 info 帮助

sync 选项：
  --dry-run                       预览改动，不写入
  --bluez-root PATH               临时覆盖 BlueZ 数据目录
  --classic-type MAC=TYPE         覆盖指定经典蓝牙设备的密钥类型
  --default-classic-type TYPE     新经典蓝牙设备的默认类型
  --help                          显示 sync 帮助

TYPE：legacy、ssp、sc 或 0-8。MAC：蓝牙地址（AA:BB:CC:DD:EE:FF）。
```

`bls info` 从配置的 BlueZ 根目录列出当前设备，包括名称、设备 ID、服务、密钥类型，以及是否存在配对密钥或远端 IRK。它不会打印密钥内容。可用 `--adapter MAC` 或 `--device MAC` 筛选。`bls sync --bluez-root PATH` 和 `bls info --bluez-root PATH` 可临时覆盖配置值。如果当前用户无法读取 BlueZ 存储目录，请以 root 身份运行 `info`。

程序读取 `SYSTEM\Select\Current` 以选择当前 Windows 控制集，再读取 `Services\BTHPORT\Parameters\Keys` 和 `Devices`。它只会写入与 Windows 蓝牙适配器 MAC 匹配的 BlueZ 适配器目录。新设备文件的权限为 `0600`。重复运行不会产生额外改动。请先在 Windows 中与设备配对。

### 设备信息

对于已配对设备，`bls` 会将 Windows `Name`（或 BLE `LEName`）导入 BlueZ `[General] Name`，将 `COD` 导入 `Class`，将 `LEAppearance` 导入 `Appearance`，并将 `VIDType`、`VID`、`PID`、`Version` 导入 `[DeviceID]`。`ServicesFor<adapter>` 中的服务 UUID 会合并到 `[General] Services`，保留现有 UUID，并排除 Windows 的占位 UUID。BlueZ 的 `Alias`、信任设置、现有 GATT 缓存、连接时间戳和原始 SDP 缓存都会保留。无效或缺失的 Windows 元数据不会覆盖现有 BlueZ 值。

### 经典蓝牙密钥类型

Windows 中的 16 字节经典蓝牙密钥值本身**不能**确定配对方式。不过，Windows 还会在 `BTHPORT\Parameters\Devices\<device>\ServicesFor<adapter>` 中保存 `SSP Paired` 和 `SSP MITM Protected`。若 `SSP Paired=0`，`bls` 使用 Legacy（`Type=0`）。若其值为 `1`，且远端 `HostSupportedFeaturesMap` 未声明支持 Secure Connections，则根据 MITM 标志使用 SSP（`Type=4` 或 `5`）。该功能位表示设备的**能力**，不能说明现有密钥实际采用的配对方式，因此支持 SC 的设备仍有歧义。在这种情况下，`bls` 会保留 BlueZ 现有的 `[LinkKey] Type` 和 `PINLength`。现有 LinkKey 如果没有 `Type`，BlueZ 默认视为 `0`；对于全新的经典蓝牙设备，回退值为 `4`。如果已通过配对抓包确认类型，可设置 `--classic-type AA:BB:CC:DD:EE:FF=sc`（或明确指定 BlueZ 类型 `0..8`）。也可以在 systemd drop-in 中设置覆盖参数，使其在后续启动时生效。

### BLE 与隐私地址

Windows 中的 `LTK`、`KeyLength`、`EDIV`、小端序 `ERand`、`IRK`、`Address` 和 `AddressType` 会映射到 BlueZ 的 `LongTermKey`、`IdentityResolvingKey` 和 `[General] AddressType`。`EDIV` 与 `ERand` 均为零的 SC 风格 LTK 也会写入 BlueZ 的外围设备角色分组；非零值的 Legacy LTK 会保留已有的不同外围设备密钥。如果存在 `CSRK` 和 `CSRKInbound`，也会导入。`Address` 提供稳定的身份 MAC，即使 Linux 之前看到的是另一个可解析地址。Windows `CentralIRK`（或 `MasterIRK`）会作为**本地** IRK 写入适配器的 `identity` 文件；每个设备的 `IRK` 是**远端** IRK。BlueZ 启动时会通过内核管理接口的 `Load Identity Resolving Keys` 操作加载远端 IRK。这需要控制器和内核支持 LE Privacy。

如果 Windows 的身份地址与旧 Linux 目录不同，`bls` 会创建身份地址对应的目录，并保留旧目录。它不会自动复制旧目录中的 GATT 缓存；如果 Windows 中有名称，会导入该名称。Windows 注册表字段并未完整描述蓝牙配对模式和协商的认证方式；如果设备拒绝连接，请检查现有 BlueZ 密钥类型和认证属性。

请关闭 Windows 快速启动和休眠，以便 Linux 读取状态正常的 SYSTEM 配置单元和 NTFS 卷。请妥善保护 `/var/lib/bluetooth` 和注册表配置单元：两者都包含可重复使用的配对密钥。

## 参考资料

- [ArchWiki 蓝牙：双系统配对](https://wiki.archlinux.org/title/Bluetooth#Dual_boot_pairing)
- [BlueZ 设置存储格式](https://bluez.readthedocs.io/en/latest/settings-storage/)
- [BlueZ 管理协议](https://github.com/bluez/bluez/blob/master/doc/mgmt-protocol.rst)
- [蓝牙核心规范：BR/EDR 功能掩码与配对](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/br-edr-controller/link-manager-protocol-specification.html)
