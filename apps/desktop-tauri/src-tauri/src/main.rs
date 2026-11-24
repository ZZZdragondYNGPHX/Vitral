#![cfg_attr(windows, windows_subsystem = "windows")]
#[cfg(windows)]
mod windows;
// Share shortcut validation and defaults without loading the input engine in the Linux settings process.
#[path = "../../../../crates/vitral-core/src/shortcuts.rs"]
mod shortcuts;
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
};
use tauri::{Manager, State};
#[cfg(windows)]
use windows::*;

struct Settings {
    path: PathBuf,
    lock: Mutex<()>,
}

fn preferences(settings: &Settings) -> Result<Value, String> {
    match fs::read(&settings.path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(
            json!({"adaptive_learning":true,"learn_from_clipboard":true,"pure_offline":false,"schema":"double_pinyin_flypy","opencc_mode":"simplified","clipboard_ring_capacity":50,"candidate_font_px":16,"theme":"system"}),
        ),
        Err(e) => Err(e.to_string()),
    }
}

fn public_settings(mut cfg: Value) -> Value {
    if cfg.get("shortcuts").is_none() {
        cfg["shortcuts"] = json!(shortcuts::defaults());
    }
    for key in [
        "adaptive_learning",
        "learn_from_clipboard",
        "prediction_enabled",
        "profile_distill_enabled",
        "ai_prefer_local",
        "auto_english_for_dev_apps",
        "disable_paired_punct_in_ide",
        "undo_commit_on_backspace",
    ] {
        if cfg.get(key).is_none() {
            cfg[key] = json!(true);
        }
    }
    if cfg.get("auto_micro_space").is_none() {
        cfg["auto_micro_space"] = json!(false);
    }
    if cfg.get("quote_style").is_none() {
        cfg["quote_style"] = json!("curly");
    }
    for key in [
        "default_english_apps",
        "disable_paired_punct_apps",
        "fuzzy_pinyin_pairs",
    ] {
        if cfg.get(key).is_none() {
            cfg[key] = json!([]);
        }
    }
    if cfg.get("prediction_delay_ms").is_none() {
        cfg["prediction_delay_ms"] = json!(380);
    }
    if cfg.get("typo_correction_level").is_none() {
        cfg["typo_correction_level"] = json!("standard");
    }
    if cfg.get("fuzzy_pinyin").is_none() {
        cfg["fuzzy_pinyin"] = json!(false);
    }
    cfg["shortcut_defaults"] = json!(shortcuts::defaults());
    cfg["shortcut_actions"] = json!(shortcuts::ShortcutAction::ALL.map(|action| json!({"id":action,"label":action.label()})));
    cfg["launch_page"] = json!(if std::env::args().any(|a| a == "--clipboard") { "library" } else { "input" });
    if cfg!(target_os = "linux") {
        cfg["linux_english_mode"] = json!(cfg["linux_english_mode"].as_bool().unwrap_or(true));
    } else if let Some(fields) = cfg.as_object_mut() {
        fields.remove("linux_english_mode");
    }
    for key in ["ai_api_key", "koofr_password", "sync_key"] {
        cfg[format!("has_{key}")] = json!(cfg.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty()));
        if let Some(fields) = cfg.as_object_mut() {
            fields.remove(key);
        }
    }
    cfg
}

#[tauri::command]
fn get_settings(settings: State<Settings>) -> Result<Value, String> {
    Ok(public_settings(preferences(&settings)?))
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

#[tauri::command]
async fn engine(command: String, payload: Value) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || call_engine(&command, &payload))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn save_settings(app: tauri::AppHandle, patch: Value) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app.state::<Settings>();
        let _lock = settings.lock.lock().map_err(|e| e.to_string())?;
        let mut cfg = preferences(&settings)?;
        let fields = cfg.as_object_mut().ok_or("配置格式错误")?;
        for (key, value) in patch.as_object().ok_or("配置格式错误")? {
            match key.as_str() {
                "shortcuts" => {
                    let mut bindings: Vec<shortcuts::ShortcutBinding> = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    shortcuts::normalize(&mut bindings).map_err(|e| e.to_string())?;
                    fields.insert(key.clone(), json!(bindings));
                    continue;
                }
                "adaptive_learning"
                | "learn_from_clipboard"
                | "prediction_enabled"
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
                "default_english_apps" | "disable_paired_punct_apps" => {
                    if !value
                        .as_array()
                        .is_some_and(|arr| arr.iter().all(|v| v.is_string()))
                    {
                        return Err("应用规则列表应为字符串数组".into());
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
                "linux_english_mode" if cfg!(target_os = "linux") => {
                    if !value.is_boolean() {
                        return Err("输入法内英文模式应为开启或关闭".into());
                    }
                }
                "prediction_delay_ms" if !(150..=2000).contains(&value.as_u64().unwrap_or(0)) => {
                    return Err("预测延迟应在 150–2000 ms 之间".into())
                }
                "candidate_font_px" if !(13..=22).contains(&value.as_u64().unwrap_or(0)) => {
                    return Err("候选字号应在 13–22 px 之间".into())
                }
                "clipboard_ring_capacity" if !(20..=200).contains(&value.as_u64().unwrap_or(0)) => {
                    return Err("剪贴板容量应在 20–200 条之间".into())
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
                | "clipboard_ring_capacity"
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
        let dir = settings.path.parent().ok_or("配置目录无效")?;
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(&cfg).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(&settings.path).map_err(|e| e.to_string())?;
        let live = call_engine("configure", &cfg);
        Ok(json!({"settings":public_settings(cfg),"active":live.is_ok(),"message":live.err()}))
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
            engine,
            choose_file,
            start_recording,
            stop_recording
        ])
        .run(tauri::generate_context!())
        .expect("Vitral desktop failed");
}
