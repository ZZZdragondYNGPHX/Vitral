#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
mod windows;
// Share shortcut validation and defaults without loading the input engine in the Linux settings process.
#[path = "../../../../crates/vitral-core/src/shortcuts.rs"]
mod shortcuts;
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};
use tauri::{Manager, State};
#[cfg(windows)]
use windows::*;

struct Settings {
    path: PathBuf,
    lock: Mutex<()>,
}

#[cfg(target_os = "linux")]
#[link(name = "c")]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(target_os = "linux")]
fn lock_file_exclusive(file: &fs::File) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    const LOCK_EX: i32 = 2;
    loop {
        if unsafe { flock(file.as_raw_fd(), LOCK_EX) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err.to_string());
        }
    }
}

#[cfg(windows)]
#[repr(C)]
struct RawOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: *mut core::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LockFileEx(
        hFile: *mut core::ffi::c_void,
        dwFlags: u32,
        dwReserved: u32,
        nNumberOfBytesToLockLow: u32,
        nNumberOfBytesToLockHigh: u32,
        lpOverlapped: *mut RawOverlapped,
    ) -> i32;
}

#[cfg(windows)]
fn lock_file_exclusive(file: &fs::File) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    let mut overlapped = RawOverlapped {
        internal: 0,
        internal_high: 0,
        offset: u32::MAX,
        offset_high: u32::MAX,
        h_event: core::ptr::null_mut(),
    };
    if unsafe {
        LockFileEx(
            file.as_raw_handle() as *mut core::ffi::c_void,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &mut overlapped,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

struct SettingsLock<'a> {
    _mem: MutexGuard<'a, ()>,
    lock_file: fs::File,
}

impl SettingsLock<'_> {
    fn read_rev(&mut self) -> Result<u64, String> {
        self.lock_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| e.to_string())?;
        let mut s = String::new();
        self.lock_file
            .read_to_string(&mut s)
            .map_err(|e| e.to_string())?;
        Ok(s.trim().parse::<u64>().unwrap_or(0))
    }

    fn write_rev(&mut self, rev: u64) -> Result<(), String> {
        self.lock_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| e.to_string())?;
        self.lock_file.set_len(0).map_err(|e| e.to_string())?;
        self.lock_file
            .write_all(rev.to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        self.lock_file.sync_all().map_err(|e| e.to_string())
    }
}

fn lock_settings(settings: &Settings) -> Result<SettingsLock<'_>, String> {
    let mem = settings.lock.lock().map_err(|e| e.to_string())?;
    let dir = settings.path.parent().ok_or("配置目录无效")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let lock_path = settings.path.with_extension("json.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    lock_file_exclusive(&lock_file)?;
    Ok(SettingsLock {
        _mem: mem,
        lock_file,
    })
}

fn merge_setting_alias(
    fields: &mut serde_json::Map<String, Value>,
    key: &str,
    alias: &str,
) -> Result<(), String> {
    if let Some(legacy) = fields.remove(alias) {
        if fields.get(key).is_some_and(|current| current != &legacy) {
            return Err(format!("配置项 {key} 与旧键 {alias} 冲突"));
        }
        fields.entry(key.to_string()).or_insert(legacy);
    }
    Ok(())
}

fn normalize_raw_settings(cfg: &mut Value) -> Result<(), String> {
    let fields = cfg.as_object_mut().ok_or("配置格式错误")?;
    fields.retain(|_, v| !v.is_null());
    merge_setting_alias(fields, "koofr_endpoint", "webdav_url")?;
    merge_setting_alias(fields, "clipboard_ring_capacity", "clipboard_capacity")?;
    if let Some(devices) = fields.get_mut("known_devices").and_then(Value::as_array_mut) {
        for device in devices {
            if let Some(dev_fields) = device.as_object_mut() {
                dev_fields.retain(|_, v| !v.is_null());
                merge_setting_alias(dev_fields, "device_id", "id")?;
                merge_setting_alias(dev_fields, "usage_count", "transfer_count")?;
            }
        }
    }
    Ok(())
}

fn read_preferences_with_hash(path: &Path) -> Result<(Value, u64), String> {
    match fs::read(path) {
        Ok(bytes) => {
            let hash = fnv1a64(&bytes);
            let mut cfg = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            normalize_raw_settings(&mut cfg)?;
            Ok((cfg, hash))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((
            json!({"adaptive_learning":true,"pure_offline":false,"schema":"double_pinyin_flypy","opencc_mode":"simplified","clipboard_ring_capacity":50,"candidate_font_px":16,"theme":"system"}),
            0,
        )),
        Err(e) => Err(e.to_string()),
    }
}

fn public_settings(mut cfg: Value, token: &str) -> Value {
    if cfg.get("shortcuts").is_none_or(Value::is_null) {
        cfg["shortcuts"] = json!(shortcuts::defaults());
    }
    for key in [
        "adaptive_learning",
        "prediction_enabled",
        "prediction_skip_dev_apps",
        "profile_distill_enabled",
        "ai_prefer_local",
        "auto_english_for_dev_apps",
        "disable_paired_punct_in_ide",
    ] {
        if cfg.get(key).is_none_or(Value::is_null) {
            cfg[key] = json!(true);
        }
    }
    if cfg.get("undo_commit_on_backspace").is_none_or(Value::is_null) {
        cfg["undo_commit_on_backspace"] = json!(false);
    }
    if cfg.get("auto_micro_space").is_none_or(Value::is_null) {
        cfg["auto_micro_space"] = json!(false);
    }
    if cfg.get("quote_style").is_none_or(Value::is_null) {
        cfg["quote_style"] = json!("curly");
    }
    for key in [
        "default_english_apps",
        "disable_paired_punct_apps",
        "fuzzy_pinyin_pairs",
        "privacy_apps",
    ] {
        if cfg.get(key).is_none_or(Value::is_null) {
            cfg[key] = json!([]);
        }
    }
    if cfg.get("voice_cleanup_mode").is_none_or(Value::is_null) {
        cfg["voice_cleanup_mode"] = json!("light");
    }
    if cfg.get("prediction_delay_ms").is_none_or(Value::is_null) {
        cfg["prediction_delay_ms"] = json!(380);
    }
    if cfg.get("typo_correction_level").is_none_or(Value::is_null) {
        cfg["typo_correction_level"] = json!("standard");
    }
    if cfg.get("fuzzy_pinyin").is_none_or(Value::is_null) {
        cfg["fuzzy_pinyin"] = json!(false);
    }
    if cfg.get("frecency_half_life_sec").is_none_or(Value::is_null) {
        cfg["frecency_half_life_sec"] = json!(604_800u64);
    }
    if cfg.get("schema").is_none_or(Value::is_null) {
        cfg["schema"] = json!("double_pinyin_flypy");
    }
    if cfg.get("opencc_mode").is_none_or(Value::is_null) {
        cfg["opencc_mode"] = json!("simplified");
    }
    if cfg.get("clipboard_ring_capacity").is_none_or(Value::is_null) {
        cfg["clipboard_ring_capacity"] = json!(50u64);
    }
    if cfg.get("candidate_font_px").is_none_or(Value::is_null) {
        cfg["candidate_font_px"] = json!(16u64);
    }
    if cfg.get("theme").is_none_or(Value::is_null) {
        cfg["theme"] = json!("system");
    }
    if cfg.get("pure_offline").is_none_or(Value::is_null) {
        cfg["pure_offline"] = json!(false);
    }
    cfg["shortcut_defaults"] = json!(shortcuts::defaults());
    cfg["shortcut_actions"] = json!(shortcuts::ShortcutAction::ALL.map(|action| json!({"id":action,"label":action.label()})));
    cfg["launch_page"] = json!(if std::env::args().any(|a| a == "--clipboard") { "library" } else { "input" });
    if let Some(fields) = cfg.as_object_mut() {
        fields.remove("linux_english_mode");
    }
    for key in ["ai_api_key", "koofr_password", "sync_key"] {
        let present = cfg.get(key).and_then(Value::as_str).is_some_and(|s| {
            if key == "koofr_password" {
                !s.trim_matches(['\r', '\n']).is_empty()
            } else {
                !s.is_empty()
            }
        });
        cfg[format!("has_{key}")] = json!(present);
        if let Some(fields) = cfg.as_object_mut() {
            fields.remove(key);
        }
    }
    cfg["_settings_token"] = json!(token);
    cfg
}

#[tauri::command]
fn get_settings(settings: State<Settings>) -> Result<Value, String> {
    let mut guard = lock_settings(&settings)?;
    let rev = guard.read_rev()?;
    let (cfg, hash) = read_preferences_with_hash(&settings.path)?;
    let token = format!("{rev}:{hash:016x}");
    Ok(public_settings(cfg, &token))
}

#[cfg(target_os = "linux")]
fn call_engine(command: &str, payload: &Value) -> Result<Value, String> {
    let bus = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let proxy =
        zbus::blocking::Proxy::new(&bus, "dev.vitral.IME", "/dev/vitral/IME", "dev.vitral.IME")
            .map_err(|e| e.to_string())?;
    let reply: String = proxy
        .call("Command", &(command, payload.to_string()))
        .map_err(|e| format!("请确认已在 Fcitx5 中启用 Vitral。{e}"))?;
    serde_json::from_str(&reply).map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
fn probe_engine_raw(command: &str, payload: &Value) -> Result<Value, String> {
    call_engine(command, payload)
}

#[tauri::command]
async fn probe_engine(
    app: tauri::AppHandle,
    include_models: Option<bool>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app.state::<Settings>();
        let public = {
            let mut guard = lock_settings(&settings)?;
            let rev = guard.read_rev()?;
            let (cfg, hash) = read_preferences_with_hash(&settings.path)?;
            let token = format!("{rev}:{hash:016x}");
            public_settings(cfg, &token)
        };
        match probe_engine_raw("is-pure-offline", &json!({})) {
            Ok(live) => {
                let (models_state, ai_route) = if include_models == Some(true) {
                    let models = probe_engine_raw("models-state", &json!({})).ok();
                    let route = probe_engine_raw("ai-inline-route", &json!({}))
                        .ok()
                        .and_then(|v| v.get("route").and_then(Value::as_str).map(String::from));
                    (models.unwrap_or(Value::Null), json!(route))
                } else {
                    (Value::Null, Value::Null)
                };
                Ok(json!({
                    "connected": true,
                    "live_pure_offline": live.get("pure_offline").and_then(Value::as_bool),
                    "ai_route": ai_route,
                    "models_state": models_state,
                    "settings": public,
                }))
            }
            Err(_) => Ok(json!({
                "connected": false,
                "live_pure_offline": Value::Null,
                "ai_route": Value::Null,
                "models_state": Value::Null,
                "settings": public,
            })),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn engine(command: String, payload: Value) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || call_engine(&command, &payload))
        .await
        .map_err(|e| e.to_string())?
}

fn effective_engine_config(mut cfg: Value) -> Value {
    if cfg.get("frecency_half_life_sec").is_none_or(Value::is_null) {
        cfg["frecency_half_life_sec"] = json!(604_800u64);
    }
    if cfg.get("shortcuts").is_none_or(Value::is_null) {
        cfg["shortcuts"] = json!(shortcuts::defaults());
    }
    if cfg.get("schema").is_none_or(Value::is_null) {
        cfg["schema"] = json!("double_pinyin_flypy");
    }
    if cfg.get("opencc_mode").is_none_or(Value::is_null) {
        cfg["opencc_mode"] = json!("simplified");
    }
    if cfg.get("clipboard_ring_capacity").is_none_or(Value::is_null) {
        cfg["clipboard_ring_capacity"] = json!(50u64);
    }
    cfg
}

fn configure_payload(cfg: &Value) -> Value {
    let mut payload = effective_engine_config(cfg.clone());
    let has_koofr_pw = payload
        .get("koofr_password")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim_matches(['\r', '\n']).is_empty());
    if !has_koofr_pw {
        payload["clear_koofr_password"] = json!(true);
    }
    payload
}

#[tauri::command]
async fn save_settings(
    app: tauri::AppHandle,
    patch: Value,
    expected_token: Option<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app.state::<Settings>();
        let mut guard = lock_settings(&settings)?;
        let rev = guard.read_rev()?;
        let (mut cfg, hash) = read_preferences_with_hash(&settings.path)?;
        let current_token = format!("{rev}:{hash:016x}");
        if expected_token
            .as_deref()
            .is_some_and(|exp| exp != current_token)
        {
            let persisted_cfg = effective_engine_config(cfg.clone());
            let mut out_token = current_token;
            if cfg != persisted_cfg {
                let serialized =
                    serde_json::to_vec_pretty(&persisted_cfg).map_err(|e| e.to_string())?;
                let next_hash = fnv1a64(&serialized);
                let dir = settings.path.parent().ok_or("配置目录无效")?;
                fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
                file.write_all(&serialized).map_err(|e| e.to_string())?;
                file.as_file().sync_all().map_err(|e| e.to_string())?;
                file.persist(&settings.path).map_err(|e| e.to_string())?;
                out_token = format!("{rev}:{next_hash:016x}");
                cfg = persisted_cfg;
            }
            let engine_cfg = configure_payload(&cfg);
            let live = call_engine("configure", &engine_cfg);
            let active = live.is_ok();
            let message = live.err();
            return Ok(json!({
                "settings": public_settings(cfg, &out_token),
                "active": active,
                "stale": true,
                "message": message,
            }));
        }
        let patch_map = patch.as_object().ok_or("配置格式错误")?;
        let has_effective_patch = patch_map
            .keys()
            .any(|k| k != "_settings_token" && k != "_settings_version");
        if !has_effective_patch {
            let live = probe_engine_raw("is-pure-offline", &json!({}));
            let active = live.is_ok();
            let message = live.err();
            return Ok(json!({
                "settings": public_settings(cfg, &current_token),
                "active": active,
                "message": message,
            }));
        }
        let fields = cfg.as_object_mut().ok_or("配置格式错误")?;
        let was_offline = fields
            .get("pure_offline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        for (key, value) in patch_map {
            if key == "_settings_token" || key == "_settings_version" {
                continue;
            }
            match key.as_str() {
                "shortcuts" => {
                    let mut bindings: Vec<shortcuts::ShortcutBinding> = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    shortcuts::normalize(&mut bindings).map_err(|e| e.to_string())?;
                    fields.insert(key.clone(), json!(bindings));
                    continue;
                }
                "adaptive_learning"
                | "prediction_enabled"
                | "prediction_skip_dev_apps"
                | "profile_distill_enabled"
                | "auto_english_for_dev_apps"
                | "disable_paired_punct_in_ide"
                | "auto_micro_space"
                | "undo_commit_on_backspace" => {
                    if !value.is_boolean() {
                        return Err("该开关设置应为开启或关闭".into());
                    }
                }
                "fuzzy_pinyin_pairs" => {
                    const VALID_FUZZY: &[&str] = &[
                        "zh_z", "ch_c", "sh_s", "n_l", "f_h", "an_ang", "en_eng", "in_ing",
                    ];
                    if !value.as_array().is_some_and(|arr| {
                        arr.iter().all(|v| {
                            v.as_str().is_some_and(|pair| VALID_FUZZY.contains(&pair))
                        })
                    }) {
                        return Err("模糊音分组包含无效项".into());
                    }
                }
                "quote_style" => {
                    if !matches!(value.as_str(), Some("curly" | "corner")) {
                        return Err("引号风格应为弯引号或直角引号".into());
                    }
                }
                "default_english_apps" | "disable_paired_punct_apps" | "privacy_apps" => {
                    if !value
                        .as_array()
                        .is_some_and(|arr| arr.iter().all(|v| v.is_string()))
                    {
                        return Err("应用规则列表应为字符串数组".into());
                    }
                }
                "voice_cleanup_mode" => {
                    if !matches!(value.as_str(), Some("off" | "light" | "polish")) {
                        return Err("语音口语规整模式应为关闭、轻度规整或保真润色".into());
                    }
                }
                "fuzzy_pinyin" => {
                    if !value.is_boolean() {
                        return Err("模糊音设置应为开启或关闭".into());
                    }
                }
                "typo_correction_level" => {
                    if !matches!(
                        value.as_str(),
                        Some("off" | "conservative" | "standard" | "aggressive")
                    ) {
                        return Err("拼写纠错强度应为关闭、保守、标准或积极".into());
                    }
                }
                "prediction_delay_ms" if !(150..=2000).contains(&value.as_u64().unwrap_or(0)) => {
                    return Err("预测延迟应在 150–2000 ms 之间".into())
                }
                "candidate_font_px" if !(13..=22).contains(&value.as_u64().unwrap_or(0)) => {
                    return Err("候选字号应在 13–22 px 之间".into())
                }
                "clipboard_ring_capacity" => {
                    let cap = value.as_u64().unwrap_or(0);
                    let valid = if expected_token.is_some() {
                        cap >= 1
                    } else {
                        (20..=200).contains(&cap)
                    };
                    if !valid {
                        return Err("剪贴板容量应在 20–200 条之间".into());
                    }
                }
                "model_catalog_url" if !value.is_null() && !value.is_string() => {
                    return Err("模型目录地址应为 HTTP 或 HTTPS 地址".into());
                }
                "koofr_endpoint" | "ai_endpoint" | "model_catalog_url"
                    if value.as_str().is_some_and(|s| !s.is_empty()) =>
                {
                    let url = url::Url::parse(value.as_str().unwrap())
                        .map_err(|_| "请输入完整的服务地址")?;
                    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                        return Err("请输入 HTTP 或 HTTPS 地址".into());
                    }
                }
                "frecency_half_life_sec" if value.as_u64().is_none_or(|s| s == 0) => {
                    return Err("请输入正数秒".into())
                }
                "prediction_delay_ms"
                | "frecency_half_life_sec"
                | "schema"
                | "opencc_mode"
                | "theme"
                | "candidate_font_px"
                | "model_catalog_url"
                | "koofr_endpoint"
                | "koofr_username"
                | "koofr_password"
                | "sync_key"
                | "ai_endpoint"
                | "ai_api_key"
                | "ai_model"
                | "pure_offline"
                | "ai_tier3_enabled"
                | "ai_prefer_local"
                | "toolbar" => (),
                _ => return Err(format!("未知设置 {key}")),
            }
            fields.insert(key.clone(), value.clone());
        }
        normalize_raw_settings(&mut cfg)?;
        cfg = effective_engine_config(cfg);
        let serialized = serde_json::to_vec_pretty(&cfg).map_err(|e| e.to_string())?;
        let next_hash = fnv1a64(&serialized);
        let dir = settings.path.parent().ok_or("配置目录无效")?;
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        file.write_all(&serialized).map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&settings.path).map_err(|e| e.to_string())?;
        let next_rev = rev.saturating_add(1);
        guard.write_rev(next_rev)?;
        let next_token = format!("{next_rev}:{next_hash:016x}");
        let next_offline = cfg
            .get("pure_offline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let offline_ok = if !was_offline && next_offline {
            call_engine("set-pure-offline", &json!({"enabled": true}))
        } else {
            Ok(Value::Null)
        };
        let engine_cfg = configure_payload(&cfg);
        let live = call_engine("configure", &engine_cfg);
        let active = live.is_ok() && offline_ok.is_ok();
        let message = live.err().or_else(|| offline_ok.err());
        Ok(json!({"settings":public_settings(cfg, &next_token),"active":active,"message":message}))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn choose_file(lexicon: bool) -> Option<String> {
    use gtk::prelude::*;
    let chooser = gtk::FileChooserNative::new(
        Some(if lexicon {
            "导入词库"
        } else {
            "选择要发送的文件"
        }),
        None::<&gtk::Window>,
        gtk::FileChooserAction::Open,
        Some("选择"),
        Some("取消"),
    );
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Vitral 词库"));
    for pattern in ["*.scel", "*.dict.yaml", "*.tsv", "*.txt"] {
        filter.add_pattern(pattern);
    }
    if lexicon {
        chooser.add_filter(filter);
    }
    (chooser.run() == gtk::ResponseType::Accept)
        .then(|| chooser.filename())
        .flatten()
        .and_then(|p| p.into_os_string().into_string().ok())
}

#[cfg(target_os = "linux")]
struct Capture {
    child: std::process::Child,
    reader: Option<std::thread::JoinHandle<Result<(), String>>>,
}
#[cfg(target_os = "linux")]
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
#[cfg(target_os = "linux")]
type Microphone = Mutex<Option<Capture>>;

#[cfg(target_os = "linux")]
#[tauri::command]
fn start_recording(microphone: State<Microphone>) -> Result<(), String> {
    use base64::prelude::*;
    use std::{
        io::Read,
        process::{Command, Stdio},
    };
    let mut active = microphone.lock().map_err(|e| e.to_string())?;
    if active.is_some() {
        return Err("正在录音".into());
    }
    call_engine("voice-start", &json!({}))?;
    let mut child = Command::new("arecord")
        .args(["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("无法读取麦克风")?;
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 32000];
        let mut used = 0;
        loop {
            let n = stdout.read(&mut buf[used..]).map_err(|e| e.to_string())?;
            used += n;
            if used == buf.len() || n == 0 {
                if used >= 2 {
                    call_engine(
                        "voice-push-pcm",
                        &json!({"pcm_i16_b64":BASE64_STANDARD.encode(&buf[..used/2*2])}),
                    )?;
                }
                used = 0;
            }
            if n == 0 {
                break;
            }
        }
        Ok(())
    });
    *active = Some(Capture {
        child,
        reader: Some(reader),
    });
    Ok(())
}

#[cfg(target_os = "linux")]
#[tauri::command]
async fn stop_recording(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mic = app.state::<Microphone>();
        let mut active = mic.lock().map_err(|e| e.to_string())?;
        let mut capture = active.take().ok_or("尚未开始录音")?;
        if let Some(status) = capture.child.try_wait().map_err(|e| e.to_string())? {
            if !status.success() {
                return Err("无法从默认麦克风录音".into());
            }
        } else {
            capture.child.kill().map_err(|e| e.to_string())?;
        }
        capture.child.wait().map_err(|e| e.to_string())?;
        if let Some(reader) = capture.reader.take() {
            reader.join().map_err(|_| "麦克风读取失败")??;
        }
        call_engine("voice-stop", &json!({}))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn main() {
    #[cfg(windows)]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--engine")) {
        if vitral_core::windows_host::serve().is_err() {
            std::process::exit(1);
        }
        return;
    }
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(windows)]
            let path = vitral_core::windows_host::settings_path()?;
            #[cfg(target_os = "linux")]
            let path = app.path().config_dir()?.join("vitral/settings.json");
            app.manage(Microphone::default());
            app.manage(Settings {
                path,
                lock: Mutex::new(()),
            });
            if std::env::args().any(|arg| arg == "--recorder") {
                tauri::WebviewWindowBuilder::new(
                    app,
                    "main",
                    tauri::WebviewUrl::App("recorder.html".into()),
                )
                .title("Vitral · 录音笔")
                .inner_size(380., 280.)
                .min_inner_size(300., 200.)
                .always_on_top(true)
                .build()?;
            } else {
                tauri::WebviewWindowBuilder::from_config(app, &app.config().app.windows[0])?
                    .build()?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            probe_engine,
            engine,
            choose_file,
            start_recording,
            stop_recording
        ])
        .run(tauri::generate_context!())
        .expect("Vitral desktop failed");
}
