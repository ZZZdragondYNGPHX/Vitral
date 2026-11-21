use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::ManuallyDrop,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    ptr,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        System::Com::*,
        UI::{Input::KeyboardAndMouse::*, TextServices::*},
    },
};

const CLASS: GUID = GUID::from_u128(0x7c9e6679_7425_40de_944b_e07fc1f90ae7);
static OBJECTS: AtomicU32 = AtomicU32::new(0);
static NEXT: AtomicU32 = AtomicU32::new(0);
struct Live;
impl Live {
    fn new() -> Self {
        OBJECTS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::Release);
    }
}
extern "C" {
    fn vitral_register(install: bool) -> HRESULT;
    fn vitral_module_path(path: *mut u16, size: u32) -> u32;
    fn vitral_panel_open(
        owner: *const c_void,
        callback: unsafe extern "C" fn(*const c_void, i32, i32, *const u16, *const f32, usize),
    ) -> *mut c_void;
    fn vitral_panel_close(panel: *mut c_void);
    fn vitral_panel_update(
        panel: *mut c_void,
        active: bool,
        english: bool,
        mode: i32,
        caret: *const RECT,
        words: *const *const u16,
        count: i32,
        selected: i32,
        glow: bool,
        activity: bool,
    );
}
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    unsafe { vitral_register(true) }
}
#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    unsafe { vitral_register(false) }
}
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    if clsid.is_null() || iid.is_null() {
        return E_POINTER;
    }
    if *clsid != CLASS {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let f: IClassFactory = Factory(Live::new()).into();
    f.query(iid, out)
}
#[implement(IClassFactory)]
struct Factory(Live);
impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Option<&IUnknown>,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if out.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *out = ptr::null_mut() }
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let tip: ITfTextInputProcessor = Service::new()?.into();
        unsafe { tip.query(iid, out).ok() }
    }
    fn LockServer(&self, lock: BOOL) -> Result<()> {
        if lock.as_bool() {
            OBJECTS.fetch_add(1, Ordering::Relaxed);
        } else {
            OBJECTS.fetch_sub(1, Ordering::Release);
        }
        Ok(())
    }
}
#[derive(Default)]
struct View {
    active: bool,
    english: bool,
    paste_id: String,
    mode: i32,
    caret: RECT,
    selected: usize,
    words: Vec<String>,
    values: Vec<String>,
    slots: Vec<Value>,
}
struct Editor {
    id: String,
    host: PathBuf,
    mgr: RefCell<Option<ITfThreadMgr>>,
    tid: Cell<u32>,
    cookie: Cell<Option<u32>>,
    composition: RefCell<Option<(ITfContext, ITfComposition)>>,
    panel: Cell<*mut c_void>,
    view: RefCell<View>,
    shift_held: Cell<bool>,
}
impl Editor {
    fn call(&self, name: &str, v: Value) -> Result<Value> {
        vitral_core::windows_host::request(&self.host, &self.id, name, &v)
            .map_err(|e| Error::new(E_FAIL, e.to_string()))
    }
    fn draw(&self, glow: bool, activity: bool) {
        let v = self.view.borrow();
        let strings: Vec<Vec<u16>> = v
            .words
            .iter()
            .map(|s| s.encode_utf16().chain(Some(0)).collect())
            .collect();
        let pointers: Vec<_> = strings.iter().map(|s| s.as_ptr()).collect();
        unsafe {
            vitral_panel_update(
                self.panel.get(),
                v.active,
                v.english,
                v.mode,
                &v.caret,
                pointers.as_ptr(),
                pointers.len() as _,
                v.selected as _,
                glow,
                activity,
            )
        }
    }
    fn on_ime_open(&self) -> Result<Value> {
        let app = std::env::current_exe().unwrap_or_default();
        self.call("on-ime-open", json!({"app_id":app}))
    }
    fn on_ime_close(&self) {
        let _ = self.call("on-ime-close", json!({}));
    }
    fn focus(&self, open: bool) {
        if self.view.borrow().active == open {
            return;
        }
        if open {
            let opened = self.on_ime_open();
            let mut v = self.view.borrow_mut();
            v.active = opened.is_ok();
            if let Ok(r) = opened {
                v.paste_id = r["quick_paste"]["id"].as_str().unwrap_or("").into();
                if let Ok(action) = self.call("candidate-state", json!({})) {
                    v.words = action["candidates"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| {
                            let text = c["text"].as_str().unwrap_or("");
                            if c["quick_paste_id"].is_string() {
                                format!("粘贴 {text}")
                            } else {
                                text.into()
                            }
                        })
                        .collect();
                }
                v.english = r["lang_mode"] == "en";
            }
        } else {
            self.shift_held.set(false);
            self.view.replace(View::default());
            self.on_ime_close();
            let old = self.composition.borrow_mut().take();
            if let Some((ctx, comp)) = old {
                let session: ITfEditSession = Finish {
                    comp,
                    _live: Live::new(),
                }
                .into();
                unsafe {
                    let _ = ctx.RequestEditSession(
                        self.tid.get(),
                        &session,
                        TF_ES_ASYNCDONTCARE | TF_ES_READWRITE,
                    );
                }
            }
        }
        self.draw(false, true);
    }
}
#[implement(ITfEditSession)]
struct Finish {
    comp: ITfComposition,
    _live: Live,
}
impl ITfEditSession_Impl for Finish_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            self.comp.GetRange()?.SetText(ec, 0, &[])?;
            self.comp.EndComposition(ec)
        }
    }
}
#[implement(
    ITfTextInputProcessor,
    ITfKeyEventSink,
    ITfCompositionSink,
    ITfThreadMgrEventSink
)]
struct Service {
    editor: Rc<Editor>,
    _live: Live,
}
impl Service {
    fn new() -> Result<Self> {
        let mut buf = vec![0u16; 32768];
        let n = unsafe { vitral_module_path(buf.as_mut_ptr(), buf.len() as _) };
        if n == 0 || n as usize >= buf.len() {
            return Err(Error::from_win32());
        }
        let mut host = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize]));
        host.set_file_name("vitral-desktop.exe");
        Ok(Self {
            editor: Rc::new(Editor {
                id: format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ),
                host,
                mgr: Default::default(),
                tid: Cell::new(0),
                cookie: Cell::new(None),
                composition: Default::default(),
                panel: Cell::new(ptr::null_mut()),
                view: Default::default(),
                shift_held: Cell::new(false),
            }),
            _live: Live::new(),
        })
    }
}
impl ITfTextInputProcessor_Impl for Service_Impl {
    fn Activate(&self, mgr: Option<&ITfThreadMgr>, tid: u32) -> Result<()> {
        self.Deactivate()?;
        let mgr = mgr.ok_or(E_POINTER)?;
        let keys: ITfKeystrokeMgr = mgr.cast()?;
        let source: ITfSource = mgr.cast()?;
        unsafe {
            keys.AdviseKeyEventSink(tid, &self.to_interface::<ITfKeyEventSink>(), true)?;
            let sink = self.to_interface::<ITfThreadMgrEventSink>();
            match source.AdviseSink(&ITfThreadMgrEventSink::IID, &sink.cast::<IUnknown>()?) {
                Ok(c) => self.editor.cookie.set(Some(c)),
                Err(e) => {
                    let _ = keys.UnadviseKeyEventSink(tid);
                    return Err(e);
                }
            }
            self.editor.tid.set(tid);
            self.editor.mgr.replace(Some(mgr.clone()));
            self.editor
                .panel
                .set(vitral_panel_open((self as *const Self).cast(), panel_event));
            if mgr.GetFocus().and_then(|d| d.GetTop()).is_ok() {
                self.editor.focus(true);
            }
        }
        Ok(())
    }
    fn Deactivate(&self) -> Result<()> {
        self.editor.focus(false);
        let mgr = self.editor.mgr.borrow_mut().take();
        if let Some(mgr) = mgr {
            unsafe {
                if let (Ok(s), Some(c)) = (mgr.cast::<ITfSource>(), self.editor.cookie.take()) {
                    let _ = s.UnadviseSink(c);
                }
                if let Ok(k) = mgr.cast::<ITfKeystrokeMgr>() {
                    let _ = k.UnadviseKeyEventSink(self.editor.tid.get());
                }
            }
        }
        unsafe {
            vitral_panel_close(self.editor.panel.replace(ptr::null_mut()));
        }
        Ok(())
    }
}
impl ITfThreadMgrEventSink_Impl for Service_Impl {
    fn OnInitDocumentMgr(&self, _: Option<&ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }
    fn OnUninitDocumentMgr(&self, _: Option<&ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }
    fn OnSetFocus(&self, doc: Option<&ITfDocumentMgr>, _: Option<&ITfDocumentMgr>) -> Result<()> {
        self.editor.focus(false);
        if doc.is_some() {
            self.editor.focus(true)
        }
        Ok(())
    }
    fn OnPushContext(&self, _: Option<&ITfContext>) -> Result<()> {
        self.editor.focus(false);
        self.editor.focus(true);
        Ok(())
    }
    fn OnPopContext(&self, _: Option<&ITfContext>) -> Result<()> {
        self.editor.focus(false);
        Ok(())
    }
}
impl ITfCompositionSink_Impl for Service_Impl {
    fn OnCompositionTerminated(&self, _: u32, comp: Option<&ITfComposition>) -> Result<()> {
        let matches = self
            .editor
            .composition
            .borrow()
            .as_ref()
            .is_some_and(|(_, c)| Some(c) == comp);
        if matches {
            self.editor.composition.replace(None);
            let _ = self.editor.call("process-key", json!({"keycode":"Escape"}));
            self.editor.view.borrow_mut().words.clear();
            self.editor.draw(false, true);
        }
        Ok(())
    }
}
struct Action {
    cmd: &'static str,
    args: Value,
}
impl Action {
    fn new(cmd: &'static str, args: Value) -> Self {
        Self { cmd, args }
    }
}
impl Service_Impl {
    fn shortcut_name(code: u32) -> Option<String> {
        let down = |key| unsafe { GetKeyState(key) < 0 };
        if down(0x5b) || down(0x5c) {
            return None;
        }
        if matches!(code, 0x10 | 0xa0 | 0xa1) {
            return (!down(0x11) && !down(0x12)).then(|| "Shift".into());
        }
        let base = if (0x70..=0x7b).contains(&code) {
            format!("F{}", code - 0x70 + 1)
        } else if down(0x11) || down(0x12) {
            match code {
                0x30..=0x39 | 0x41..=0x5a => char::from_u32(code)?.to_string(),
                32 => "Space".into(),
                13 => "Enter".into(),
                _ => return None,
            }
        } else {
            return None;
        };
        let mut parts = Vec::new();
        if down(0x11) {
            parts.push("Ctrl".to_string());
        }
        if down(0x12) {
            parts.push("Alt".to_string());
        }
        if down(0x10) {
            parts.push("Shift".to_string());
        }
        parts.push(base);
        Some(parts.join("+"))
    }
    fn key(&self, w: WPARAM, l: LPARAM) -> Option<Action> {
        let code = w.0 as u32;
        let down = |key| unsafe { GetKeyState(key) < 0 };
        let composing = self.editor.composition.borrow().is_some();
        let view = self.editor.view.borrow();
        if !view.active {
            return None;
        }
        if let Some(key) = Self::shortcut_name(code) {
            if self
                .editor
                .call("shortcut-match", json!({"key":key}))
                .ok()
                .is_some_and(|v| v.is_string())
            {
                return Some(Action::new(
                    if (l.0 & (1 << 30)) != 0 {
                        "shortcut-repeat"
                    } else {
                        "shortcut-key"
                    },
                    json!({"key":key}),
                ));
            }
        }
        if composing && (49..=53).contains(&code) && ((code - 49) as usize) < view.words.len() {
            let op = if down(0x11) {
                Some("pin-candidate")
            } else if down(0x12) {
                Some("erase-candidate")
            } else {
                None
            };
            if let Some(cmd) = op {
                return Some(Action::new(cmd, json!({"idx":code-49})));
            }
        }
        if view.english || down(0x11) || down(0x12) || down(0x5b) || down(0x5c) {
            return None;
        }
        if composing
            && (49..=53).contains(&code)
            && !down(0x10)
            && ((code - 49) as usize) < view.words.len()
        {
            return Some(Action::new("select-candidate", json!({"idx":code-49})));
        }
        if code == 32 && !view.words.is_empty() && !composing {
            return Some(Action::new("select-candidate", json!({"idx":0})));
        }
        if !view.words.is_empty() && view.mode == 0 && !down(0x10) {
            if code == 37 || code == 39 {
                return Some(Action::new(
                    "move-candidate",
                    json!({"delta":if code == 37 {-1} else {1}}),
                ));
            }
            if code == 38 || code == 40 {
                return Some(Action::new(
                    "process-key",
                    json!({"keycode":if code == 38 {"PageUp"} else {"PageDown"}}),
                ));
            }
            if code == 32 {
                return Some(Action::new(
                    "select-candidate",
                    json!({"idx":view.selected}),
                ));
            }
        }
        let key = match code {
            8 | 13 | 27 | 32 | 33 | 34 if composing => match code {
                8 => "BackSpace",
                13 => "Return",
                27 => "Escape",
                32 => "space",
                33 => "PageUp",
                _ => "PageDown",
            }
            .into(),
            0x30..=0x5a | 0xba..=0xe2 => {
                let mut state = [0; 256];
                let mut text = [0; 8];
                unsafe {
                    GetKeyboardState(&mut state).ok()?;
                    let n = ToUnicodeEx(
                        code,
                        ((l.0 >> 16) & 255) as u32,
                        &state,
                        &mut text,
                        4,
                        GetKeyboardLayout(0),
                    );
                    if n != 1 {
                        return None;
                    }
                }
                let c = char::from_u32(text[0] as u32)?;
                if !c.is_ascii_graphic() {
                    return None;
                }
                c.to_string()
            }
            _ => return None,
        };
        Some(Action::new("process-key", json!({"keycode":key})))
    }
    fn edit(&self, ctx: &ITfContext, action: Action) -> Result<bool> {
        let handled = Rc::new(Cell::new(false));
        let session: ITfEditSession = Edit {
            ctx: ctx.clone(),
            editor: self.editor.clone(),
            sink: self.to_interface::<ITfCompositionSink>(),
            action,
            handled: handled.clone(),
            _live: Live::new(),
        }
        .into();
        unsafe {
            ctx.RequestEditSession(
                self.editor.tid.get(),
                &session,
                TF_ES_SYNC | TF_ES_READWRITE,
            )?
            .ok()?;
        }
        Ok(handled.get())
    }
}
impl ITfKeyEventSink_Impl for Service_Impl {
    fn OnSetFocus(&self, fg: BOOL) -> Result<()> {
        self.editor.focus(fg.as_bool());
        Ok(())
    }
    fn OnTestKeyDown(&self, ctx: Option<&ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        if !matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1) {
            self.editor.shift_held.set(false);
        }
        Ok((ctx.is_some() && self.key(w, l).is_some()).into())
    }
    fn OnKeyDown(&self, ctx: Option<&ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        if matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1) {
            self.editor
                .shift_held
                .set(Self::shortcut_name(w.0 as u32).is_some());
            return Ok(false.into());
        }
        self.editor.shift_held.set(false);
        Ok(ctx
            .zip(self.key(w, l))
            .is_some_and(|(c, a)| self.edit(c, a).unwrap_or(false))
            .into())
    }
    fn OnTestKeyUp(&self, ctx: Option<&ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        Ok((ctx.is_some()
            && matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1)
            && self.editor.shift_held.get())
        .into())
    }
    fn OnKeyUp(&self, ctx: Option<&ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        if matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1)
            && self.editor.shift_held.replace(false)
            && Self::shortcut_name(w.0 as u32).is_some()
        {
            return Ok(ctx
                .is_some_and(|c| {
                    self.edit(c, Action::new("shortcut-key", json!({"key":"Shift"})))
                        .unwrap_or(false)
                })
                .into());
        }
        Ok(false.into())
    }
    fn OnPreservedKey(&self, _: Option<&ITfContext>, _: *const GUID) -> Result<BOOL> {
        Ok(false.into())
    }
}
#[implement(ITfEditSession)]
struct Edit {
    ctx: ITfContext,
    editor: Rc<Editor>,
    sink: ITfCompositionSink,
    action: Action,
    handled: Rc<Cell<bool>>,
    _live: Live,
}
impl ITfEditSession_Impl for Edit_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let ed = &self.editor;
        let a = &self.action;
        if a.cmd == "shortcut-repeat" {
            self.handled.set(true);
            return Ok(());
        }
        if a.cmd == "move-candidate" {
            let mut view = ed.view.borrow_mut();
            let delta = a.args["delta"].as_i64().unwrap_or(0);
            view.selected = (view.selected as i64 + delta)
                .clamp(0, view.words.len().saturating_sub(1) as i64)
                as usize;
            drop(view);
            ed.draw(false, true);
            self.handled.set(true);
            return Ok(());
        }
        let r = if a.cmd == "insert" {
            a.args.clone()
        } else {
            ed.call(a.cmd, a.args.clone())?
        };
        let mut commit = r["commit_text"]
            .as_str()
            .or(r["consumed"]["content"].as_str());
        if a.cmd == "hw-confirm" {
            commit = r["text"].as_str().or(commit);
        }
        let pre = r["preedit"]["text"]
            .as_str()
            .or(r["composing_text"].as_str())
            .unwrap_or("");
        let old = ed.composition.borrow().clone();
        if old.as_ref().is_some_and(|(ctx, _)| ctx != &self.ctx) {
            return Err(E_UNEXPECTED.into());
        }
        let edits_text = old.is_some() || !pre.is_empty() || commit.is_some();
        let toggle = a.args["modifiers"]
            .as_array()
            .is_some_and(|m| m.iter().any(|v| v == "lang_toggle"));
        if (a.cmd == "process-key" || a.cmd == "shortcut-key")
            && !edits_text
            && !toggle
            && !r["function_action"].is_string()
        {
            return Ok(());
        }
        if edits_text {
            unsafe {
                let insert: ITfInsertAtSelection = self.ctx.cast()?;
                let (range, composition) = if let Some((_, comp)) = old {
                    (comp.GetRange()?, Some(comp))
                } else if commit.is_none() {
                    let range = insert.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])?;
                    let owner: ITfContextComposition = self.ctx.cast()?;
                    let comp = owner.StartComposition(ec, &range, &self.sink)?;
                    ed.composition
                        .replace(Some((self.ctx.clone(), comp.clone())));
                    (range, Some(comp))
                } else {
                    (
                        insert.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])?,
                        None,
                    )
                };
                let text = commit.unwrap_or(pre);
                let wide: Vec<u16> = text.encode_utf16().collect();
                range.SetText(ec, 0, &wide)?;
                let end = range.Clone()?;
                end.Collapse(ec, TF_ANCHOR_END)?;
                if let Ok(view) = self.ctx.GetActiveView() {
                    let mut rect = RECT::default();
                    let mut clipped = BOOL(0);
                    if view.GetTextExt(ec, &end, &mut rect, &mut clipped).is_ok() {
                        ed.view.borrow_mut().caret = rect;
                    }
                }
                let delta = r["cursor_delta"].as_i64().unwrap_or(0) as i32;
                if delta < 0 {
                    let mut moved = 0;
                    end.ShiftStart(ec, delta, &mut moved, ptr::null())?;
                    end.Collapse(ec, TF_ANCHOR_START)?;
                }
                let mut selection = TF_SELECTION {
                    range: ManuallyDrop::new(Some(end)),
                    style: TF_SELECTIONSTYLE {
                        ase: TF_AE_NONE,
                        fInterimChar: false.into(),
                    },
                };
                let sel = self.ctx.SetSelection(ec, std::slice::from_ref(&selection));
                ManuallyDrop::drop(&mut selection.range);
                sel?;
                if (commit.is_some() || pre.is_empty()) && composition.is_some() {
                    ed.composition.replace(None);
                    composition.unwrap().EndComposition(ec)?;
                }
            }
        }
        let mut v = ed.view.borrow_mut();
        if let Some(lang) = r["lang_mode"].as_str() {
            v.english = lang == "en";
        }
        if a.cmd == "hw-confirm" {
            v.mode = 0;
        }
        v.selected = 0;
        v.words.clear();
        v.values.clear();
        v.slots.clear();
        if let Some(candidates) = r["candidates"].as_array() {
            for c in candidates.iter().take(5) {
                let text = c["text"].as_str().or(c["char"].as_str()).unwrap_or("");
                v.values.push(text.into());
                v.words.push(if c["quick_paste_id"].is_string() {
                    format!("粘贴 {text}")
                } else if let Some(py) = c["toned_pinyin"].as_str() {
                    format!(
                        "{} {} {}",
                        text,
                        py,
                        c["xiaohe_code"].as_str().unwrap_or("")
                    )
                } else {
                    text.into()
                });
            }
        }
        let mut glow = false;
        if let Some(slots) = r["slots"].as_array() {
            v.slots = slots.clone();
            for slot in slots.iter().take(5) {
                v.words.push(slot["char"].as_str().unwrap_or("").into());
            }
            glow = slots.iter().any(|s| s["changed_retroactively"] == true);
        }
        let function = r["function_action"].as_str().unwrap_or("");
        if function == "lookup" || function == "handwriting" {
            v.mode = if function == "lookup" { 1 } else { 2 };
        }
        drop(v);
        match function {
            "settings" => {
                let _ = std::process::Command::new(&ed.host).spawn();
            }
            "clipboard" => {
                let _ = std::process::Command::new(&ed.host)
                    .arg("--clipboard")
                    .spawn();
            }
            "lookup" | "handwriting" => {
                ed.call("hw-begin", json!({"mode":if function == "lookup" {"PhoneticLookup"} else {"ContinuousLattice"}}))?;
            }
            _ => {}
        }
        ed.draw(glow, true);
        self.handled.set(true);
        Ok(())
    }
}
unsafe extern "C" fn panel_event(
    owner: *const c_void,
    op: i32,
    index: i32,
    _text: *const u16,
    points: *const f32,
    n: usize,
) {
    let service = &*owner.cast::<Service_Impl>();
    let _live: ITfTextInputProcessor = service.to_interface();
    let ed = &service.editor;
    let mode = ed.view.borrow().mode;
    let action = match op {
        0 if mode == 1 => {
            let word = ed.view.borrow().values.get(index as usize).cloned();
            let Some(word) = word else { return };
            let _ = ed.call("hw-clear", json!({}));
            Action::new("insert", json!({"commit_text":word}))
        }
        0 if mode == 2 => {
            let slots = ed.view.borrow();
            let Some(s) = slots.slots.get(index as usize) else {
                return;
            };
            let Some(alts) = s["top8"].as_array() else {
                return;
            };
            if alts.is_empty() {
                return;
            }
            let current = alts
                .iter()
                .position(|item| item["char"] == s["char"])
                .unwrap_or(0);
            Action::new(
                "hw-override-slot",
                json!({"slot_idx":index,"candidate_idx":(current+1)%alts.len()}),
            )
        }
        0 => Action::new("select-candidate", json!({"idx":index})),
        1 => Action::new(
            "process-key",
            json!({"keycode":"","modifiers":["lang_toggle"]}),
        ),
        3 if index == 5 => {
            let _ = std::process::Command::new(&ed.host).spawn();
            return;
        }
        3 if index == 1 || index == 2 => {
            ed.view.borrow_mut().mode = index;
            Action::new(
                "hw-begin",
                json!({"mode":if index==1{"PhoneticLookup"}else{"ContinuousLattice"}}),
            )
        }
        3 if index == 3 => Action::new("hw-confirm", json!({})),
        3 if index == 4 => {
            ed.view.borrow_mut().mode = 0;
            Action::new("hw-clear", json!({}))
        }
        4 if !points.is_null() => {
            let pts: Vec<_> = std::slice::from_raw_parts(points, n * 3)
                .chunks_exact(3)
                .map(|p| json!({"x":p[0],"y":p[1],"timestamp_ms":p[2] as u64}))
                .collect();
            Action::new("hw-push-stroke", json!({"points":pts,"pen_up":true}))
        }
        10 => {
            let mut view = ed.view.borrow_mut();
            if index >= 0 && (index as usize) < view.words.len() {
                view.selected = index as usize;
            }
            drop(view);
            ed.draw(false, false);
            return;
        }
        8 => {
            let Ok(r) = ed.call("quick-paste-state", json!({})) else {
                return;
            };
            let id = r["quick_paste"]["id"].as_str().unwrap_or("");
            let mut view = ed.view.borrow_mut();
            if view.paste_id == id || view.mode != 0 {
                return;
            }
            view.paste_id = id.into();
            drop(view);
            Action::new("candidate-state", json!({}))
        }
        _ => return,
    };
    let mgr = ed.mgr.borrow().clone();
    if let Some(ctx) = mgr
        .and_then(|m| m.GetFocus().ok())
        .and_then(|d| d.GetTop().ok())
    {
        let _ = service.edit(&ctx, action);
    }
}
