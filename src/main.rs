use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

mod registry;
use registry::Hive;

type Result<T> = std::result::Result<T, String>;
type Registry = BTreeMap<String, BTreeMap<String, RegValue>>;

#[derive(Clone, Debug)]
enum RegValue {
    Bytes(Vec<u8>),
    Dword(u32),
}

#[derive(Debug)]
struct Options {
    bluez_root: Option<PathBuf>,
    dry_run: bool,
    classic_types: BTreeMap<String, u8>,
    default_classic_type: u8,
}

fn help() {
    print!(
        "{}",
        r#"bls — Windows → BlueZ dual-boot pairing sync
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

Run as root for config changes, info, and sync. "bls config" shows settings.
Sync mounts the Windows partition read-only, unmounts it, then restarts
bluetooth.service. Reads the Windows SYSTEM hive directly.
Default mount point: /mnt/blsTemp; BlueZ root: /var/lib/bluetooth.
Classic Type is preserved from existing BlueZ info. New Classic devices
default to SSP unauthenticated (Type=4); set an override for Legacy/SC.
The Windows BTHPORT key alone does not record the Classic key type.
"#
    );
}

fn config_help() {
    print!(
        "{}",
        r#"Usage: bls config [OPTIONS]

Show saved settings when no options are given; otherwise update them.

Options:
  --device DEVICE       Windows block device
  --mount-point PATH    Mount point for the Windows partition
  --bluez-root PATH     BlueZ storage directory
  --help                Show this help
"#
    );
}

fn info_help() {
    print!(
        "{}",
        r#"Usage: bls info [OPTIONS]

List BlueZ devices and key status.

Options:
  --bluez-root PATH    Override the BlueZ storage directory
  --adapter MAC        Filter by adapter address
  --device MAC         Filter by device address
  --help               Show this help

MAC: Bluetooth address (AA:BB:CC:DD:EE:FF).
"#
    );
}

fn sync_help() {
    print!(
        "{}",
        r#"Usage: bls sync [OPTIONS]

Copy Windows pairing data to BlueZ.

Options:
  --dry-run                    Preview changes without writing
  --bluez-root PATH            Override the BlueZ storage directory
  --classic-type MAC=TYPE      Override one Classic device's key type
  --default-classic-type TYPE  Fallback for new Classic devices
  --help                       Show this help

TYPE: legacy, ssp, sc, or 0-8. MAC: Bluetooth address (AA:BB:CC:DD:EE:FF).
"#
    );
}

fn mac(raw: &str) -> Option<String> {
    let compact: String = raw.chars().filter(|c| *c != ':').collect();
    if compact.len() != 12 || !compact.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        compact
            .to_ascii_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).unwrap())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn parse_classic_type(s: &str) -> Result<u8> {
    match s.to_ascii_lowercase().as_str() {
        "legacy" => Ok(0),
        "ssp" => Ok(4),
        "sc" => Ok(7),
        _ => s
            .parse::<u8>()
            .ok()
            .filter(|n| *n <= 8)
            .ok_or_else(|| format!("invalid Classic type: {s}")),
    }
}

fn options(args: &[String]) -> Result<Options> {
    let mut out = Options {
        bluez_root: None,
        dry_run: false,
        classic_types: BTreeMap::new(),
        default_classic_type: 4,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => out.dry_run = true,
            "--bluez-root" | "--classic-type" | "--default-classic-type" => {
                let flag = &args[i];
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("missing value for {flag}"))?;
                match flag.as_str() {
                    "--bluez-root" => out.bluez_root = Some(value.into()),
                    "--default-classic-type" => {
                        out.default_classic_type = parse_classic_type(value)?
                    }
                    _ => {
                        let (address, kind) = value
                            .split_once('=')
                            .ok_or("--classic-type needs MAC=TYPE")?;
                        out.classic_types.insert(
                            mac(address).ok_or("invalid Classic MAC")?,
                            parse_classic_type(kind)?,
                        );
                    }
                }
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(out)
}

const CONFIG: &str = "/etc/bls.conf";

struct MountConfig {
    device: String,
    mount_point: PathBuf,
    bluez_root: PathBuf,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            device: String::new(),
            mount_point: "/mnt/blsTemp".into(),
            bluez_root: "/var/lib/bluetooth".into(),
        }
    }
}

fn parse_mount_config(data: &str) -> Result<MountConfig> {
    let mut lines = data.lines();
    let device = lines.next().ok_or("missing device in bls.conf")?;
    let mount_point = lines.next().ok_or("missing mount point in bls.conf")?;
    let bluez_root = lines.next().ok_or("missing BlueZ root in bls.conf")?;
    if lines.next().is_some()
        || (!device.is_empty()
            && (!device.starts_with("/dev/")
                || device.len() <= "/dev/".len()
                || device.starts_with("/dev/disk/by-uuid/")))
        || device.chars().any(|c| c == '\r' || c == '\n')
        || !Path::new(mount_point).is_absolute()
        || !Path::new(bluez_root).is_absolute()
        || mount_point.chars().any(|c| c == '\r' || c == '\n')
        || bluez_root.chars().any(|c| c == '\r' || c == '\n')
    {
        return Err("invalid /etc/bls.conf".into());
    }
    Ok(MountConfig {
        device: device.into(),
        mount_point: mount_point.into(),
        bluez_root: bluez_root.into(),
    })
}

fn canonical_device(device: &str) -> Result<String> {
    let metadata = fs::metadata(device).map_err(|e| format!("{device}: {e}"))?;
    if !metadata.file_type().is_block_device() {
        return Err(format!("{device} is not a block device"));
    }
    let path = fs::canonicalize(device).map_err(|e| format!("{device}: {e}"))?;
    let path = path.to_str().ok_or("device path is not UTF-8")?;
    if !path.starts_with("/dev/") {
        return Err("device must resolve under /dev".into());
    }
    Ok(path.into())
}

fn load_config() -> Result<MountConfig> {
    load_config_from(Path::new(CONFIG))
}

fn load_config_from(path: &Path) -> Result<MountConfig> {
    match fs::read_to_string(path) {
        Ok(data) => parse_mount_config(&data),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MountConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn config(args: &[String]) -> Result<()> {
    let current = load_config()?;
    if args.is_empty() {
        let device = if current.device.is_empty() {
            "(unset)".into()
        } else {
            canonical_device(&current.device).unwrap_or_else(|_| current.device.clone())
        };
        println!("device={}", device);
        println!("mount_point={}", current.mount_point.display());
        println!("bluez_root={}", current.bluez_root.display());
        return Ok(());
    }
    let mut device = None;
    let mut mount_point = None;
    let mut bluez_root = None;
    let mut i = 0;
    while i < args.len() {
        let flag = &args[i];
        i += 1;
        let value = args
            .get(i)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--device" => device = Some(value.as_str()),
            "--mount-point" => mount_point = Some(PathBuf::from(value)),
            "--bluez-root" => bluez_root = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown config option: {flag}")),
        }
        i += 1;
    }
    let verify_windows = device.is_some() || mount_point.is_some();
    let mount_point = mount_point.unwrap_or(current.mount_point);
    let bluez_root = bluez_root.unwrap_or(current.bluez_root);
    if !mount_point.is_absolute()
        || mount_point
            .to_string_lossy()
            .chars()
            .any(|c| c == '\r' || c == '\n')
    {
        return Err("mount point must be an absolute path without newlines".into());
    }
    if !bluez_root.is_absolute()
        || bluez_root
            .to_string_lossy()
            .chars()
            .any(|c| c == '\r' || c == '\n')
    {
        return Err("BlueZ root must be an absolute path without newlines".into());
    }
    if !bluez_root.is_dir() {
        return Err(format!(
            "BlueZ root is not a directory: {}",
            bluez_root.display()
        ));
    }
    let device = if let Some(device) = device {
        canonical_device(device)?
    } else {
        canonical_device(&current.device).unwrap_or(current.device)
    };
    let config = MountConfig {
        device,
        mount_point,
        bluez_root,
    };
    if verify_windows && !config.device.is_empty() {
        mount_windows(&config)?;
        let hive = config.mount_point.join("Windows/System32/config/SYSTEM");
        let valid = File::open(&hive)
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", hive.display()));
        let unmounted = unmount_windows(&config);
        valid?;
        unmounted?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(CONFIG)
        .map_err(|e| format!("{CONFIG}: {e}"))?;
    writeln!(
        file,
        "{}\n{}\n{}",
        config.device,
        config.mount_point.display(),
        config.bluez_root.display()
    )
    .map_err(|e| format!("{CONFIG}: {e}"))?;
    fs::set_permissions(CONFIG, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("{CONFIG}: {e}"))?;
    println!(
        "Configuration saved: device={}, mount_point={}, bluez_root={}",
        if config.device.is_empty() {
            "(unset)"
        } else {
            &config.device
        },
        config.mount_point.display(),
        config.bluez_root.display()
    );
    Ok(())
}

fn mount_windows(config: &MountConfig) -> Result<()> {
    mount_windows_with(config, &SystemCommands::default())
}

#[derive(Clone, Debug)]
struct SystemCommands {
    findmnt: PathBuf,
    mount: PathBuf,
    umount: PathBuf,
    systemctl: PathBuf,
}

impl Default for SystemCommands {
    fn default() -> Self {
        Self {
            findmnt: "findmnt".into(),
            mount: "mount".into(),
            umount: "umount".into(),
            systemctl: "systemctl".into(),
        }
    }
}

fn mount_windows_with(config: &MountConfig, commands: &SystemCommands) -> Result<()> {
    safe_dir(&config.mount_point)?;
    let occupied = Command::new(&commands.findmnt)
        .args([
            "--mountpoint",
            config.mount_point.to_str().ok_or("invalid mount point")?,
        ])
        .output()
        .map_err(|e| format!("findmnt: {e}"))?;
    if occupied.status.success() {
        return Err(format!(
            "mount point already in use: {}",
            config.mount_point.display()
        ));
    }
    let existing = Command::new(&commands.findmnt)
        .args(["-n", "-o", "TARGET", "--source", &config.device])
        .output()
        .map_err(|e| format!("findmnt: {e}"))?;
    if existing.status.success() {
        let output =
            String::from_utf8(existing.stdout).map_err(|_| "invalid existing mount path")?;
        let source = output.lines().next().ok_or("missing existing mount path")?;
        let status = Command::new(&commands.mount)
            .arg("--bind")
            .arg("--")
            .arg(source)
            .arg(&config.mount_point)
            .status()
            .map_err(|e| format!("bind mount: {e}"))?;
        if !status.success() {
            return Err(format!("bind mount failed: {status}"));
        }
        let status = Command::new(&commands.mount)
            .args(["-o", "remount,bind,ro,nosuid,nodev,noexec", "--"])
            .arg(&config.mount_point)
            .status()
            .map_err(|e| format!("read-only bind remount: {e}"));
        if !matches!(status, Ok(s) if s.success()) {
            let _ = unmount_windows_with(config, commands);
            return Err("could not make bind mount read-only".into());
        }
        return Ok(());
    }
    let status = Command::new(&commands.mount)
        .args(["-t", "ntfs3", "-o", "ro,nosuid,nodev,noexec", "--"])
        .arg(&config.device)
        .arg(&config.mount_point)
        .status()
        .map_err(|e| format!("mount: {e}"))?;
    if !status.success() {
        return Err(format!("mount failed: {status}"));
    }
    Ok(())
}

fn unmount_windows(config: &MountConfig) -> Result<()> {
    unmount_windows_with(config, &SystemCommands::default())
}

fn unmount_windows_with(config: &MountConfig, commands: &SystemCommands) -> Result<()> {
    let status = Command::new(&commands.umount)
        .arg(&config.mount_point)
        .status()
        .map_err(|e| format!("umount: {e}"))?;
    if !status.success() {
        return Err(format!("unmount failed: {status}"));
    }
    Ok(())
}

fn bytes<'a>(values: &'a BTreeMap<String, RegValue>, name: &str, len: usize) -> Option<&'a [u8]> {
    match values.get(&name.to_ascii_lowercase()) {
        Some(RegValue::Bytes(v)) if v.len() == len => Some(v),
        _ => None,
    }
}
fn dword(values: &BTreeMap<String, RegValue>, name: &str) -> Option<u32> {
    match values.get(&name.to_ascii_lowercase()) {
        Some(RegValue::Dword(n)) => Some(*n),
        _ => None,
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn registry_name(values: &BTreeMap<String, RegValue>, key: &str) -> Option<String> {
    let RegValue::Bytes(raw) = values.get(&key.to_ascii_lowercase())? else {
        return None;
    };
    let name = std::str::from_utf8(raw.split(|b| *b == 0).next()?)
        .ok()?
        .trim();
    if name.is_empty() || name.len() > 248 || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_string())
}

fn service_uuid(raw: &str) -> Option<String> {
    let uuid = raw.strip_prefix('{')?.strip_suffix('}')?;
    if uuid.len() != 36
        || uuid.as_bytes()[8] != b'-'
        || uuid.as_bytes()[13] != b'-'
        || uuid.as_bytes()[18] != b'-'
        || uuid.as_bytes()[23] != b'-'
        || !uuid
            .bytes()
            .filter(|b| *b != b'-')
            .all(|b| b.is_ascii_hexdigit())
        || uuid.eq_ignore_ascii_case("99999999-9999-9999-9999-999999999999")
    {
        return None;
    }
    Some(uuid.to_ascii_lowercase())
}

fn apply_windows_metadata(
    ini: &mut Ini,
    device: Option<&BTreeMap<String, RegValue>>,
    registry: &Registry,
    services_path: &str,
    is_le: bool,
) {
    if let Some(device) = device {
        let name = if is_le {
            registry_name(device, "LEName").or_else(|| registry_name(device, "Name"))
        } else {
            registry_name(device, "Name")
        };
        if let Some(name) = name {
            ini.set("General", "Name", &name);
        }
        if is_le {
            if let Some(appearance) =
                dword(device, "LEAppearance").filter(|v| *v > 0 && *v <= 0xffff)
            {
                ini.set("General", "Appearance", &format!("0x{appearance:04x}"));
            }
        } else if let Some(class) = dword(device, "COD").filter(|v| *v > 0 && *v <= 0xffffff) {
            ini.set("General", "Class", &format!("0x{class:06x}"));
        }
        if let (Some(source), Some(vendor), Some(product), Some(version)) = (
            dword(device, "VIDType").filter(|v| *v == 1 || *v == 2),
            dword(device, "VID").filter(|v| *v <= 0xffff),
            dword(device, "PID").filter(|v| *v <= 0xffff),
            dword(device, "Version").filter(|v| *v <= 0xffff),
        ) {
            for (key, value) in [
                ("Source", source),
                ("Vendor", vendor),
                ("Product", product),
                ("Version", version),
            ] {
                ini.set("DeviceID", key, &value.to_string());
            }
        }
    }

    let prefix = format!("{}\\", services_path.to_ascii_lowercase());
    let mut uuids: Vec<String> = ini
        .get("General", "Services")
        .unwrap_or("")
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut added = false;
    for path in registry.keys() {
        if let Some(raw) = path.strip_prefix(&prefix) {
            if let Some(uuid) = service_uuid(raw) {
                if !uuids.iter().any(|old| old.eq_ignore_ascii_case(&uuid)) {
                    uuids.push(uuid);
                    added = true;
                }
            }
        }
    }
    if added {
        ini.set("General", "Services", &format!("{};", uuids.join(";")));
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Ini {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}
impl Ini {
    fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let input = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut ini = Self::default();
        let mut group = String::new();
        for line in input.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.ends_with(']') {
                group = line[1..line.len() - 1].to_string();
                ini.sections.entry(group.clone()).or_default();
            } else if let Some((key, value)) = line.split_once('=') {
                if !group.is_empty() {
                    ini.set(&group, key.trim(), value.trim());
                }
            }
        }
        Ok(ini)
    }
    fn set(&mut self, group: &str, key: &str, value: &str) {
        self.sections
            .entry(group.into())
            .or_default()
            .insert(key.into(), value.into());
    }
    fn get(&self, group: &str, key: &str) -> Option<&str> {
        self.sections.get(group)?.get(key).map(String::as_str)
    }
    fn add_technology(&mut self, technology: &str) {
        let old = self.get("General", "SupportedTechnologies").unwrap_or("");
        let mut list: Vec<&str> = old.split(';').filter(|s| !s.is_empty()).collect();
        if !list.contains(&technology) {
            list.push(technology);
        }
        self.set(
            "General",
            "SupportedTechnologies",
            &format!("{};", list.join(";")),
        );
    }
    fn trust_if_new(&mut self) {
        if self.get("General", "Trusted").is_none() {
            self.set("General", "Trusted", "true");
        }
    }
    fn encode(&self) -> String {
        let mut out = String::new();
        for (group, keys) in &self.sections {
            out.push_str(&format!("[{group}]\n"));
            for (key, value) in keys {
                out.push_str(&format!("{key}={value}\n"));
            }
            out.push('\n');
        }
        out
    }
}

fn info(args: &[String]) -> Result<()> {
    info_to(Path::new(CONFIG), args, &mut io::stdout().lock())
}

fn info_to(config_path: &Path, args: &[String], mut output: impl Write) -> Result<()> {
    let rendered = info_from_config(config_path, args)?;
    output
        .write_all(rendered.as_bytes())
        .map_err(|e| format!("cannot write info output: {e}"))
}

fn info_from_config(config_path: &Path, args: &[String]) -> Result<String> {
    info_at(load_config_from(config_path)?.bluez_root, args)
}

fn info_at(mut root: PathBuf, args: &[String]) -> Result<String> {
    let mut adapter_filter = None;
    let mut device_filter = None;
    let mut i = 0;
    while i < args.len() {
        let flag = &args[i];
        i += 1;
        let value = args
            .get(i)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--bluez-root" => root = PathBuf::from(value),
            "--adapter" => {
                adapter_filter = Some(mac(value).ok_or("invalid adapter MAC")?);
            }
            "--device" => {
                device_filter = Some(mac(value).ok_or("invalid device MAC")?);
            }
            _ => return Err(format!("unknown info option: {flag}")),
        }
        i += 1;
    }
    let adapters = fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut found = 0;
    let mut output = String::new();
    for adapter in adapters {
        let adapter = adapter.map_err(|e| e.to_string())?;
        let name = adapter.file_name().to_string_lossy().into_owned();
        let Some(address) = mac(&name) else { continue };
        if adapter_filter
            .as_ref()
            .is_some_and(|filter| filter != &address)
        {
            continue;
        }
        if !adapter.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let devices = fs::read_dir(adapter.path()).map_err(|e| e.to_string())?;
        for device in devices {
            let device = device.map_err(|e| e.to_string())?;
            let name = device.file_name().to_string_lossy().into_owned();
            let Some(device_address) = mac(&name) else {
                continue;
            };
            if device_filter
                .as_ref()
                .is_some_and(|filter| filter != &device_address)
            {
                continue;
            }
            if !device.file_type().map_err(|e| e.to_string())?.is_dir() {
                continue;
            }
            let path = device.path().join("info");
            if !path.is_file() {
                continue;
            }
            let ini = Ini::read(&path)?;
            found += 1;
            writeln!(output, "Adapter: {address}  Device: {device_address}").unwrap();
            for (group, key, label) in [
                ("General", "Name", "Name"),
                ("General", "Alias", "Alias"),
                ("General", "AddressType", "Address type"),
                ("General", "SupportedTechnologies", "Technologies"),
                ("General", "Trusted", "Trusted"),
                ("General", "Class", "Class"),
                ("General", "Appearance", "Appearance"),
                ("General", "Services", "Services"),
                ("DeviceID", "Source", "Device ID source"),
                ("DeviceID", "Vendor", "Vendor ID"),
                ("DeviceID", "Product", "Product ID"),
                ("DeviceID", "Version", "Version"),
                ("LinkKey", "Type", "Classic key type"),
                ("LinkKey", "PINLength", "Classic PIN length"),
                ("LongTermKey", "Authenticated", "BLE authenticated"),
                ("LongTermKey", "EncSize", "BLE encryption size"),
            ] {
                if let Some(value) = ini.get(group, key) {
                    writeln!(output, "  {label}: {value}").unwrap();
                }
            }
            for (group, label) in [
                ("LinkKey", "Classic bond"),
                ("LongTermKey", "BLE bond"),
                ("IdentityResolvingKey", "Remote IRK"),
            ] {
                writeln!(
                    output,
                    "  {label}: {}",
                    if ini.get(group, "Key").is_some() {
                        "present"
                    } else {
                        "absent"
                    }
                )
                .unwrap();
            }
            output.push('\n');
        }
    }
    if found == 0 {
        output.push_str("No BlueZ devices found.\n");
    }
    Ok(output)
}

fn safe_dir(path: &Path) -> Result<()> {
    if path.exists() {
        let md = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !md.file_type().is_dir() {
            return Err(format!("unsafe directory: {}", path.display()));
        }
    } else {
        fs::create_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn write_ini(path: &Path, ini: &Ini, dry_run: bool) -> Result<bool> {
    let data = ini.encode();
    match fs::symlink_metadata(path) {
        Ok(md) => {
            if !md.file_type().is_file() {
                return Err(format!("unsafe file: {}", path.display()));
            }
            // bluetoothd may reorder groups and keys; compare parsed values instead.
            if Ini::read(path)? == *ini {
                return Ok(false);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    if dry_run {
        return Ok(true);
    }
    safe_dir(path.parent().ok_or("invalid output path")?)?;
    let temp = path.with_extension(format!("bls-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| format!("{}: {e}", temp.display()))?;
    file.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

fn set_ltk(ini: &mut Ini, key: &[u8], enc_size: u32, ediv: u32, rand: u64, authenticated: bool) {
    let groups: &[&str] = if ediv == 0 && rand == 0 {
        &["LongTermKey", "PeripheralLongTermKey", "SlaveLongTermKey"]
    } else {
        &["LongTermKey"]
    };
    for group in groups {
        ini.set(group, "Key", &hex(key));
        ini.set(
            group,
            "Authenticated",
            if authenticated { "1" } else { "0" },
        );
        ini.set(group, "EncSize", &enc_size.to_string());
        ini.set(group, "EDiv", &ediv.to_string());
        ini.set(group, "Rand", &rand.to_string());
    }
}

fn windows_classic_type(
    device: Option<&BTreeMap<String, RegValue>>,
    service: Option<&BTreeMap<String, RegValue>>,
) -> Option<u8> {
    let paired = dword(service?, "SSP Paired")?;
    if paired == 0 {
        return Some(0);
    }
    if paired != 1 {
        return None;
    }
    // This is the remote Host Supported Features page: bit 3 is SC support.
    // When it is set, SSP Paired alone cannot reveal P-192 vs P-256.
    let feature = bytes(device?, "HostSupportedFeaturesMap", 8)?[0];
    if feature & 0x08 != 0 {
        return None;
    }
    let mitm = dword(service?, "SSP MITM Protected")?;
    Some(if mitm == 0 { 4 } else { 5 })
}

fn classic_type(
    ini: &Ini,
    override_type: Option<u8>,
    windows_type: Option<u8>,
    default_type: u8,
) -> u8 {
    override_type
        .or(windows_type)
        .or_else(|| {
            ini.get("LinkKey", "Type")
                .and_then(|s| s.parse::<u8>().ok())
        })
        // BlueZ reads a missing Type as zero for an existing [LinkKey].
        .or_else(|| ini.get("LinkKey", "Key").map(|_| 0))
        .unwrap_or(default_type)
}

fn sync(options: Options) -> Result<()> {
    sync_with(options, Path::new(CONFIG), &SystemCommands::default())
}

fn sync_with(mut options: Options, config_path: &Path, commands: &SystemCommands) -> Result<()> {
    let dry_run = options.dry_run;
    let config = load_config_from(config_path)?;
    if config.device.is_empty() {
        return Err("Windows partition is unset; run bls config --device DEVICE first".into());
    }
    if options.bluez_root.is_none() {
        options.bluez_root = Some(config.bluez_root.clone());
    }
    if !options.bluez_root.as_ref().unwrap().is_dir() {
        return Err("configured BlueZ root is not a directory".into());
    }
    mount_windows_with(&config, commands)?;
    let result = sync_files(options, &config.mount_point);
    let unmounted = unmount_windows_with(&config, commands);
    result?;
    unmounted?;
    if !dry_run {
        restart_bluetooth_with(commands)?;
    }
    Ok(())
}

fn restart_bluetooth_with(commands: &SystemCommands) -> Result<()> {
    let status = Command::new(&commands.systemctl)
        .args(["restart", "bluetooth.service"])
        .status()
        .map_err(|e| format!("cannot restart bluetooth.service: {e}"))?;
    if !status.success() {
        return Err(format!("bluetooth.service restart failed: {status}"));
    }
    println!("bluetooth.service restarted");
    Ok(())
}

fn sync_files(options: Options, windows_root: &Path) -> Result<()> {
    let hive = windows_root.join("Windows/System32/config/SYSTEM");
    File::open(&hive).map_err(|e| format!("{}: {e}", hive.display()))?;
    let hive = Hive::open(&hive)?;
    let select = hive.read_tree("Select")?;
    let current = select
        .values()
        .find_map(|v| dword(v, "Current"))
        .ok_or("SYSTEM hive has no Select\\Current")?;
    if current == 0 || current > 999 {
        return Err("invalid Windows CurrentControlSet".into());
    }
    let prefix = format!(
        "hkey_local_machine\\system\\ControlSet{current:03}\\Services\\BTHPORT\\Parameters\\Keys"
    )
    .to_ascii_lowercase();
    let registry = hive.read_tree(&format!(
        "ControlSet{current:03}\\Services\\BTHPORT\\Parameters\\Keys"
    ))?;
    if !registry.contains_key(&prefix) {
        return Err("Windows BTHPORT Keys registry path not found".into());
    }
    let devices_prefix = format!(
        "hkey_local_machine\\system\\ControlSet{current:03}\\Services\\BTHPORT\\Parameters\\Devices"
    )
    .to_ascii_lowercase();
    let devices = match hive.read_tree(&format!(
        "ControlSet{current:03}\\Services\\BTHPORT\\Parameters\\Devices"
    )) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("bls: device metadata unavailable: {e}");
            Registry::new()
        }
    };
    sync_registry(options, &registry, &devices, &prefix, &devices_prefix)
}

fn sync_registry(
    options: Options,
    registry: &Registry,
    devices: &Registry,
    prefix: &str,
    devices_prefix: &str,
) -> Result<()> {
    let bluez_root = options.bluez_root.as_deref().ok_or("BlueZ root is unset")?;
    let mut changed = 0;
    let mut skipped = 0;
    for (section, values) in registry {
        let suffix = match section.strip_prefix(&(prefix.to_owned() + "\\")) {
            Some(s) => s,
            None => continue,
        };
        let parts: Vec<_> = suffix.split('\\').collect();
        if parts.len() != 1 {
            continue;
        }
        let Some(adapter) = mac(parts[0]) else {
            continue;
        };
        let adapter_dir = bluez_root.join(&adapter);
        if !adapter_dir.is_dir() {
            continue;
        }

        // CentralIRK is the Windows host's local IRK, not a remote-device IRK.
        if let Some(irk) =
            bytes(values, "CentralIRK", 16).or_else(|| bytes(values, "MasterIRK", 16))
        {
            let identity = adapter_dir.join("identity");
            let mut ini = Ini::read(&identity)?;
            ini.set("General", "IdentityResolvingKey", &hex(irk));
            if write_ini(&identity, &ini, options.dry_run)? {
                changed += 1;
                println!("{}: local IRK updated", adapter);
            }
        }

        for (name, value) in values {
            let Some(device) = mac(name) else { continue };
            let RegValue::Bytes(link_key) = value else {
                continue;
            };
            if link_key.len() != 16 {
                skipped += 1;
                continue;
            }
            let path = adapter_dir.join(&device).join("info");
            let mut ini = Ini::read(&path)?;
            let device_path = format!("{}\\{}", devices_prefix, name.to_ascii_lowercase());
            let service_path = format!(
                "{}\\servicesfor{}",
                device_path,
                parts[0].to_ascii_lowercase()
            );
            let windows_type =
                windows_classic_type(devices.get(&device_path), devices.get(&service_path));
            let kind = classic_type(
                &ini,
                options.classic_types.get(&device).copied(),
                windows_type,
                options.default_classic_type,
            );
            let pin_length = ini
                .get("LinkKey", "PINLength")
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(0);
            ini.add_technology("BR/EDR");
            ini.trust_if_new();
            ini.set("LinkKey", "Key", &hex(link_key));
            ini.set("LinkKey", "Type", &kind.to_string());
            ini.set("LinkKey", "PINLength", &pin_length.to_string());
            apply_windows_metadata(
                &mut ini,
                devices.get(&device_path),
                &devices,
                &service_path,
                false,
            );
            if write_ini(&path, &ini, options.dry_run)? {
                changed += 1;
                println!(
                    "{} / {}: Classic bond updated (Type={kind})",
                    adapter, device
                );
            }
        }
        for (section, values) in registry {
            let Some(device_raw) =
                section.strip_prefix(&(prefix.to_owned() + "\\" + parts[0] + "\\"))
            else {
                continue;
            };
            if device_raw.contains('\\') {
                continue;
            }
            let Some(device_key) = mac(device_raw) else {
                continue;
            };
            let Some(ltk) = bytes(values, "LTK", 16) else {
                skipped += 1;
                continue;
            };
            let identity = if let Some(address) = bytes(values, "Address", 8) {
                let mut address = address[..6].to_vec();
                address.reverse();
                mac(&hex(&address)).ok_or("invalid BLE identity address")?
            } else {
                device_key.clone()
            };
            let address_type = dword(values, "AddressType").unwrap_or(0);
            if address_type > 1 {
                skipped += 1;
                eprintln!("{}: unsupported BLE address type", identity);
                continue;
            }
            if address_type == 1
                && u8::from_str_radix(&identity[..2], 16).unwrap_or(0) & 0xc0 != 0xc0
            {
                skipped += 1;
                eprintln!("{}: random BLE address is not static identity", identity);
                continue;
            }
            let ediv = dword(values, "EDIV").unwrap_or(0);
            if ediv > u16::MAX as u32 {
                skipped += 1;
                continue;
            }
            let rand = bytes(values, "ERand", 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0);
            let size = dword(values, "KeyLength").unwrap_or(16);
            if !(7..=16).contains(&size) {
                skipped += 1;
                continue;
            }
            let authenticated = dword(values, "AuthReq").unwrap_or(0) & 0x04 != 0;
            let path = adapter_dir.join(&identity).join("info");
            let mut ini = Ini::read(&path)?;
            ini.set(
                "General",
                "AddressType",
                if address_type == 1 {
                    "static"
                } else {
                    "public"
                },
            );
            ini.add_technology("LE");
            ini.trust_if_new();
            let device_path = format!("{}\\{}", devices_prefix, device_raw);
            let service_path = format!("{}\\servicesfor{}", device_path, parts[0]);
            apply_windows_metadata(
                &mut ini,
                devices.get(&device_path),
                &devices,
                &service_path,
                true,
            );
            set_ltk(&mut ini, ltk, size, ediv, rand, authenticated);
            if let Some(irk) = bytes(values, "IRK", 16) {
                ini.set("IdentityResolvingKey", "Key", &hex(irk));
            }
            for (registry_name, group) in [
                ("CSRK", "LocalSignatureKey"),
                ("CSRKInbound", "RemoteSignatureKey"),
            ] {
                if let Some(csrk) = bytes(values, registry_name, 16) {
                    ini.set(group, "Key", &hex(csrk));
                    if ini.get(group, "Counter").is_none() {
                        ini.set(group, "Counter", "0");
                    }
                    ini.set(
                        group,
                        "Authenticated",
                        if authenticated { "1" } else { "0" },
                    );
                }
            }
            if write_ini(&path, &ini, options.dry_run)? {
                changed += 1;
                println!(
                    "{} / {}: BLE bond updated{}",
                    adapter,
                    identity,
                    if identity != device_key {
                        " (Windows identity address)"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    println!(
        "{}: {changed} file(s) {}, {skipped} invalid record(s) skipped",
        if options.dry_run { "dry run" } else { "sync" },
        if options.dry_run {
            "would change"
        } else {
            "changed"
        }
    );
    Ok(())
}

fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("--help") if args.len() == 1 => {
            help();
            Ok(())
        }
        Some("sync") if args[1..].iter().any(|arg| arg == "--help") => {
            sync_help();
            Ok(())
        }
        Some("config") if args[1..].iter().any(|arg| arg == "--help") => {
            config_help();
            Ok(())
        }
        Some("info") if args[1..].iter().any(|arg| arg == "--help") => {
            info_help();
            Ok(())
        }
        Some("sync") => options(&args[1..]).and_then(sync),
        Some("config") => config(&args[1..]),
        Some("info") => info(&args[1..]),
        Some(other) => Err(format!("unknown command: {other}; run bls --help")),
        None => Err("missing command; run bls --help".into()),
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = dispatch(&args);
    if let Err(e) = result {
        eprintln!("bls: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_path(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "bls-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn fake_command(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn fake_system_commands(
        dir: &Path,
        findmnt: &str,
        mount: &str,
        umount: &str,
        systemctl: &str,
    ) -> SystemCommands {
        SystemCommands {
            findmnt: fake_command(dir, "findmnt", findmnt),
            mount: fake_command(dir, "mount", mount),
            umount: fake_command(dir, "umount", umount),
            systemctl: fake_command(dir, "systemctl", systemctl),
        }
    }

    fn values(entries: &[(&str, RegValue)]) -> BTreeMap<String, RegValue> {
        entries
            .iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
            .collect()
    }

    struct HiveFixtureKey {
        name: String,
        values: Vec<(String, u32, Vec<u8>)>,
        children: Vec<HiveFixtureKey>,
        cell: u32,
    }

    #[derive(Default)]
    struct HiveFixture {
        next_cell: u32,
        cells: BTreeMap<u32, Vec<u8>>,
    }

    impl HiveFixture {
        fn new() -> Self {
            Self {
                next_cell: 0x20,
                cells: BTreeMap::new(),
            }
        }

        fn allocate(&mut self, content_len: usize) -> u32 {
            let total_len = (content_len + 7) & !3;
            let offset = self.next_cell;
            self.next_cell += total_len as u32;
            let mut cell = vec![0; total_len];
            cell[..4].copy_from_slice(&(-(total_len as i32)).to_le_bytes());
            self.cells.insert(offset, cell);
            offset
        }

        fn write(&mut self, cell: u32, content_offset: usize, bytes: &[u8]) {
            let target = self.cells.get_mut(&cell).unwrap();
            let start = 4 + content_offset;
            target[start..start + bytes.len()].copy_from_slice(bytes);
        }

        fn allocate_keys(&mut self, key: &mut HiveFixtureKey) {
            key.cell = self.allocate(0x4c + key.name.len());
            for child in &mut key.children {
                self.allocate_keys(child);
            }
        }

        fn write_keys(&mut self, key: &HiveFixtureKey) {
            let index_cell = if key.children.is_empty() {
                0
            } else {
                let cell = self.allocate(4 + key.children.len() * 8);
                self.write(cell, 0, &0x666c_u16.to_le_bytes()); // lf
                self.write(cell, 2, &(key.children.len() as u16).to_le_bytes());
                for (i, child) in key.children.iter().enumerate() {
                    let offset = 4 + i * 8;
                    self.write(cell, offset, &child.cell.to_le_bytes());
                    let name = child.name.as_bytes();
                    let mut hash = [0; 4];
                    let copy_len = name.len().min(hash.len());
                    hash[..copy_len].copy_from_slice(&name[..copy_len]);
                    self.write(cell, offset + 4, &hash);
                }
                cell
            };

            let mut value_cells = Vec::new();
            for (name, kind, data) in &key.values {
                let data_cell = self.allocate(data.len());
                self.write(data_cell, 0, data);
                let value_cell = self.allocate(0x14 + name.len());
                self.write(value_cell, 0, &0x6b76_u16.to_le_bytes()); // vk
                self.write(value_cell, 2, &(name.len() as u16).to_le_bytes());
                self.write(value_cell, 4, &(data.len() as u32).to_le_bytes());
                self.write(value_cell, 8, &data_cell.to_le_bytes());
                self.write(value_cell, 12, &kind.to_le_bytes());
                self.write(value_cell, 16, &1_u16.to_le_bytes()); // ANSI name
                self.write(value_cell, 0x14, name.as_bytes());
                value_cells.push(value_cell);
            }

            let value_list = if value_cells.is_empty() {
                0
            } else {
                let cell = self.allocate(value_cells.len() * 4);
                for (i, value) in value_cells.iter().enumerate() {
                    self.write(cell, i * 4, &value.to_le_bytes());
                }
                cell
            };

            self.write(key.cell, 0, &0x6b6e_u16.to_le_bytes()); // nk
            self.write(
                key.cell,
                2,
                &(if key.name.is_empty() {
                    0x2c_u16
                } else {
                    0x20_u16
                })
                .to_le_bytes(),
            );
            self.write(key.cell, 0x14, &(key.children.len() as u32).to_le_bytes());
            self.write(key.cell, 0x1c, &index_cell.to_le_bytes());
            self.write(key.cell, 0x24, &(value_cells.len() as u32).to_le_bytes());
            self.write(key.cell, 0x28, &value_list.to_le_bytes());
            self.write(key.cell, 0x48, &(key.name.len() as u16).to_le_bytes());
            self.write(key.cell, 0x4c, key.name.as_bytes());

            for child in &key.children {
                self.write_keys(child);
            }
        }

        fn finish(mut self, root_cell: u32) -> Vec<u8> {
            let used = self.next_cell;
            let free_len = 0x1000 - used;
            assert!(free_len >= 4);
            let mut free_cell = vec![0; free_len as usize];
            free_cell[..4].copy_from_slice(&(free_len as i32).to_le_bytes());
            self.cells.insert(used, free_cell);

            let mut hive = vec![0; 0x2000];
            hive[..4].copy_from_slice(b"regf");
            hive[0x24..0x28].copy_from_slice(&root_cell.to_le_bytes());
            hive[0x28..0x2c].copy_from_slice(&0x1000_u32.to_le_bytes());
            hive[0x1000..0x1004].copy_from_slice(b"hbin");
            hive[0x1008..0x100c].copy_from_slice(&0x1000_u32.to_le_bytes());
            for (offset, cell) in self.cells {
                let start = 0x1000 + offset as usize;
                hive[start..start + cell.len()].copy_from_slice(&cell);
            }
            let checksum = hive[..0x1fc].chunks_exact(4).fold(0_u32, |sum, word| {
                sum ^ u32::from_le_bytes(word.try_into().unwrap())
            });
            hive[0x1fc..0x200].copy_from_slice(&checksum.to_le_bytes());
            hive
        }
    }

    fn fixture_key(
        name: &str,
        values: Vec<(String, u32, Vec<u8>)>,
        children: Vec<HiveFixtureKey>,
    ) -> HiveFixtureKey {
        HiveFixtureKey {
            name: name.into(),
            values,
            children,
            cell: 0,
        }
    }

    fn fixture_value(name: &str, kind: u32, data: &[u8]) -> (String, u32, Vec<u8>) {
        (name.into(), kind, data.to_vec())
    }

    fn write_system_hive(key: HiveFixtureKey, path: &Path) {
        let mut key = key;
        let mut fixture = HiveFixture::new();
        fixture.allocate_keys(&mut key);
        let root_cell = key.cell;
        fixture.write_keys(&key);
        fs::write(path, fixture.finish(root_cell)).unwrap();
    }

    #[test]
    fn mount_unmount_and_restart_handle_command_statuses() {
        let root = temp_path("system-command-tests");
        fs::create_dir_all(&root).unwrap();
        let mount_point = root.join("mnt");
        fs::create_dir(&mount_point).unwrap();
        let config = MountConfig {
            device: "/dev/test-device".into(),
            mount_point: mount_point.clone(),
            bluez_root: root.clone(),
        };

        let occupied = fake_system_commands(&root, "exit 0", "exit 0", "exit 0", "exit 0");
        assert!(mount_windows_with(&config, &occupied)
            .unwrap_err()
            .contains("already in use"));

        let direct = fake_system_commands(&root, "exit 1", "exit 0", "exit 0", "exit 0");
        mount_windows_with(&config, &direct).unwrap();
        unmount_windows_with(&config, &direct).unwrap();

        let failed_mount = fake_system_commands(&root, "exit 1", "exit 7", "exit 0", "exit 0");
        assert!(mount_windows_with(&config, &failed_mount)
            .unwrap_err()
            .contains("mount failed"));
        let failed_unmount = fake_system_commands(&root, "exit 1", "exit 0", "exit 9", "exit 0");
        assert!(unmount_windows_with(&config, &failed_unmount)
            .unwrap_err()
            .contains("unmount failed"));

        let log = root.join("rollback.log");
        let log_path = log.display();
        let existing = "if [ \"$1\" = \"--mountpoint\" ]; then exit 1; fi\necho /source\nexit 0";
        let remount_failure =
            format!("echo \"$*\" >> '{log_path}'\nif [ \"$1\" = \"-o\" ]; then exit 1; fi\nexit 0");
        let rollback = fake_system_commands(
            &root,
            &existing,
            &remount_failure,
            &format!("echo umount >> '{log_path}'\nexit 0"),
            "exit 0",
        );
        assert!(mount_windows_with(&config, &rollback)
            .unwrap_err()
            .contains("read-only"));
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("--bind"));
        assert!(calls.contains("remount,bind,ro"));
        assert!(calls.contains("umount"));

        let existing_mount = fake_system_commands(&root, &existing, "exit 0", "exit 0", "exit 0");
        mount_windows_with(&config, &existing_mount).unwrap();
        let bind_failure = fake_system_commands(&root, &existing, "exit 1", "exit 0", "exit 0");
        assert!(mount_windows_with(&config, &bind_failure)
            .unwrap_err()
            .contains("bind mount failed"));

        let failed_restart = fake_system_commands(&root, "exit 1", "exit 0", "exit 0", "exit 1");
        assert!(restart_bluetooth_with(&failed_restart)
            .unwrap_err()
            .contains("restart failed"));
        let restart = fake_system_commands(&root, "exit 1", "exit 0", "exit 0", "exit 0");
        restart_bluetooth_with(&restart).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn test_options(root: &Path, dry_run: bool) -> Options {
        Options {
            bluez_root: Some(root.to_path_buf()),
            dry_run,
            classic_types: BTreeMap::new(),
            default_classic_type: 4,
        }
    }

    #[test]
    fn config_requires_three_lines_and_direct_device_path() {
        assert!(parse_mount_config("/dev/nvme0n1p3\n/mnt/blsTemp\n").is_err());
        assert!(parse_mount_config("/dev/disk/by-uuid/ABCD\n/mnt/blsTemp\n/tmp/bluez\n").is_err());
        let direct = parse_mount_config("/dev/nvme0n1p3\n/mnt/blsTemp\n/tmp/bluez\n").unwrap();
        assert_eq!(direct.device, "/dev/nvme0n1p3");
        let unconfigured = parse_mount_config("\n/mnt/blsTemp\n/tmp/bluez\n").unwrap();
        assert!(unconfigured.device.is_empty());
        assert!(parse_mount_config("/dev/nvme0n1p3\n/mnt/blsTemp\nrelative\n").is_err());
    }

    #[test]
    fn sync_files_reads_system_hive_and_writes_classic_bond() {
        let windows_root = temp_path("system-hive");
        let bluez_root = temp_path("system-hive-bluez");
        let adapter = "11:22:33:44:55:66";
        let device = "AA:BB:CC:DD:EE:FF";
        let hive_path = windows_root.join("Windows/System32/config/SYSTEM");
        fs::create_dir_all(hive_path.parent().unwrap()).unwrap();
        fs::create_dir_all(bluez_root.join(adapter)).unwrap();

        let key_tree = fixture_key(
            "Keys",
            vec![],
            vec![fixture_key(
                "112233445566",
                vec![
                    fixture_value("CentralIRK", 3, &[0xA5; 16]),
                    fixture_value("AABBCCDDEEFF", 3, &[0x11; 16]),
                ],
                vec![],
            )],
        );
        let devices_tree = fixture_key(
            "Devices",
            vec![],
            vec![fixture_key(
                "AABBCCDDEEFF",
                vec![fixture_value("Name", 3, b"Hive Headset\0")],
                vec![],
            )],
        );
        let parameters = fixture_key("Parameters", vec![], vec![key_tree, devices_tree]);
        let bthport = fixture_key("BTHPORT", vec![], vec![parameters]);
        let services = fixture_key("Services", vec![], vec![bthport]);
        let control_set = fixture_key("ControlSet001", vec![], vec![services]);
        let select = fixture_key(
            "Select",
            vec![fixture_value("Current", 4, &1_u32.to_le_bytes())],
            vec![],
        );
        write_system_hive(
            fixture_key("", vec![], vec![select, control_set]),
            &hive_path,
        );

        sync_files(test_options(&bluez_root, false), &windows_root).unwrap();

        let identity = Ini::read(&bluez_root.join(adapter).join("identity")).unwrap();
        assert_eq!(
            identity.get("General", "IdentityResolvingKey"),
            Some("A5".repeat(16).as_str())
        );
        let info = Ini::read(&bluez_root.join(adapter).join(device).join("info")).unwrap();
        assert_eq!(info.get("General", "Name"), Some("Hive Headset"));
        assert_eq!(info.get("LinkKey", "Key"), Some("11".repeat(16).as_str()));
        assert_eq!(info.get("LinkKey", "Type"), Some("4"));
        fs::remove_dir_all(windows_root).unwrap();
        fs::remove_dir_all(bluez_root).unwrap();
    }

    #[test]
    fn sync_files_rejects_invalid_current_and_tolerates_missing_device_metadata() {
        for (label, current, expected) in [
            (
                "zero-current",
                Some(0_u32),
                "invalid Windows CurrentControlSet",
            ),
            (
                "large-current",
                Some(1000),
                "invalid Windows CurrentControlSet",
            ),
            (
                "missing-current",
                None,
                "SYSTEM hive has no Select\\Current",
            ),
        ] {
            let root = temp_path(label);
            let hive_path = root.join("Windows/System32/config/SYSTEM");
            fs::create_dir_all(hive_path.parent().unwrap()).unwrap();
            let select_value = current
                .map(|n| fixture_value("Current", 4, &n.to_le_bytes()))
                .unwrap_or_else(|| fixture_value("Default", 4, &1_u32.to_le_bytes()));
            let system = fixture_key(
                "",
                vec![],
                vec![fixture_key("Select", vec![select_value], vec![])],
            );
            write_system_hive(system, &hive_path);
            let error = sync_files(test_options(&root, true), &root).unwrap_err();
            assert!(error.contains(expected), "unexpected error: {error}");
            fs::remove_dir_all(root).unwrap();
        }

        // Keys exists but the optional Devices tree does not. Sync should still
        // complete with metadata omitted and no adapter records to write.
        let root = temp_path("missing-devices-tree");
        let hive_path = root.join("Windows/System32/config/SYSTEM");
        fs::create_dir_all(hive_path.parent().unwrap()).unwrap();
        let keys = fixture_key("Keys", vec![], vec![]);
        let control_set = fixture_key(
            "ControlSet001",
            vec![],
            vec![fixture_key(
                "Services",
                vec![],
                vec![fixture_key(
                    "BTHPORT",
                    vec![],
                    vec![fixture_key("Parameters", vec![], vec![keys])],
                )],
            )],
        );
        let system = fixture_key(
            "",
            vec![],
            vec![
                fixture_key(
                    "Select",
                    vec![fixture_value("Current", 4, &1_u32.to_le_bytes())],
                    vec![],
                ),
                control_set,
            ],
        );
        write_system_hive(system, &hive_path);
        sync_files(test_options(&root, true), &root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_orchestration_mounts_unmounts_and_restarts_with_cleanup_on_errors() {
        let root = temp_path("sync-orchestration");
        let windows_root = root.join("windows");
        let bluez_root = root.join("bluez");
        let commands_dir = root.join("commands");
        let config_path = root.join("bls.conf");
        let log = root.join("commands.log");
        fs::create_dir_all(&windows_root).unwrap();
        fs::create_dir_all(&bluez_root).unwrap();
        fs::create_dir_all(&commands_dir).unwrap();
        let hive_path = windows_root.join("Windows/System32/config/SYSTEM");
        fs::create_dir_all(hive_path.parent().unwrap()).unwrap();
        let control_set = fixture_key(
            "ControlSet001",
            vec![],
            vec![fixture_key(
                "Services",
                vec![],
                vec![fixture_key(
                    "BTHPORT",
                    vec![],
                    vec![fixture_key(
                        "Parameters",
                        vec![],
                        vec![fixture_key("Keys", vec![], vec![])],
                    )],
                )],
            )],
        );
        write_system_hive(
            fixture_key(
                "",
                vec![],
                vec![
                    fixture_key(
                        "Select",
                        vec![fixture_value("Current", 4, &1_u32.to_le_bytes())],
                        vec![],
                    ),
                    control_set,
                ],
            ),
            &hive_path,
        );
        fs::write(
            &config_path,
            format!(
                "/dev/fake\n{}\n{}\n",
                windows_root.display(),
                bluez_root.display()
            ),
        )
        .unwrap();
        let log_path = log.display();
        let commands = fake_system_commands(
            &commands_dir,
            &format!("echo findmnt >> '{log_path}'\nexit 1"),
            &format!("echo mount >> '{log_path}'\nexit 0"),
            &format!("echo umount >> '{log_path}'\nexit 0"),
            &format!("echo systemctl >> '{log_path}'\nexit 0"),
        );

        sync_with(test_options(&bluez_root, false), &config_path, &commands).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            ["findmnt", "findmnt", "mount", "umount", "systemctl"]
        );

        fs::write(&log, "").unwrap();
        fs::remove_file(&hive_path).unwrap();
        assert!(sync_with(test_options(&bluez_root, false), &config_path, &commands).is_err());
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().last(), Some("umount"));
        assert!(!calls.lines().any(|call| call == "systemctl"));

        let control_set = fixture_key(
            "ControlSet001",
            vec![],
            vec![fixture_key(
                "Services",
                vec![],
                vec![fixture_key(
                    "BTHPORT",
                    vec![],
                    vec![fixture_key(
                        "Parameters",
                        vec![],
                        vec![fixture_key("Keys", vec![], vec![])],
                    )],
                )],
            )],
        );
        write_system_hive(
            fixture_key(
                "",
                vec![],
                vec![
                    fixture_key(
                        "Select",
                        vec![fixture_value("Current", 4, &1_u32.to_le_bytes())],
                        vec![],
                    ),
                    control_set,
                ],
            ),
            &hive_path,
        );
        fs::write(&log, "").unwrap();
        sync_with(test_options(&bluez_root, true), &config_path, &commands).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().last(), Some("umount"));
        assert!(!calls.lines().any(|call| call == "systemctl"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn info_lists_valid_devices_and_applies_adapter_device_filters() {
        let root = temp_path("info");
        let adapter = "AA:BB:CC:DD:EE:FF";
        let device = "11:22:33:44:55:66";
        let adapter_dir = root.join(adapter);
        let device_dir = adapter_dir.join(device);
        fs::create_dir_all(&device_dir).unwrap();
        fs::write(
            device_dir.join("info"),
            "[General]\nName=Keyboard\nAlias=Daily Driver\nAddressType=random\nSupportedTechnologies=LE;\nTrusted=true\nServices=0000180f-0000-1000-8000-00805f9b34fb;\n[DeviceID]\nVendor=123\n[LinkKey]\nKey=AA\nType=4\nPINLength=16\n[LongTermKey]\nKey=BB\nAuthenticated=1\nEncSize=16\n[IdentityResolvingKey]\nKey=CC\n",
        )
        .unwrap();

        let minimal_device = "22:33:44:55:66:77";
        let minimal_dir = adapter_dir.join(minimal_device);
        fs::create_dir_all(&minimal_dir).unwrap();
        fs::write(minimal_dir.join("info"), "[General]\nName=Mouse\n").unwrap();

        // Entries that do not represent a valid adapter/device are ignored.
        fs::create_dir_all(adapter_dir.join("not-a-device")).unwrap();
        fs::create_dir_all(adapter_dir.join("22:22:22:22:22:22")).unwrap(); // missing info
        fs::write(adapter_dir.join("33:33:33:33:33:33"), "not a directory").unwrap();
        fs::write(root.join("not-an-adapter"), "ignored").unwrap();
        fs::write(root.join("00:00:00:00:00:01"), "not a directory").unwrap();

        let all = info_at(root.clone(), &[]).unwrap();
        assert!(all.contains("Adapter: AA:BB:CC:DD:EE:FF  Device: 11:22:33:44:55:66"));
        for expected in [
            "Name: Keyboard",
            "Alias: Daily Driver",
            "Address type: random",
            "Technologies: LE;",
            "Trusted: true",
            "Vendor ID: 123",
            "Classic key type: 4",
            "Classic bond: present",
            "BLE bond: present",
            "Remote IRK: present",
        ] {
            assert!(all.contains(expected), "missing {expected:?} in {all}");
        }
        assert_eq!(all.matches("Adapter:").count(), 2);
        assert!(all.contains("Name: Mouse"));
        assert!(all.contains("Classic bond: absent"));
        assert!(all.contains("BLE bond: absent"));
        assert!(all.contains("Remote IRK: absent"));

        let adapter_only =
            info_at(root.clone(), &["--adapter".into(), "aabbccddeeff".into()]).unwrap();
        assert_eq!(adapter_only.matches("Adapter:").count(), 2);
        let device_only = info_at(
            root.clone(),
            &["--device".into(), "11:22:33:44:55:66".into()],
        )
        .unwrap();
        assert_eq!(device_only.matches("Adapter:").count(), 1);
        let override_root = info_at(
            root.join("missing"),
            &["--bluez-root".into(), root.display().to_string()],
        )
        .unwrap();
        assert_eq!(override_root.matches("Adapter:").count(), 2);
        assert!(info_at(
            root.clone(),
            &["--device".into(), "AA:AA:AA:AA:AA:AA".into()]
        )
        .unwrap()
        .contains("No BlueZ devices found."));

        assert!(info_at(root.clone(), &["--adapter".into()]).is_err());
        assert!(info_at(root.clone(), &["--adapter".into(), "bad".into()]).is_err());
        assert!(info_at(root.clone(), &["--device".into(), "bad".into()]).is_err());
        assert!(info_at(root.clone(), &["--unknown".into(), "value".into()]).is_err());
        assert!(info_at(root.join("missing"), &[]).is_err());

        let config_path = temp_path("info-config");
        fs::write(
            &config_path,
            format!("\n/mnt/blsTemp\n{}\n", root.display()),
        )
        .unwrap();
        let configured = info_from_config(&config_path, &[]).unwrap();
        assert_eq!(configured.matches("Adapter:").count(), 2);
        let mut printed = Vec::new();
        info_to(&config_path, &[], &mut printed).unwrap();
        assert_eq!(String::from_utf8(printed).unwrap(), configured);
        assert_eq!(
            load_config_from(&config_path.with_extension("missing"))
                .unwrap()
                .bluez_root,
            PathBuf::from("/var/lib/bluetooth")
        );
        assert!(info_from_config(&root, &[]).is_err());
        fs::remove_file(config_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_dispatch_handles_help_and_parse_errors() {
        for command in ["--help", "sync", "config", "info"] {
            if command == "--help" {
                assert!(dispatch(&[command.into()]).is_ok());
            } else {
                assert!(dispatch(&[command.into(), "--help".into()]).is_ok());
            }
        }
        assert!(dispatch(&[]).unwrap_err().contains("missing command"));
        assert!(dispatch(&["unknown".into()])
            .unwrap_err()
            .contains("unknown command"));
        assert!(dispatch(&["sync".into(), "--invalid".into()])
            .unwrap_err()
            .contains("unknown argument"));
    }

    #[test]
    fn ini_preserves_both_transports_and_trust() {
        let mut ini = Ini::default();
        ini.set("General", "SupportedTechnologies", "LE;");
        ini.set("General", "Trusted", "false");
        ini.add_technology("BR/EDR");
        ini.trust_if_new();
        assert_eq!(
            ini.get("General", "SupportedTechnologies"),
            Some("LE;BR/EDR;")
        );
        assert_eq!(ini.get("General", "Trusted"), Some("false"));
    }

    #[test]
    fn unchanged_ini_is_not_rewritten_for_different_field_order() {
        let path = std::env::temp_dir().join(format!("bls-ini-test-{}", std::process::id()));
        fs::write(&path, "[General]\nTrusted=true\nAddressType=public\n").unwrap();
        let mut ini = Ini::default();
        ini.set("General", "AddressType", "public");
        ini.set("General", "Trusted", "true");
        assert!(!write_ini(&path, &ini, false).unwrap());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn classic_modes_are_all_accepted() {
        assert_eq!(parse_classic_type("legacy").unwrap(), 0);
        assert_eq!(parse_classic_type("ssp").unwrap(), 4);
        assert_eq!(parse_classic_type("sc").unwrap(), 7);
        assert_eq!(parse_classic_type("8").unwrap(), 8);
        let mut ini = Ini::default();
        assert_eq!(classic_type(&ini, None, None, 4), 4);
        ini.set("LinkKey", "Key", "EXISTING");
        assert_eq!(classic_type(&ini, None, None, 4), 0);
        assert_eq!(classic_type(&ini, None, Some(4), 4), 4);
        assert_eq!(classic_type(&ini, Some(7), Some(4), 4), 7);
    }

    #[test]
    fn windows_classic_metadata_distinguishes_legacy_and_ssp() {
        let mut device = BTreeMap::new();
        let mut service = BTreeMap::new();
        device.insert(
            "hostsupportedfeaturesmap".into(),
            RegValue::Bytes(vec![1, 0, 0, 0, 0, 0, 0, 0]),
        );
        service.insert("ssp paired".into(), RegValue::Dword(0));
        assert_eq!(windows_classic_type(Some(&device), Some(&service)), Some(0));
        service.insert("ssp paired".into(), RegValue::Dword(1));
        service.insert("ssp mitm protected".into(), RegValue::Dword(0));
        assert_eq!(windows_classic_type(Some(&device), Some(&service)), Some(4));
        service.insert("ssp mitm protected".into(), RegValue::Dword(1));
        assert_eq!(windows_classic_type(Some(&device), Some(&service)), Some(5));
        device.insert(
            "hostsupportedfeaturesmap".into(),
            RegValue::Bytes(vec![9, 0, 0, 0, 0, 0, 0, 0]),
        );
        assert_eq!(windows_classic_type(Some(&device), Some(&service)), None);
    }

    #[test]
    fn secure_connections_ltk_populates_both_roles() {
        let mut ini = Ini::default();
        set_ltk(&mut ini, &[0xab; 16], 16, 0, 0, true);
        assert_eq!(
            ini.get("LongTermKey", "Key"),
            ini.get("PeripheralLongTermKey", "Key")
        );
        assert_eq!(ini.get("SlaveLongTermKey", "Rand"), Some("0"));
    }

    #[test]
    fn legacy_ltk_keeps_distinct_peripheral_key() {
        let mut ini = Ini::default();
        ini.set("PeripheralLongTermKey", "Key", "OLD");
        set_ltk(&mut ini, &[0xab; 16], 16, 4, 9, false);
        assert_eq!(ini.get("PeripheralLongTermKey", "Key"), Some("OLD"));
        assert_eq!(ini.get("LongTermKey", "EDiv"), Some("4"));
    }

    #[test]
    fn windows_metadata_merges_services_and_preserves_alias() {
        let mut ini = Ini::default();
        ini.set("General", "Alias", "My controller");
        ini.set(
            "General",
            "Services",
            "00001124-0000-1000-8000-00805f9b34fb;",
        );
        let mut device = BTreeMap::new();
        device.insert(
            "name".into(),
            RegValue::Bytes(b"Xbox Wireless Controller\0".to_vec()),
        );
        for (key, value) in [
            ("cod", 0x2508),
            ("vidtype", 2),
            ("vid", 0x45e),
            ("pid", 0x2e0),
            ("version", 1),
        ] {
            device.insert(key.into(), RegValue::Dword(value));
        }
        let prefix = "registry\\devices\\aabbccddeeff\\servicesfor001122334455";
        let mut registry = Registry::new();
        registry.insert(
            format!("{prefix}\\{{00001124-0000-1000-8000-00805f9b34fb}}"),
            BTreeMap::new(),
        );
        registry.insert(
            format!("{prefix}\\{{00001200-0000-1000-8000-00805f9b34fb}}"),
            BTreeMap::new(),
        );
        registry.insert(
            format!("{prefix}\\{{99999999-9999-9999-9999-999999999999}}"),
            BTreeMap::new(),
        );
        apply_windows_metadata(&mut ini, Some(&device), &registry, prefix, false);
        assert_eq!(ini.get("General", "Name"), Some("Xbox Wireless Controller"));
        assert_eq!(ini.get("General", "Alias"), Some("My controller"));
        assert_eq!(ini.get("General", "Class"), Some("0x002508"));
        assert_eq!(ini.get("DeviceID", "Vendor"), Some("1118"));
        assert_eq!(
            ini.get("General", "Services"),
            Some("00001124-0000-1000-8000-00805f9b34fb;00001200-0000-1000-8000-00805f9b34fb;")
        );
    }

    #[test]
    fn le_name_and_appearance_take_precedence() {
        let mut ini = Ini::default();
        let mut device = BTreeMap::new();
        device.insert("name".into(), RegValue::Bytes(b"Old\0".to_vec()));
        device.insert("lename".into(), RegValue::Bytes(b"Rainy 75-1\0".to_vec()));
        device.insert("leappearance".into(), RegValue::Dword(0x03c1));
        apply_windows_metadata(&mut ini, Some(&device), &Registry::new(), "none", true);
        assert_eq!(ini.get("General", "Name"), Some("Rainy 75-1"));
        assert_eq!(ini.get("General", "Appearance"), Some("0x03c1"));
    }

    #[test]
    fn mac_normalizes_input_and_rejects_malformed_addresses() {
        assert_eq!(
            mac("a1:b2:c3:d4:e5:f6").as_deref(),
            Some("A1:B2:C3:D4:E5:F6")
        );
        assert_eq!(mac("A1B2C3D4E5F6").as_deref(), Some("A1:B2:C3:D4:E5:F6"));
        for invalid in [
            "",
            "A1:B2:C3:D4:E5",
            "A1:B2:C3:D4:E5:FG",
            "A1-B2-C3-D4-E5-F6",
        ] {
            assert_eq!(mac(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn options_parse_overrides_and_reject_invalid_values() {
        let args = [
            "--dry-run",
            "--bluez-root",
            "/tmp/bluez",
            "--classic-type",
            "aa:bb:cc:dd:ee:ff=sc",
            "--default-classic-type",
            "legacy",
        ]
        .map(str::to_string);
        let parsed = options(&args).unwrap();
        assert!(parsed.dry_run);
        assert_eq!(parsed.bluez_root.as_deref(), Some(Path::new("/tmp/bluez")));
        assert_eq!(parsed.classic_types.get("AA:BB:CC:DD:EE:FF"), Some(&7));
        assert_eq!(parsed.default_classic_type, 0);

        for args in [
            vec!["--bluez-root"],
            vec!["--classic-type", "not-a-mac=ssp"],
            vec!["--classic-type", "AA:BB:CC:DD:EE:FF"],
            vec!["--default-classic-type", "9"],
            vec!["--mystery"],
        ] {
            assert!(options(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        }
    }

    #[test]
    fn registry_value_helpers_enforce_types_and_lengths() {
        let fields = values(&[
            ("key", RegValue::Bytes(vec![1, 2, 3])),
            ("count", RegValue::Dword(42)),
            ("wrong", RegValue::Bytes(vec![42])),
        ]);
        assert_eq!(bytes(&fields, "KEY", 3), Some(&[1, 2, 3][..]));
        assert_eq!(bytes(&fields, "key", 2), None);
        assert_eq!(bytes(&fields, "count", 4), None);
        assert_eq!(dword(&fields, "COUNT"), Some(42));
        assert_eq!(dword(&fields, "key"), None);
        assert_eq!(hex(&[0, 10, 255]), "000AFF");
    }

    #[test]
    fn registry_names_and_service_uuids_reject_invalid_metadata() {
        let names = values(&[
            ("valid", RegValue::Bytes(b"  Headset\0ignored".to_vec())),
            ("empty", RegValue::Bytes(b" \0".to_vec())),
            ("utf8", RegValue::Bytes(vec![0xff])),
            ("control", RegValue::Bytes(b"bad\nname\0".to_vec())),
            ("wrong-type", RegValue::Dword(3)),
        ]);
        assert_eq!(registry_name(&names, "VALID").as_deref(), Some("Headset"));
        for key in ["empty", "utf8", "control", "wrong-type", "missing"] {
            assert_eq!(registry_name(&names, key), None);
        }

        assert_eq!(
            service_uuid("{00001124-0000-1000-8000-00805F9B34FB}").as_deref(),
            Some("00001124-0000-1000-8000-00805f9b34fb")
        );
        for invalid in [
            "00001124-0000-1000-8000-00805f9b34fb",
            "{00001124-0000-1000-8000-00805f9b34fg}",
            "{99999999-9999-9999-9999-999999999999}",
        ] {
            assert_eq!(service_uuid(invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn ini_round_trips_and_ignores_values_outside_sections() {
        let path = temp_path("ini-read");
        fs::write(
            &path,
            "Outside=value\n[General]\n Name = Device \nUnknown\n[LinkKey]\nType=4=extra\n",
        )
        .unwrap();
        let ini = Ini::read(&path).unwrap();
        assert_eq!(ini.get("General", "Name"), Some("Device"));
        assert_eq!(ini.get("LinkKey", "Type"), Some("4=extra"));
        assert_eq!(ini.get("", "Outside"), None);
        assert_eq!(
            Ini::read(&path.with_extension("missing")).unwrap(),
            Ini::default()
        );
        let encoded = temp_path("ini-roundtrip");
        fs::write(&encoded, ini.encode()).unwrap();
        assert_eq!(Ini::read(&encoded).unwrap(), ini);
        fs::remove_file(path).unwrap();
        fs::remove_file(encoded).unwrap();
    }

    #[test]
    fn write_ini_dry_run_does_not_create_and_real_write_is_atomic() {
        let dir = temp_path("write");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("nested/info");
        let mut ini = Ini::default();
        ini.set("General", "Name", "Keyboard");
        assert!(write_ini(&path, &ini, true).unwrap());
        assert!(!path.exists());
        assert!(write_ini(&path, &ini, false).unwrap());
        assert_eq!(Ini::read(&path).unwrap(), ini);
        assert!(!write_ini(&path, &ini, false).unwrap());
        ini.set("General", "Trusted", "true");
        assert!(write_ini(&path, &ini, false).unwrap());
        assert_eq!(Ini::read(&path).unwrap(), ini);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn safe_directory_and_output_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let root = temp_path("unsafe");
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let dir_link = root.join("dir-link");
        symlink(&target, &dir_link).unwrap();
        assert!(safe_dir(&dir_link).is_err());

        let file_link = root.join("info");
        symlink(target.join("missing"), &file_link).unwrap();
        assert!(write_ini(&file_link, &Ini::default(), false).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn metadata_fallbacks_preserve_existing_values_and_validate_ranges() {
        let mut ini = Ini::default();
        ini.set("General", "Name", "Existing");
        ini.set("General", "Class", "0x000001");
        ini.set("DeviceID", "Vendor", "77");
        let device = values(&[
            ("name", RegValue::Bytes(b"\xff\0".to_vec())),
            ("cod", RegValue::Dword(0x1000000)),
            ("vidtype", RegValue::Dword(3)),
            ("vid", RegValue::Dword(1)),
            ("pid", RegValue::Dword(2)),
            ("version", RegValue::Dword(3)),
            ("leappearance", RegValue::Dword(0x10000)),
        ]);
        apply_windows_metadata(&mut ini, Some(&device), &Registry::new(), "none", false);
        assert_eq!(ini.get("General", "Name"), Some("Existing"));
        assert_eq!(ini.get("General", "Class"), Some("0x000001"));
        assert_eq!(ini.get("DeviceID", "Vendor"), Some("77"));
        apply_windows_metadata(&mut ini, Some(&device), &Registry::new(), "none", true);
        assert_eq!(ini.get("General", "Appearance"), None);
    }

    #[test]
    fn classic_type_falls_back_safely_when_stored_type_is_invalid() {
        let mut ini = Ini::default();
        ini.set("LinkKey", "Key", "EXISTING");
        ini.set("LinkKey", "Type", "invalid");
        assert_eq!(classic_type(&ini, None, None, 4), 0);
        ini.set("LinkKey", "Type", "9");
        assert_eq!(classic_type(&ini, None, None, 4), 9);

        assert_eq!(parse_classic_type("LEGACY").unwrap(), 0);
        assert!(parse_classic_type("9").is_err());
        assert!(parse_classic_type("nope").is_err());
    }

    #[test]
    fn windows_classic_type_handles_missing_and_invalid_fields() {
        assert_eq!(windows_classic_type(None, None), None);
        let no_device = BTreeMap::new();
        let service = values(&[("ssp paired", RegValue::Dword(1))]);
        assert_eq!(windows_classic_type(Some(&no_device), Some(&service)), None);
        let device = values(&[("hostsupportedfeaturesmap", RegValue::Bytes(vec![0; 7]))]);
        assert_eq!(windows_classic_type(Some(&device), Some(&service)), None);
        let unknown = values(&[("ssp paired", RegValue::Dword(2))]);
        assert_eq!(windows_classic_type(Some(&device), Some(&unknown)), None);
    }

    #[test]
    fn ltk_writes_authentication_and_leaves_unrelated_sections_alone() {
        let mut ini = Ini::default();
        set_ltk(&mut ini, &[0x12; 16], 12, 5, 8, false);
        assert_eq!(
            ini.get("LongTermKey", "Key"),
            Some("12".repeat(16).as_str())
        );
        assert_eq!(ini.get("LongTermKey", "Authenticated"), Some("0"));
        assert_eq!(ini.get("LongTermKey", "EncSize"), Some("12"));
        assert_eq!(ini.get("LongTermKey", "EDiv"), Some("5"));
        assert_eq!(ini.get("LongTermKey", "Rand"), Some("8"));
        assert_eq!(ini.get("PeripheralLongTermKey", "Key"), None);
    }

    #[test]
    fn sync_registry_imports_classic_ble_identity_and_metadata() {
        let root = temp_path("registry-sync");
        let adapter = "11:22:33:44:55:66";
        let classic_device = "AA:BB:CC:DD:EE:FF";
        let ble_device = "10:20:30:40:50:60";
        let identity = "66:55:44:33:22:11";
        let adapter_dir = root.join(adapter);
        fs::create_dir_all(adapter_dir.join(classic_device)).unwrap();
        fs::create_dir_all(adapter_dir.join(identity)).unwrap();

        let prefix =
            "hkey_local_machine\\system\\controlset001\\services\\bthport\\parameters\\keys";
        let devices_prefix =
            "hkey_local_machine\\system\\controlset001\\services\\bthport\\parameters\\devices";
        let adapter_path = format!("{prefix}\\{}", adapter.replace(':', ""));
        let mut adapter_values = values(&[
            ("centralirk", RegValue::Bytes(vec![0xA5; 16])),
            (
                &classic_device.replace(':', ""),
                RegValue::Bytes(vec![0x11; 16]),
            ),
            ("22:22:22:22:22:22", RegValue::Bytes(vec![0x44; 7])),
        ]);

        // A classic entry needs the adapter's key record; include metadata that
        // identifies SSP with MITM and verify BlueZ's existing user settings survive.
        let mut registry = Registry::new();
        registry.insert(adapter_path, std::mem::take(&mut adapter_values));
        let ble_path = format!(
            "{prefix}\\{}\\{}",
            adapter.replace(':', ""),
            ble_device.replace(':', "")
        );
        registry.insert(
            ble_path,
            values(&[
                ("ltk", RegValue::Bytes(vec![0x22; 16])),
                (
                    "address",
                    RegValue::Bytes(vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0, 0]),
                ),
                ("addresstype", RegValue::Dword(0)),
                ("ediv", RegValue::Dword(0)),
                ("erand", RegValue::Bytes(vec![0; 8])),
                ("keylength", RegValue::Dword(16)),
                ("authreq", RegValue::Dword(4)),
                ("irk", RegValue::Bytes(vec![0x33; 16])),
                ("csrk", RegValue::Bytes(vec![0x44; 16])),
                ("csrkinbound", RegValue::Bytes(vec![0x55; 16])),
            ]),
        );

        let classic_key = format!(
            "{devices_prefix}\\{}",
            classic_device.replace(':', "").to_ascii_lowercase()
        );
        let classic_service = format!(
            "{classic_key}\\servicesfor{}",
            adapter.replace(':', "").to_ascii_lowercase()
        );
        let ble_key = format!(
            "{devices_prefix}\\{}",
            ble_device.replace(':', "").to_ascii_lowercase()
        );
        let mut devices = Registry::new();
        for key in [&classic_key, &ble_key] {
            devices.insert(
                key.clone(),
                values(&[
                    ("name", RegValue::Bytes(b"Synced Device\0".to_vec())),
                    ("cod", RegValue::Dword(0x2508)),
                    ("hostsupportedfeaturesmap", RegValue::Bytes(vec![0; 8])),
                ]),
            );
        }
        devices.insert(
            classic_service.clone(),
            values(&[
                ("ssp paired", RegValue::Dword(1)),
                ("ssp mitm protected", RegValue::Dword(1)),
            ]),
        );
        devices.insert(
            format!("{classic_service}\\{{00001124-0000-1000-8000-00805f9b34fb}}"),
            BTreeMap::new(),
        );
        let mut services_probe = Ini::default();
        apply_windows_metadata(&mut services_probe, None, &devices, &classic_service, false);
        assert_eq!(
            services_probe.get("General", "Services"),
            Some("00001124-0000-1000-8000-00805f9b34fb;")
        );
        let ble_service = format!(
            "{ble_key}\\servicesfor{}",
            adapter.replace(':', "").to_ascii_lowercase()
        );
        devices.insert(
            format!("{ble_service}\\{{0000180f-0000-1000-8000-00805f9b34fb}}"),
            BTreeMap::new(),
        );

        let classic_info = adapter_dir.join(classic_device).join("info");
        fs::write(
            &classic_info,
            "[General]\nAlias=Keep me\nTrusted=false\nSupportedTechnologies=LE;\n[LinkKey]\nPINLength=7\n",
        )
        .unwrap();
        fs::write(adapter_dir.join("identity"), "[General]\nName=Adapter\n").unwrap();

        sync_registry(
            test_options(&root, false),
            &registry,
            &devices,
            prefix,
            devices_prefix,
        )
        .unwrap();

        let classic = Ini::read(&classic_info).unwrap();
        assert_eq!(
            classic.get("LinkKey", "Key"),
            Some("11".repeat(16).as_str())
        );
        assert_eq!(classic.get("LinkKey", "Type"), Some("5"));
        assert_eq!(classic.get("LinkKey", "PINLength"), Some("7"));
        assert_eq!(classic.get("General", "Alias"), Some("Keep me"));
        assert_eq!(classic.get("General", "Trusted"), Some("false"));
        assert_eq!(classic.get("General", "Class"), Some("0x002508"));
        assert!(classic
            .get("General", "Services")
            .unwrap()
            .contains("00001124-0000-1000-8000-00805f9b34fb"));

        let adapter_identity = Ini::read(&adapter_dir.join("identity")).unwrap();
        assert_eq!(
            adapter_identity.get("General", "IdentityResolvingKey"),
            Some("A5".repeat(16).as_str())
        );
        let ble = Ini::read(&adapter_dir.join(identity).join("info")).unwrap();
        assert_eq!(ble.get("General", "AddressType"), Some("public"));
        assert_eq!(ble.get("General", "Name"), Some("Synced Device"));
        assert_eq!(
            ble.get("LongTermKey", "Key"),
            Some("22".repeat(16).as_str())
        );
        assert_eq!(
            ble.get("PeripheralLongTermKey", "Key"),
            ble.get("LongTermKey", "Key")
        );
        assert_eq!(ble.get("LongTermKey", "Authenticated"), Some("1"));
        assert_eq!(
            ble.get("IdentityResolvingKey", "Key"),
            Some("33".repeat(16).as_str())
        );
        assert_eq!(ble.get("LocalSignatureKey", "Counter"), Some("0"));
        assert_eq!(ble.get("RemoteSignatureKey", "Authenticated"), Some("1"));
        assert!(ble
            .get("General", "Services")
            .unwrap()
            .contains("0000180f-0000-1000-8000-00805f9b34fb"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_registry_skips_invalid_ble_records_and_dry_run_does_not_write() {
        let root = temp_path("registry-dry-run");
        let adapter = "11:22:33:44:55:66";
        fs::create_dir_all(root.join(adapter)).unwrap();
        let prefix = "hkey_local_machine\\system\\controlset001\\keys";
        let devices_prefix = "hkey_local_machine\\system\\controlset001\\devices";
        let adapter_key = adapter.replace(':', "");
        let mut registry = Registry::new();
        registry.insert(
            format!("{prefix}\\{adapter_key}"),
            values(&[
                ("centralirk", RegValue::Bytes(vec![1; 16])),
                ("22:22:22:22:22:22", RegValue::Dword(5)),
                ("not-a-mac", RegValue::Bytes(vec![3; 16])),
            ]),
        );
        registry.insert(format!("{prefix}\\not-an-adapter"), BTreeMap::new());
        registry.insert(
            format!("{prefix}\\AABBCCDDEEFF"),
            values(&[("centralirk", RegValue::Bytes(vec![4; 16]))]),
        );
        registry.insert("outside\\the\\requested\\tree".into(), BTreeMap::new());
        let ltk = RegValue::Bytes(vec![2; 16]);
        for (device, extra) in [
            ("aabbccddee01", ("addresstype", RegValue::Dword(2))),
            ("aabbccddee02", ("ediv", RegValue::Dword(65536))),
            ("aabbccddee03", ("keylength", RegValue::Dword(6))),
            ("aabbccddee04", ("addresstype", RegValue::Dword(1))),
        ] {
            let mut record = values(&[("ltk", ltk.clone())]);
            if device.ends_with("04") {
                record.insert(
                    "address".into(),
                    RegValue::Bytes(vec![1, 2, 3, 4, 5, 6, 0, 0]),
                );
            }
            record.insert(extra.0.into(), extra.1);
            registry.insert(format!("{prefix}\\{adapter_key}\\{device}"), record);
        }
        registry.insert(
            format!("{prefix}\\{adapter_key}\\not-a-device"),
            values(&[("ltk", ltk.clone())]),
        );
        registry.insert(
            format!("{prefix}\\{adapter_key}\\aabbccddee05"),
            BTreeMap::new(),
        );
        registry.insert(
            format!("{prefix}\\{adapter_key}\\aabbccddee06"),
            values(&[("ltk", RegValue::Bytes(vec![2; 15]))]),
        );
        registry.insert(
            format!("{prefix}\\{adapter_key}\\aabbccddee07\\nested"),
            values(&[("ltk", ltk.clone())]),
        );
        let options = test_options(&root, true);
        let devices = Registry::new();
        assert!(sync_registry(options, &registry, &devices, prefix, devices_prefix).is_ok());
        assert!(!root.join(adapter).join("identity").exists());
        for device in [
            "AA:BB:CC:DD:EE:01",
            "AA:BB:CC:DD:EE:02",
            "AA:BB:CC:DD:EE:03",
            "AA:BB:CC:DD:EE:04",
            "AA:BB:CC:DD:EE:05",
            "AA:BB:CC:DD:EE:06",
            "AA:BB:CC:DD:EE:07",
        ] {
            assert!(!root.join(adapter).join(device).join("info").exists());
        }

        let missing_root = Options {
            bluez_root: None,
            dry_run: true,
            classic_types: BTreeMap::new(),
            default_classic_type: 4,
        };
        assert!(sync_registry(missing_root, &registry, &devices, prefix, devices_prefix).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_registry_imports_static_ble_and_falls_back_to_device_key() {
        let root = temp_path("registry-ble-boundaries");
        let adapter = "11:22:33:44:55:66";
        let static_identity = "C2:11:22:33:44:55";
        let public_identity = "AA:BB:CC:DD:EE:01";
        let adapter_dir = root.join(adapter);
        fs::create_dir_all(adapter_dir.join(static_identity)).unwrap();
        fs::create_dir_all(adapter_dir.join(public_identity)).unwrap();
        fs::write(adapter_dir.join("identity"), "[General]\nName=Adapter\n").unwrap();

        let prefix = "hkey_local_machine\\system\\controlset001\\keys";
        let devices_prefix = "hkey_local_machine\\system\\controlset001\\devices";
        let adapter_key = adapter.replace(':', "");
        let mut registry = Registry::new();
        registry.insert(
            format!("{prefix}\\{adapter_key}"),
            values(&[("centralirk", RegValue::Bytes(vec![0xA5; 16]))]),
        );
        registry.insert(
            format!("{prefix}\\{adapter_key}\\aabbccddee01"),
            values(&[("ltk", RegValue::Bytes(vec![0x11; 16]))]),
        );
        registry.insert(
            format!("{prefix}\\{adapter_key}\\aabbccddee02"),
            values(&[
                ("ltk", RegValue::Bytes(vec![0x22; 16])),
                // Windows stores the address least-significant byte first.
                (
                    "address",
                    RegValue::Bytes(vec![0x55, 0x44, 0x33, 0x22, 0x11, 0xC2, 0, 0]),
                ),
                ("addresstype", RegValue::Dword(1)),
            ]),
        );

        sync_registry(
            test_options(&root, false),
            &registry,
            &Registry::new(),
            prefix,
            devices_prefix,
        )
        .unwrap();

        let public = Ini::read(&adapter_dir.join(public_identity).join("info")).unwrap();
        assert_eq!(public.get("General", "AddressType"), Some("public"));
        assert_eq!(
            public.get("LongTermKey", "Key"),
            Some("11".repeat(16).as_str())
        );
        let static_ble = Ini::read(&adapter_dir.join(static_identity).join("info")).unwrap();
        assert_eq!(static_ble.get("General", "AddressType"), Some("static"));
        assert_eq!(
            static_ble.get("LongTermKey", "Key"),
            Some("22".repeat(16).as_str())
        );

        // A second import sees identical contents and leaves existing files alone.
        let before = fs::read(adapter_dir.join("identity")).unwrap();
        sync_registry(
            test_options(&root, false),
            &registry,
            &Registry::new(),
            prefix,
            devices_prefix,
        )
        .unwrap();
        assert_eq!(fs::read(adapter_dir.join("identity")).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}
