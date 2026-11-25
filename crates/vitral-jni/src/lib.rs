use jni::objects::{JClass, JFloatArray, JLongArray, JShortArray, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use serde::Serialize;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use vitral_core::ai::AiMode;
use vitral_core::clip::ClipboardPayload;
use vitral_core::eng::{OpenCcMode, SchemaKind, SwipeDir};
use vitral_core::error::VitralError;
use vitral_core::hw::{HwMode, StrokePoint};
use vitral_core::imp::LexiconFormat;
use vitral_core::{TouchKeyHint, VitralCore};

const ABI_VERSION: jint = 1;

#[derive(Serialize)]
struct JniEnvelope<T: Serialize> {
    status: i32,
    data: Option<T>,
    error: Option<String>,
}

fn status_code(err: &VitralError) -> i32 {
    match err {
        VitralError::NullPointer(_) => 1,
        VitralError::InvalidUtf8(_) => 2,
        VitralError::InvalidArgument(_) => 3,
        VitralError::NotInitialized => 4,
        VitralError::PureOfflineViolation(_) => 5,
        VitralError::Io(_) => 6,
        VitralError::Parse(_) => 7,
        VitralError::NotFound(_) => 8,
        VitralError::Integrity(_) => 9,
        VitralError::ModelUnavailable(_) => 10,
        VitralError::Internal(_) => 11,
    }
}

fn to_jstring(env: &mut JNIEnv<'_>, text: &str) -> jstring {
    match env.new_string(text) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn ok_envelope<T: Serialize>(env: &mut JNIEnv<'_>, data: &T) -> jstring {
    let payload = serde_json::to_string(&JniEnvelope {
        status: 0,
        data: Some(data),
        error: None,
    })
    .unwrap_or_else(|e| format!(r#"{{"status":11,"data":null,"error":"{e}"}}"#));
    to_jstring(env, &payload)
}

fn err_envelope(env: &mut JNIEnv<'_>, code: i32, msg: &str) -> jstring {
    let _ = env.exception_clear();
    let payload = serde_json::to_string(&JniEnvelope::<serde_json::Value> {
        status: code,
        data: None,
        error: Some(msg.to_string()),
    })
    .unwrap_or_else(|_| r#"{"status":11,"data":null,"error":"serialization error"}"#.to_string());
    to_jstring(env, &payload)
}

fn core_from_handle<'a>(handle: jlong) -> Result<&'a VitralCore, VitralError> {
    if handle == 0 {
        return Err(VitralError::NullPointer("handle".into()));
    }
    Ok(unsafe { &*(handle as *const VitralCore) })
}

fn read_jstring(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Result<String, VitralError> {
    if s.is_null() {
        return Err(VitralError::NullPointer("jstring".into()));
    }
    env.get_string(s)
        .map(|js| js.into())
        .map_err(|e| VitralError::InvalidUtf8(e.to_string()))
}

fn read_opt_jstring(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Result<Option<String>, VitralError> {
    if s.is_null() {
        return Ok(None);
    }
    read_jstring(env, s).map(Some)
}

fn guard_jstring<F>(env: &mut JNIEnv<'_>, f: F) -> jstring
where
    F: FnOnce(&mut JNIEnv<'_>) -> jstring,
{
    match catch_unwind(AssertUnwindSafe(|| f(env))) {
        Ok(res) => res,
        Err(_) => err_envelope(env, 99, "panic in rust core"),
    }
}

fn guard_jint<F>(f: F) -> jint
where
    F: FnOnce() -> jint,
{
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(99)
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_abiVersion(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jint {
    ABI_VERSION
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_nativeInit(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    config_json: JString<'_>,
) -> jlong {
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default().with_max_level(log::LevelFilter::Info),
        );
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let cfg = read_jstring(&mut env, &config_json)?;
        VitralCore::init(&cfg)
    })) {
        Ok(Ok(core)) => Box::into_raw(Box::new(core)) as jlong,
        _ => 0,
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_shutdown(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if handle != 0 {
            unsafe {
                drop(Box::from_raw(handle as *mut VitralCore));
            }
        }
    }));
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_setPureOffline(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    enabled: jboolean,
) -> jint {
    guard_jint(|| match core_from_handle(handle) {
        Ok(core) => {
            core.set_pure_offline(enabled != JNI_FALSE);
            0
        }
        Err(e) => status_code(&e),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_isPureOffline(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    match catch_unwind(AssertUnwindSafe(|| {
        core_from_handle(handle)
            .map(|c| c.is_pure_offline())
            .unwrap_or(false)
    })) {
        Ok(true) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_setSchema(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    schema_id: jint,
) -> jint {
    guard_jint(|| match core_from_handle(handle) {
        Ok(core) => {
            let schema = match schema_id {
                0 => SchemaKind::DoublePinyinFlypy,
                1 => SchemaKind::RimeIce,
                _ => return 3,
            };
            core.set_schema(schema);
            0
        }
        Err(e) => status_code(&e),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_setOpenccMode(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    mode: jint,
) -> jint {
    guard_jint(|| match core_from_handle(handle) {
        Ok(core) => {
            let m = match mode {
                0 => OpenCcMode::Simplified,
                1 => OpenCcMode::Traditional,
                2 => OpenCcMode::TaiwanStandard,
                3 => OpenCcMode::HongKong,
                _ => return 3,
            };
            core.set_opencc_mode(m);
            0
        }
        Err(e) => status_code(&e),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_reloadAssets(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => match core.reload_assets() {
            Ok(rep) => ok_envelope(env, &rep),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        },
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_onAppFocus(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    app_id: JString<'_>,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let app = match read_jstring(env, &app_id) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        match core.on_app_focus(&app) {
            Ok(st) => ok_envelope(env, &st),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_processKey(
    env: JNIEnv<'_>,
    class: JClass<'_>,
    handle: jlong,
    keycode: jint,
    modifiers: jint,
    swipe_dir: jint,
) -> jstring {
    Java_dev_vitral_ime_core_VitralNative_processTouchKey(
        env, class, handle, keycode, modifiers, swipe_dir, 0, 0.0, 0, 0.0,
    )
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_processTouchKey(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    keycode: jint,
    modifiers: jint,
    swipe_dir: jint,
    alt1_char: jint,
    alt1_penalty: jfloat,
    alt2_char: jint,
    alt2_penalty: jfloat,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let dir = match swipe_dir {
            0 => SwipeDir::None,
            1 => SwipeDir::Up,
            2 => SwipeDir::Down,
            3 => SwipeDir::Left,
            4 => SwipeDir::Right,
            _ => return err_envelope(env, 3, "invalid swipeDir"),
        };
        let hint = TouchKeyHint::from_keycodes(
            keycode as u32,
            [(alt1_char as u32, alt1_penalty), (alt2_char as u32, alt2_penalty)],
        );
        match core.process_touch_key(keycode as u32, modifiers as u32, dir, hint) {
            Ok(act) => ok_envelope(env, &act),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_selectCandidate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    index: jint,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        if index < 0 {
            return err_envelope(env, 3, "negative index");
        }
        match core.select_candidate(index as usize) {
            Ok(act) => ok_envelope(env, &act),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_pinCandidate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    index: jint,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        if index < 0 {
            return err_envelope(env, 3, "negative index");
        }
        match core.pin_candidate(index as usize) {
            Ok(act) => ok_envelope(env, &act),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_eraseCandidate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    index: jint,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        if index < 0 {
            return err_envelope(env, 3, "negative index");
        }
        match core.erase_candidate(index as usize) {
            Ok(act) => ok_envelope(env, &act),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_hwBegin(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    mode: jint,
) -> jint {
    guard_jint(|| match core_from_handle(handle) {
        Ok(core) => {
            let m = match mode {
                0 => HwMode::PhoneticLookup,
                1 => HwMode::ContinuousLattice,
                _ => return 3,
            };
            core.hw_begin(m);
            0
        }
        Err(e) => status_code(&e),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_hwPushStroke(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    xs: JFloatArray<'_>,
    ys: JFloatArray<'_>,
    timestamps_ms: JLongArray<'_>,
    pen_up: jboolean,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        if xs.is_null() || ys.is_null() || timestamps_ms.is_null() {
            return err_envelope(env, 1, "xs, ys, and timestampsMs arrays must not be null");
        }
        let len_x = match env.get_array_length(&xs) {
            Ok(n) => n as usize,
            Err(e) => return err_envelope(env, 3, &e.to_string()),
        };
        let len_y = match env.get_array_length(&ys) {
            Ok(n) => n as usize,
            Err(e) => return err_envelope(env, 3, &e.to_string()),
        };
        let len_t = match env.get_array_length(&timestamps_ms) {
            Ok(n) => n as usize,
            Err(e) => return err_envelope(env, 3, &e.to_string()),
        };
        if len_x != len_y || len_x != len_t {
            return err_envelope(env, 3, "xs, ys, timestampsMs lengths must match");
        }
        let mut bx = vec![0.0f32; len_x];
        let mut by = vec![0.0f32; len_x];
        let mut bt = vec![0i64; len_x];
        if len_x > 0 {
            if let Err(e) = env.get_float_array_region(&xs, 0, &mut bx) {
                return err_envelope(env, 11, &e.to_string());
            }
            if let Err(e) = env.get_float_array_region(&ys, 0, &mut by) {
                return err_envelope(env, 11, &e.to_string());
            }
            if let Err(e) = env.get_long_array_region(&timestamps_ms, 0, &mut bt) {
                return err_envelope(env, 11, &e.to_string());
            }
        }
        let pts: Vec<StrokePoint> = (0..len_x)
            .map(|i| StrokePoint {
                x: bx[i],
                y: by[i],
                timestamp_ms: bt[i].max(0) as u64,
            })
            .collect();
        match core.hw_push_stroke(&pts, pen_up != JNI_FALSE) {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_hwEnd(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => match core.hw_end() {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        },
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_hwClear(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    guard_jint(|| match core_from_handle(handle) {
        Ok(core) => {
            core.hw_clear();
            0
        }
        Err(e) => status_code(&e),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_hwOverrideSlot(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    slot_idx: jint,
    candidate_idx: jint,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        if slot_idx < 0 || candidate_idx < 0 {
            return err_envelope(env, 3, "negative slot or candidate index");
        }
        match core.hw_override_slot(slot_idx as usize, candidate_idx as usize) {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_importLexicon(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    path: JString<'_>,
    format_hint: jint,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let p = match read_jstring(env, &path) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let fmt = match format_hint {
            0 => LexiconFormat::Auto,
            1 => LexiconFormat::Scel,
            2 => LexiconFormat::DictYaml,
            3 => LexiconFormat::Tsv,
            4 => LexiconFormat::Txt,
            _ => return err_envelope(env, 3, "invalid formatHint"),
        };
        match core.import_lexicon(Path::new(&p), fmt) {
            Ok(rep) => ok_envelope(env, &rep),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_clipboardPush(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    payload_json: JString<'_>,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let raw = match read_jstring(env, &payload_json) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let payload: ClipboardPayload = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => return err_envelope(env, 7, &e.to_string()),
        };
        ok_envelope(env, &core.clipboard_push(payload))
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_clipboardState(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.clipboard_state()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_clipboardIngestRemote(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    payload_json: JString<'_>,
    is_cold_boot: jboolean,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let raw = match read_jstring(env, &payload_json) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let payload: ClipboardPayload = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => return err_envelope(env, 7, &e.to_string()),
        };
        ok_envelope(
            env,
            &core.clipboard_ingest_remote(payload, is_cold_boot != JNI_FALSE),
        )
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_quickPasteState(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.quick_paste_state()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_consumeQuickPaste(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.consume_quick_paste()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_onImeOpen(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.on_ime_open()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_onImeClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.on_ime_close()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_dismissQuickPaste(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.dismiss_quick_paste()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_clipboardPin(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    item_id: JString<'_>,
    shortcut_code: JString<'_>,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let id = match read_jstring(env, &item_id) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let code = match read_opt_jstring(env, &shortcut_code) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        match core.clipboard_pin(&id, code.as_deref()) {
            Ok(st) => ok_envelope(env, &st),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_clipboardClearUnpinned(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.clipboard_clear_unpinned()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_relayDevicesMru(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => ok_envelope(env, &core.relay_devices_mru()),
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_relaySend(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    device_id: JString<'_>,
    payload_or_path: JString<'_>,
    is_file_path: jboolean,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let dev = match read_jstring(env, &device_id) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let payload = match read_jstring(env, &payload_or_path) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        match core.relay_send(&dev, &payload, is_file_path != JNI_FALSE) {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_relayPollInbox(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => match core.relay_poll_inbox() {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        },
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_relayCleanupOrphans(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => match core.relay_cleanup_orphans() {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        },
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_voiceStart(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    guard_jint(|| match core_from_handle(handle) {
        Ok(core) => {
            core.voice_start();
            0
        }
        Err(e) => status_code(&e),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_voicePushPcm(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    pcm: JShortArray<'_>,
    len: jint,
) -> jint {
    guard_jint(|| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return status_code(&e),
        };
        if len < 0 {
            return 3;
        }
        if pcm.is_null() {
            return 1;
        }
        let n = len as usize;
        if n == 0 {
            return match core.voice_push_pcm(&[]) {
                Ok(()) => 0,
                Err(e) => status_code(&e),
            };
        }
        let pcm_len = match env.get_array_length(&pcm) {
            Ok(m) if m >= 0 => m as usize,
            _ => {
                let _ = env.exception_clear();
                return 3;
            }
        };
        if n > pcm_len {
            return 3;
        }
        let mut buf = vec![0i16; n];
        if env.get_short_array_region(&pcm, 0, &mut buf).is_err() {
            let _ = env.exception_clear();
            return 11;
        }
        match core.voice_push_pcm(&buf) {
            Ok(()) => 0,
            Err(e) => status_code(&e),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_voiceStop(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    guard_jstring(&mut env, |env| match core_from_handle(handle) {
        Ok(core) => match core.voice_stop() {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        },
        Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_aiInline(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    mode: jint,
    text: JString<'_>,
    target_lang: JString<'_>,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let ai_mode = match mode {
            0 => AiMode::Translate,
            1 => AiMode::Polish,
            2 => AiMode::Summary,
            _ => return err_envelope(env, 3, "invalid AiMode"),
        };
        let txt = match read_jstring(env, &text) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let lang = match read_jstring(env, &target_lang) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        match core.ai_inline(ai_mode, &txt, &lang) {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_VitralNative_restoreFromKoofr(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    webdav_cfg_json: JString<'_>,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let core = match core_from_handle(handle) {
            Ok(c) => c,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        let cfg = match read_jstring(env, &webdav_cfg_json) {
            Ok(s) => s,
            Err(e) => return err_envelope(env, status_code(&e), &e.to_string()),
        };
        match core.restore_from_koofr(&cfg) {
            Ok(res) => ok_envelope(env, &res),
            Err(e) => err_envelope(env, status_code(&e), &e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_vitral_ime_core_PlatformCommands_execute(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    name: JString<'_>,
    payload: JString<'_>,
) -> jstring {
    guard_jstring(&mut env, |env| {
        let result = (|| -> vitral_core::commands::Result<_> {
            let core = core_from_handle(handle)?;
            let name = read_jstring(env, &name)?;
            let payload = serde_json::from_str(&read_jstring(env, &payload)?)?;
            vitral_core::commands::execute(core, &name, &payload)
        })();
        match result {
            Ok(value) => ok_envelope(env, &value),
            Err(error) => err_envelope(env,
                error.downcast_ref::<VitralError>().map(status_code).unwrap_or(3),
                &error.to_string()),
        }
    })
}
