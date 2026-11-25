use serde::Serialize;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use vitral_core::ai::AiMode;
use vitral_core::clip::ClipboardPayload;
use vitral_core::eng::{OpenCcMode, SchemaKind, SwipeDir};
use vitral_core::error::VitralError;
use vitral_core::hw::{HwMode, StrokePoint};
use vitral_core::imp::LexiconFormat;
use vitral_core::{TouchKeyHint, VitralCore};

pub const VITRAL_ABI_VERSION: u32 = 1;

pub struct VitralEngine {
    core: VitralCore,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VitralStatus {
    Ok = 0,
    ErrNullPointer = 1,
    ErrInvalidUtf8 = 2,
    ErrInvalidArgument = 3,
    ErrNotInitialized = 4,
    ErrPureOfflineViolation = 5,
    ErrIo = 6,
    ErrParse = 7,
    ErrNotFound = 8,
    ErrIntegrity = 9,
    ErrModelUnavailable = 10,
    ErrInternal = 11,
    ErrPanic = 99,
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitralSchema(pub u32);
pub const VITRAL_SCHEMA_DOUBLE_PINYIN_FLYPY: VitralSchema = VitralSchema(0);
pub const VITRAL_SCHEMA_RIME_ICE: VitralSchema = VitralSchema(1);

/// cbindgen:ignore
impl VitralSchema {
    #[allow(non_upper_case_globals)]
    pub const DoublePinyinFlypy: Self = VITRAL_SCHEMA_DOUBLE_PINYIN_FLYPY;
    #[allow(non_upper_case_globals)]
    pub const RimeIce: Self = VITRAL_SCHEMA_RIME_ICE;
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitralOpenCcMode(pub u32);
pub const VITRAL_OPEN_CC_MODE_SIMPLIFIED: VitralOpenCcMode = VitralOpenCcMode(0);
pub const VITRAL_OPEN_CC_MODE_TRADITIONAL: VitralOpenCcMode = VitralOpenCcMode(1);
pub const VITRAL_OPEN_CC_MODE_TAIWAN_STANDARD: VitralOpenCcMode = VitralOpenCcMode(2);
pub const VITRAL_OPEN_CC_MODE_HONG_KONG: VitralOpenCcMode = VitralOpenCcMode(3);

/// cbindgen:ignore
impl VitralOpenCcMode {
    #[allow(non_upper_case_globals)]
    pub const Simplified: Self = VITRAL_OPEN_CC_MODE_SIMPLIFIED;
    #[allow(non_upper_case_globals)]
    pub const Traditional: Self = VITRAL_OPEN_CC_MODE_TRADITIONAL;
    #[allow(non_upper_case_globals)]
    pub const TaiwanStandard: Self = VITRAL_OPEN_CC_MODE_TAIWAN_STANDARD;
    #[allow(non_upper_case_globals)]
    pub const HongKong: Self = VITRAL_OPEN_CC_MODE_HONG_KONG;
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitralSwipeDir(pub u32);
pub const VITRAL_SWIPE_DIR_NONE: VitralSwipeDir = VitralSwipeDir(0);
pub const VITRAL_SWIPE_DIR_UP: VitralSwipeDir = VitralSwipeDir(1);
pub const VITRAL_SWIPE_DIR_DOWN: VitralSwipeDir = VitralSwipeDir(2);
pub const VITRAL_SWIPE_DIR_LEFT: VitralSwipeDir = VitralSwipeDir(3);
pub const VITRAL_SWIPE_DIR_RIGHT: VitralSwipeDir = VitralSwipeDir(4);

/// cbindgen:ignore
impl VitralSwipeDir {
    #[allow(non_upper_case_globals)]
    pub const None: Self = VITRAL_SWIPE_DIR_NONE;
    #[allow(non_upper_case_globals)]
    pub const Up: Self = VITRAL_SWIPE_DIR_UP;
    #[allow(non_upper_case_globals)]
    pub const Down: Self = VITRAL_SWIPE_DIR_DOWN;
    #[allow(non_upper_case_globals)]
    pub const Left: Self = VITRAL_SWIPE_DIR_LEFT;
    #[allow(non_upper_case_globals)]
    pub const Right: Self = VITRAL_SWIPE_DIR_RIGHT;
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitralHwMode(pub u32);
pub const VITRAL_HW_MODE_PHONETIC_LOOKUP: VitralHwMode = VitralHwMode(0);
pub const VITRAL_HW_MODE_CONTINUOUS_LATTICE: VitralHwMode = VitralHwMode(1);

/// cbindgen:ignore
impl VitralHwMode {
    #[allow(non_upper_case_globals)]
    pub const PhoneticLookup: Self = VITRAL_HW_MODE_PHONETIC_LOOKUP;
    #[allow(non_upper_case_globals)]
    pub const ContinuousLattice: Self = VITRAL_HW_MODE_CONTINUOUS_LATTICE;
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitralAiMode(pub u32);
pub const VITRAL_AI_MODE_TRANSLATE: VitralAiMode = VitralAiMode(0);
pub const VITRAL_AI_MODE_POLISH: VitralAiMode = VitralAiMode(1);
pub const VITRAL_AI_MODE_SUMMARY: VitralAiMode = VitralAiMode(2);

/// cbindgen:ignore
impl VitralAiMode {
    #[allow(non_upper_case_globals)]
    pub const Translate: Self = VITRAL_AI_MODE_TRANSLATE;
    #[allow(non_upper_case_globals)]
    pub const Polish: Self = VITRAL_AI_MODE_POLISH;
    #[allow(non_upper_case_globals)]
    pub const Summary: Self = VITRAL_AI_MODE_SUMMARY;
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VitralLexiconFormat(pub u32);
pub const VITRAL_LEXICON_FORMAT_AUTO: VitralLexiconFormat = VitralLexiconFormat(0);
pub const VITRAL_LEXICON_FORMAT_SCEL: VitralLexiconFormat = VitralLexiconFormat(1);
pub const VITRAL_LEXICON_FORMAT_DICT_YAML: VitralLexiconFormat = VitralLexiconFormat(2);
pub const VITRAL_LEXICON_FORMAT_TSV: VitralLexiconFormat = VitralLexiconFormat(3);
pub const VITRAL_LEXICON_FORMAT_TXT: VitralLexiconFormat = VitralLexiconFormat(4);

/// cbindgen:ignore
impl VitralLexiconFormat {
    #[allow(non_upper_case_globals)]
    pub const Auto: Self = VITRAL_LEXICON_FORMAT_AUTO;
    #[allow(non_upper_case_globals)]
    pub const Scel: Self = VITRAL_LEXICON_FORMAT_SCEL;
    #[allow(non_upper_case_globals)]
    pub const DictYaml: Self = VITRAL_LEXICON_FORMAT_DICT_YAML;
    #[allow(non_upper_case_globals)]
    pub const Tsv: Self = VITRAL_LEXICON_FORMAT_TSV;
    #[allow(non_upper_case_globals)]
    pub const Txt: Self = VITRAL_LEXICON_FORMAT_TXT;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VitralStrokePoint {
    pub x: f32,
    pub y: f32,
    pub timestamp_ms: u64,
}

#[repr(C)]
#[derive(Debug)]
pub struct VitralStringResult {
    pub status: VitralStatus,
    pub json_utf8: *mut c_char,
    pub error_message: *mut c_char,
}

impl VitralStringResult {
    fn ok_json<T: Serialize>(value: &T) -> Self {
        match serde_json::to_string(value) {
            Ok(s) => Self {
                status: VitralStatus::Ok,
                json_utf8: CString::new(s).expect("JSON escapes NUL").into_raw(),
                error_message: std::ptr::null_mut(),
            },
            Err(e) => Self::err(VitralStatus::ErrInternal, &e.to_string()),
        }
    }

    fn err(status: VitralStatus, msg: &str) -> Self {
        Self {
            status,
            json_utf8: std::ptr::null_mut(),
            error_message: to_c_char_lossy(msg),
        }
    }

    fn from_core_err(err: VitralError) -> Self {
        let status = map_error(&err);
        Self::err(status, &err.to_string())
    }
}

fn to_c_char_lossy(s: &str) -> *mut c_char {
    let clean = s.replace('\0', "");
    CString::new(clean)
        .expect("NUL stripped")
        .into_raw()
}

fn map_error(err: &VitralError) -> VitralStatus {
    match err {
        VitralError::NullPointer(_) => VitralStatus::ErrNullPointer,
        VitralError::InvalidUtf8(_) => VitralStatus::ErrInvalidUtf8,
        VitralError::InvalidArgument(_) => VitralStatus::ErrInvalidArgument,
        VitralError::NotInitialized => VitralStatus::ErrNotInitialized,
        VitralError::PureOfflineViolation(_) => VitralStatus::ErrPureOfflineViolation,
        VitralError::Io(_) => VitralStatus::ErrIo,
        VitralError::Parse(_) => VitralStatus::ErrParse,
        VitralError::NotFound(_) => VitralStatus::ErrNotFound,
        VitralError::Integrity(_) => VitralStatus::ErrIntegrity,
        VitralError::ModelUnavailable(_) => VitralStatus::ErrModelUnavailable,
        VitralError::Internal(_) => VitralStatus::ErrInternal,
    }
}

unsafe fn read_c_str<'a>(ptr: *const c_char, name: &str) -> Result<&'a str, VitralError> {
    if ptr.is_null() {
        return Err(VitralError::NullPointer(name.to_string()));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| VitralError::InvalidUtf8(name.to_string()))
}

unsafe fn read_opt_c_str<'a>(ptr: *const c_char, name: &str) -> Result<Option<&'a str>, VitralError> {
    if ptr.is_null() {
        return Ok(None);
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(Some)
        .map_err(|_| VitralError::InvalidUtf8(name.to_string()))
}

unsafe fn engine_ref<'a>(engine: *const VitralEngine) -> Result<&'a VitralCore, VitralError> {
    if engine.is_null() {
        return Err(VitralError::NullPointer("engine".to_string()));
    }
    Ok(&unsafe { &*engine }.core)
}

fn guard_status<F>(f: F) -> VitralStatus
where
    F: FnOnce() -> VitralStatus,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(s) => s,
        Err(_) => VitralStatus::ErrPanic,
    }
}

fn guard_result<F>(f: F) -> VitralStringResult
where
    F: FnOnce() -> VitralStringResult,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => VitralStringResult::err(VitralStatus::ErrPanic, "panic across FFI boundary"),
    }
}

impl TryFrom<VitralSchema> for SchemaKind {
    type Error = VitralError;
    fn try_from(s: VitralSchema) -> Result<Self, Self::Error> {
        match s.0 {
            0 => Ok(SchemaKind::DoublePinyinFlypy),
            1 => Ok(SchemaKind::RimeIce),
            other => Err(VitralError::InvalidArgument(format!(
                "invalid VitralSchema discriminant: {other}"
            ))),
        }
    }
}

impl TryFrom<VitralOpenCcMode> for OpenCcMode {
    type Error = VitralError;
    fn try_from(m: VitralOpenCcMode) -> Result<Self, Self::Error> {
        match m.0 {
            0 => Ok(OpenCcMode::Simplified),
            1 => Ok(OpenCcMode::Traditional),
            2 => Ok(OpenCcMode::TaiwanStandard),
            3 => Ok(OpenCcMode::HongKong),
            other => Err(VitralError::InvalidArgument(format!(
                "invalid VitralOpenCcMode discriminant: {other}"
            ))),
        }
    }
}

impl TryFrom<VitralSwipeDir> for SwipeDir {
    type Error = VitralError;
    fn try_from(d: VitralSwipeDir) -> Result<Self, Self::Error> {
        match d.0 {
            0 => Ok(SwipeDir::None),
            1 => Ok(SwipeDir::Up),
            2 => Ok(SwipeDir::Down),
            3 => Ok(SwipeDir::Left),
            4 => Ok(SwipeDir::Right),
            other => Err(VitralError::InvalidArgument(format!(
                "invalid VitralSwipeDir discriminant: {other}"
            ))),
        }
    }
}

impl TryFrom<VitralHwMode> for HwMode {
    type Error = VitralError;
    fn try_from(m: VitralHwMode) -> Result<Self, Self::Error> {
        match m.0 {
            0 => Ok(HwMode::PhoneticLookup),
            1 => Ok(HwMode::ContinuousLattice),
            other => Err(VitralError::InvalidArgument(format!(
                "invalid VitralHwMode discriminant: {other}"
            ))),
        }
    }
}

impl TryFrom<VitralAiMode> for AiMode {
    type Error = VitralError;
    fn try_from(m: VitralAiMode) -> Result<Self, Self::Error> {
        match m.0 {
            0 => Ok(AiMode::Translate),
            1 => Ok(AiMode::Polish),
            2 => Ok(AiMode::Summary),
            other => Err(VitralError::InvalidArgument(format!(
                "invalid VitralAiMode discriminant: {other}"
            ))),
        }
    }
}

impl TryFrom<VitralLexiconFormat> for LexiconFormat {
    type Error = VitralError;
    fn try_from(f: VitralLexiconFormat) -> Result<Self, Self::Error> {
        match f.0 {
            0 => Ok(LexiconFormat::Auto),
            1 => Ok(LexiconFormat::Scel),
            2 => Ok(LexiconFormat::DictYaml),
            3 => Ok(LexiconFormat::Tsv),
            4 => Ok(LexiconFormat::Txt),
            other => Err(VitralError::InvalidArgument(format!(
                "invalid VitralLexiconFormat discriminant: {other}"
            ))),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "fcitx-control"))]
#[no_mangle]
/// Private to the Fcitx adapter, built and installed with the matching engine.
/// # Safety
/// Pass a live engine and NUL-terminated UTF-8 strings. Free the result with vitral_free_result.
pub unsafe extern "C" fn vitral_fcitx_command(engine: *mut VitralEngine, command: *const c_char, payload: *const c_char) -> VitralStringResult {
    guard_result(|| {
        let dispatch = || -> Result<_, VitralError> {
            let core = unsafe { engine_ref(engine)? };
            let cmd = unsafe { read_c_str(command,"command")? };
            let raw = unsafe { read_c_str(payload,"payload")? };
            let args = serde_json::from_str(raw).map_err(|e|VitralError::Parse(format!("{e}")))?;
            vitral_core::commands::execute(core,cmd,&args).map_err(|e| e.downcast::<VitralError>().map(|e|*e).unwrap_or_else(|e| VitralError::InvalidArgument(e.to_string())))
        };
        match dispatch() {
            Ok(value) => VitralStringResult::ok_json(&value),
            Err(e) => VitralStringResult::from_core_err(e)
        }
    })
}

#[no_mangle]
pub extern "C" fn vitral_abi_version() -> u32 {
    VITRAL_ABI_VERSION
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_init(
    config_json: *const c_char,
    out_engine: *mut *mut VitralEngine,
    out_error_utf8: *mut *mut c_char,
) -> VitralStatus {
    guard_status(|| {
        if !out_error_utf8.is_null() {
            unsafe { *out_error_utf8 = std::ptr::null_mut() };
        }
        if out_engine.is_null() {
            if !out_error_utf8.is_null() {
                unsafe { *out_error_utf8 = to_c_char_lossy("out_engine is null") };
            }
            return VitralStatus::ErrNullPointer;
        }
        unsafe { *out_engine = std::ptr::null_mut() };
        let cfg = match unsafe { read_c_str(config_json, "config_json") } {
            Ok(s) => s,
            Err(e) => {
                if !out_error_utf8.is_null() {
                    unsafe { *out_error_utf8 = to_c_char_lossy(&e.to_string()) };
                }
                return map_error(&e);
            }
        };
        match VitralCore::init(cfg) {
            Ok(core) => {
                let boxed = Box::new(VitralEngine { core });
                unsafe { *out_engine = Box::into_raw(boxed) };
                VitralStatus::Ok
            }
            Err(e) => {
                if !out_error_utf8.is_null() {
                    unsafe { *out_error_utf8 = to_c_char_lossy(&e.to_string()) };
                }
                map_error(&e)
            }
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_shutdown(engine: *mut VitralEngine) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !engine.is_null() {
            unsafe {
                drop(Box::from_raw(engine));
            }
        }
    }));
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_set_pure_offline(engine: *mut VitralEngine, enabled: bool) -> VitralStatus {
    guard_status(|| match unsafe { engine_ref(engine) } {
        Ok(core) => {
            core.set_pure_offline(enabled);
            VitralStatus::Ok
        }
        Err(e) => map_error(&e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_is_pure_offline(
    engine: *const VitralEngine,
    out_enabled: *mut bool,
) -> VitralStatus {
    guard_status(|| {
        if out_enabled.is_null() {
            return VitralStatus::ErrNullPointer;
        }
        match unsafe { engine_ref(engine) } {
            Ok(core) => {
                unsafe { *out_enabled = core.is_pure_offline() };
                VitralStatus::Ok
            }
            Err(e) => map_error(&e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_set_schema(
    engine: *mut VitralEngine,
    schema: VitralSchema,
) -> VitralStatus {
    guard_status(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match SchemaKind::try_from(schema) {
            Ok(s) => {
                core.set_schema(s);
                VitralStatus::Ok
            }
            Err(e) => map_error(&e),
        },
        Err(e) => map_error(&e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_set_opencc_mode(
    engine: *mut VitralEngine,
    mode: VitralOpenCcMode,
) -> VitralStatus {
    guard_status(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match OpenCcMode::try_from(mode) {
            Ok(m) => {
                core.set_opencc_mode(m);
                VitralStatus::Ok
            }
            Err(e) => map_error(&e),
        },
        Err(e) => map_error(&e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_reload_assets(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.reload_assets() {
            Ok(r) => VitralStringResult::ok_json(&r),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_free_string(ptr: *mut c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !ptr.is_null() {
            unsafe {
                drop(CString::from_raw(ptr));
            }
        }
    }));
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_free_result(result: VitralStringResult) {
    vitral_free_string(result.json_utf8);
    vitral_free_string(result.error_message);
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_on_app_focus(
    engine: *mut VitralEngine,
    app_id_utf8: *const c_char,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let app_id = match unsafe { read_c_str(app_id_utf8, "app_id_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        match core.on_app_focus(app_id) {
            Ok(st) => VitralStringResult::ok_json(&st),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_process_key(
    engine: *mut VitralEngine,
    keycode: u32,
    modifiers: u32,
    swipe_dir: VitralSwipeDir,
) -> VitralStringResult {
    unsafe { vitral_process_touch_key(engine, keycode, modifiers, swipe_dir, 0, 0.0, 0, 0.0) }
}

#[no_mangle]
/// Process a key with up to two alternate lowercase ASCII keys. Zero means absent.
///
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_process_touch_key(
    engine: *mut VitralEngine,
    keycode: u32,
    modifiers: u32,
    swipe_dir: VitralSwipeDir,
    alt1_char: u32,
    alt1_penalty: f32,
    alt2_char: u32,
    alt2_penalty: f32,
) -> VitralStringResult {
    let hint = TouchKeyHint::from_keycodes(
        keycode, [(alt1_char, alt1_penalty), (alt2_char, alt2_penalty)],
    );
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match SwipeDir::try_from(swipe_dir) {
            Ok(dir) => match core.process_touch_key(keycode, modifiers, dir, hint) {
                Ok(act) => VitralStringResult::ok_json(&act),
                Err(e) => VitralStringResult::from_core_err(e),
            },
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_select_candidate(
    engine: *mut VitralEngine,
    index: usize,
) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.select_candidate(index) {
            Ok(act) => VitralStringResult::ok_json(&act),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_pin_candidate(
    engine: *mut VitralEngine,
    index: usize,
) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.pin_candidate(index) {
            Ok(act) => VitralStringResult::ok_json(&act),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_erase_candidate(
    engine: *mut VitralEngine,
    index: usize,
) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.erase_candidate(index) {
            Ok(act) => VitralStringResult::ok_json(&act),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_hw_begin(
    engine: *mut VitralEngine,
    mode: VitralHwMode,
) -> VitralStatus {
    guard_status(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match HwMode::try_from(mode) {
            Ok(m) => {
                core.hw_begin(m);
                VitralStatus::Ok
            }
            Err(e) => map_error(&e),
        },
        Err(e) => map_error(&e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_hw_push_stroke(
    engine: *mut VitralEngine,
    points_ptr: *const VitralStrokePoint,
    len: usize,
    pen_up: bool,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let slice = if len == 0 {
            &[][..]
        } else {
            if points_ptr.is_null() {
                return VitralStringResult::from_core_err(VitralError::NullPointer(
                    "points_ptr".into(),
                ));
            }
            if len > (isize::MAX as usize) / std::mem::size_of::<VitralStrokePoint>() {
                return VitralStringResult::from_core_err(VitralError::InvalidArgument(
                    "len exceeds maximum slice size".into(),
                ));
            }
            unsafe { std::slice::from_raw_parts(points_ptr, len) }
        };
        let pts: Vec<StrokePoint> = slice
            .iter()
            .map(|p| StrokePoint {
                x: p.x,
                y: p.y,
                timestamp_ms: p.timestamp_ms,
            })
            .collect();
        match core.hw_push_stroke(&pts, pen_up) {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_hw_end(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.hw_end() {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_hw_clear(engine: *mut VitralEngine) -> VitralStatus {
    guard_status(|| match unsafe { engine_ref(engine) } {
        Ok(core) => {
            core.hw_clear();
            VitralStatus::Ok
        }
        Err(e) => map_error(&e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_hw_override_slot(
    engine: *mut VitralEngine,
    slot_idx: usize,
    candidate_idx: usize,
) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.hw_override_slot(slot_idx, candidate_idx) {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_import_lexicon(
    engine: *mut VitralEngine,
    path_utf8: *const c_char,
    format_hint: VitralLexiconFormat,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let path_str = match unsafe { read_c_str(path_utf8, "path_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let fmt = match LexiconFormat::try_from(format_hint) {
            Ok(f) => f,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        match core.import_lexicon(Path::new(path_str), fmt) {
            Ok(rep) => VitralStringResult::ok_json(&rep),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_clipboard_push(
    engine: *mut VitralEngine,
    payload_json_utf8: *const c_char,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let json_str = match unsafe { read_c_str(payload_json_utf8, "payload_json_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let payload: ClipboardPayload = match serde_json::from_str(json_str) {
            Ok(p) => p,
            Err(e) => {
                return VitralStringResult::from_core_err(VitralError::Parse(e.to_string()))
            }
        };
        let state = core.clipboard_push(payload);
        VitralStringResult::ok_json(&state)
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_clipboard_state(engine: *const VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.clipboard_state()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_clipboard_ingest_remote(
    engine: *mut VitralEngine,
    payload_json_utf8: *const c_char,
    is_cold_boot: bool,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let json_str = match unsafe { read_c_str(payload_json_utf8, "payload_json_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let payload: ClipboardPayload = match serde_json::from_str(json_str) {
            Ok(p) => p,
            Err(e) => {
                return VitralStringResult::from_core_err(VitralError::Parse(e.to_string()))
            }
        };
        let state = core.clipboard_ingest_remote(payload, is_cold_boot);
        VitralStringResult::ok_json(&state)
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_quick_paste_state(engine: *const VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.quick_paste_state()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_consume_quick_paste(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.consume_quick_paste()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_on_ime_open(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.on_ime_open()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_on_ime_close(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.on_ime_close()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_dismiss_quick_paste(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.dismiss_quick_paste()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_clipboard_pin(
    engine: *mut VitralEngine,
    item_id_utf8: *const c_char,
    shortcut_code_utf8: *const c_char,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let item_id = match unsafe { read_c_str(item_id_utf8, "item_id_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let shortcut = match unsafe { read_opt_c_str(shortcut_code_utf8, "shortcut_code_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        match core.clipboard_pin(item_id, shortcut) {
            Ok(st) => VitralStringResult::ok_json(&st),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_clipboard_clear_unpinned(
    engine: *mut VitralEngine,
) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.clipboard_clear_unpinned()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_relay_devices_mru(engine: *const VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => VitralStringResult::ok_json(&core.relay_devices_mru()),
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_relay_send(
    engine: *mut VitralEngine,
    device_id_utf8: *const c_char,
    payload_or_path_utf8: *const c_char,
    is_file_path: bool,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let device_id = match unsafe { read_c_str(device_id_utf8, "device_id_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let payload = match unsafe { read_c_str(payload_or_path_utf8, "payload_or_path_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        match core.relay_send(device_id, payload, is_file_path) {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_relay_poll_inbox(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.relay_poll_inbox() {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_relay_cleanup_orphans(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.relay_cleanup_orphans() {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_voice_start(engine: *mut VitralEngine) -> VitralStatus {
    guard_status(|| match unsafe { engine_ref(engine) } {
        Ok(core) => {
            core.voice_start();
            VitralStatus::Ok
        }
        Err(e) => map_error(&e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_voice_push_pcm(
    engine: *mut VitralEngine,
    pcm_ptr: *const i16,
    len: usize,
) -> VitralStatus {
    guard_status(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return map_error(&e),
        };
        let slice = if len == 0 {
            &[][..]
        } else {
            if pcm_ptr.is_null() {
                return VitralStatus::ErrNullPointer;
            }
            if len > (isize::MAX as usize) / std::mem::size_of::<i16>() {
                return VitralStatus::ErrInvalidArgument;
            }
            unsafe { std::slice::from_raw_parts(pcm_ptr, len) }
        };
        match core.voice_push_pcm(slice) {
            Ok(()) => VitralStatus::Ok,
            Err(e) => map_error(&e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_voice_stop(engine: *mut VitralEngine) -> VitralStringResult {
    guard_result(|| match unsafe { engine_ref(engine) } {
        Ok(core) => match core.voice_stop() {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        },
        Err(e) => VitralStringResult::from_core_err(e),
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_ai_inline(
    engine: *mut VitralEngine,
    mode: VitralAiMode,
    text_utf8: *const c_char,
    target_lang_utf8: *const c_char,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let text = match unsafe { read_c_str(text_utf8, "text_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let target_lang = match unsafe { read_c_str(target_lang_utf8, "target_lang_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let ai_mode = match AiMode::try_from(mode) {
            Ok(m) => m,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        match core.ai_inline(ai_mode, text, target_lang) {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[no_mangle]
/// # Safety
/// Caller must ensure any non-null pointer arguments are valid for reads/writes of the
/// expected C types and any returned strings are freed via `vitral_free_string` / `vitral_free_result`.
pub unsafe extern "C" fn vitral_restore_from_koofr(
    engine: *mut VitralEngine,
    webdav_cfg_json_utf8: *const c_char,
) -> VitralStringResult {
    guard_result(|| {
        let core = match unsafe { engine_ref(engine) } {
            Ok(c) => c,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        let cfg = match unsafe { read_c_str(webdav_cfg_json_utf8, "webdav_cfg_json_utf8") } {
            Ok(s) => s,
            Err(e) => return VitralStringResult::from_core_err(e),
        };
        match core.restore_from_koofr(cfg) {
            Ok(res) => VitralStringResult::ok_json(&res),
            Err(e) => VitralStringResult::from_core_err(e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn process_touch_key_roundtrips_a_corrected_candidate() {
        unsafe {
            let cfg = CString::new(
                r#"{"platform":"android","schema":"rime_ice","assets_dir":"/nonexistent-vitral-assets"}"#,
            ).unwrap();
            let mut engine = std::ptr::null_mut();
            assert_eq!(vitral_init(cfg.as_ptr(), &mut engine, std::ptr::null_mut()), VitralStatus::Ok);
            for (key, alternative) in [(b'b', b'n'), (b'i', 0), (b'h', 0), (b's', b'a'), (b'o', 0)] {
                let result = vitral_process_touch_key(
                    engine, u32::from(key), 0, VitralSwipeDir::None,
                    u32::MAX, f32::NAN, u32::from(alternative), 1.0,
                );
                assert_eq!(result.status, VitralStatus::Ok);
                let action: serde_json::Value =
                    serde_json::from_slice(CStr::from_ptr(result.json_utf8).to_bytes()).unwrap();
                vitral_free_result(result);
                if key == b'o' {
                    assert_eq!(action["preedit"]["text"], "bihso");
                    assert_eq!(action["candidates"][0]["text"], "你好");
                }
            }
            let result = vitral_select_candidate(engine, 0);
            assert_eq!(result.status, VitralStatus::Ok);
            let action: serde_json::Value =
                serde_json::from_slice(CStr::from_ptr(result.json_utf8).to_bytes()).unwrap();
            vitral_free_result(result);
            assert_eq!(action["commit_text"], "你好");
            vitral_shutdown(engine);
        }
    }

    #[test]
    fn abi_version_and_init_shutdown_roundtrip() {
        unsafe {
        assert_eq!(vitral_abi_version(), 1);
        let cfg = CString::new("{}").unwrap();
        let mut engine: *mut VitralEngine = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        let status = vitral_init(cfg.as_ptr(), &mut engine, &mut err);
        assert_eq!(status, VitralStatus::Ok);
        assert!(!engine.is_null());
        assert!(err.is_null());

        let mut offline = true;
        assert_eq!(
            vitral_is_pure_offline(engine, &mut offline),
            VitralStatus::Ok
        );
        assert!(!offline);
        assert_eq!(vitral_set_pure_offline(engine, true), VitralStatus::Ok);
        assert_eq!(
            vitral_is_pure_offline(engine, &mut offline),
            VitralStatus::Ok
        );
        assert!(offline);

        let dev = CString::new("dev-1").unwrap();
        let txt = CString::new("hello").unwrap();
        let relay_res = vitral_relay_send(engine, dev.as_ptr(), txt.as_ptr(), false);
        assert_eq!(relay_res.status, VitralStatus::ErrPureOfflineViolation);
        assert!(relay_res.json_utf8.is_null());
        assert!(!relay_res.error_message.is_null());
        vitral_free_result(relay_res);

        vitral_shutdown(engine);
        vitral_shutdown(std::ptr::null_mut());
        }
    }

    #[test]
    fn null_pointers_and_invalid_utf8_handled_safely() {
        unsafe {
        let mut err: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            vitral_init(std::ptr::null(), std::ptr::null_mut(), &mut err),
            VitralStatus::ErrNullPointer
        );
        assert!(!err.is_null());
        vitral_free_string(err);
        vitral_free_string(std::ptr::null_mut());

        assert_eq!(
            vitral_set_pure_offline(std::ptr::null_mut(), true),
            VitralStatus::ErrNullPointer
        );
        let focus_res = vitral_on_app_focus(std::ptr::null_mut(), std::ptr::null());
        assert_eq!(focus_res.status, VitralStatus::ErrNullPointer);
        vitral_free_result(focus_res);

        let invalid_utf8: [u8; 2] = [0xFF, 0x00];
        let mut engine: *mut VitralEngine = std::ptr::null_mut();
        assert_eq!(
            vitral_init(
                invalid_utf8.as_ptr() as *const c_char,
                &mut engine,
                std::ptr::null_mut(),
            ),
            VitralStatus::ErrInvalidUtf8
        );
        assert!(engine.is_null());
        }
    }

    #[test]
    fn panic_firewall_catches_panics_cleanly() {
        unsafe {
        let s = guard_status(|| panic!("boom"));
        assert_eq!(s, VitralStatus::ErrPanic);
        let r = guard_result(|| panic!("boom result"));
        assert_eq!(r.status, VitralStatus::ErrPanic);
        assert!(!r.error_message.is_null());
        vitral_free_result(r);
        }
    }

    #[test]
    fn quick_paste_ffi_lifecycle_and_ring_only_clipboard_state() {
        unsafe {
        let cfg = CString::new("{}").unwrap();
        let mut engine: *mut VitralEngine = std::ptr::null_mut();
        assert_eq!(
            vitral_init(cfg.as_ptr(), &mut engine, std::ptr::null_mut()),
            VitralStatus::Ok
        );

        let cold_payload = CString::new(
            r#"{"id":"c0","content":"cold","mime_type":"text/plain","source_device":"a","timestamp_ms":1}"#,
        )
        .unwrap();
        let cold_res = vitral_clipboard_ingest_remote(engine, cold_payload.as_ptr(), true);
        assert_eq!(cold_res.status, VitralStatus::Ok);
        let cold_json = CStr::from_ptr(cold_res.json_utf8).to_str().unwrap();
        assert!(cold_json.contains(r#""current_copy":{"id":"c0""#));
        vitral_free_result(cold_res);

        let live_payload = CString::new(
            r#"{"id":"c1","content":"otp-9988","mime_type":"text/plain","source_device":"a","timestamp_ms":2}"#,
        )
        .unwrap();
        let live_res = vitral_clipboard_ingest_remote(engine, live_payload.as_ptr(), false);
        assert_eq!(live_res.status, VitralStatus::Ok);
        let live_json = CStr::from_ptr(live_res.json_utf8).to_str().unwrap();
        assert!(live_json.contains(r#""content":"otp-9988""#));
        vitral_free_result(live_res);

        let clip_state = vitral_clipboard_state(engine);
        assert_eq!(clip_state.status, VitralStatus::Ok);
        let clip_json = CStr::from_ptr(clip_state.json_utf8).to_str().unwrap();
        assert!(!clip_json.contains("live_slot"));
        assert!(clip_json.contains(r#""ring""#));
        vitral_free_result(clip_state);

        let open_res = vitral_on_ime_open(engine);
        assert_eq!(open_res.status, VitralStatus::Ok);
        vitral_free_result(open_res);

        let close_res = vitral_on_ime_close(engine);
        assert_eq!(close_res.status, VitralStatus::Ok);
        let close_json = CStr::from_ptr(close_res.json_utf8).to_str().unwrap();
        assert!(close_json.contains(r#""content":"otp-9988""#));
        assert!(close_json.contains(r#""candidate_visible":false"#));
        vitral_free_result(close_res);

        let push_res = vitral_clipboard_push(engine, live_payload.as_ptr());
        assert_eq!(push_res.status, VitralStatus::Ok);
        vitral_free_result(push_res);

        let consume_res = vitral_consume_quick_paste(engine);
        assert_eq!(consume_res.status, VitralStatus::Ok);
        let consume_json = CStr::from_ptr(consume_res.json_utf8).to_str().unwrap();
        assert!(consume_json.contains(r#""consumed""#));
        vitral_free_result(consume_res);

        let dismiss_res = vitral_dismiss_quick_paste(engine);
        assert_eq!(dismiss_res.status, VitralStatus::Ok);
        vitral_free_result(dismiss_res);

        vitral_shutdown(engine);
        }
    }

    #[test]
    fn two_engines_auto_id_no_collision_on_ingest_remote() {
        unsafe {
            let cfg_a = CString::new(r#"{"device_id":"devA"}"#).unwrap();
            let cfg_b = CString::new(r#"{"device_id":"devB"}"#).unwrap();
            let mut dev_a: *mut VitralEngine = std::ptr::null_mut();
            let mut dev_b: *mut VitralEngine = std::ptr::null_mut();
            assert_eq!(
                vitral_init(cfg_a.as_ptr(), &mut dev_a, std::ptr::null_mut()),
                VitralStatus::Ok
            );
            assert_eq!(
                vitral_init(cfg_b.as_ptr(), &mut dev_b, std::ptr::null_mut()),
                VitralStatus::Ok
            );

            let push_a_payload = CString::new(
                r#"{"content":"from-devA","mime_type":"text/plain","source_device":"devA","timestamp_ms":100}"#,
            )
            .unwrap();
            let push_b_payload = CString::new(
                r#"{"content":"from-devB","mime_type":"text/plain","source_device":"devB","timestamp_ms":100}"#,
            )
            .unwrap();

            let res_a = vitral_clipboard_push(dev_a, push_a_payload.as_ptr());
            assert_eq!(res_a.status, VitralStatus::Ok);
            let json_a = CStr::from_ptr(res_a.json_utf8).to_str().unwrap();
            let state_a: serde_json::Value = serde_json::from_str(json_a).unwrap();
            let entry_a = &state_a["ring"][0];
            let id_a = entry_a["id"].as_str().unwrap().to_string();
            assert!(id_a.starts_with("devA-"));
            let entry_a_json = CString::new(serde_json::to_string(entry_a).unwrap()).unwrap();
            vitral_free_result(res_a);

            let res_b = vitral_clipboard_push(dev_b, push_b_payload.as_ptr());
            assert_eq!(res_b.status, VitralStatus::Ok);
            let json_b = CStr::from_ptr(res_b.json_utf8).to_str().unwrap();
            let state_b: serde_json::Value = serde_json::from_str(json_b).unwrap();
            let id_b = state_b["ring"][0]["id"].as_str().unwrap().to_string();
            assert!(id_b.starts_with("devB-"));
            assert_ne!(id_a, id_b);
            vitral_free_result(res_b);

            let ingest_res = vitral_clipboard_ingest_remote(dev_b, entry_a_json.as_ptr(), false);
            assert_eq!(ingest_res.status, VitralStatus::Ok);
            let ingest_json = CStr::from_ptr(ingest_res.json_utf8).to_str().unwrap();
            let qp_b: serde_json::Value = serde_json::from_str(ingest_json).unwrap();
            assert_eq!(qp_b["current_copy"]["id"].as_str(), Some(id_a.max(id_b).as_str()));
            vitral_free_result(ingest_res);

            let clip_b = vitral_clipboard_state(dev_b);
            assert_eq!(clip_b.status, VitralStatus::Ok);
            let clip_b_json = CStr::from_ptr(clip_b.json_utf8).to_str().unwrap();
            let clip_b_val: serde_json::Value = serde_json::from_str(clip_b_json).unwrap();
            assert_eq!(clip_b_val["ring"].as_array().unwrap().len(), 2);
            vitral_free_result(clip_b);

            vitral_shutdown(dev_a);
            vitral_shutdown(dev_b);
        }
    }

    #[test]
    fn oversized_slice_len_returns_invalid_argument() {
        unsafe {
            let cfg = CString::new("{}").unwrap();
            let mut engine: *mut VitralEngine = std::ptr::null_mut();
            assert_eq!(
                vitral_init(cfg.as_ptr(), &mut engine, std::ptr::null_mut()),
                VitralStatus::Ok
            );

            let pt = VitralStrokePoint {
                x: 0.0,
                y: 0.0,
                timestamp_ms: 0,
            };
            let hw_res = vitral_hw_push_stroke(engine, &pt as *const VitralStrokePoint, usize::MAX, true);
            assert_eq!(hw_res.status, VitralStatus::ErrInvalidArgument);
            vitral_free_result(hw_res);

            let sample: i16 = 0;
            let voice_status = vitral_voice_push_pcm(engine, &sample as *const i16, usize::MAX);
            assert_eq!(voice_status, VitralStatus::ErrInvalidArgument);

            vitral_shutdown(engine);
        }
    }

    #[test]
    fn invalid_enum_discriminants_return_invalid_argument() {
        unsafe {
            let cfg = CString::new("{}").unwrap();
            let mut engine: *mut VitralEngine = std::ptr::null_mut();
            assert_eq!(
                vitral_init(cfg.as_ptr(), &mut engine, std::ptr::null_mut()),
                VitralStatus::Ok
            );

            for bad in [2u32, 5u32, 99u32, u32::MAX] {
                // 1. VitralSchema (valid: 0..=1)
                assert_eq!(
                    vitral_set_schema(engine, VitralSchema(bad)),
                    VitralStatus::ErrInvalidArgument
                );
                // 2. VitralHwMode (valid: 0..=1)
                assert_eq!(
                    vitral_hw_begin(engine, VitralHwMode(bad)),
                    VitralStatus::ErrInvalidArgument
                );
            }

            for bad in [3u32, 5u32, 99u32, u32::MAX] {
                // 3. VitralAiMode (valid: 0..=2)
                let txt = CString::new("hello").unwrap();
                let lang = CString::new("en").unwrap();
                let ai_res = vitral_ai_inline(engine, VitralAiMode(bad), txt.as_ptr(), lang.as_ptr());
                assert_eq!(ai_res.status, VitralStatus::ErrInvalidArgument);
                vitral_free_result(ai_res);
            }

            for bad in [4u32, 5u32, 99u32, u32::MAX] {
                // 4. VitralOpenCcMode (valid: 0..=3)
                assert_eq!(
                    vitral_set_opencc_mode(engine, VitralOpenCcMode(bad)),
                    VitralStatus::ErrInvalidArgument
                );
            }

            for bad in [5u32, 99u32, u32::MAX] {
                // 5. VitralSwipeDir (valid: 0..=4)
                let key_res = vitral_process_key(engine, b'a' as u32, 0, VitralSwipeDir(bad));
                assert_eq!(key_res.status, VitralStatus::ErrInvalidArgument);
                vitral_free_result(key_res);

                // 6. VitralLexiconFormat (valid: 0..=4)
                let path = CString::new("/tmp/nonexistent.txt").unwrap();
                let imp_res =
                    vitral_import_lexicon(engine, path.as_ptr(), VitralLexiconFormat(bad));
                assert_eq!(imp_res.status, VitralStatus::ErrInvalidArgument);
                vitral_free_result(imp_res);
            }

            vitral_shutdown(engine);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn select_candidate_purerust_preserves_confirmed_prefix() {
        unsafe {
            let cfg = CString::new(r#"{"schema":"rime_ice"}"#).unwrap();
            let mut engine: *mut VitralEngine = std::ptr::null_mut();
            assert_eq!(
                vitral_init(cfg.as_ptr(), &mut engine, std::ptr::null_mut()),
                VitralStatus::Ok
            );

            let mut last_cands: Vec<serde_json::Value> = Vec::new();
            for b in b"nihaoshouxiezhongwen" {
                let res = vitral_process_key(engine, *b as u32, 0, VitralSwipeDir::None);
                assert_eq!(res.status, VitralStatus::Ok);
                let json = CStr::from_ptr(res.json_utf8).to_str().unwrap();
                let v: serde_json::Value = serde_json::from_str(json).unwrap();
                last_cands = v["candidates"].as_array().cloned().unwrap_or_default();
                vitral_free_result(res);
            }

            let nihao_idx = last_cands
                .iter()
                .position(|c| c["text"].as_str() == Some("你好"))
                .expect("expected partial '你好' candidate");

            let partial_res = vitral_select_candidate(engine, nihao_idx);
            assert_eq!(partial_res.status, VitralStatus::Ok);
            let partial_json = CStr::from_ptr(partial_res.json_utf8).to_str().unwrap();
            let partial_val: serde_json::Value = serde_json::from_str(partial_json).unwrap();
            let after_cands = partial_val["candidates"].as_array().unwrap();
            let pr_idx = after_cands
                .iter()
                .position(|c| c["comment"].as_str() == Some("shou xie zhong wen"))
                .expect("expected contextual PureRust candidate after partial '你好'");
            let expected = format!(
                "你好{}",
                after_cands[pr_idx]["text"].as_str().unwrap_or_default()
            );
            vitral_free_result(partial_res);

            let sel_res = vitral_select_candidate(engine, pr_idx);
            assert_eq!(sel_res.status, VitralStatus::Ok);
            let sel_json = CStr::from_ptr(sel_res.json_utf8).to_str().unwrap();
            let sel_val: serde_json::Value = serde_json::from_str(sel_json).unwrap();
            assert_eq!(
                sel_val["commit_text"].as_str(),
                Some(expected.as_str()),
                "vitral_select_candidate on PureRust candidate must preserve prefix"
            );
            vitral_free_result(sel_res);

            vitral_shutdown(engine);
        }
    }
}
