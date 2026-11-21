//! One system clipboard bridge per engine host, independent of focused TSF editors.
use crate::{clip::ClipboardPayload, ret::RetentionEngine, VitralCore};
use std::{ptr::null_mut, sync::Arc, time::Duration};
use windows_sys::Win32::{
    Foundation::*,
    System::{DataExchange::*, Memory::*},
    UI::WindowsAndMessaging::*,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
unsafe fn read_text(format: u32) -> Option<String> {
    let handle = GetClipboardData(format);
    if handle.is_null() {
        return None;
    }
    let data = GlobalLock(handle).cast::<u16>();
    if data.is_null() {
        return None;
    }
    let words = std::slice::from_raw_parts(data, GlobalSize(handle) / 2);
    let result = String::from_utf16_lossy(
        &words[..words.iter().position(|&c| c == 0).unwrap_or(words.len())],
    );
    GlobalUnlock(handle);
    Some(result)
}
unsafe fn allocate(text: &str) -> HGLOBAL {
    let words = wide(text);
    let handle = GlobalAlloc(GMEM_MOVEABLE, words.len() * 2);
    if handle.is_null() {
        return handle;
    }
    let data = GlobalLock(handle).cast::<u16>();
    if data.is_null() {
        GlobalFree(handle);
        return null_mut();
    }
    std::ptr::copy_nonoverlapping(words.as_ptr(), data, words.len());
    GlobalUnlock(handle);
    handle
}

pub(super) fn start(core: Arc<VitralCore>) {
    std::thread::spawn(move || unsafe {
        let window = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide("Vitral Clipboard").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null_mut(),
        );
        if window.is_null() {
            return;
        }
        let marker = RegisterClipboardFormatW(wide("Vitral.CopyId").as_ptr());
        let mut sequence = None;
        let mut applied = String::new();
        let mut local_changed_at = 0;
        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if OpenClipboard(window) != 0 {
                let revision = GetClipboardSequenceNumber();
                let text = read_text(13); // CF_UNICODETEXT
                let own = read_text(marker);
                let current = core.quick_paste_state().current_copy;
                if sequence != Some(revision) && own.is_none() {
                    if sequence.is_some() || (text.is_none() && CountClipboardFormats() > 0) {
                        local_changed_at = RetentionEngine::now_epoch_ms();
                    }
                    if let Some(text) = text.as_ref().filter(|t| !t.is_empty()) {
                        let initial_same = sequence.is_none()
                            && current.as_ref().is_some_and(|c| c.content == *text);
                        if !initial_same {
                            core.clipboard_push_local(ClipboardPayload {
                                id: None,
                                content: text.clone(),
                                mime_type: "text/plain".into(),
                                source_device: String::new(),
                                timestamp_ms: RetentionEngine::now_epoch_ms(),
                            });
                        }
                    }
                }
                if let Some(current) = core
                    .quick_paste_state()
                    .current_copy
                    .filter(|c| c.id != applied && c.timestamp_ms >= local_changed_at)
                {
                    if text.as_deref() == Some(&current.content) {
                        applied = current.id;
                    } else {
                        let data = allocate(&current.content);
                        let id = allocate(&current.id);
                        if !data.is_null() && !id.is_null() && EmptyClipboard() != 0 {
                            if !SetClipboardData(13, data).is_null() {
                                applied = current.id;
                                if SetClipboardData(marker, id).is_null() {
                                    GlobalFree(id);
                                }
                            } else {
                                GlobalFree(data);
                                GlobalFree(id);
                            }
                        } else {
                            if !data.is_null() {
                                GlobalFree(data);
                            }
                            if !id.is_null() {
                                GlobalFree(id);
                            }
                        }
                    }
                }
                sequence = Some(GetClipboardSequenceNumber());
                CloseClipboard();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}
