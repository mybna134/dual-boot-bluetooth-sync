use crate::{RegValue, Registry, Result};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[repr(C)]
struct NativeHive {
    _private: [u8; 0],
}

type Visit = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    c_int,
    *const u8,
    c_int,
) -> c_int;

unsafe extern "C" {
    fn bls_hive_open(path: *const c_char) -> *mut NativeHive;
    fn bls_hive_close(hive: *mut NativeHive);
    fn bls_hive_walk(
        hive: *mut NativeHive,
        key: *const c_char,
        visit: Visit,
        context: *mut c_void,
    ) -> c_int;
}

pub struct Hive(*mut NativeHive);

impl Hive {
    pub fn open(path: &Path) -> Result<Self> {
        let path_c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("invalid hive path: {}", path.display()))?;
        // The wrapper always opens with HMODE_RO and owns the native hive.
        let hive = unsafe { bls_hive_open(path_c.as_ptr()) };
        if hive.is_null() {
            return Err(format!(
                "cannot open Windows registry hive: {}",
                path.display()
            ));
        }
        Ok(Self(hive))
    }

    pub fn read_tree(&self, key: &str) -> Result<Registry> {
        let key_c = CString::new(key).map_err(|_| "invalid registry key path")?;
        let mut result = Registry::new();
        // The callback and result remain alive for the synchronous native walk.
        let status = unsafe {
            bls_hive_walk(
                self.0,
                key_c.as_ptr(),
                collect,
                &mut result as *mut Registry as *mut c_void,
            )
        };
        match status {
            0 => Ok(result),
            -1 => Err(format!("Windows registry key not found: {key}")),
            _ => Err(format!("failed reading Windows registry key: {key}")),
        }
    }
}

impl Drop for Hive {
    fn drop(&mut self) {
        unsafe { bls_hive_close(self.0) };
    }
}

unsafe extern "C" fn collect(
    context: *mut c_void,
    path: *const c_char,
    name: *const c_char,
    kind: c_int,
    data: *const u8,
    length: c_int,
) -> c_int {
    if context.is_null() || path.is_null() || length < 0 || (length > 0 && data.is_null()) {
        return 1;
    }
    let registry = unsafe { &mut *(context as *mut Registry) };
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy();
    let path = format!("hkey_local_machine\\system\\{}", path.to_ascii_lowercase());
    let values = registry.entry(path).or_default();
    if name.is_null() {
        return 0;
    }
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .to_ascii_lowercase();
    let bytes = if length == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, length as usize) }
    };
    let value = match kind {
        3 | 11 => RegValue::Bytes(bytes.to_vec()), // REG_BINARY, REG_QWORD
        4 if bytes.len() == 4 => RegValue::Dword(u32::from_le_bytes(bytes.try_into().unwrap())),
        _ => return 0,
    };
    values.insert(name, value);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::CString;

    fn put_u16(hive: &mut [u8], offset: usize, value: u16) {
        hive[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(hive: &mut [u8], offset: usize, value: u32) {
        hive[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i32(hive: &mut [u8], offset: usize, value: i32) {
        hive[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    // Minimal valid REGF/HBIN with a root key, a Select subkey, and its
    // inline-independent DWORD value. Cell offsets follow the hive format.
    fn minimal_select_hive() -> Vec<u8> {
        let mut hive = vec![0; 0x2000];
        put_u32(&mut hive, 0x0000, 0x66676572); // regf
        put_u32(&mut hive, 0x0024, 0x20); // root cell offset
        put_u32(&mut hive, 0x0028, 0x1000); // hbin bytes after header
        put_u32(&mut hive, 0x01fc, 0x66677552); // XOR checksum of the REGF header

        put_u32(&mut hive, 0x1000, 0x6e696268); // hbin
        put_u32(&mut hive, 0x1004, 0); // self offset
        put_u32(&mut hive, 0x1008, 0x1000); // hbin length

        // Root NK cell at relative offset 0x20.
        put_i32(&mut hive, 0x1020, -0x54);
        put_u16(&mut hive, 0x1024, 0x6b6e); // nk
        put_u16(&mut hive, 0x1026, 0x2c); // root key
        put_u32(&mut hive, 0x1038, 1); // one subkey
        put_u32(&mut hive, 0x1040, 0x74); // LF cell offset

        // LF index references the Select NK cell at relative offset 0x84.
        put_i32(&mut hive, 0x1074, -0x10);
        put_u16(&mut hive, 0x1078, 0x666c); // lf
        put_u16(&mut hive, 0x107a, 1);
        put_u32(&mut hive, 0x107c, 0x84);
        hive[0x1080..0x1084].copy_from_slice(b"Sele");

        // Select NK cell at relative offset 0x84, with one value.
        put_i32(&mut hive, 0x1084, -0x58);
        put_u16(&mut hive, 0x1088, 0x6b6e); // nk
        put_u16(&mut hive, 0x108a, 0x20); // compressed ANSI name
        put_u32(&mut hive, 0x10ac, 1); // one value
        put_u32(&mut hive, 0x10b0, 0xdc); // value-list cell offset
        put_u16(&mut hive, 0x10d0, 6);
        hive[0x10d4..0x10da].copy_from_slice(b"Select");

        // Value list points at a VK cell named Current.
        put_i32(&mut hive, 0x10dc, -8);
        put_u32(&mut hive, 0x10e0, 0xe4);
        put_i32(&mut hive, 0x10e4, -0x20);
        put_u16(&mut hive, 0x10e8, 0x6b76); // vk
        put_u16(&mut hive, 0x10ea, 7);
        put_u32(&mut hive, 0x10ec, 4); // data length
        put_u32(&mut hive, 0x10f0, 0x104); // data cell offset
        put_u32(&mut hive, 0x10f4, 4); // REG_DWORD
        put_u16(&mut hive, 0x10f8, 1); // ANSI name
        hive[0x10fc..0x1103].copy_from_slice(b"Current");

        // External DWORD data followed by a free cell through the end of HBIN.
        put_i32(&mut hive, 0x1104, -8);
        put_u32(&mut hive, 0x1108, 1);
        put_i32(&mut hive, 0x110c, 0xef4);
        hive
    }

    fn write_test_hive() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bls-minimal-hive-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, minimal_select_hive()).unwrap();
        path
    }

    #[test]
    fn hive_open_reports_invalid_paths_without_panicking() {
        let nul_path = Path::new(std::ffi::OsStr::from_bytes(b"bad\0path"));
        assert!(Hive::open(nul_path)
            .err()
            .unwrap()
            .starts_with("invalid hive path:"));

        let missing = Path::new("/definitely/missing/bls-test-hive");
        assert!(Hive::open(missing)
            .err()
            .unwrap()
            .contains("cannot open Windows registry hive"));
    }

    #[test]
    fn reads_a_real_minimal_hive_and_collects_dword_values() {
        let path = write_test_hive();
        let hive = Hive::open(&path).unwrap();
        let selected = hive.read_tree("Select").unwrap();
        let values = &selected["hkey_local_machine\\system\\select"];
        assert!(matches!(values["current"], RegValue::Dword(1)));
        assert!(hive.read_tree("Missing").is_err());
        assert!(hive.read_tree("bad\0key").is_err());
        drop(hive);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reports_an_error_when_a_value_cell_is_malformed() {
        let path =
            std::env::temp_dir().join(format!("bls-malformed-value-hive-{}", std::process::id()));
        let mut fixture = minimal_select_hive();
        fixture[0x10e8..0x10ea].copy_from_slice(b"xx"); // corrupt the VK signature
        std::fs::write(&path, fixture).unwrap();
        let hive = Hive::open(&path).unwrap();
        let error = hive.read_tree("Select").unwrap_err();
        assert!(error.contains("failed reading Windows registry key: Select"));
        drop(hive);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_files_without_a_registry_header() {
        let invalid = std::env::temp_dir().join(format!("bls-invalid-hive-{}", std::process::id()));
        std::fs::write(&invalid, vec![0; 0x1000]).unwrap();
        assert!(Hive::open(&invalid).is_err());

        let mut bad_root = minimal_select_hive();
        put_u32(&mut bad_root, 0x24, 0);
        std::fs::write(&invalid, bad_root).unwrap();
        assert!(Hive::open(&invalid).is_err());
        std::fs::remove_file(invalid).unwrap();
    }

    #[test]
    fn callback_collects_supported_registry_values_case_insensitively() {
        let mut registry = Registry::new();
        let context = &mut registry as *mut Registry as *mut c_void;
        let path = CString::new("ControlSet001\\Devices\\AA").unwrap();
        let bytes_name = CString::new("Blob").unwrap();
        let raw = [1_u8, 2, 3, 4];
        let status = unsafe {
            collect(
                context,
                path.as_ptr(),
                bytes_name.as_ptr(),
                3,
                raw.as_ptr(),
                raw.len() as c_int,
            )
        };
        assert_eq!(status, 0);
        assert!(matches!(
            registry["hkey_local_machine\\system\\controlset001\\devices\\aa"]["blob"],
            RegValue::Bytes(ref value) if value == &raw
        ));

        let dword_name = CString::new("Count").unwrap();
        let raw = 0x12345678_u32.to_le_bytes();
        assert_eq!(
            unsafe {
                collect(
                    context,
                    path.as_ptr(),
                    dword_name.as_ptr(),
                    4,
                    raw.as_ptr(),
                    raw.len() as c_int,
                )
            },
            0
        );
        assert!(matches!(
            registry["hkey_local_machine\\system\\controlset001\\devices\\aa"]["count"],
            RegValue::Dword(0x12345678)
        ));

        let empty_name = CString::new("Empty").unwrap();
        assert_eq!(
            unsafe {
                collect(
                    context,
                    path.as_ptr(),
                    empty_name.as_ptr(),
                    3,
                    std::ptr::null(),
                    0,
                )
            },
            0
        );
        assert!(matches!(
            registry["hkey_local_machine\\system\\controlset001\\devices\\aa"]["empty"],
            RegValue::Bytes(ref value) if value.is_empty()
        ));

        let qword_name = CString::new("Wide").unwrap();
        let raw = [0x11_u8; 8];
        assert_eq!(
            unsafe {
                collect(
                    context,
                    path.as_ptr(),
                    qword_name.as_ptr(),
                    11,
                    raw.as_ptr(),
                    raw.len() as c_int,
                )
            },
            0
        );
        assert!(matches!(
            registry["hkey_local_machine\\system\\controlset001\\devices\\aa"]["wide"],
            RegValue::Bytes(ref value) if value == &raw
        ));

        // A DWORD with a non-four-byte payload is ignored.
        let bad_dword = [1_u8, 2, 3];
        assert_eq!(
            unsafe {
                collect(
                    context,
                    path.as_ptr(),
                    dword_name.as_ptr(),
                    4,
                    bad_dword.as_ptr(),
                    bad_dword.len() as c_int,
                )
            },
            0
        );
        assert!(matches!(
            registry["hkey_local_machine\\system\\controlset001\\devices\\aa"]["count"],
            RegValue::Dword(0x12345678)
        ));

        // Unsupported values are skipped without aborting the hive walk.
        assert_eq!(
            unsafe {
                collect(
                    context,
                    path.as_ptr(),
                    dword_name.as_ptr(),
                    1,
                    raw.as_ptr(),
                    raw.len() as c_int,
                )
            },
            0
        );
        assert!(matches!(
            registry["hkey_local_machine\\system\\controlset001\\devices\\aa"]["count"],
            RegValue::Dword(0x12345678)
        ));
    }

    #[test]
    fn callback_rejects_invalid_pointers_and_lengths() {
        let path = CString::new("Devices").unwrap();
        let name = CString::new("Value").unwrap();
        let mut registry = BTreeMap::new();
        let context = &mut registry as *mut Registry as *mut c_void;
        let byte = [1_u8];
        for (context, path_ptr, length, data) in [
            (std::ptr::null_mut(), path.as_ptr(), 1, byte.as_ptr()),
            (context, std::ptr::null(), 1, byte.as_ptr()),
            (context, path.as_ptr(), -1, byte.as_ptr()),
            (context, path.as_ptr(), 1, std::ptr::null()),
        ] {
            assert_eq!(
                unsafe { collect(context, path_ptr, name.as_ptr(), 3, data, length) },
                1
            );
        }

        // A value without a name still creates its containing registry key.
        assert_eq!(
            unsafe {
                collect(
                    context,
                    path.as_ptr(),
                    std::ptr::null(),
                    3,
                    std::ptr::null(),
                    0,
                )
            },
            0
        );
        assert!(registry.contains_key("hkey_local_machine\\system\\devices"));
    }
}
