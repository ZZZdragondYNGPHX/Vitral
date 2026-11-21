use ort::environment::Environment;
#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;
use std::sync::OnceLock;

static ORT_ENV: OnceLock<Option<Environment>> = OnceLock::new();

#[cfg(target_os = "windows")]
fn windows_module_ort_path() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    extern "system" {
        fn GetModuleHandleExW(
            dw_flags: u32,
            lp_module_name: *const u16,
            ph_module: *mut *mut std::ffi::c_void,
        ) -> i32;
        fn GetModuleFileNameW(
            h_module: *mut std::ffi::c_void,
            lp_filename: *mut u16,
            n_size: u32,
        ) -> u32;
    }
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;
    let mut hmod: *mut std::ffi::c_void = std::ptr::null_mut();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows_module_ort_path as *const () as *const u16,
            &mut hmod,
        )
    };
    if ok == 0 || hmod.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let mod_path = PathBuf::from(OsString::from_wide(&buf[..len]));
    let candidate = mod_path.parent()?.join("onnxruntime.dll");
    candidate.is_file().then_some(candidate)
}

pub(crate) fn get_ort_env() -> Option<&'static Environment> {
    ORT_ENV
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            {
                ort::init().with_name("vitral").build().ok()
            }
            #[cfg(not(target_os = "linux"))]
            {
                #[cfg(target_os = "windows")]
                let name = "onnxruntime.dll";
                #[cfg(not(target_os = "windows"))]
                let name = "libonnxruntime.so";

                let mut candidates: Vec<PathBuf> = Vec::new();
                if let Some(p) = std::env::var_os("ORT_DYLIB_PATH").map(PathBuf::from) {
                    candidates.push(p);
                }
                #[cfg(target_os = "windows")]
                if let Some(p) = windows_module_ort_path() {
                    candidates.push(p);
                }
                if let Ok(exe) = std::env::current_exe() {
                    if let Some(dir) = exe.parent() {
                        let p = dir.join(name);
                        if p.is_file() {
                            candidates.push(p);
                        }
                    }
                }
                candidates.push(PathBuf::from(name));

                for path in candidates {
                    if let Ok(builder) = ort::init_from(path) {
                        if let Ok(env) = builder.with_name("vitral").build() {
                            return Some(env);
                        }
                    }
                }
                None
            }
        })
        .as_ref()
}
