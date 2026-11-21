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
    fn opencc_open(config_file_name: *const c_char) -> OpenCcHandle;
    fn opencc_close(opencc: OpenCcHandle) -> c_int;
    fn opencc_convert_utf8(
        opencc: OpenCcHandle,
        input: *const c_char,
        length: usize,
    ) -> *mut c_char;
    fn opencc_convert_utf8_free(str_ptr: *mut c_char);
}

struct OpenCcInstance {
    handle: OpenCcHandle,
}

// OpenCC conversion instances are documented thread-safe in `/usr/include/opencc/opencc.h`
// (only `opencc_error()` is non-thread-safe, which we intentionally never call unlocked).
unsafe impl Send for OpenCcInstance {}
unsafe impl Sync for OpenCcInstance {}

impl Drop for OpenCcInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() && (self.handle as usize) != usize::MAX {
            unsafe {
                opencc_close(self.handle);
            }
        }
    }
}

static OPENCC_POOL: OnceLock<Mutex<HashMap<(PathBuf, u8), OpenCcInstance>>> = OnceLock::new();

/// Reference `opencc_open` so GNU ld keeps `DT_NEEDED` on `libopencc.so` in downstream binaries.
pub fn probe_linked() {
    std::hint::black_box(opencc_open as *const ());
}

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

    let pool = OPENCC_POOL.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = pool.lock();
    let key = (opencc_dir.to_path_buf(), mode_key(mode));

    if !guard.contains_key(&key) {
        let c_path = CString::new(cfg_path.to_string_lossy().as_bytes())
            .map_err(|_| VitralError::InvalidArgument("NUL byte in OpenCC path".into()))?;
        let handle = unsafe { opencc_open(c_path.as_ptr()) };
        if handle.is_null() || (handle as usize) == usize::MAX {
            return Err(VitralError::Internal(format!(
                "opencc_open failed for {}",
                cfg_path.display()
            )));
        }
        guard.insert(key.clone(), OpenCcInstance { handle });
    }

    let inst = guard.get(&key).expect("inserted above");
    let c_in = CString::new(input.as_bytes())
        .map_err(|_| VitralError::InvalidArgument("NUL byte in OpenCC input".into()))?;
    let out_ptr = unsafe { opencc_convert_utf8(inst.handle, c_in.as_ptr(), input.len()) };
    if out_ptr.is_null() {
        return Err(VitralError::Internal("opencc_convert_utf8 returned NULL".into()));
    }
    let converted = unsafe { CStr::from_ptr(out_ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        opencc_convert_utf8_free(out_ptr);
    }
    Ok(converted)
}
