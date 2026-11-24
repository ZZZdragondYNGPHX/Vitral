#![cfg(all(target_os = "linux", feature = "native-rime"))]

use crate::eng::SchemaKind;
use crate::error::VitralError;
use parking_lot::Mutex;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub type RimeSessionId = usize;
pub type RimeBool = c_int;

#[repr(C)]
pub struct RimeTraits {
    pub data_size: c_int,
    pub shared_data_dir: *const c_char,
    pub user_data_dir: *const c_char,
    pub distribution_name: *const c_char,
    pub distribution_code_name: *const c_char,
    pub distribution_version: *const c_char,
    pub app_name: *const c_char,
    pub modules: *const *const c_char,
    pub min_log_level: c_int,
    pub log_dir: *const c_char,
    pub prebuilt_data_dir: *const c_char,
    pub staging_dir: *const c_char,
}

#[repr(C)]
pub struct RimeComposition {
    pub length: c_int,
    pub cursor_pos: c_int,
    pub sel_start: c_int,
    pub sel_end: c_int,
    pub preedit: *mut c_char,
}

#[repr(C)]
pub struct RimeCandidate {
    pub text: *mut c_char,
    pub comment: *mut c_char,
    pub reserved: *mut c_void,
}

#[repr(C)]
pub struct RimeMenu {
    pub page_size: c_int,
    pub page_no: c_int,
    pub is_last_page: RimeBool,
    pub highlighted_candidate_index: c_int,
    pub num_candidates: c_int,
    pub candidates: *mut RimeCandidate,
    pub select_keys: *mut c_char,
}

#[repr(C)]
pub struct RimeCommit {
    pub data_size: c_int,
    pub text: *mut c_char,
}

#[repr(C)]
pub struct RimeContext {
    pub data_size: c_int,
    pub composition: RimeComposition,
    pub menu: RimeMenu,
    pub commit_text_preview: *mut c_char,
    pub select_labels: *mut *mut c_char,
}

#[repr(C)]
pub struct RimeStatus {
    pub data_size: c_int,
    pub schema_id: *mut c_char,
    pub schema_name: *mut c_char,
    pub is_disabled: RimeBool,
    pub is_composing: RimeBool,
    pub is_ascii_mode: RimeBool,
    pub is_full_shape: RimeBool,
    pub is_simplified: RimeBool,
    pub is_traditional: RimeBool,
    pub is_ascii_punct: RimeBool,
}

#[repr(C)]
pub struct RimeCandidateListIterator {
    pub ptr: *mut c_void,
    pub index: c_int,
    pub candidate: RimeCandidate,
}

#[repr(C)]
pub struct RimeConfig {
    pub ptr: *mut c_void,
}

#[repr(C)]
pub struct RimeConfigIterator {
    pub list: *mut c_void,
    pub map: *mut c_void,
    pub index: c_int,
    pub key: *const c_char,
    pub path: *const c_char,
}

#[repr(C)]
pub struct RimeSchemaListItem {
    pub schema_id: *mut c_char,
    pub name: *mut c_char,
    pub reserved: *mut c_void,
}

#[repr(C)]
pub struct RimeSchemaList {
    pub size: usize,
    pub list: *mut RimeSchemaListItem,
}

#[repr(C)]
pub struct RimeStringSlice {
    pub str_ptr: *const c_char,
    pub length: usize,
}

type RimeNotificationHandler =
    Option<unsafe extern "C" fn(*mut c_void, RimeSessionId, *const c_char, *const c_char)>;

#[repr(C)]
pub struct RimeApi {
    pub data_size: c_int,
    pub setup: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    pub set_notification_handler:
        Option<unsafe extern "C" fn(RimeNotificationHandler, *mut c_void)>,
    pub initialize: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    pub finalize: Option<unsafe extern "C" fn()>,
    pub start_maintenance: Option<unsafe extern "C" fn(RimeBool) -> RimeBool>,
    pub is_maintenance_mode: Option<unsafe extern "C" fn() -> RimeBool>,
    pub join_maintenance_thread: Option<unsafe extern "C" fn()>,
    pub deployer_initialize: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    pub prebuild: Option<unsafe extern "C" fn() -> RimeBool>,
    pub deploy: Option<unsafe extern "C" fn() -> RimeBool>,
    pub deploy_schema: Option<unsafe extern "C" fn(*const c_char) -> RimeBool>,
    pub deploy_config_file: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> RimeBool>,
    pub sync_user_data: Option<unsafe extern "C" fn() -> RimeBool>,
    pub create_session: Option<unsafe extern "C" fn() -> RimeSessionId>,
    pub find_session: Option<unsafe extern "C" fn(RimeSessionId) -> RimeBool>,
    pub destroy_session: Option<unsafe extern "C" fn(RimeSessionId) -> RimeBool>,
    pub cleanup_stale_sessions: Option<unsafe extern "C" fn()>,
    pub cleanup_all_sessions: Option<unsafe extern "C" fn()>,
    pub process_key: Option<unsafe extern "C" fn(RimeSessionId, c_int, c_int) -> RimeBool>,
    pub commit_composition: Option<unsafe extern "C" fn(RimeSessionId) -> RimeBool>,
    pub clear_composition: Option<unsafe extern "C" fn(RimeSessionId)>,
    pub get_commit: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCommit) -> RimeBool>,
    pub free_commit: Option<unsafe extern "C" fn(*mut RimeCommit) -> RimeBool>,
    pub get_context: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeContext) -> RimeBool>,
    pub free_context: Option<unsafe extern "C" fn(*mut RimeContext) -> RimeBool>,
    pub get_status: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeStatus) -> RimeBool>,
    pub free_status: Option<unsafe extern "C" fn(*mut RimeStatus) -> RimeBool>,
    pub set_option: Option<unsafe extern "C" fn(RimeSessionId, *const c_char, RimeBool)>,
    pub get_option: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> RimeBool>,
    pub set_property: Option<unsafe extern "C" fn(RimeSessionId, *const c_char, *const c_char)>,
    pub get_property:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char, *mut c_char, usize) -> RimeBool>,
    pub get_schema_list: Option<unsafe extern "C" fn(*mut RimeSchemaList) -> RimeBool>,
    pub free_schema_list: Option<unsafe extern "C" fn(*mut RimeSchemaList)>,
    pub get_current_schema: Option<unsafe extern "C" fn(RimeSessionId, *mut c_char, usize) -> RimeBool>,
    pub select_schema: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> RimeBool>,
    pub schema_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> RimeBool>,
    pub config_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> RimeBool>,
    pub config_close: Option<unsafe extern "C" fn(*mut RimeConfig) -> RimeBool>,
    pub config_get_bool:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeBool) -> RimeBool>,
    pub config_get_int:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_int) -> RimeBool>,
    pub config_get_double:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut f64) -> RimeBool>,
    pub config_get_string:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_char, usize) -> RimeBool>,
    pub config_get_cstring: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> *const c_char>,
    pub config_update_signature:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> RimeBool>,
    pub config_begin_map: Option<
        unsafe extern "C" fn(*mut RimeConfigIterator, *mut RimeConfig, *const c_char) -> RimeBool,
    >,
    pub config_next: Option<unsafe extern "C" fn(*mut RimeConfigIterator) -> RimeBool>,
    pub config_end: Option<unsafe extern "C" fn(*mut RimeConfigIterator)>,
    pub simulate_key_sequence:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> RimeBool>,
    pub register_module: Option<unsafe extern "C" fn(*mut c_void) -> RimeBool>,
    pub find_module: Option<unsafe extern "C" fn(*const c_char) -> *mut c_void>,
    pub run_task: Option<unsafe extern "C" fn(*const c_char) -> RimeBool>,
    pub get_shared_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_user_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_sync_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_user_id: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_user_data_sync_dir: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub config_init: Option<unsafe extern "C" fn(*mut RimeConfig) -> RimeBool>,
    pub config_load_string: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> RimeBool>,
    pub config_set_bool:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, RimeBool) -> RimeBool>,
    pub config_set_int:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, c_int) -> RimeBool>,
    pub config_set_double:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, f64) -> RimeBool>,
    pub config_set_string:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *const c_char) -> RimeBool>,
    pub config_get_item:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeConfig) -> RimeBool>,
    pub config_set_item:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeConfig) -> RimeBool>,
    pub config_clear: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> RimeBool>,
    pub config_create_list: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> RimeBool>,
    pub config_create_map: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> RimeBool>,
    pub config_list_size: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> usize>,
    pub config_begin_list: Option<
        unsafe extern "C" fn(*mut RimeConfigIterator, *mut RimeConfig, *const c_char) -> RimeBool,
    >,
    pub get_input: Option<unsafe extern "C" fn(RimeSessionId) -> *const c_char>,
    pub get_caret_pos: Option<unsafe extern "C" fn(RimeSessionId) -> usize>,
    pub select_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> RimeBool>,
    pub get_version: Option<unsafe extern "C" fn() -> *const c_char>,
    pub set_caret_pos: Option<unsafe extern "C" fn(RimeSessionId, usize)>,
    pub select_candidate_on_current_page:
        Option<unsafe extern "C" fn(RimeSessionId, usize) -> RimeBool>,
    pub candidate_list_begin:
        Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCandidateListIterator) -> RimeBool>,
    pub candidate_list_next: Option<unsafe extern "C" fn(*mut RimeCandidateListIterator) -> RimeBool>,
    pub candidate_list_end: Option<unsafe extern "C" fn(*mut RimeCandidateListIterator)>,
    pub user_config_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> RimeBool>,
    pub candidate_list_from_index:
        Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCandidateListIterator, c_int) -> RimeBool>,
    pub get_prebuilt_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_staging_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub commit_proto: Option<unsafe extern "C" fn(RimeSessionId, *mut c_void)>,
    pub context_proto: Option<unsafe extern "C" fn(RimeSessionId, *mut c_void)>,
    pub status_proto: Option<unsafe extern "C" fn(RimeSessionId, *mut c_void)>,
    pub get_state_label:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char, RimeBool) -> *const c_char>,
    pub delete_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> RimeBool>,
    pub delete_candidate_on_current_page:
        Option<unsafe extern "C" fn(RimeSessionId, usize) -> RimeBool>,
    pub get_state_label_abbreviated: Option<
        unsafe extern "C" fn(RimeSessionId, *const c_char, RimeBool, RimeBool) -> RimeStringSlice,
    >,
    pub set_input: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> RimeBool>,
    pub get_shared_data_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_user_data_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_prebuilt_data_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_staging_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_sync_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub highlight_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> RimeBool>,
    pub highlight_candidate_on_current_page:
        Option<unsafe extern "C" fn(RimeSessionId, usize) -> RimeBool>,
    pub change_page: Option<unsafe extern "C" fn(RimeSessionId, RimeBool) -> RimeBool>,
}

extern "C" {
    fn rime_get_api() -> *mut RimeApi;
}

/// Reference `rime_get_api` so GNU ld always keeps `DT_NEEDED` on `librime.so`.
pub fn probe_linked() {
    let api = unsafe { rime_get_api() };
    std::hint::black_box(api);
}

#[derive(Debug, Clone)]
pub struct RimeRawCandidate {
    pub text: String,
    pub comment: String,
}

#[derive(Debug, Clone)]
pub struct RimeSnapshot {
    pub commit_text: Option<String>,
    pub preedit: String,
    pub cursor_utf8: usize,
    pub raw_input: String,
    pub raw_cursor: usize,
    pub page_index: usize,
    pub page_size: usize,
    pub has_prev: bool,
    pub has_next: bool,
    pub candidates: Vec<RimeRawCandidate>,
}

pub struct RimeRuntime {
    api: *mut RimeApi,
    version: String,
    octagram_loaded: bool,
    user_data_dir: PathBuf,
    staging_dir: PathBuf,
    // Keep CStrings alive for the lifetime of librime process singleton
    _owned_cstrs: Vec<CString>,
}

// Serialized strictly under `RIME_RUNTIME: OnceLock<Mutex<Option<RimeRuntime>>>`
unsafe impl Send for RimeRuntime {}

static RIME_RUNTIME: OnceLock<Mutex<Option<RimeRuntime>>> = OnceLock::new();

pub fn clear_runtime() {
    if let Some(cell) = RIME_RUNTIME.get() {
        if let Some(rt) = cell.lock().take() {
            unsafe {
                if let Some(join) = (*rt.api).join_maintenance_thread { join(); }
                if let Some(finalize) = (*rt.api).finalize { finalize(); }
            }
        }
    }
}

fn c_data_size<T>() -> c_int {
    (std::mem::size_of::<T>() - std::mem::size_of::<c_int>()) as c_int
}

fn zeroed_commit() -> RimeCommit {
    RimeCommit {
        data_size: c_data_size::<RimeCommit>(),
        text: std::ptr::null_mut(),
    }
}

fn zeroed_context() -> RimeContext {
    RimeContext {
        data_size: c_data_size::<RimeContext>(),
        composition: RimeComposition {
            length: 0,
            cursor_pos: 0,
            sel_start: 0,
            sel_end: 0,
            preedit: std::ptr::null_mut(),
        },
        menu: RimeMenu {
            page_size: 0,
            page_no: 0,
            is_last_page: 0,
            highlighted_candidate_index: 0,
            num_candidates: 0,
            candidates: std::ptr::null_mut(),
            select_keys: std::ptr::null_mut(),
        },
        commit_text_preview: std::ptr::null_mut(),
        select_labels: std::ptr::null_mut(),
    }
}

fn resolve_cache_root(config_cache_dir: &str, config_data_dir: &str) -> (PathBuf, PathBuf) {
    let default_cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("vitral-cache"))
        .join("vitral");

    let user_data_dir = if !config_data_dir.trim().is_empty() {
        PathBuf::from(config_data_dir).join("rime")
    } else if !config_cache_dir.trim().is_empty() {
        PathBuf::from(config_cache_dir).join("rime")
    } else {
        default_cache.join("rime")
    };

    let staging_dir = if !config_cache_dir.trim().is_empty() {
        PathBuf::from(config_cache_dir).join("rime-build")
    } else {
        default_cache.join("rime-build")
    };

    (user_data_dir, staging_dir)
}

fn stage_user_data_dir(assets_dir: &Path, user_data_dir: &Path, grammar: Option<&Path>) -> Result<bool, VitralError> {
    fs::create_dir_all(user_data_dir).map_err(|e| VitralError::Io(e.to_string()))?;
    fs::create_dir_all(user_data_dir.join("logs")).map_err(|e| VitralError::Io(e.to_string()))?;

    let default_custom = r#"patch:
  schema_list:
    - schema: double_pinyin_flypy
    - schema: rime_ice
  "menu/page_size": 5
"#;
    let grammar_patch = r#"patch:
  grammar:
    language: wanxiang-lts-zh-hans
    collocation_max_length: 8
    collocation_min_length: 2
    collocation_penalty: -12
    non_collocation_penalty: -24
    weak_collocation_penalty: -12
    rear_penalty: -18
  "translator/contextual_suggestions": true
  "translator/max_sentences": 4
  "translator/max_homophones": 7
  "translator/max_homographs": 7
"#;

    let grammar_patch = if grammar.is_some() { grammar_patch } else { "patch:\n  \"translator/contextual_suggestions\": false\n" };
    let patch_changed = fs::read_to_string(user_data_dir.join("rime_ice.custom.yaml")).ok().as_deref() != Some(grammar_patch);
    let gram_link = user_data_dir.join("wanxiang-lts-zh-hans.gram");
    if gram_link.symlink_metadata().is_ok() { fs::remove_file(&gram_link).map_err(|e| VitralError::Io(e.to_string()))?; }
    if let Some(path) = grammar {
        let source = fs::canonicalize(path).map_err(|e| VitralError::Io(e.to_string()))?;
        std::os::unix::fs::symlink(source, &gram_link).map_err(|e| VitralError::Io(e.to_string()))?;
    }
    write_if_changed(&user_data_dir.join("default.custom.yaml"), default_custom)?;
    write_if_changed(&user_data_dir.join("rime_ice.custom.yaml"), grammar_patch)?;
    write_if_changed(
        &user_data_dir.join("double_pinyin_flypy.custom.yaml"),
        grammar_patch,
    )?;

    // Stage opencc configs/tables (both assets/rime/opencc/* and assets/opencc/*)
    let target_opencc = user_data_dir.join("opencc");
    fs::create_dir_all(&target_opencc).map_err(|e| VitralError::Io(e.to_string()))?;
    for src_dir in [assets_dir.join("rime").join("opencc"), assets_dir.join("opencc")] {
        if src_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&src_dir) {
                for ent in entries.flatten() {
                    let src_path = ent.path();
                    let Some(name) = src_path.file_name() else {
                        continue;
                    };
                    let dst_path = target_opencc.join(name);
                    if !dst_path.exists() {
                        #[cfg(unix)]
                        {
                            if std::os::unix::fs::symlink(&src_path, &dst_path).is_err() && src_path.is_file() {
                                let _ = fs::copy(&src_path, &dst_path);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(patch_changed)
}

fn write_if_changed(path: &Path, content: &str) -> Result<(), VitralError> {
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
    }
    fs::write(path, content).map_err(|e| VitralError::Io(e.to_string()))
}

fn init_runtime(
    assets_dir: &Path,
    cache_dir: &str,
    data_dir: &str,
) -> Result<RimeRuntime, VitralError> {
    let rime_shared = assets_dir.join("rime");
    if !rime_shared.is_dir() {
        return Err(VitralError::NotFound(format!(
            "rime assets directory not found: {}",
            rime_shared.display()
        )));
    }

    let (user_data_dir, staging_dir) = resolve_cache_root(cache_dir, data_dir);
    let config = super::EngineConfig { data_dir: data_dir.into(), cache_dir: cache_dir.into(), ..Default::default() };
    let paths = crate::models::ModelPaths::from_config(&config, Some(assets_dir))?;
    let patch_changed = stage_user_data_dir(assets_dir, &user_data_dir, paths.grammar.as_deref())?;
    fs::create_dir_all(&staging_dir).map_err(|e| VitralError::Io(e.to_string()))?;

    let api = unsafe { rime_get_api() };
    if api.is_null() {
        return Err(VitralError::Internal("rime_get_api returned NULL".into()));
    }

    let shared_c = CString::new(rime_shared.to_string_lossy().as_bytes())
        .map_err(|_| VitralError::InvalidArgument("NUL in shared_data_dir".into()))?;
    let user_c = CString::new(user_data_dir.to_string_lossy().as_bytes())
        .map_err(|_| VitralError::InvalidArgument("NUL in user_data_dir".into()))?;
    let prebuilt_c = CString::new(staging_dir.to_string_lossy().as_bytes())
        .map_err(|_| VitralError::InvalidArgument("NUL in staging_dir".into()))?;
    let log_dir_c = CString::new(user_data_dir.join("logs").to_string_lossy().as_bytes())
        .map_err(|_| VitralError::InvalidArgument("NUL in log_dir".into()))?;
    let dist_name = CString::new("Vitral").unwrap();
    let dist_code = CString::new("vitral").unwrap();
    let dist_ver = CString::new("0.1.0").unwrap();
    let app_name = CString::new("rime.vitral").unwrap();

    let mut traits = RimeTraits {
        data_size: c_data_size::<RimeTraits>(),
        shared_data_dir: shared_c.as_ptr(),
        user_data_dir: user_c.as_ptr(),
        distribution_name: dist_name.as_ptr(),
        distribution_code_name: dist_code.as_ptr(),
        distribution_version: dist_ver.as_ptr(),
        app_name: app_name.as_ptr(),
        modules: std::ptr::null(),
        min_log_level: 2, // ERROR only
        log_dir: log_dir_c.as_ptr(),
        prebuilt_data_dir: prebuilt_c.as_ptr(),
        staging_dir: prebuilt_c.as_ptr(),
    };

    unsafe {
        let api_ref = &*api;
        if let Some(setup) = api_ref.setup {
            setup(&mut traits);
        }
        if let Some(init) = api_ref.initialize {
            init(&mut traits);
        }
        let has_compiled_tables = staging_dir.join("rime_ice.table.bin").is_file()
            && staging_dir.join("double_pinyin_flypy.prism.bin").is_file()
            && staging_dir.join("rime_ice.schema.yaml").is_file();
        let full_check = if has_compiled_tables && !patch_changed { 0 } else { 1 };
        if let Some(maint) = api_ref.start_maintenance {
            let started = maint(full_check);
            if started != 0 {
                if let Some(join) = api_ref.join_maintenance_thread {
                    join();
                }
            }
        }
    }

    let version = unsafe {
        (*api)
            .get_version
            .map(|f| {
                let p = f();
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            })
            .unwrap_or_default()
    };

    let octagram_loaded = unsafe {
        let m1 = CString::new("octagram").unwrap();
        let m2 = CString::new("grammar").unwrap();
        (*api)
            .find_module
            .map(|f| !f(m1.as_ptr()).is_null() || !f(m2.as_ptr()).is_null())
            .unwrap_or(false)
    };

    Ok(RimeRuntime {
        api,
        version,
        octagram_loaded,
        user_data_dir,
        staging_dir,
        _owned_cstrs: vec![
            shared_c,
            user_c,
            prebuilt_c,
            log_dir_c,
            dist_name,
            dist_code,
            dist_ver,
            app_name,
        ],
    })
}

pub fn with_runtime<F, R>(
    assets_dir: &Path,
    cache_dir: &str,
    data_dir: &str,
    f: F,
) -> Result<R, VitralError>
where
    F: FnOnce(&mut RimeRuntime) -> Result<R, VitralError>,
{
    let cell = RIME_RUNTIME.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock();
    if guard.is_none() {
        *guard = Some(init_runtime(assets_dir, cache_dir, data_dir)?);
    }
    let rt = guard.as_mut().expect("initialized");
    f(rt)
}

impl RimeRuntime {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn octagram_loaded(&self) -> bool {
        self.octagram_loaded
    }

    pub fn user_data_dir(&self) -> &Path {
        &self.user_data_dir
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    pub fn create_session(&mut self, schema: SchemaKind) -> Result<RimeSessionId, VitralError> {
        let api = unsafe { &*self.api };
        let sid = unsafe {
            api.create_session
                .ok_or_else(|| VitralError::Internal("RimeApi::create_session missing".into()))?(
            )
        };
        if sid == 0 {
            return Err(VitralError::Internal("Rime create_session returned 0".into()));
        }
        self.select_schema(sid, schema)?;
        Ok(sid)
    }

    pub fn destroy_session(&mut self, sid: RimeSessionId) {
        if sid == 0 {
            return;
        }
        unsafe {
            if let Some(f) = (*self.api).destroy_session {
                f(sid);
            }
        }
    }

    pub fn select_schema(&mut self, sid: RimeSessionId, schema: SchemaKind) -> Result<(), VitralError> {
        let api = unsafe { &*self.api };
        let schema_c = CString::new(schema.as_str()).unwrap();
        let ok = unsafe {
            api.select_schema
                .ok_or_else(|| VitralError::Internal("RimeApi::select_schema missing".into()))?(
                sid,
                schema_c.as_ptr(),
            )
        };
        if ok == 0 {
            return Err(VitralError::NotFound(format!(
                "Rime schema '{}' not available",
                schema.as_str()
            )));
        }
        // Keep Rime's internal traditionalization disabled so Vitral's OpenCcConverter applies
        // Simplified / Traditional / TaiwanStandard / HongKong consistently across all modes.
        if let Some(set_opt) = api.set_option {
            let trad = CString::new("traditionalization").unwrap();
            let ascii = CString::new("ascii_mode").unwrap();
            unsafe {
                set_opt(sid, trad.as_ptr(), 0);
                set_opt(sid, ascii.as_ptr(), 0);
            }
        }
        Ok(())
    }

    pub fn current_schema(&self, sid: RimeSessionId) -> String {
        let api = unsafe { &*self.api };
        let mut buf = [0u8; 64];
        unsafe {
            if let Some(f) = api.get_current_schema {
                if f(sid, buf.as_mut_ptr() as *mut c_char, buf.len()) != 0 {
                    return CStr::from_ptr(buf.as_ptr() as *const c_char)
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }
        String::new()
    }

    pub fn clear_composition(&mut self, sid: RimeSessionId) {
        unsafe {
            if let Some(f) = (*self.api).clear_composition {
                f(sid);
            }
        }
    }

    /// Normalize ASCII control keycodes (`0x08 -> 0xFF08`, `0x0D -> 0xFF0D`, `0x1B -> 0xFF1B`)
    /// and process a keystroke in `librime`.
    pub fn process_key(
        &mut self,
        sid: RimeSessionId,
        keycode: u32,
        modifiers: u32,
    ) -> Result<RimeSnapshot, VitralError> {
        let api = unsafe { &*self.api };
        let normalized = match keycode {
            0x08 => 0xFF08, // BackSpace
            0x0D => 0xFF0D, // Return
            0x1B => 0xFF1B, // Escape
            0x09 => 0xFF09, // Tab
            k => k,
        };
        if normalized != 0 {
            if let Some(pk) = api.process_key {
                let _handled = unsafe { pk(sid, normalized as c_int, modifiers as c_int) };
            }
        }
        self.snapshot(sid)
    }

    pub fn select_candidate_on_current_page(
        &mut self,
        sid: RimeSessionId,
        index_on_page: usize,
    ) -> Result<RimeSnapshot, VitralError> {
        let api = unsafe { &*self.api };
        if let Some(sel) = api.select_candidate_on_current_page {
            unsafe {
                sel(sid, index_on_page);
            }
        }
        self.snapshot(sid)
    }

    pub fn set_caret_pos(
        &mut self,
        sid: RimeSessionId,
        cursor: usize,
    ) -> Result<RimeSnapshot, VitralError> {
        let set_caret_pos = unsafe { (*self.api).set_caret_pos }
            .ok_or_else(|| VitralError::Internal("librime set_caret_pos is unavailable".into()))?;
        unsafe { set_caret_pos(sid, cursor) };
        self.snapshot(sid)
    }

    pub fn change_page(&mut self, sid: RimeSessionId, backward: bool) -> Result<RimeSnapshot, VitralError> {
        let api = unsafe { &*self.api };
        if let Some(cp) = api.change_page {
            unsafe {
                cp(sid, if backward { 1 } else { 0 });
            }
        }
        self.snapshot(sid)
    }

    pub fn snapshot(&mut self, sid: RimeSessionId) -> Result<RimeSnapshot, VitralError> {
        let api = unsafe { &*self.api };
        let mut commit_text = None;
        let mut commit = zeroed_commit();
        unsafe {
            if let Some(gc) = api.get_commit {
                if gc(sid, &mut commit) != 0 {
                    if !commit.text.is_null() {
                        commit_text =
                            Some(CStr::from_ptr(commit.text).to_string_lossy().into_owned());
                    }
                    if let Some(fc) = api.free_commit {
                        fc(&mut commit);
                    }
                }
            }
        }

        let mut ctx = zeroed_context();
        let mut preedit = String::new();
        let mut cursor_utf8 = 0usize;
        let mut page_index = 0usize;
        let mut page_size = 5usize;
        let mut has_prev = false;
        let mut has_next = false;
        let mut candidates = Vec::new();

        unsafe {
            if let Some(gctx) = api.get_context {
                if gctx(sid, &mut ctx) != 0 {
                    if !ctx.composition.preedit.is_null() {
                        preedit = CStr::from_ptr(ctx.composition.preedit)
                            .to_string_lossy()
                            .into_owned();
                    }
                    cursor_utf8 = ctx.composition.cursor_pos.max(0) as usize;
                    page_index = ctx.menu.page_no.max(0) as usize;
                    if ctx.menu.page_size > 0 {
                        page_size = ctx.menu.page_size as usize;
                    }
                    has_prev = page_index > 0;
                    has_next = ctx.menu.is_last_page == 0 && ctx.menu.num_candidates > 0;
                    if ctx.menu.num_candidates > 0 && !ctx.menu.candidates.is_null() {
                        let slice = std::slice::from_raw_parts(
                            ctx.menu.candidates,
                            ctx.menu.num_candidates as usize,
                        );
                        for rc in slice {
                            let text = if rc.text.is_null() {
                                String::new()
                            } else {
                                CStr::from_ptr(rc.text).to_string_lossy().into_owned()
                            };
                            let comment = if rc.comment.is_null() {
                                String::new()
                            } else {
                                CStr::from_ptr(rc.comment).to_string_lossy().into_owned()
                            };
                            if !text.is_empty() {
                                candidates.push(RimeRawCandidate { text, comment });
                            }
                        }
                    }
                    if let Some(fctx) = api.free_context {
                        fctx(&mut ctx);
                    }
                }
            }
        }

        let (raw_input, raw_cursor) = unsafe {
            let input = api.get_input.map_or(std::ptr::null(), |get| get(sid));
            let input = if input.is_null() {
                String::new()
            } else {
                CStr::from_ptr(input).to_string_lossy().into_owned()
            };
            let cursor = api.get_caret_pos.map_or(input.len(), |get| get(sid));
            (input, cursor)
        };
        Ok(RimeSnapshot {
            commit_text,
            preedit,
            cursor_utf8,
            raw_input,
            raw_cursor,
            page_index,
            page_size,
            has_prev,
            has_next,
            candidates,
        })
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;

    #[test]
    fn grammar_patch_and_symlink_follow_enable_state() {
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); }
        }
        let temp = Temp(std::env::temp_dir().join(format!("vitral-rime-model-stage-{}", std::process::id())));
        fs::create_dir_all(&temp.0).unwrap();
        let model = temp.0.join("model.gram");
        fs::write(&model, b"grammar fixture").unwrap();
        let user = temp.0.join("user");
        let assets = temp.0.join("assets");
        assert!(stage_user_data_dir(&assets, &user, Some(&model)).unwrap());
        assert!(fs::read_to_string(user.join("rime_ice.custom.yaml")).unwrap().contains("grammar:"));
        assert_eq!(fs::read_link(user.join("wanxiang-lts-zh-hans.gram")).unwrap(), model);
        assert!(!stage_user_data_dir(&assets, &user, Some(&model)).unwrap());
        assert!(stage_user_data_dir(&assets, &user, None).unwrap());
        assert!(!fs::read_to_string(user.join("rime_ice.custom.yaml")).unwrap().contains("grammar:"));
        assert!(user.join("wanxiang-lts-zh-hans.gram").symlink_metadata().is_err());
    }
}
