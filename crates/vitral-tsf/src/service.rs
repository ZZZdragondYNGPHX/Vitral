use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    ffi::c_void,
    mem::ManuallyDrop,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    ptr,
    rc::{Rc, Weak},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};
use vitral_core::windows_host::PipeClient;
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        System::Com::*,
        UI::{
            Input::KeyboardAndMouse::*,
            TextServices::*,
            WindowsAndMessaging::{KillTimer, SetTimer},
        },
    },
};

const CLASS: GUID = GUID::from_u128(0x7c9e6679_7425_40de_944b_e07fc1f90ae7);
const INTERACTIVE_IPC_TIMEOUT: Duration = Duration::from_millis(150);
const IPC_BACKOFF: Duration = Duration::from_millis(1500);

fn ipc_in_backoff(until: Option<Instant>, now: Instant) -> bool {
    until.is_some_and(|until| now < until)
}

fn should_update_focus(active: bool, open: bool, needs_refocus: bool) -> bool {
    active != open || (open && needs_refocus)
}

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
#[derive(Clone, Copy)]
#[repr(C)]
struct PanelText {
    data: *const u8,
    len: usize,
}
impl From<&str> for PanelText {
    fn from(text: &str) -> Self {
        Self {
            data: text.as_ptr(),
            len: text.len(),
        }
    }
}
extern "C" {
    fn vitral_physical_caret(hwnd: HWND, caret: *mut RECT);
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
        preedit: PanelText,
        page: u64,
        caret: *const RECT,
        words: *const PanelText,
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
    prediction_id: u64,
    prediction_text: String,
    context_before: String,
    mode: i32,
    preedit: String,
    page: u64,
    caret: RECT,
    selected: usize,
    words: Vec<String>,
    values: Vec<String>,
    slots: Vec<Value>,
}
struct Editor {
    id: String,
    host: PathBuf,
    pipe: RefCell<PipeClient>,
    mgr: RefCell<Option<ITfThreadMgr>>,
    tid: Cell<u32>,
    cookie: Cell<Option<u32>>,
    composition: RefCell<Option<(ITfContext, ITfComposition)>>,
    commit_end: RefCell<Option<(ITfContext, ITfRange)>>,
    prediction_anchor: RefCell<Option<(ITfContext, ITfRange)>>,
    panel: Cell<*mut c_void>,
    view: RefCell<View>,
    shift_held: Cell<bool>,
    shortcuts: RefCell<Vec<String>>,
    ipc_failed: Cell<bool>,
    ipc_backoff_until: Cell<Option<Instant>>,
    needs_refocus: Cell<bool>,
    prediction_timer: Cell<usize>,
    prediction_polls: Cell<u32>,
    prediction_trigger: Cell<bool>,
}
thread_local! {
    static PREDICTION_TIMERS: RefCell<BTreeMap<usize, Weak<Editor>>> = const { RefCell::new(BTreeMap::new()) };
}
unsafe extern "system" fn prediction_tick(_: HWND, _: u32, timer: usize, _: u32) {
    let owner = PREDICTION_TIMERS.with(|timers| timers.borrow_mut().remove(&timer));
    let _ = KillTimer(None, timer);
    if let Some(editor) = owner.and_then(|owner| owner.upgrade()) {
        editor.prediction_timer.set(0);
        editor.poll_prediction();
    }
}
impl Drop for Editor {
    fn drop(&mut self) {
        let timer = self.prediction_timer.replace(0);
        if timer != 0 {
            unsafe {
                let _ = KillTimer(None, timer);
            }
            let _ = PREDICTION_TIMERS.try_with(|timers| {
                timers.borrow_mut().remove(&timer);
            });
        }
    }
}
impl Editor {
    fn bypass_keys(&self) -> bool {
        let bypass = ipc_in_backoff(self.ipc_backoff_until.get(), Instant::now());
        if bypass {
            self.shift_held.set(false);
        } else {
            self.ipc_failed.set(false);
        }
        bypass
    }

    fn call(&self, name: &str, v: Value) -> Result<Value> {
        if ipc_in_backoff(self.ipc_backoff_until.get(), Instant::now()) {
            self.ipc_failed.set(true);
            return Err(Error::new(E_FAIL, "Vitral IPC is cooling down"));
        }
        self.ipc_backoff_until.set(None);
        let timeout = match name {
            "hw-push-stroke" | "hw-confirm" => Duration::from_millis(1500),
            _ => INTERACTIVE_IPC_TIMEOUT,
        };
        let result = self.pipe.borrow_mut().request_with_timeout(
            &self.host, &self.id, name, &v, timeout,
        );
        result.map_err(|error| {
            self.ipc_failed.set(true);
            if error.is::<std::io::Error>() {
                self.ipc_backoff_until.set(Some(Instant::now() + IPC_BACKOFF));
                self.needs_refocus.set(true);
            }
            let message = error.to_string();
            // PipeClient may reconnect before the host has registered this editor.
            if message == "editor focus changed" {
                self.needs_refocus.set(true);
            }
            Error::new(E_FAIL, message)
        })
    }
    fn draw(&self, glow: bool, activity: bool) {
        let v = self.view.borrow();
        let prediction =
            (!v.prediction_text.is_empty() && self.composition.borrow().is_none() && v.mode == 0)
                .then(|| format!("Tab 补全：{}", v.prediction_text));
        let words = prediction
            .as_ref()
            .map_or(v.words.as_slice(), std::slice::from_ref);
        let mut text = [PanelText::from(""); 5];
        for (dest, word) in text.iter_mut().zip(words) {
            *dest = PanelText::from(word.as_str());
        }
        unsafe {
            vitral_panel_update(
                self.panel.get(),
                v.active,
                v.english,
                v.mode,
                PanelText::from(v.preedit.as_str()),
                v.page,
                &v.caret,
                text.as_ptr(),
                words.len().min(text.len()) as _,
                v.selected as _,
                glow,
                activity,
            )
        }
    }
    fn clear_prediction(&self) -> (u64, String) {
        self.prediction_anchor.replace(None);
        let timer = self.prediction_timer.replace(0);
        if timer != 0 {
            unsafe {
                let _ = KillTimer(None, timer);
            }
            PREDICTION_TIMERS.with(|timers| {
                timers.borrow_mut().remove(&timer);
            });
        }
        let mut v = self.view.borrow_mut();
        let id = std::mem::take(&mut v.prediction_id);
        let text = std::mem::take(&mut v.prediction_text);
        (id, text)
    }
    fn cancel_prediction(&self, dismissed: bool) {
        let (id, text) = self.clear_prediction();
        let visible = !text.is_empty();
        if id != 0 {
            let payload = if dismissed && visible {
                json!({"id": id, "text": text})
            } else {
                json!({"id": id})
            };
            let _ = self.call(
                if dismissed {
                    "predict-dismiss"
                } else {
                    "predict-cancel"
                },
                payload,
            );
        }
        if self.ipc_failed.get() {
            self.recover_ipc_failure();
        } else if visible {
            self.draw(false, true);
        }
    }
    fn recover_ipc_failure(&self) {
        self.commit_end.take();
        if self.composition.borrow().is_none() {
            self.reset_local();
            return;
        }
        self.clear_prediction();
        self.pipe.take();
        self.shift_held.set(false);
    }
    fn reset_local(&self) {
        self.commit_end.take();
        self.clear_prediction();
        self.pipe.take();
        self.shift_held.set(false);
        self.view.replace(View::default());
        self.shortcuts.borrow_mut().clear();
        let old = self.composition.borrow_mut().take();
        if let Some((ctx, comp)) = old {
            let session: ITfEditSession = Finish {
                comp,
                clear_text: true,
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
        self.draw(false, true);
    }
    fn finish_composition_after_reconnect(&self) -> Result<()> {
        let Some((ctx, comp)) = self.composition.take() else {
            return Ok(());
        };
        let session: ITfEditSession = Finish {
            comp: comp.clone(),
            clear_text: false,
            _live: Live::new(),
        }
        .into();
        let result = unsafe {
            ctx.RequestEditSession(self.tid.get(), &session, TF_ES_SYNC | TF_ES_READWRITE)
                .and_then(|hr| hr.ok())
        };
        if result.is_err() {
            self.composition.replace(Some((ctx, comp)));
        }
        result
    }
    fn current_context(&self) -> Option<ITfContext> {
        unsafe { self.mgr.borrow().as_ref()?.GetFocus().ok()?.GetTop().ok() }
    }
    fn input_access(&self, ctx: Option<&ITfContext>) -> InputAccess {
        let Some(ctx) = ctx else {
            return InputAccess::Blocked;
        };
        if input_disabled(ctx) {
            return InputAccess::Blocked;
        }
        let access = Rc::new(Cell::new(InputAccess::Unknown));
        let session: ITfEditSession = InputScopeCheck {
            ctx: ctx.clone(),
            access: access.clone(),
            _live: Live::new(),
        }
        .into();
        let result = unsafe { ctx.RequestEditSession(self.tid.get(), &session, TF_ES_SYNC | TF_ES_READ) };
        if result.is_ok_and(|hr| hr.is_ok()) {
            access.get()
        } else {
            InputAccess::Unknown
        }
    }
    fn clear_input(&self, edit: Option<(&ITfContext, u32)>) {
        let composing = self.composition.borrow().is_some();
        if let Some((ctx, ec)) = edit {
            if self.composition.borrow().as_ref().is_some_and(|(owner, _)| owner == ctx) {
                let composition = self.composition.borrow_mut().take();
                if let Some((_, comp)) = composition {
                    unsafe {
                        let _ = comp.GetRange().and_then(|range| range.SetText(ec, 0, &[]));
                        let _ = comp.EndComposition(ec);
                    }
                }
            }
        }
        self.cancel_prediction(false);
        if composing && !self.ipc_failed.get() {
            let _ = self.call("process-key", json!({"keycode":"Escape"}));
        }
        self.reset_local();
    }
    fn protect_input(&self, ctx: Option<&ITfContext>, composing_ok: bool) -> bool {
        let access = self.input_access(ctx);
        let composing = composing_ok && self.composition.borrow().as_ref().is_some_and(
            |(owner, _)| ctx == Some(owner),
        );
        if access.permits_key(composing) {
            return true;
        }
        if access == InputAccess::Blocked {
            self.clear_input(None);
        } else {
            self.shift_held.set(false);
            self.cancel_prediction(false);
        }
        false
    }
    fn arm_prediction(self: &Rc<Self>, delay: u32) {
        let timer = unsafe { SetTimer(None, 0, delay, Some(prediction_tick)) };
        if timer != 0 {
            self.prediction_timer.set(timer);
            PREDICTION_TIMERS.with(|timers| {
                timers.borrow_mut().insert(timer, Rc::downgrade(self));
            });
        }
    }
    fn schedule_prediction(
        self: &Rc<Self>,
        catch_up: bool,
        anchor: Option<(ITfContext, ITfRange)>,
    ) {
        self.cancel_prediction(false);
        if !self.view.borrow().active
            || self.composition.borrow().is_some()
            || self.view.borrow().mode != 0
            || self.view.borrow().context_before.trim().is_empty()
        {
            return;
        }
        self.prediction_anchor.replace(anchor);
        self.prediction_polls.set(0);
        self.prediction_trigger.set(catch_up);
        let Ok(state) = self.call("profile-state", json!({})) else {
            self.recover_ipc_failure();
            return;
        };
        if state["prediction_enabled"] == false {
            return;
        }
        let delay = if catch_up {
            1
        } else {
            state["prediction_delay_ms"]
                .as_u64()
                .unwrap_or(380)
                .clamp(150, 2000) as u32
        };
        self.arm_prediction(delay);
    }
    fn poll_prediction(self: &Rc<Self>) {
        if self.bypass_keys() || !self.protect_input(self.current_context().as_ref(), false) {
            return;
        }
        if !self.view.borrow().active
            || self.composition.borrow().is_some()
            || self.view.borrow().mode != 0
            || self.view.borrow().context_before.trim().is_empty()
        {
            return;
        }
        let polls = self.prediction_polls.get();
        let r = if polls == 0 {
            let app = std::env::current_exe()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let context = self.view.borrow().context_before.clone();
            self.call("predict-request", json!({"app_id":app,"context_before":context,"trigger":if self.prediction_trigger.get() {"catch_up"} else {"live"},"is_composing":false,"is_password":false}))
        } else {
            self.call(
                "predict-poll",
                json!({"id":self.view.borrow().prediction_id}),
            )
        };
        let Ok(r) = r else {
            self.recover_ipc_failure();
            return;
        };
        self.view.borrow_mut().prediction_id = r["id"].as_u64().unwrap_or(0);
        let upgrading = r["upgrading"].as_bool().unwrap_or(false);
        if r["status"] == "pending" {
            if polls >= 6 {
                self.cancel_prediction(false);
                return;
            }
            self.prediction_polls.set(polls + 1);
            self.arm_prediction(100);
        } else if r["status"] == "ready" {
            self.view.borrow_mut().prediction_text = r["text"].as_str().unwrap_or("").into();
            self.draw(false, false);
            if upgrading && polls < 5 {
                self.prediction_polls.set(polls + 1);
                self.arm_prediction(100);
            }
        } else {
            self.cancel_prediction(false);
        }
    }
    fn refresh_shortcuts(&self) -> Result<()> {
        let state = self.call("shortcuts-state", json!({}))?;
        self.shortcuts.replace(shortcut_bindings(&state));
        Ok(())
    }
    fn on_ime_open(&self) -> Result<Value> {
        let app = std::env::current_exe().unwrap_or_default();
        self.call("on-ime-open", json!({"app_id":app}))
    }
    fn on_ime_close(&self) {
        let _ = self.call("on-ime-close", json!({}));
    }
    fn focus(self: &Rc<Self>, open: bool) {
        if !should_update_focus(self.view.borrow().active, open, self.needs_refocus.get()) {
            return;
        }
        if open {
            if self.bypass_keys() || !self.protect_input(self.current_context().as_ref(), true) {
                return;
            }
            self.commit_end.take();
            let opened = self.on_ime_open().and_then(|r| {
                self.call("process-key", json!({"keycode":"Escape"}))?;
                self.finish_composition_after_reconnect()?;
                self.refresh_shortcuts()?;
                Ok(r)
            });
            self.needs_refocus.set(opened.is_err());
            let mut v = self.view.borrow_mut();
            v.active = opened.is_ok();
            if let Ok(r) = opened {
                if let Ok(action) = self.call("candidate-state", json!({})) {
                    v.preedit = action["preedit"]["text"].as_str().unwrap_or("").into();
                    v.page = action["page"]["page_index"].as_u64().unwrap_or(0);
                    v.words = action["candidates"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| c["text"].as_str().unwrap_or("").into())
                        .collect();
                }
                v.english = r["lang_mode"] == "en";
            }
            drop(v);
            if self.ipc_failed.get() {
                self.recover_ipc_failure();
                return;
            }
            let mut catch_up_anchor = None;
            if self.view.borrow().active && self.composition.borrow().is_none() {
                if let Some(ctx) = self.current_context().filter(|c| !input_disabled(c)) {
                    let out = Rc::new(RefCell::new(None));
                    let session: ITfEditSession = FocusContextCheck {
                        ctx: ctx.clone(),
                        out: out.clone(),
                        _live: Live::new(),
                    }
                    .into();
                    let ok = unsafe {
                        ctx.RequestEditSession(self.tid.get(), &session, TF_ES_SYNC | TF_ES_READ)
                    };
                    if ok.is_ok_and(|hr| hr.is_ok()) {
                        if let Some((text, anchor)) = out.borrow_mut().take() {
                            self.view.borrow_mut().context_before = text;
                            catch_up_anchor = Some((ctx, anchor));
                        }
                    }
                }
            }
            self.schedule_prediction(true, catch_up_anchor);
        } else {
            self.cancel_prediction(false);
            self.on_ime_close();
            self.reset_local();
        }
        self.draw(false, true);
    }
}
fn shortcut_bindings(state: &Value) -> Vec<String> {
    state["bindings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|binding| binding["key"].as_str())
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAccess {
    Allowed,
    Blocked,
    Unknown,
}
impl InputAccess {
    fn permits_key(self, composing: bool) -> bool {
        self == Self::Allowed || (self == Self::Unknown && composing)
    }
}
fn input_disabled(ctx: &ITfContext) -> bool {
    let Ok(mgr) = ctx.cast::<ITfCompartmentMgr>() else {
        return false;
    };
    [
        GUID_COMPARTMENT_KEYBOARD_DISABLED,
        GUID_COMPARTMENT_EMPTYCONTEXT,
    ]
    .iter()
    .any(|guid| unsafe {
        mgr.GetCompartment(guid)
            .and_then(|c| c.GetValue())
            .ok()
            .and_then(|v| i32::try_from(&v).ok())
            .is_some_and(|v| v != 0)
    })
}
fn classify_scopes(scopes: &[InputScope]) -> InputAccess {
    if scopes.iter().any(|s| matches!(*s, IS_PASSWORD | IS_NUMERIC_PIN | IS_ALPHANUMERIC_PIN)) {
        InputAccess::Blocked
    } else {
        InputAccess::Allowed
    }
}
// App input-scope properties require a read edit cookie for the current selection.
fn read_input_scope(ctx: &ITfContext, ec: u32) -> Result<InputAccess> {
    unsafe {
        let property = ctx.GetAppProperty(&GUID_PROP_INPUTSCOPE)?;
        let mut selection = [TF_SELECTION::default()];
        let mut fetched = 0;
        let selected = ctx.GetSelection(ec, TF_DEFAULT_SELECTION, &mut selection, &mut fetched);
        let range = ManuallyDrop::take(&mut selection[0].range);
        selected?;
        let Some(range) = range.filter(|_| fetched != 0) else {
            return Ok(InputAccess::Unknown);
        };
        let value = property.GetValue(ec, &range)?;
        if value.is_empty() {
            return Ok(InputAccess::Allowed);
        }
        let scope = IUnknown::try_from(&value)?.cast::<ITfInputScope>()?;
        let mut scopes = ptr::null_mut();
        let mut count = 0;
        let result = scope.GetInputScopes(&mut scopes, &mut count);
        let access = if result.is_err() || (count != 0 && scopes.is_null()) {
            InputAccess::Unknown
        } else if count == 0 {
            InputAccess::Allowed
        } else {
            classify_scopes(std::slice::from_raw_parts(scopes, count as usize))
        };
        CoTaskMemFree(Some(scopes.cast()));
        result?;
        Ok(access)
    }
}
#[implement(ITfEditSession)]
struct InputScopeCheck {
    ctx: ITfContext,
    access: Rc<Cell<InputAccess>>,
    _live: Live,
}
impl ITfEditSession_Impl for InputScopeCheck_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        self.access.set(read_input_scope(&self.ctx, ec)?);
        Ok(())
    }
}
fn read_focus_context(ctx: &ITfContext, ec: u32) -> Result<Option<(String, ITfRange)>> {
    if read_input_scope(ctx, ec)? != InputAccess::Allowed {
        return Ok(None);
    }
    unsafe {
        let mut selection = [TF_SELECTION::default()];
        let mut fetched = 0;
        let selected = ctx.GetSelection(ec, TF_DEFAULT_SELECTION, &mut selection, &mut fetched);
        let range = ManuallyDrop::take(&mut selection[0].range);
        selected?;
        let Some(range) = range.filter(|_| fetched == 1) else {
            return Ok(None);
        };
        if selection[0].style.fInterimChar.as_bool() || !range.IsEmpty(ec)?.as_bool() {
            return Ok(None);
        }
        let probe = range.Clone()?;
        let mut shifted = 0;
        probe.ShiftStart(ec, -160, &mut shifted, ptr::null())?;
        if shifted >= 0 {
            return Ok(None);
        }
        let len = (-shifted) as usize;
        let mut buf = vec![0u16; len + 1];
        let mut read = 0;
        probe.GetText(ec, 0, &mut buf, &mut read)?;
        let mut units = &buf[..(read as usize).min(len)];
        if units.first().is_some_and(|u| (0xDC00..=0xDFFF).contains(u)) {
            units = &units[1..];
        }
        let text = String::from_utf16_lossy(units);
        if text.trim().is_empty() {
            return Ok(None);
        }
        let anchor = range.Clone()?;
        anchor.SetGravity(ec, TF_GRAVITY_BACKWARD, TF_GRAVITY_BACKWARD)?;
        Ok(Some((text, anchor)))
    }
}
#[implement(ITfEditSession)]
struct FocusContextCheck {
    ctx: ITfContext,
    out: Rc<RefCell<Option<(String, ITfRange)>>>,
    _live: Live,
}
impl ITfEditSession_Impl for FocusContextCheck_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        *self.out.borrow_mut() = read_focus_context(&self.ctx, ec)?;
        Ok(())
    }
}
#[implement(ITfEditSession)]
struct Finish {
    comp: ITfComposition,
    clear_text: bool,
    _live: Live,
}
impl ITfEditSession_Impl for Finish_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            if self.clear_text {
                self.comp.GetRange()?.SetText(ec, 0, &[])?;
            }
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
                pipe: Default::default(),
                mgr: Default::default(),
                tid: Cell::new(0),
                cookie: Cell::new(None),
                composition: Default::default(),
                commit_end: Default::default(),
                prediction_anchor: Default::default(),
                panel: Cell::new(ptr::null_mut()),
                view: Default::default(),
                shift_held: Cell::new(false),
                shortcuts: Default::default(),
                ipc_failed: Cell::new(false),
                ipc_backoff_until: Cell::new(None),
                needs_refocus: Cell::new(false),
                prediction_timer: Cell::new(0),
                prediction_polls: Cell::new(0),
                prediction_trigger: Cell::new(false),
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
            if self
                .editor
                .call("process-key", json!({"keycode":"Escape"}))
                .is_err()
            {
                self.editor.reset_local();
                return Ok(());
            }
            let mut view = self.editor.view.borrow_mut();
            view.preedit.clear();
            view.page = 0;
            view.words.clear();
            drop(view);
            self.editor.draw(false, true);
        }
        Ok(())
    }
}
struct Action {
    cmd: &'static str,
    args: Value,
    anchor_commit: bool,
}
impl Action {
    fn new(cmd: &'static str, args: Value) -> Self {
        Self { cmd, args, anchor_commit: cmd == "select-candidate" }
    }

    fn composing_digit(code: u32, composing: bool, shift: bool) -> Option<Self> {
        let digit = char::from_u32(code)?;
        if !composing || shift || !('1'..='5').contains(&digit) {
            return None;
        }
        let mut action = Self::new("process-key", json!({"keycode":digit.to_string()}));
        action.anchor_commit = true;
        Some(action)
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
        if !composing
            && !view.prediction_text.is_empty()
            && !down(0x10)
            && !down(0x11)
            && !down(0x12)
            && !down(0x5b)
            && !down(0x5c)
        {
            if code == u32::from(VK_TAB.0) {
                return Some(Action::new(
                    "prediction-accept",
                    json!({"id":view.prediction_id,"text":view.prediction_text}),
                ));
            }
            if code == 27 {
                return Some(Action::new(
                    "prediction-dismiss",
                    json!({"id":view.prediction_id}),
                ));
            }
        }
        if let Some(key) = Self::shortcut_name(code) {
            if self.editor.shortcuts.borrow().contains(&key) {
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
        if !composing && code == u32::from(VK_BACK.0) {
            return Some(Action::new("process-key", json!({"keycode":"BackSpace"})));
        }
        if let Some(action) = Action::composing_digit(code, composing, down(0x10)) {
            return Some(action);
        }
        if code == 32 && !view.words.is_empty() && !composing {
            return Some(Action::new("select-candidate", json!({"idx":0})));
        }
        if !view.words.is_empty() && view.mode == 0 && !down(0x10) {
            if !composing && (code == 37 || code == 39) {
                return Some(Action::new(
                    "move-candidate",
                    json!({"delta":if code == 37 {-1} else {1}}),
                ));
            }
            if code == 38 || code == 40 {
                let step = if code == 38 { -1 } else { 1 };
                let at_edge = if code == 38 {
                    view.selected == 0
                } else {
                    view.selected + 1 >= view.words.len()
                };
                if at_edge {
                    return Some(Action::new(
                        "process-key",
                        json!({"keycode":if code == 38 {"PageUp"} else {"PageDown"}}),
                    ));
                }
                return Some(Action::new("move-candidate", json!({"delta":step})));
            }
            if code == 32 {
                return Some(Action::new(
                    "select-candidate",
                    json!({"idx":view.selected}),
                ));
            }
        }
        let key = match code {
            32 if !composing => "space".into(),
            8 | 13 | 27 | 32..=37 | 39 | 46 if composing => match code {
                8 => "BackSpace",
                13 => "Return",
                27 => "Escape",
                32 => "space",
                33 => "PageUp",
                34 => "PageDown",
                35 => "End",
                36 => "Home",
                37 => "Left",
                39 => "Right",
                _ => "Delete",
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
        let result = unsafe {
            ctx.RequestEditSession(
                self.editor.tid.get(),
                &session,
                TF_ES_SYNC | TF_ES_READWRITE,
            )
            .and_then(|hr| hr.ok())
        };
        if self.editor.ipc_failed.get() {
            self.editor.recover_ipc_failure();
            // A later prediction/settings call may fail after text was inserted.
            // Only pass the key through when the edit session did not handle it.
            return Ok(handled.get());
        }
        if handled.get() {
            return Ok(true);
        }
        result?;
        Ok(false)
    }
}
impl ITfKeyEventSink_Impl for Service_Impl {
    fn OnSetFocus(&self, fg: BOOL) -> Result<()> {
        self.editor.focus(fg.as_bool());
        Ok(())
    }
    fn OnTestKeyDown(&self, ctx: Option<&ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        if self.editor.bypass_keys() || !self.editor.protect_input(ctx, true) {
            return Ok(false.into());
        }
        if !matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1) {
            self.editor.shift_held.set(false);
        }
        self.editor.focus(true);
        if self.editor.ipc_failed.get() || self.editor.needs_refocus.get() {
            return Ok(false.into());
        }
        let shortcut = Self::shortcut_name(w.0 as u32);
        if shortcut.is_some() && self.editor.view.borrow().active && self.editor.refresh_shortcuts().is_err() {
            self.editor.recover_ipc_failure();
            return Ok(false.into());
        }
        let action = self.key(w, l);
        let shift_shortcut = shortcut.as_deref() == Some("Shift")
            && self.editor.shortcuts.borrow().iter().any(|key| key == "Shift");
        if !action
            .as_ref()
            .is_some_and(|a| matches!(a.cmd, "prediction-accept" | "prediction-dismiss"))
        {
            self.editor.cancel_prediction(false);
        }
        Ok((ctx.is_some() && (action.is_some() || shift_shortcut) && !self.editor.ipc_failed.get()).into())
    }
    fn OnKeyDown(&self, ctx: Option<&ITfContext>, w: WPARAM, l: LPARAM) -> Result<BOOL> {
        if self.editor.bypass_keys() || !self.editor.protect_input(ctx, true) {
            return Ok(false.into());
        }
        self.editor.focus(true);
        if self.editor.ipc_failed.get() || self.editor.needs_refocus.get() {
            return Ok(false.into());
        }
        if matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1) {
            self.editor.shift_held.set(
                Self::shortcut_name(w.0 as u32).is_some()
                    && self
                        .editor
                        .shortcuts
                        .borrow()
                        .iter()
                        .any(|key| key == "Shift"),
            );
            return Ok(false.into());
        }
        self.editor.shift_held.set(false);
        Ok(ctx
            .zip(self.key(w, l))
            .is_some_and(|(c, a)| self.edit(c, a).unwrap_or(false))
            .into())
    }
    fn OnTestKeyUp(&self, ctx: Option<&ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        Ok((!self.editor.bypass_keys()
            && ctx.is_some()
            && self.editor.view.borrow().active
            && matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1)
            && self.editor.shift_held.get())
        .into())
    }
    fn OnKeyUp(&self, ctx: Option<&ITfContext>, w: WPARAM, _: LPARAM) -> Result<BOOL> {
        if self.editor.bypass_keys() || !self.editor.view.borrow().active {
            self.editor.shift_held.set(false);
            return Ok(false.into());
        }
        if matches!(w.0 as u32, 0x10 | 0xa0 | 0xa1)
            && self.editor.shift_held.replace(false)
            && Self::shortcut_name(w.0 as u32).is_some()
        {
            if !self.editor.protect_input(ctx, true) {
                return Ok(false.into());
            }
            if self.editor.refresh_shortcuts().is_err() {
                self.editor.recover_ipc_failure();
                return Ok(false.into());
            }
            if !self.editor.shortcuts.borrow().iter().any(|key| key == "Shift") {
                return Ok(false.into());
            }
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
unsafe fn verified_commit_range(
    ctx: &ITfContext,
    ec: u32,
    end: &(ITfContext, ITfRange),
    text: &str,
) -> Result<Option<ITfRange>> {
    if &end.0 != ctx {
        return Ok(None);
    }
    let mut selection = [TF_SELECTION::default()];
    let mut fetched = 0;
    let selected = ctx.GetSelection(ec, TF_DEFAULT_SELECTION, &mut selection, &mut fetched);
    let range = ManuallyDrop::take(&mut selection[0].range);
    selected?;
    let Some(range) = range.filter(|_| fetched == 1) else {
        return Ok(None);
    };
    if selection[0].style.fInterimChar.as_bool()
        || !range.IsEmpty(ec)?.as_bool()
        || !range.IsEqualStart(ec, &end.1, TF_ANCHOR_END)?.as_bool()
    {
        return Ok(None);
    }
    let expected: Vec<u16> = text.encode_utf16().collect();
    let Ok(length) = i32::try_from(expected.len()) else {
        return Ok(None);
    };
    let mut shifted = 0;
    range.ShiftStart(ec, -length, &mut shifted, ptr::null())?;
    if shifted != -length {
        return Ok(None);
    }
    let mut actual = vec![0; expected.len() + 1];
    let mut read = 0;
    range.GetText(ec, 0, &mut actual, &mut read)?;
    Ok((read as usize == expected.len() && actual[..expected.len()] == expected).then_some(range))
}

unsafe fn verified_prediction_caret(
    ctx: &ITfContext,
    ec: u32,
    anchor: Option<&(ITfContext, ITfRange)>,
    context_before: &str,
) -> Result<Option<ITfRange>> {
    let mut selection = [TF_SELECTION::default()];
    let mut fetched = 0;
    let selected = ctx.GetSelection(ec, TF_DEFAULT_SELECTION, &mut selection, &mut fetched);
    let range = ManuallyDrop::take(&mut selection[0].range);
    selected?;
    let Some(range) = range.filter(|_| fetched == 1) else {
        return Ok(None);
    };
    if selection[0].style.fInterimChar.as_bool() || !range.IsEmpty(ec)?.as_bool() {
        return Ok(None);
    }
    if let Some((anchor_ctx, anchor_range)) = anchor {
        if anchor_ctx != ctx || !range.IsEqualStart(ec, anchor_range, TF_ANCHOR_END)?.as_bool() {
            return Ok(None);
        }
        return Ok(Some(range));
    }
    if context_before.is_empty() {
        return Ok(None);
    }
    let tail: String = context_before
        .chars()
        .rev()
        .take(16)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let expected: Vec<u16> = tail.encode_utf16().collect();
    let Ok(length) = i32::try_from(expected.len()) else {
        return Ok(None);
    };
    let probe = range.Clone()?;
    let mut shifted = 0;
    probe.ShiftStart(ec, -length, &mut shifted, ptr::null())?;
    if shifted != -length {
        return Ok(None);
    }
    let mut actual = vec![0; expected.len() + 1];
    let mut read = 0;
    probe.GetText(ec, 0, &mut actual, &mut read)?;
    Ok((read as usize == expected.len() && actual[..expected.len()] == expected).then_some(range))
}

fn prediction_contains_candidate(current: &Value, expected: &str) -> bool {
    if current["status"] != "ready" {
        return false;
    }
    current["text"].as_str() == Some(expected)
        || current["previous_candidate"].as_str() == Some(expected)
        || current["candidates"]
            .as_array()
            .is_some_and(|list| list.iter().any(|item| item.as_str() == Some(expected)))
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
        let access = if input_disabled(&self.ctx) {
            InputAccess::Blocked
        } else {
            read_input_scope(&self.ctx, ec).unwrap_or(InputAccess::Unknown)
        };
        if access != InputAccess::Allowed {
            ed.clear_input(Some((&self.ctx, ec)));
            return Ok(());
        }
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
        if a.cmd == "prediction-dismiss" {
            ed.cancel_prediction(true);
            self.handled.set(true);
            return Ok(());
        }
        let commit_end = ed.commit_end.take();
        let mut prediction_caret: Option<ITfRange> = None;
        let r = if a.cmd == "prediction-accept" {
            let expected = a.args["text"].as_str().unwrap_or("");
            let anchor = ed.prediction_anchor.borrow().clone();
            let context_before = ed.view.borrow().context_before.clone();
            let verified = unsafe {
                verified_prediction_caret(&self.ctx, ec, anchor.as_ref(), &context_before)
                    .ok()
                    .flatten()
            };
            let Some(caret_range) = verified else {
                ed.cancel_prediction(false);
                return Ok(());
            };
            let current = ed.call("predict-poll", json!({"id":a.args["id"]}))?;
            if expected.is_empty() || !prediction_contains_candidate(&current, expected) {
                ed.cancel_prediction(false);
                return Ok(());
            }
            prediction_caret = Some(caret_range);
            json!({"commit_text":expected})
        } else if a.cmd == "insert" {
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
        let backspace = a.cmd == "process-key" && a.args["keycode"] == "BackSpace";
        let undo_text = r["undo_commit_text"].as_str().filter(|text| !text.is_empty());
        let undo_requested = undo_text.is_some() || r["undo_commit_chars"].as_u64().unwrap_or(0) > 0;
        let undo_range = if undo_requested {
            let verified = undo_text
                .filter(|_| backspace && old.is_none() && commit.is_none() && !pre.is_empty())
                .zip(commit_end.as_ref())
                .and_then(|(text, end)| unsafe {
                    verified_commit_range(&self.ctx, ec, end, text).ok().flatten()
                });
            let Some(range) = verified else {
                let _ = ed.call("process-key", json!({"keycode":"Escape"}));
                if backspace && old.is_none() {
                    ed.view.borrow_mut().context_before.pop();
                }
                return Ok(());
            };
            Some(range)
        } else {
            None
        };
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
            if backspace {
                ed.view.borrow_mut().context_before.pop();
            }
            return Ok(());
        }
        let mut next_prediction_anchor: Option<(ITfContext, ITfRange)> = None;
        if edits_text {
            unsafe {
                let (range, composition) = if let Some(range) = undo_range {
                    let restored = (|| -> Result<ITfComposition> {
                        let owner: ITfContextComposition = self.ctx.cast()?;
                        let comp = owner.StartComposition(ec, &range, &self.sink)?;
                        let wide: Vec<u16> = pre.encode_utf16().collect();
                        if let Err(error) = range.SetText(ec, 0, &wide) {
                            let _ = comp.EndComposition(ec);
                            return Err(error);
                        }
                        Ok(comp)
                    })();
                    let Ok(comp) = restored else {
                        let _ = ed.call("process-key", json!({"keycode":"Escape"}));
                        ed.view.borrow_mut().context_before.pop();
                        return Ok(());
                    };
                    ed.composition.replace(Some((self.ctx.clone(), comp.clone())));
                    if let Some(text) = undo_text {
                        let mut view = ed.view.borrow_mut();
                        if let Some(prefix) = view.context_before.strip_suffix(text) {
                            let len = prefix.len();
                            view.context_before.truncate(len);
                        }
                    }
                    self.handled.set(true);
                    (range, Some(comp))
                } else if let Some((_, comp)) = old {
                    (comp.GetRange()?, Some(comp))
                } else if let Some(range) = prediction_caret {
                    (range, None)
                } else {
                    let insert: ITfInsertAtSelection = self.ctx.cast()?;
                    let range = insert.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])?;
                    let comp = if commit.is_none() {
                        let owner: ITfContextComposition = self.ctx.cast()?;
                        let comp = owner.StartComposition(ec, &range, &self.sink)?;
                        ed.composition.replace(Some((self.ctx.clone(), comp.clone())));
                        Some(comp)
                    } else {
                        None
                    };
                    (range, comp)
                };
                if !undo_requested {
                    let wide: Vec<u16> = commit.unwrap_or(pre).encode_utf16().collect();
                    range.SetText(ec, 0, &wide)?;
                }
                if commit.is_some() {
                    self.handled.set(true);
                }
                let mut caret = range.Clone()?;
                caret.Collapse(ec, TF_ANCHOR_END)?;
                if (commit.is_some() || pre.is_empty()) && composition.is_some() {
                    ed.composition.replace(None);
                    composition.unwrap().EndComposition(ec)?;
                }
                if commit.is_some() && !pre.is_empty() {
                    let owner: ITfContextComposition = self.ctx.cast()?;
                    let next_comp = owner.StartComposition(ec, &caret, &self.sink)?;
                    let pre_range = next_comp.GetRange()?;
                    let pre_wide: Vec<u16> = pre.encode_utf16().collect();
                    pre_range.SetText(ec, 0, &pre_wide)?;
                    ed.composition
                        .replace(Some((self.ctx.clone(), next_comp)));
                    caret = pre_range.Clone()?;
                    caret.Collapse(ec, TF_ANCHOR_END)?;
                }
                let mut delta = r["cursor_delta"].as_i64().unwrap_or(0) as i32;
                if !pre.is_empty() {
                    if let Some(cursor) = r["preedit"]["cursor_utf8"].as_u64() {
                        let mut cursor = cursor.min(pre.len() as u64) as usize;
                        while !pre.is_char_boundary(cursor) {
                            cursor -= 1;
                        }
                        delta -= pre[cursor..].encode_utf16().count() as i32;
                    }
                }
                if delta < 0 {
                    let mut moved = 0;
                    caret.ShiftStart(ec, delta, &mut moved, ptr::null())?;
                    caret.Collapse(ec, TF_ANCHOR_START)?;
                }
                if let Ok(view) = self.ctx.GetActiveView() {
                    let mut rect = RECT::default();
                    let mut clipped = BOOL(0);
                    if view.GetTextExt(ec, &caret, &mut rect, &mut clipped).is_ok() {
                        if let Ok(hwnd) = view.GetWnd() {
                            vitral_physical_caret(hwnd, &mut rect);
                        }
                        ed.view.borrow_mut().caret = rect;
                    }
                }
                let mut selection = TF_SELECTION {
                    range: ManuallyDrop::new(Some(caret.clone())),
                    style: TF_SELECTIONSTYLE {
                        ase: TF_AE_NONE,
                        fInterimChar: false.into(),
                    },
                };
                let sel = self.ctx.SetSelection(ec, std::slice::from_ref(&selection));
                ManuallyDrop::drop(&mut selection.range);
                sel?;
                let clipboard_commit = r["clipboard_commit"].as_bool().unwrap_or(false);
                if a.anchor_commit
                    && !clipboard_commit
                    && commit.is_some()
                    && pre.is_empty()
                    && delta == 0
                {
                    // Keep the anchor at the original end when text is inserted there.
                    if let Ok(end) = caret.Clone() {
                        if end.SetGravity(ec, TF_GRAVITY_BACKWARD, TF_GRAVITY_BACKWARD).is_ok() {
                            ed.commit_end.replace(Some((self.ctx.clone(), end)));
                        }
                    }
                }
                if commit.is_some()
                    && !clipboard_commit
                    && a.cmd != "prediction-accept"
                    && ed.composition.borrow().is_none()
                    && pre.is_empty()
                {
                    if let Ok(anchor) = caret.Clone() {
                        if anchor.SetGravity(ec, TF_GRAVITY_BACKWARD, TF_GRAVITY_BACKWARD).is_ok() {
                            next_prediction_anchor = Some((self.ctx.clone(), anchor));
                        }
                    }
                }
            }
        }
        let clipboard_commit = r["clipboard_commit"].as_bool().unwrap_or(false);
        if a.cmd == "prediction-accept" && commit.is_some() {
            ed.call(
                "predict-accept",
                json!({"id":a.args["id"],"text":a.args["text"]}),
            )?;
            ed.cancel_prediction(false);
        } else if clipboard_commit {
            ed.cancel_prediction(false);
        }
        let mut v = ed.view.borrow_mut();
        if let Some(text) = commit {
            v.context_before.push_str(text);
            v.context_before = v
                .context_before
                .chars()
                .rev()
                .take(160)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
        }
        if let Some(lang) = r["lang_mode"].as_str() {
            v.english = lang == "en";
        }
        if a.cmd == "hw-confirm" {
            v.mode = 0;
        }
        v.preedit.clear();
        v.preedit.push_str(pre);
        v.page = r["page"]["page_index"].as_u64().unwrap_or(0);
        v.selected = 0;
        v.words.clear();
        v.values.clear();
        v.slots.clear();
        if let Some(candidates) = r["candidates"].as_array() {
            for c in candidates.iter().take(5) {
                let text = c["text"].as_str().or(c["char"].as_str()).unwrap_or("");
                v.values.push(text.into());
                v.words.push(if let Some(py) = c["toned_pinyin"].as_str() {
                    format!(
                        "{} {} {}",
                        text,
                        py,
                        c["xiaohe_code"].as_str().unwrap_or("")
                    )
                } else if let Some(comment) = c["comment"].as_str().filter(|s| s.contains(" · ")) {
                    format!("{text} {comment}")
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
        if commit.is_some()
            && !clipboard_commit
            && a.cmd != "prediction-accept"
            && ed.composition.borrow().is_none()
        {
            ed.schedule_prediction(false, next_prediction_anchor);
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
    let Some(ctx) = ed.current_context() else { return };
    ed.ipc_failed.set(false);
    if !ed.protect_input(Some(&ctx), true) {
        return;
    }
    if ed.needs_refocus.get() {
        ed.focus(true);
        if ed.needs_refocus.get() {
            return;
        }
    }
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
        0 if !ed.view.borrow().prediction_text.is_empty() && ed.composition.borrow().is_none() => {
            let v = ed.view.borrow();
            Action::new(
                "prediction-accept",
                json!({"id":v.prediction_id,"text":v.prediction_text}),
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
        _ => return,
    };
    if action.cmd != "prediction-accept" {
        ed.cancel_prediction(false);
    }
    if ed.ipc_failed.get() {
        ed.recover_ipc_failure();
        return;
    }
    let _ = service.edit(&ctx, action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composing_digits_are_routed_to_process_key() {
        for digit in '1'..='5' {
            let action = Action::composing_digit(u32::from(digit), true, false).unwrap();
            assert_eq!(action.cmd, "process-key");
            assert_eq!(action.args, json!({"keycode": digit.to_string()}));
            assert!(action.anchor_commit);
            assert!(Action::composing_digit(u32::from(digit), false, false).is_none());
            assert!(Action::composing_digit(u32::from(digit), true, true).is_none());
        }
        assert!(Action::composing_digit(u32::from('6'), true, false).is_none());
    }

    #[implement(ITfComposition)]
    struct ReconnectComposition {
        ended_at: Rc<Cell<Option<u32>>>,
    }
    impl ITfComposition_Impl for ReconnectComposition_Impl {
        fn GetRange(&self) -> Result<ITfRange> {
            Err(E_NOTIMPL.into())
        }
        fn ShiftStart(&self, _: u32, _: Option<&ITfRange>) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn ShiftEnd(&self, _: u32, _: Option<&ITfRange>) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EndComposition(&self, ec: u32) -> Result<()> {
            self.ended_at.set(Some(ec));
            Ok(())
        }
    }

    #[test]
    fn reconnect_ends_composition_without_opening_its_text_range() {
        let ended_at = Rc::new(Cell::new(None));
        let session: ITfEditSession = Finish {
            comp: ReconnectComposition { ended_at: ended_at.clone() }.into(),
            clear_text: false,
            _live: Live::new(),
        }
        .into();
        unsafe { session.DoEditSession(42) }.unwrap();
        assert_eq!(ended_at.get(), Some(42));
    }

    #[test]
    fn ipc_backoff_allows_probe_at_deadline() {
        let start = Instant::now();
        let until = Some(start + IPC_BACKOFF);
        assert!(!ipc_in_backoff(None, start));
        assert!(ipc_in_backoff(until, start));
        assert!(ipc_in_backoff(until, start + IPC_BACKOFF - Duration::from_millis(1)));
        assert!(!ipc_in_backoff(until, start + IPC_BACKOFF));
    }

    #[test]
    fn active_editor_registers_focus_again_after_ipc_backoff() {
        let start = Instant::now();
        let until = Some(start + IPC_BACKOFF);
        assert!(!should_update_focus(true, true, false));
        assert!(should_update_focus(true, true, true));
        assert!(ipc_in_backoff(until, start));
        assert!(!ipc_in_backoff(until, start + IPC_BACKOFF));
        assert!(should_update_focus(true, false, true));
        assert!(!should_update_focus(false, false, true));
    }

    #[test]
    fn refreshed_shortcut_bindings_drop_removed_and_empty_keys() {
        let shortcuts = RefCell::new(shortcut_bindings(&json!({"bindings":[{"key":"F1"}]})));
        shortcuts.replace(shortcut_bindings(&json!({"bindings":[{"key":""},{"key":"Ctrl+F2"}]})));
        assert_eq!(&*shortcuts.borrow(), &["Ctrl+F2"]);
    }

    #[test]
    fn protected_scopes_always_pass_keys_through() {
        for scope in [IS_PASSWORD, IS_NUMERIC_PIN, IS_ALPHANUMERIC_PIN] {
            let access = classify_scopes(&[IS_DEFAULT, scope]);
            assert_eq!(access, InputAccess::Blocked);
            assert!(!access.permits_key(false));
            assert!(!access.permits_key(true));
        }
        assert!(classify_scopes(&[IS_DEFAULT]).permits_key(false));
        assert!(!InputAccess::Unknown.permits_key(false));
        assert!(InputAccess::Unknown.permits_key(true));
    }
}
