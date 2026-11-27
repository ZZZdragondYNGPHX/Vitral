#![cfg(all(target_os = "linux", feature = "native-rime"))]

use crate::eng::OpenCcMode;
use crate::error::VitralError;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

type OpenCcHandle = *mut c_void;

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NEXT: *mut c_void = (-1isize) as *mut c_void;

struct OpenCcApi {
    open: unsafe extern "C" fn(*const c_char) -> OpenCcHandle,
    close: unsafe extern "C" fn(OpenCcHandle) -> c_int,
    convert_utf8: unsafe extern "C" fn(OpenCcHandle, *const c_char, usize) -> *mut c_char,
    convert_utf8_free: unsafe extern "C" fn(*mut c_char),
}

static OPENCC_API: OnceLock<Result<OpenCcApi, String>> = OnceLock::new();

fn load_api() -> Result<OpenCcApi, String> {
    crate::eng::rime_ffi::probe_linked();
    let resolve = |name: &CStr| {
        let symbol = unsafe { dlsym(RTLD_NEXT, name.as_ptr()) };
        if symbol.is_null() {
            Err(format!("dlsym failed for {}", name.to_string_lossy()))
        } else {
            Ok(symbol)
        }
    };
    // OpenCC's C ABI defines these function signatures.
    unsafe {
        Ok(OpenCcApi {
            open: std::mem::transmute::<
                *mut c_void,
                unsafe extern "C" fn(*const c_char) -> OpenCcHandle,
            >(resolve(c"opencc_open")?),
            close: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(OpenCcHandle) -> c_int>(
                resolve(c"opencc_close")?,
            ),
            convert_utf8: std::mem::transmute::<
                *mut c_void,
                unsafe extern "C" fn(OpenCcHandle, *const c_char, usize) -> *mut c_char,
            >(resolve(c"opencc_convert_utf8")?),
            convert_utf8_free: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_char)>(
                resolve(c"opencc_convert_utf8_free")?,
            ),
        })
    }
}

struct OpenCcInstance {
    handle: OpenCcHandle,
    api: &'static OpenCcApi,
}

// OpenCC conversion instances are documented thread-safe in `/usr/include/opencc/opencc.h`
// (only `opencc_error()` is non-thread-safe, which we intentionally never call unlocked).
unsafe impl Send for OpenCcInstance {}
unsafe impl Sync for OpenCcInstance {}

impl Drop for OpenCcInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() && (self.handle as usize) != usize::MAX {
            unsafe {
                (self.api.close)(self.handle);
            }
        }
    }
}

static OPENCC_POOL: OnceLock<Mutex<HashMap<(PathBuf, u8), OpenCcInstance>>> = OnceLock::new();

fn config_filename_for_mode(mode: OpenCcMode) -> Option<&'static str> {
    match mode {
        OpenCcMode::Simplified => None,
        OpenCcMode::Traditional => Some("s2t.json"),
        OpenCcMode::TaiwanStandard => Some("s2twp.json"),
        OpenCcMode::HongKong => Some("s2hk.json"),
    }
}

fn mode_key(mode: OpenCcMode) -> u8 {
    match mode {
        OpenCcMode::Simplified => 0,
        OpenCcMode::Traditional => 1,
        OpenCcMode::TaiwanStandard => 2,
        OpenCcMode::HongKong => 3,
    }
}

pub fn convert_text(
    opencc_dir: &Path,
    mode: OpenCcMode,
    input: &str,
) -> Result<String, VitralError> {
    let Some(cfg_name) = config_filename_for_mode(mode) else {
        return Ok(input.to_string());
    };
    if input.is_empty() {
        return Ok(String::new());
    }

    let cfg_path = opencc_dir.join(cfg_name);
    if !cfg_path.is_file() {
        return Err(VitralError::NotFound(format!(
            "OpenCC config not found: {}",
            cfg_path.display()
        )));
    }

    let api = OPENCC_API
        .get_or_init(load_api)
        .as_ref()
        .map_err(|err| VitralError::Internal(err.clone()))?;
    let pool = OPENCC_POOL.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = pool.lock();
    let key = (opencc_dir.to_path_buf(), mode_key(mode));

    if !guard.contains_key(&key) {
        let c_path = CString::new(cfg_path.to_string_lossy().as_bytes())
            .map_err(|_| VitralError::InvalidArgument("NUL byte in OpenCC path".into()))?;
        let handle = unsafe { (api.open)(c_path.as_ptr()) };
        if handle.is_null() || (handle as usize) == usize::MAX {
            return Err(VitralError::Internal(format!(
                "opencc_open failed for {}",
                cfg_path.display()
            )));
        }
        guard.insert(key.clone(), OpenCcInstance { handle, api });
    }

    let inst = guard.get(&key).expect("inserted above");
    let c_in = CString::new(input.as_bytes())
        .map_err(|_| VitralError::InvalidArgument("NUL byte in OpenCC input".into()))?;
    let out_ptr = unsafe { (inst.api.convert_utf8)(inst.handle, c_in.as_ptr(), input.len()) };
    if out_ptr.is_null() {
        return Err(VitralError::Internal(
            "opencc_convert_utf8 returned NULL".into(),
        ));
    }
    let converted = unsafe { CStr::from_ptr(out_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        (inst.api.convert_utf8_free)(out_ptr);
    }
    Ok(converted)
}
