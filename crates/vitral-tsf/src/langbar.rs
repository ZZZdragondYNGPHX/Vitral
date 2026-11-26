use super::{Live, CLASS};
use std::{cell::{Cell, RefCell}, ffi::c_void, path::Path, ptr, rc::Rc};
use windows::{core::*, Win32::{Foundation::*, UI::{TextServices::*, WindowsAndMessaging::HICON}}};

extern "C" {
    fn vitral_mode_icon(english: bool) -> HICON;
}

struct State {
    owner: Cell<*const c_void>,
    toggle: unsafe fn(*const c_void),
    english: Cell<bool>,
    conversion: Option<ITfCompartment>,
    client_id: u32,
    sink: RefCell<Option<ITfLangBarItemSink>>,
}
impl State {
    fn update(&self, english: bool) {
        let changed = self.english.replace(english) != english;
        if let Some(conversion) = &self.conversion {
            let mode = if english { 0i32 } else { TF_CONVERSIONMODE_NATIVE as i32 };
            unsafe {
                let current = conversion.GetValue().ok().and_then(|v| i32::try_from(&v).ok());
                if current != Some(mode) { let _ = conversion.SetValue(self.client_id, &VARIANT::from(mode)); }
            }
        }
        if !changed { return; }
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            unsafe { let _ = sink.OnUpdate(TF_LBI_ICON | TF_LBI_TEXT | TF_LBI_TOOLTIP | TF_LBI_STATUS); }
        }
    }
    fn toggle(&self) {
        let owner = self.owner.get();
        if !owner.is_null() { unsafe { (self.toggle)(owner); } }
    }
}

// This handle detaches callbacks before removing the COM item. A shell-held
// reference can outlive the service without retaining a stale service pointer.
pub(super) struct LanguageBar {
    manager: ITfLangBarItemMgr,
    item: ITfLangBarItemButton,
    state: Rc<State>,
    conversion_source: ITfSource,
    conversion_cookie: Option<u32>,
}
impl LanguageBar {
    pub(super) fn new(mgr: &ITfThreadMgr, owner: *const c_void,
        toggle: unsafe fn(*const c_void), _icon_path: &Path, client_id: u32) -> Result<Self> {
        let manager: ITfLangBarItemMgr = mgr.cast()?;
        let compartments: ITfCompartmentMgr = mgr.cast()?;
        let conversion = unsafe { compartments.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)? };
        let conversion_source: ITfSource = conversion.cast()?;
        let state = Rc::new(State {
            owner: Cell::new(owner), toggle, english: Cell::new(false),
            conversion: Some(conversion), client_id,
            sink: RefCell::new(None),
        });
        let item: ITfLangBarItemButton = ModeButton { state: state.clone(), _live: Live::new() }.into();
        unsafe { manager.AddItem(&item)?; }
        let mut bar = Self { manager, item, state, conversion_source, conversion_cookie: None };
        bar.conversion_cookie = Some(unsafe { bar.conversion_source.AdviseSink(
            &ITfCompartmentEventSink::IID, &bar.item.cast::<IUnknown>()?)? });
        bar.update(false);
        Ok(bar)
    }
    pub(super) fn update(&self, english: bool) { self.state.update(english); }
}
impl Drop for LanguageBar {
    fn drop(&mut self) {
        self.state.owner.set(ptr::null());
        if let Some(cookie) = self.conversion_cookie.take() {
            unsafe { let _ = self.conversion_source.UnadviseSink(cookie); }
        }
        unsafe { let _ = self.manager.RemoveItem(&self.item); }
        self.state.sink.take();
    }
}

#[implement(ITfLangBarItemButton, ITfSource, ITfCompartmentEventSink)]
struct ModeButton { state: Rc<State>, _live: Live }
impl ITfLangBarItem_Impl for ModeButton_Impl {
    fn GetInfo(&self, info: *mut TF_LANGBARITEMINFO) -> Result<()> {
        if info.is_null() { return Err(E_POINTER.into()); }
        let mut value = TF_LANGBARITEMINFO {
            clsidService: CLASS, guidItem: GUID_LBI_INPUTMODE,
            dwStyle: TF_LBI_STYLE_BTN_BUTTON | TF_LBI_STYLE_SHOWNINTRAY | TF_LBI_STYLE_TEXTCOLORICON,
            ..Default::default()
        };
        for (dest, ch) in value.szDescription.iter_mut().zip("Vitral".encode_utf16()) { *dest = ch; }
        unsafe { info.write(value); }
        Ok(())
    }
    fn GetStatus(&self) -> Result<u32> { Ok(0) }
    fn Show(&self, _: BOOL) -> Result<()> { Ok(()) }
    fn GetTooltipString(&self) -> Result<BSTR> {
        Ok(BSTR::from(if self.state.english.get() { "Vitral · English（点击切换中文）" }
            else { "Vitral · 中文（点击切换英文）" }))
    }
}
impl ITfLangBarItemButton_Impl for ModeButton_Impl {
    fn OnClick(&self, click: TfLBIClick, _: &POINT, _: *const RECT) -> Result<()> {
        if click == TF_LBI_CLK_LEFT { self.state.toggle(); }
        Ok(())
    }
    fn InitMenu(&self, _: Option<&ITfMenu>) -> Result<()> { Ok(()) }
    fn OnMenuSelect(&self, _: u32) -> Result<()> { Ok(()) }
    fn GetIcon(&self) -> Result<HICON> {
        let icon = unsafe { vitral_mode_icon(self.state.english.get()) };
        if icon.is_invalid() { Err(E_FAIL.into()) } else { Ok(icon) }
    }
    fn GetText(&self) -> Result<BSTR> {
        Ok(BSTR::from(if self.state.english.get() { "EN" } else { "中" }))
    }
}
impl ITfCompartmentEventSink_Impl for ModeButton_Impl {
    fn OnChange(&self, guid: *const GUID) -> Result<()> {
        if guid.is_null() { return Err(E_POINTER.into()); }
        if unsafe { *guid } != GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION { return Ok(()); }
        if let Some(conversion) = &self.state.conversion {
            let mode = unsafe { conversion.GetValue()? };
            let english = i32::try_from(&mode)? & TF_CONVERSIONMODE_NATIVE as i32 == 0;
            if english != self.state.english.get() { self.state.toggle(); }
        }
        Ok(())
    }
}
impl ITfSource_Impl for ModeButton_Impl {
    fn AdviseSink(&self, iid: *const GUID, sink: Option<&IUnknown>) -> Result<u32> {
        if iid.is_null() { return Err(E_POINTER.into()); }
        if unsafe { *iid } != ITfLangBarItemSink::IID { return Err(E_NOINTERFACE.into()); }
        let sink: ITfLangBarItemSink = sink.ok_or(E_POINTER)?.cast()?;
        let mut stored = self.state.sink.borrow_mut();
        if stored.is_some() { return Err(E_FAIL.into()); }
        *stored = Some(sink);
        Ok(1)
    }
    fn UnadviseSink(&self, cookie: u32) -> Result<()> {
        if cookie != 1 || self.state.sink.borrow().is_none() { return Err(E_INVALIDARG.into()); }
        self.state.sink.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe fn toggle(owner: *const c_void) { (&*owner.cast::<Cell<u32>>()).set(1); }
    #[test]
    fn mode_item_identity_and_detached_callback() {
        let called = Cell::new(0u32);
        let state = Rc::new(State { owner: Cell::new((&called as *const Cell<u32>).cast()),
            toggle, english: Cell::new(false), conversion: None, client_id: 0, sink: RefCell::new(None) });
        let button: ITfLangBarItemButton = ModeButton { state: state.clone(), _live: Live::new() }.into();
        unsafe {
            let mut info = TF_LANGBARITEMINFO::default();
            button.GetInfo(&mut info).unwrap();
            assert_eq!(info.guidItem, GUID_LBI_INPUTMODE);
            assert_eq!(info.clsidService, CLASS);
            assert_ne!(info.dwStyle & TF_LBI_STYLE_SHOWNINTRAY, 0);
            button.OnClick(TF_LBI_CLK_LEFT, POINT::default(), ptr::null()).unwrap();
            assert_eq!(called.get(), 1);
            state.owner.set(ptr::null()); called.set(0);
            button.OnClick(TF_LBI_CLK_LEFT, POINT::default(), ptr::null()).unwrap();
            assert_eq!(called.get(), 0);
            state.update(true);
            assert_eq!(button.GetText().unwrap().to_string(), "EN");
        }
    }
}
