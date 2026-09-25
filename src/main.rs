use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
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
    match fs::read_to_string(CONFIG) {
        Ok(data) => parse_mount_config(&data),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MountConfig::default()),
        Err(e) => Err(format!("{CONFIG}: {e}")),
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
    safe_dir(&config.mount_point)?;
    let occupied = Command::new("findmnt")
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
    let existing = Command::new("findmnt")
        .args(["-n", "-o", "TARGET", "--source", &config.device])
        .output()
        .map_err(|e| format!("findmnt: {e}"))?;
    if existing.status.success() {
        let output =
            String::from_utf8(existing.stdout).map_err(|_| "invalid existing mount path")?;
        let source = output.lines().next().ok_or("missing existing mount path")?;
        let status = Command::new("mount")
            .arg("--bind")
            .arg("--")
            .arg(source)
            .arg(&config.mount_point)
            .status()
            .map_err(|e| format!("bind mount: {e}"))?;
        if !status.success() {
            return Err(format!("bind mount failed: {status}"));
        }
        let status = Command::new("mount")
            .args(["-o", "remount,bind,ro,nosuid,nodev,noexec", "--"])
            .arg(&config.mount_point)
            .status()
            .map_err(|e| format!("read-only bind remount: {e}"));
        if !matches!(status, Ok(s) if s.success()) {
            let _ = unmount_windows(config);
            return Err("could not make bind mount read-only".into());
        }
        return Ok(());
    }
    let status = Command::new("mount")
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
    let status = Command::new("umount")
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

#[derive(Clone, Default, PartialEq, Eq)]
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
    let mut root = load_config()?.bluez_root;
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
            println!("Adapter: {address}  Device: {device_address}");
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
                    println!("  {label}: {value}");
                }
            }
            for (group, label) in [
                ("LinkKey", "Classic bond"),
                ("LongTermKey", "BLE bond"),
                ("IdentityResolvingKey", "Remote IRK"),
            ] {
                println!(
                    "  {label}: {}",
                    if ini.get(group, "Key").is_some() {
                        "present"
                    } else {
                        "absent"
                    }
                );
            }
            println!();
        }
    }
    if found == 0 {
        println!("No BlueZ devices found.");
    }
    Ok(())
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
    if path.exists() {
        let md = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !md.file_type().is_file() {
            return Err(format!("unsafe file: {}", path.display()));
        }
        // bluetoothd may reorder groups and keys; compare parsed values instead.
        if Ini::read(path)? == *ini {
            return Ok(false);
        }
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

fn sync(mut options: Options) -> Result<()> {
    let dry_run = options.dry_run;
    let config = load_config()?;
    if config.device.is_empty() {
        return Err("Windows partition is unset; run bls config --device DEVICE first".into());
    }
    if options.bluez_root.is_none() {
        options.bluez_root = Some(config.bluez_root.clone());
    }
    if !options.bluez_root.as_ref().unwrap().is_dir() {
        return Err("configured BlueZ root is not a directory".into());
    }
    mount_windows(&config)?;
    let result = sync_files(options, &config.mount_point);
    let unmounted = unmount_windows(&config);
    result?;
    unmounted?;
    if !dry_run {
        restart_bluetooth()?;
    }
    Ok(())
}

fn restart_bluetooth() -> Result<()> {
    let status = Command::new("systemctl")
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
    let mut changed = 0;
    let mut skipped = 0;
    for (section, values) in &registry {
        let suffix = match section.strip_prefix(&(prefix.clone() + "\\")) {
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
        let adapter_dir = options
            .bluez_root
            .as_ref()
            .expect("BlueZ root set by sync")
            .join(&adapter);
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
        for (section, values) in &registry {
            let Some(device_raw) = section.strip_prefix(&(prefix.clone() + "\\" + parts[0] + "\\"))
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

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
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
    };
    if let Err(e) = result {
        eprintln!("bls: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
