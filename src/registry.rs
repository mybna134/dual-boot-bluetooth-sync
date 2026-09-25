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
