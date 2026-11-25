use super::*;
use base64::prelude::*;
use std::ptr::null_mut;
use windows_sys::Win32::{
    Media::Multimedia::{mciGetErrorStringW, mciSendStringW},
    UI::Controls::Dialogs::*,
};

pub(super) fn call_engine(command: &str, payload: &Value) -> Result<Value, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    vitral_core::windows_host::request(&exe, "", command, payload).map_err(|e| e.to_string())
}
pub(super) fn probe_engine_raw(command: &str, payload: &Value) -> Result<Value, String> {
    vitral_core::windows_host::probe(command, payload).map_err(|e| e.to_string())
}
#[tauri::command]
pub(super) fn choose_file(lexicon: bool) -> Option<String> {
    let filter = if lexicon {
        "词库\0*.scel;*.dict.yaml;*.tsv;*.txt\0所有文件\0*.*\0\0"
    } else {
        "所有文件\0*.*\0\0"
    };
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let mut file = vec![0u16; 32768];
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as _,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: file.as_mut_ptr(),
        nMaxFile: file.len() as _,
        Flags: OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST,
        ..Default::default()
    };
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        return None;
    }
    String::from_utf16(&file[..file.iter().position(|&c| c == 0)?]).ok()
}
fn mci(command: &str) -> Result<(), String> {
    let cmd: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let rc = unsafe { mciSendStringW(cmd.as_ptr(), null_mut(), 0, null_mut()) };
    if rc == 0 {
        return Ok(());
    }
    let mut buf = [0u16; 256];
    unsafe {
        mciGetErrorStringW(rc, buf.as_mut_ptr(), buf.len() as _);
    }
    Err(String::from_utf16_lossy(
        &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
    ))
}
#[derive(Default)]
pub(super) struct Microphone(Mutex<bool>);
impl Drop for Microphone {
    fn drop(&mut self) {
        if *self.0.get_mut().unwrap_or_else(|e| e.into_inner()) {
            let _ = mci("close VitralCapture");
        }
    }
}
#[tauri::command]
pub(super) fn start_recording(microphone: State<Microphone>) -> Result<(), String> {
    let mut active = microphone.0.lock().map_err(|e| e.to_string())?;
    if *active {
        return Err("正在录音".into());
    }
    mci("open new type waveaudio alias VitralCapture")?;
    let run=mci("set VitralCapture time format milliseconds format tag pcm channels 1 samplespersec 16000 bitspersample 16 alignment 2 bytespersec 32000")
        .and_then(|_|mci("record VitralCapture"));
    if run.is_err() {
        let _ = mci("close VitralCapture");
    }
    *active = run.is_ok();
    run
}
#[tauri::command]
pub(super) async fn stop_recording(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mic = app.state::<Microphone>();
        let mut active = mic.0.lock().map_err(|e| e.to_string())?;
        if !*active {
            return Err("尚未开始录音".into());
        }
        let dir = tempfile::tempdir().map_err(|e| e.to_string());
        let result = dir.and_then(|dir| {
            let path = dir.path().join("capture.wav");
            mci("stop VitralCapture")?;
            mci(&format!("save VitralCapture \"{}\"", path.display()))?;
            fs::read(path).map_err(|e| e.to_string())
        });
        let _ = mci("close VitralCapture");
        *active = false;
        let wav = result?;
        if wav.get(..4) != Some(b"RIFF") || wav.get(8..12) != Some(b"WAVE") {
            return Err("录音格式错误".into());
        }
        let mut at = 12usize;
        while let Some(h) = wav.get(at..at + 8) {
            let n = u32::from_le_bytes(h[4..8].try_into().unwrap()) as usize;
            let data = wav.get(at + 8..at + 8 + n).ok_or("录音文件不完整")?;
            if &h[..4] == b"data" {
                call_engine("voice-start", &json!({}))?;
                for frame in data.chunks(32000) {
                    call_engine(
                        "voice-push-pcm",
                        &json!({"pcm_i16_b64":BASE64_STANDARD.encode(frame)}),
                    )?;
                }
                return call_engine("voice-stop", &json!({}));
            }
            at += 8 + n + (n % 2);
        }
        Err("录音缺少音频数据".into())
    })
    .await
    .map_err(|e| e.to_string())?
}
