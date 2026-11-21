pub mod gram;
#[cfg(all(target_os = "linux", feature = "native-rime"))]
pub mod opencc_ffi;
pub mod pure_rust;
pub mod quanpin;
#[cfg(all(target_os = "linux", feature = "native-rime"))]
mod phrase_rank;
#[cfg(all(target_os = "linux", feature = "native-rime"))]
pub mod rime_ffi;

use crate::clip::ClipboardManager;
use crate::error::VitralError;
use crate::imp::polyphone::default_assets_dir_candidates;
use crate::imp::ImportedLexiconIndex;
use crate::ret::{expand_time_macro, match_paired_punctuation, RetentionEngine};
use crate::sync::RelayDevice;
use pure_rust::{pure_rust_opencc, PureRustEngine};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
#[cfg(all(target_os = "linux", feature = "native-rime"))]
use std::path::Path;
use std::path::PathBuf;
use vitral_tokens::TOKENS;

pub const MOD_LANG_TOGGLE: u32 = 1 << 30;
pub const MOD_VOICE_HOLD: u32 = 1 << 29;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    DoublePinyinFlypy,
    RimeIce,
}

impl SchemaKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SchemaKind::DoublePinyinFlypy => "double_pinyin_flypy",
            SchemaKind::RimeIce => "rime_ice",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenCcMode {
    Simplified,
    Traditional,
    TaiwanStandard,
    HongKong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LangMode {
    Zh,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwipeDir {
    None,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub data_dir: String,
    #[serde(default)]
    pub cache_dir: String,
    #[serde(default)]
    pub assets_dir: String,
    #[serde(default)]
    pub tokens_path: String,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default = "default_schema")]
    pub schema: SchemaKind,
    #[serde(default = "default_opencc")]
    pub opencc_mode: OpenCcMode,
    #[serde(default)]
    pub pure_offline: bool,
    #[serde(default)]
    pub background_sync: bool,
    #[serde(default = "default_device_id")]
    pub device_id: String,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    #[serde(default = "default_ring_capacity", alias = "clipboard_capacity")]
    pub clipboard_ring_capacity: usize,
    #[serde(default)]
    pub frecency_half_life_sec: Option<f64>,
    #[serde(default, alias = "webdav_url")]
    pub koofr_endpoint: Option<String>,
    #[serde(default)]
    pub koofr_username: Option<String>,
    #[serde(default)]
    pub koofr_password: Option<String>,
    #[serde(default)]
    pub koofr_root: Option<String>,
    #[serde(default)]
    pub sync_key: Option<String>,
    #[serde(default)]
    pub localsend_port: Option<u16>,
    #[serde(default)]
    pub localsend_udp_port: Option<u16>,
    #[serde(default)]
    pub ai_endpoint: Option<String>,
    #[serde(default)]
    pub ai_model: Option<String>,
    #[serde(default, skip_serializing)]
    pub ai_api_key: Option<String>,
    #[serde(default)]
    pub ai_timeout_ms: Option<u64>,
    #[serde(default)]
    pub ai_tier3_enabled: bool,
    #[serde(default)]
    pub known_devices: Vec<RelayDevice>,
}

impl EngineConfig {
    pub fn parse_json(raw: &str) -> Result<Self, VitralError> {
        let mut value: serde_json::Value = serde_json::from_str(raw).map_err(|e| VitralError::Parse(e.to_string()))?;
        crate::sync::merge_alias(&mut value,"koofr_endpoint","webdav_url")?;
        crate::sync::merge_alias(&mut value,"clipboard_ring_capacity","clipboard_capacity")?;
        if let Some(devices)=value.get_mut("known_devices").and_then(|v| v.as_array_mut()){
            for device in devices {
                crate::sync::merge_alias(device,"device_id","id")?;
                crate::sync::merge_alias(device,"usage_count","transfer_count")?;
            }
        }
        serde_json::from_value(value).map_err(|e| VitralError::Parse(e.to_string()))
    }
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("data_dir", &self.data_dir)
            .field("cache_dir", &self.cache_dir)
            .field("assets_dir", &self.assets_dir)
            .field("tokens_path", &self.tokens_path)
            .field("state_dir", &self.state_dir)
            .field("schema", &self.schema)
            .field("opencc_mode", &self.opencc_mode)
            .field("pure_offline", &self.pure_offline)
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .field("platform", &self.platform)
            .field("clipboard_ring_capacity", &self.clipboard_ring_capacity)
            .field("frecency_half_life_sec", &self.frecency_half_life_sec)
            .field("koofr_endpoint", &self.koofr_endpoint)
            .field("koofr_username", &self.koofr_username)
            .field(
                "koofr_password",
                &self.koofr_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("koofr_root", &self.koofr_root)
            .field("sync_key", &self.sync_key.as_ref().map(|_| "[REDACTED]"))
            .field("localsend_port", &self.localsend_port)
            .field("localsend_udp_port", &self.localsend_udp_port)
            .field("ai_endpoint", &self.ai_endpoint)
            .field("known_devices", &self.known_devices)
            .finish()
    }
}

fn default_schema() -> SchemaKind {
    SchemaKind::DoublePinyinFlypy
}
fn default_opencc() -> OpenCcMode {
    OpenCcMode::Simplified
}
fn default_device_id() -> String {
    "vitral-local-device".to_string()
}
fn default_device_name() -> String {
    "Vitral-Host".to_string()
}
fn default_platform() -> String {
    "linux".to_string()
}
fn default_ring_capacity() -> usize {
    50
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            data_dir: String::new(),
            cache_dir: String::new(),
            assets_dir: String::new(),
            tokens_path: String::new(),
            state_dir: None,
            schema: default_schema(),
            opencc_mode: default_opencc(),
            pure_offline: false,
            background_sync: false,
            device_id: default_device_id(),
            device_name: default_device_name(),
            platform: default_platform(),
            clipboard_ring_capacity: default_ring_capacity(),
            frecency_half_life_sec: None,
            koofr_endpoint: None,
            koofr_username: None,
            koofr_password: None,
            koofr_root: None,
            sync_key: None,
            localsend_port: None,
            localsend_udp_port: None,
            ai_endpoint: None,
            ai_model: None,
            ai_api_key: None,
            ai_timeout_ms: None,
            ai_tier3_enabled: false,
            known_devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppFocusState {
    pub app_id: String,
    pub lang_mode: LangMode,
    pub remembered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PreeditState {
    pub text: String,
    pub cursor_utf8: usize,
    pub retroactive_slots: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateItem {
    pub index: usize,
    pub text: String,
    pub comment: String,
    pub pinned: bool,
    pub is_clipboard_shortcut: bool,
    pub shortcut_code: Option<String>,
    pub frecency_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInfo {
    pub page_index: usize,
    pub page_size: usize,
    pub has_prev: bool,
    pub has_next: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputAction {
    pub lang_mode: LangMode,
    pub schema: String,
    pub preedit: PreeditState,
    pub commit_text: Option<String>,
    pub candidates: Vec<CandidateItem>,
    pub page: PageInfo,
    pub paired_punctuation: Option<String>,
    pub cursor_delta: i32,
    pub macro_expanded: Option<String>,
    #[serde(default)]
    pub voice_trigger: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadReport {
    pub reloaded: bool,
    pub assets_dir: String,
    pub assets: Vec<String>,
}

pub fn resolve_assets_dir(configured: &str) -> Option<PathBuf> {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return Some(PathBuf::from(trimmed));
    }
    default_assets_dir_candidates()
}

#[derive(Debug, Clone)]
enum CandidateOrigin {
    Overlay {
        canonical: String,
        commit_text: String,
        is_clipboard: bool,
    },
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    Rime {
        canonical: String,
        commit_text: String,
        rime_page_idx: usize,
    },
    PureRust {
        canonical: String,
        commit_text: String,
    },
}

impl CandidateOrigin {
    fn canonical(&self) -> &str {
        match self {
            CandidateOrigin::Overlay { canonical, .. }
            | CandidateOrigin::PureRust { canonical, .. } => canonical,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            CandidateOrigin::Rime { canonical, .. } => canonical,
        }
    }
}

pub struct EngineState {
    pub config: EngineConfig,
    pub resolved_assets_dir: Option<PathBuf>,
    pub schema: SchemaKind,
    pub opencc_mode: OpenCcMode,
    pub lang_mode: LangMode,
    pub active_app_id: Option<String>,
    pub app_lang_memory: HashMap<String, LangMode>,
    pub composing: String,
    pub raw_input: String,
    pub cursor_utf8: usize,
    pub current_candidates: Vec<CandidateItem>,
    pub current_page: PageInfo,
    pub pure_rust: PureRustEngine,
    pub(crate) lookup_hints: Vec<crate::hw::Mode1Candidate>,
    candidate_origins: Vec<CandidateOrigin>,
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    rime_session_id: Option<rime_ffi::RimeSessionId>,
}

impl std::fmt::Debug for EngineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineState")
            .field("schema", &self.schema)
            .field("opencc_mode", &self.opencc_mode)
            .field("lang_mode", &self.lang_mode)
            .field("composing", &self.composing)
            .finish()
    }
}

#[cfg(all(target_os = "linux", feature = "native-rime"))]
impl Drop for EngineState {
    fn drop(&mut self) {
        if let Some(sid) = self.rime_session_id.take() {
            if let Some(ref assets_dir) = self.resolved_assets_dir {
                let _ = rime_ffi::with_runtime(
                    assets_dir,
                    &self.config.cache_dir,
                    &self.config.data_dir,
                    |rt| {
                        rt.destroy_session(sid);
                        Ok(())
                    },
                );
            }
        }
    }
}

impl EngineState {
    pub fn new(mut config: EngineConfig) -> Self {
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            rime_ffi::probe_linked();
            opencc_ffi::probe_linked();
        }
        if config.data_dir.trim().is_empty() {
            if let Some(ref sd) = config.state_dir {
                if !sd.trim().is_empty() {
                    config.data_dir = sd.clone();
                }
            }
        }
        let resolved_assets_dir = resolve_assets_dir(&config.assets_dir);
        let pure_rust = PureRustEngine::new(resolved_assets_dir.as_deref());
        let page_size = TOKENS.ergonomics.desktop_candidate_page_size as usize;
        Self {
            schema: config.schema,
            opencc_mode: config.opencc_mode,
            resolved_assets_dir,
            config,
            lang_mode: LangMode::Zh,
            active_app_id: None,
            app_lang_memory: HashMap::new(),
            composing: String::new(),
            raw_input: String::new(),
            cursor_utf8: 0,
            current_candidates: Vec::new(),
            current_page: PageInfo {
                page_index: 0,
                page_size,
                has_prev: false,
                has_next: false,
            },
            pure_rust,
            lookup_hints: Vec::new(),
            candidate_origins: Vec::new(),
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            rime_session_id: None,
        }
    }

    pub(crate) fn canonical_at(&self, index: usize) -> Option<&str> {
        self.candidate_origins.get(index).map(CandidateOrigin::canonical)
    }

    pub(crate) fn clear_composition_state(&mut self) {
        self.composing.clear();
        self.raw_input.clear();
        self.cursor_utf8 = 0;
        self.current_candidates.clear();
        self.candidate_origins.clear();
        self.current_page.page_index = 0;
        self.current_page.has_prev = false;
        self.current_page.has_next = false;
        self.pure_rust.page_index = 0;
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let (Some(sid), Some(ref assets_dir)) = (self.rime_session_id, &self.resolved_assets_dir)
        {
            let _ = rime_ffi::with_runtime(
                assets_dir,
                &self.config.cache_dir,
                &self.config.data_dir,
                |rt| {
                    rt.clear_composition(sid);
                    Ok(())
                },
            );
        }
    }

    pub fn on_app_focus(&mut self, app_id: &str) -> Result<AppFocusState, VitralError> {
        if app_id.trim().is_empty() {
            return Err(VitralError::InvalidArgument("app_id cannot be empty".into()));
        }
        let prev_lang = self.lang_mode;
        self.active_app_id = Some(app_id.to_string());
        let remembered = self.app_lang_memory.contains_key(app_id);
        let lang_mode = *self.app_lang_memory.entry(app_id.to_string()).or_insert_with(|| {
            let lower = app_id.to_ascii_lowercase();
            if lower.contains("terminal")
                || lower.contains("konsole")
                || lower.contains("alacritty")
                || lower.contains("kitty")
                || lower.contains("code")
                || lower.contains("vim")
            {
                LangMode::En
            } else {
                LangMode::Zh
            }
        });
        self.lang_mode = lang_mode;
        if prev_lang != lang_mode || lang_mode == LangMode::En {
            self.clear_composition_state();
        }
        Ok(AppFocusState {
            app_id: app_id.to_string(),
            lang_mode,
            remembered,
        })
    }

    pub fn toggle_lang_mode(&mut self) -> LangMode {
        let next = match self.lang_mode {
            LangMode::Zh => LangMode::En,
            LangMode::En => LangMode::Zh,
        };
        self.lang_mode = next;
        if next == LangMode::En {
            self.clear_composition_state();
        }
        if let Some(ref app_id) = self.active_app_id {
            self.app_lang_memory.insert(app_id.clone(), next);
        }
        next
    }

    pub fn set_schema(&mut self, schema: SchemaKind) {
        self.schema = schema;
        self.clear_composition_state();
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let (Some(sid), Some(ref assets_dir)) = (self.rime_session_id, &self.resolved_assets_dir)
        {
            let _ = rime_ffi::with_runtime(
                assets_dir,
                &self.config.cache_dir,
                &self.config.data_dir,
                |rt| rt.select_schema(sid, schema),
            );
        }
    }

    pub fn set_opencc_mode(&mut self, mode: OpenCcMode) {
        self.opencc_mode = mode;
    }

    pub fn reload_assets(&mut self) -> Result<ReloadReport, VitralError> {
        self.resolved_assets_dir = resolve_assets_dir(&self.config.assets_dir);
        let assets_dir_str = self
            .resolved_assets_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.config.assets_dir.clone());
        let mut assets = Vec::new();
        if let Some(ref root) = self.resolved_assets_dir {
            if root.is_dir() {
                for sub in ["rime", "opencc", "handwriting", "voice", "reranker"] {
                    if root.join(sub).exists() {
                        assets.push(sub.to_string());
                    }
                }
            }
        }
        self.pure_rust.reload_assets(self.resolved_assets_dir.as_deref());

        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let Some(root) = self.resolved_assets_dir.clone() {
            if root.join("rime").is_dir() {
                let _ = self.ensure_rime_session(&root)?;
            }
        }

        Ok(ReloadReport {
            reloaded: true,
            assets_dir: assets_dir_str,
            assets,
        })
    }

    pub fn convert_opencc(&self, text: &str) -> String {
        if self.opencc_mode == OpenCcMode::Simplified || text.is_empty() {
            return text.to_string();
        }
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let Some(ref assets_dir) = self.resolved_assets_dir {
            let opencc_dir = assets_dir.join("opencc");
            if opencc_dir.is_dir() {
                if let Ok(converted) = opencc_ffi::convert_text(&opencc_dir, self.opencc_mode, text)
                {
                    return converted;
                }
            }
        }
        pure_rust_opencc(self.opencc_mode, text)
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn ensure_rime_session(&mut self, assets_dir: &Path) -> Result<rime_ffi::RimeSessionId, VitralError> {
        if let Some(sid) = self.rime_session_id {
            return Ok(sid);
        }
        let schema = self.schema;
        let sid = rime_ffi::with_runtime(
            assets_dir,
            &self.config.cache_dir,
            &self.config.data_dir,
            |rt| rt.create_session(schema),
        )?;
        self.rime_session_id = Some(sid);
        Ok(sid)
    }

    pub fn process_key(
        &mut self,
        keycode: u32,
        modifiers: u32,
        swipe_dir: SwipeDir,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        if modifiers & MOD_LANG_TOGGLE != 0 {
            self.toggle_lang_mode();
            return Ok(self.empty_action(None));
        }

        let is_backspace = keycode == 0x08 || keycode == 0xFF08;
        let is_space = keycode == 0x20;
        if is_space && (swipe_dir != SwipeDir::None || (modifiers & MOD_VOICE_HOLD != 0)) {
            let mut act = self.empty_action(None);
            if modifiers & MOD_VOICE_HOLD != 0 {
                act.voice_trigger = true;
                act.macro_expanded = Some("voice_trigger".to_string());
                return Ok(act);
            }
            match swipe_dir {
                SwipeDir::Left => act.cursor_delta = -1,
                SwipeDir::Right => act.cursor_delta = 1,
                SwipeDir::Up => {
                    act.voice_trigger = true;
                    act.macro_expanded = Some("voice_trigger".to_string());
                }
                SwipeDir::Down | SwipeDir::None => {}
            }
            return Ok(act);
        }
        if is_backspace && swipe_dir == SwipeDir::Left {
            let was_empty = self.raw_input.is_empty() && self.composing.is_empty();
            self.clear_composition_state();
            let mut act = self.empty_action(None);
            if was_empty {
                act.cursor_delta = -1;
            }
            return Ok(act);
        }

        if self.lang_mode == LangMode::En {
            if !self.composing.is_empty()
                || !self.current_candidates.is_empty()
                || !self.raw_input.is_empty()
            {
                self.clear_composition_state();
            }
            if let Some(ch) = char::from_u32(keycode) {
                if !ch.is_control() {
                    return Ok(self.empty_action(Some(ch.to_string())));
                }
            }
            return Ok(self.empty_action(None));
        }

        if let Some(act) = self.handle_passthrough_or_paired_punct(keycode)? {
            return Ok(act);
        }

        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            if let Some(assets_dir) = self.resolved_assets_dir.clone() {
                if assets_dir.join("rime").is_dir() {
                    return self.process_key_native_rime(
                        &assets_dir,
                        keycode,
                        modifiers,
                        clipboard,
                        imported,
                        retention,
                    );
                }
            }
        }

        self.process_key_pure_rust(keycode, clipboard, imported, retention)
    }

    #[cfg(test)]
    pub(crate) fn process_key_pure_rust_for_test(
        &mut self,
        keycode: u32,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        let mut cloned = retention.clone();
        self.process_key_pure_rust(keycode, clipboard, imported, &mut cloned)
    }

    fn is_passthrough_active(&self) -> bool {
        self.raw_input.chars().any(|c| {
            c.is_ascii_uppercase() || matches!(c, ':' | '/' | '@' | '_' | '?' | '.')
        }) || self
            .current_candidates
            .first()
            .is_some_and(|c| c.comment == "En/Token")
    }

    fn should_enter_passthrough(&self, ch: char) -> bool {
        if self.is_passthrough_active() {
            return true;
        }
        if ch.is_ascii_uppercase() {
            return true;
        }
        if !self.raw_input.is_empty() {
            if matches!(ch, ':' | '/' | '@' | '_') {
                return true;
            }
            if ch == '.'
                && (self.raw_input.eq_ignore_ascii_case("www")
                    || self
                        .current_candidates
                        .first()
                        .is_none_or(|c| c.text.is_ascii()))
            {
                return true;
            }
        }
        false
    }

    fn build_passthrough_action(&mut self) -> InputAction {
        let page_size = TOKENS.ergonomics.desktop_candidate_page_size as usize;
        self.composing = self.raw_input.clone();
        self.cursor_utf8 = self.composing.len();
        let token = self.raw_input.clone();
        let item = CandidateItem {
            index: 0,
            text: token.clone(),
            comment: "En/Token".to_string(),
            pinned: false,
            is_clipboard_shortcut: false,
            shortcut_code: None,
            frecency_score: 0.0,
        };
        let origin = CandidateOrigin::Overlay {
            canonical: token.clone(),
            commit_text: token,
            is_clipboard: true,
        };
        self.set_current_candidates(vec![(item, origin)]);
        self.current_page = PageInfo {
            page_index: 0,
            page_size,
            has_prev: false,
            has_next: false,
        };
        self.empty_action(None)
    }

    fn handle_passthrough_or_paired_punct(
        &mut self,
        keycode: u32,
    ) -> Result<Option<InputAction>, VitralError> {
        let is_backspace = keycode == 0x08 || keycode == 0xFF08;
        let is_escape = keycode == 0x1B || keycode == 0xFF1B;
        let is_space = keycode == 0x20;
        let is_return = keycode == 0x0D || keycode == 0xFF0D;

        if self.is_passthrough_active() {
            if is_escape {
                self.clear_composition_state();
                return Ok(Some(self.empty_action(None)));
            }
            if is_backspace {
                self.raw_input.pop();
                if self.raw_input.is_empty() {
                    self.clear_composition_state();
                    return Ok(Some(self.empty_action(None)));
                }
                return Ok(Some(self.build_passthrough_action()));
            }
            if is_space || is_return {
                let committed = self.raw_input.clone();
                self.clear_composition_state();
                return Ok(Some(self.empty_action(Some(committed))));
            }
            if let Some(ch) = char::from_u32(keycode) {
                if !ch.is_control() {
                    self.raw_input.push(ch);
                    return Ok(Some(self.build_passthrough_action()));
                }
            }
            return Ok(Some(self.build_passthrough_action()));
        }

        if let Some(ch) = char::from_u32(keycode) {
            if !ch.is_control() && self.should_enter_passthrough(ch) {
                self.raw_input.push(ch);
                #[cfg(all(target_os = "linux", feature = "native-rime"))]
                if let (Some(sid), Some(ref assets_dir)) =
                    (self.rime_session_id, &self.resolved_assets_dir)
                {
                    let _ = rime_ffi::with_runtime(
                        assets_dir,
                        &self.config.cache_dir,
                        &self.config.data_dir,
                        |rt| {
                            rt.clear_composition(sid);
                            Ok(())
                        },
                    );
                }
                return Ok(Some(self.build_passthrough_action()));
            }

            if let Some(pair) = match_paired_punctuation(ch) {
                if ch != '\'' || self.raw_input.is_empty() {
                    let prefix = if !self.raw_input.is_empty() {
                        self.current_candidates.first().map(|c| c.text.clone())
                    } else {
                        None
                    };
                    self.clear_composition_state();
                    let committed = match prefix {
                        Some(p) => format!("{p}{pair}"),
                        None => pair.to_string(),
                    };
                    let mut act = self.empty_action(Some(committed));
                    act.paired_punctuation = Some(pair.to_string());
                    act.cursor_delta = -1;
                    return Ok(Some(act));
                }
            }
        }
        Ok(None)
    }

    fn confirmed_non_ascii_prefix(&self) -> String {
        self.composing
            .chars()
            .take_while(|c| !c.is_ascii())
            .collect()
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn sync_raw_input_from_preedit(&mut self, raw_preedit: &str) {
        if raw_preedit.is_ascii() {
            return;
        }
        let confirmed_chars = raw_preedit.chars().take_while(|c| !c.is_ascii()).count();
        let tail_start = raw_preedit
            .char_indices()
            .find(|(_, c)| c.is_ascii())
            .map(|(idx, _)| idx)
            .unwrap_or(raw_preedit.len());
        let tail_ascii = &raw_preedit[tail_start..];
        match self.schema {
            SchemaKind::RimeIce => {
                self.raw_input = tail_ascii
                    .chars()
                    .filter(|c| c.is_ascii_alphabetic() || *c == '\'')
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
            }
            SchemaKind::DoublePinyinFlypy => {
                let xh_tail = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(tail_ascii);
                let compact_raw: String = self
                    .raw_input
                    .chars()
                    .filter(|c| c.is_ascii_alphabetic())
                    .collect();
                if !xh_tail.is_empty() && compact_raw.ends_with(&xh_tail) {
                    self.raw_input = xh_tail;
                } else {
                    let drop_n = (confirmed_chars * 2).min(compact_raw.len());
                    self.raw_input = compact_raw[drop_n..].to_string();
                }
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn apply_rime_snapshot(
        &mut self,
        snap: rime_ffi::RimeSnapshot,
        is_return: bool,
        is_escape: bool,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> InputAction {
        if let Some(committed) = snap.commit_text {
            self.clear_composition_state();
            let converted = if is_return {
                committed
            } else {
                self.convert_opencc(&committed)
            };
            return self.empty_action(Some(converted));
        }

        if is_escape
            || (snap.preedit.is_empty() && snap.candidates.is_empty() && self.raw_input.is_empty())
        {
            self.clear_composition_state();
            return self.empty_action(None);
        }

        let raw_preedit = if snap.preedit.is_empty() {
            self.raw_input.clone()
        } else {
            snap.preedit
        };
        self.sync_raw_input_from_preedit(&raw_preedit);

        // Convert only any leading confirmed CJK prefix in preedit so ASCII pinyin stays verbatim
        self.composing = if let Some(ascii_idx) = raw_preedit.find(|c: char| c.is_ascii()) {
            let (prefix, rest) = raw_preedit.split_at(ascii_idx);
            if prefix.is_empty() {
                raw_preedit.clone()
            } else {
                format!("{}{}", self.convert_opencc(prefix), rest)
            }
        } else {
            self.convert_opencc(&raw_preedit)
        };
        self.cursor_utf8 = snap.cursor_utf8.min(self.composing.len());

        let page_size = TOKENS.ergonomics.desktop_candidate_page_size as usize;
        let now_ms = RetentionEngine::now_epoch_ms();
        let mut base_pairs: Vec<(CandidateItem, CandidateOrigin, usize)> = snap
            .candidates
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !retention.erased_words.contains(&c.text) && imported.matches_reading(&c.text, &self.raw_input, self.schema == SchemaKind::DoublePinyinFlypy))
            .map(|(rime_idx, c)| {
                let converted = self.convert_opencc(&c.text);
                let frec = retention.decayed_score_at(&c.text, now_ms);
                let pinned = retention.pinned_words.contains(&c.text);
                let item = CandidateItem {
                    index: 0,
                    text: converted.clone(),
                    comment: c.comment,
                    pinned,
                    is_clipboard_shortcut: false,
                    shortcut_code: None,
                    frecency_score: frec,
                };
                let origin = CandidateOrigin::Rime {
                    canonical: c.text,
                    commit_text: converted,
                    rime_page_idx: rime_idx,
                };
                (item, origin, rime_idx * 2)
            })
            .collect();

        if snap.page_index == 0 {
            let (mut priority_overlay, mut seen) =
                self.build_overlay_candidates(clipboard, imported, retention, true);

            // Inject retention-boosted / pinned PureRust matches on page 0 so Mode-2 HW
            // confirmations (e.g. "手写中文") participate in shared Frecency ranking, and
            // ensure at least one full-span alternative exists if Rime only surfaced one.
            let max_rime_len = base_pairs
                .iter()
                .map(|(it, _, _)| it.text.chars().count())
                .max()
                .unwrap_or(0);
            let full_span_rime_count = base_pairs
                .iter()
                .filter(|(it, _, _)| it.text.chars().count() == max_rime_len)
                .count();
            let need_full_span_alt = full_span_rime_count == 1 && max_rime_len >= 2;

            if !retention.frecency.is_empty()
                || !retention.pinned_words.is_empty()
                || need_full_span_alt
            {
                let mut seen_base: HashSet<String> =
                    base_pairs.iter().map(|(it, _, _)| it.text.clone()).collect();
                let pr = self.pure_rust.query_all_candidates_with_canonical(
                    &self.raw_input,
                    self.schema,
                    OpenCcMode::Simplified,
                    Some(imported),
                    Some(retention),
                );
                let mut added_full_alt = 0usize;
                for (offset, (pr_item, canonical)) in pr.into_iter().enumerate() {
                    if retention.erased_words.contains(&canonical) {
                        continue;
                    }
                    let frec = retention.decayed_score_at(&canonical, now_ms);
                    let pinned = retention.pinned_words.contains(&canonical);
                    let is_full_alt =
                        need_full_span_alt && added_full_alt == 0 && canonical.chars().count() >= max_rime_len;
                    if frec <= 0.0 && !pinned && !is_full_alt {
                        continue;
                    }
                    let converted = self.convert_opencc(&canonical);
                    if seen.contains(&converted) || !seen_base.insert(converted.clone()) {
                        continue;
                    }
                    let item = CandidateItem {
                        index: 0,
                        text: converted.clone(),
                        comment: pr_item.comment,
                        pinned,
                        is_clipboard_shortcut: false,
                        shortcut_code: None,
                        frecency_score: frec,
                    };
                    let origin = CandidateOrigin::PureRust {
                        canonical,
                        commit_text: converted,
                    };
                    if is_full_alt && frec <= 0.0 && !pinned {
                        // Insert right after Rime's full-span candidate #0 (rank 1) without
                        // perturbing unboosted rank 0.
                        let insert_pos = base_pairs.len().min(1);
                        base_pairs.insert(insert_pos, (item, origin, 1));
                        added_full_alt += 1;
                    } else {
                        base_pairs.push((item, origin, (base_pairs.len() + offset + 1) * 2));
                    }
                }
            }

            let mut full_span = HashSet::new();
            if self.schema == SchemaKind::RimeIce && self.raw_input.len() >= 4 {
                let corpus = imported.corpus();
                let mut scores = HashMap::new();
                let mut extra = None;
                let mut native_reading = String::new();
                for (idx, (_, origin, _)) in base_pairs.iter().enumerate() {
                    let word = origin.canonical();
                    if let Some((score, reading, alternative)) =
                        phrase_rank::evaluate(word, &self.raw_input, corpus, idx == 0)
                    {
                        if idx == 0 {
                            native_reading = reading.clone();
                        }
                        scores.insert(word.to_string(), (reading == native_reading, score));
                        extra = extra.or(alternative);
                    }
                }
                if let Some(word) = extra.filter(|w| {
                    !scores.contains_key(w)
                        && !retention.erased_words.contains(w)
                        && imported.matches_reading(w, &self.raw_input, false)
                }) {
                    if let Some((score, reading, _)) =
                        phrase_rank::evaluate(&word, &self.raw_input, corpus, false)
                    {
                        let text = self.convert_opencc(&word);
                        scores.insert(word.clone(), (reading == native_reading, score));
                        base_pairs.push((
                            CandidateItem {
                                index: 0,
                                text: text.clone(),
                                comment: String::new(),
                                pinned: retention.pinned_words.contains(&word),
                                is_clipboard_shortcut: false,
                                shortcut_code: None,
                                frecency_score: retention.decayed_score_at(&word, now_ms),
                            },
                            CandidateOrigin::PureRust {
                                canonical: word,
                                commit_text: text,
                            },
                            usize::MAX,
                        ));
                    }
                }
                if base_pairs
                    .first()
                    .is_some_and(|p| scores.contains_key(p.1.canonical()))
                {
                    let lexical_best = base_pairs
                        .iter()
                        .filter_map(|(_, candidate, _)| {
                            scores
                                .get(candidate.canonical())
                                .map(|&(same_reading, value)| (candidate, same_reading, value))
                        })
                        .max_by(|a, b| a.2.total_cmp(&b.2));
                    let prefer_native = lexical_best.is_some_and(|(best, same_reading, _)| {
                        !same_reading
                            && phrase_rank::context_favors_native(
                                base_pairs[0].1.canonical(),
                                best.canonical(),
                                self.pure_rust.get_gram(),
                            )
                    });
                    base_pairs.sort_by(|a, b| {
                        let score = |p: &(CandidateItem, CandidateOrigin, usize)| {
                            scores
                                .get(p.1.canonical())
                                .copied()
                                .unwrap_or((false, f64::NEG_INFINITY))
                        };
                        let (ma, sa) = score(a);
                        let (mb, sb) = score(b);
                        (prefer_native && mb)
                            .cmp(&(prefer_native && ma))
                            .then_with(|| sb.total_cmp(&sa))
                            .then_with(|| a.2.cmp(&b.2))
                    });
                    for (n, p) in base_pairs.iter_mut().enumerate() {
                        p.2 = n;
                    }
                    full_span.extend(scores.into_keys());
                }
            }

            let mut pinned_base = Vec::new();
            let mut normal_base = Vec::new();
            for pair in base_pairs.drain(..) {
                if pair.0.pinned {
                    pinned_base.push(pair);
                } else {
                    normal_base.push(pair);
                }
            }
            pinned_base.sort_by(|a, b| {
                b.0.frecency_score
                    .total_cmp(&a.0.frecency_score)
                    .then_with(|| a.2.cmp(&b.2))
            });
            normal_base.sort_by(|a, b| {
                full_span.contains(b.1.canonical()).cmp(&full_span.contains(a.1.canonical()))
                    .then_with(|| b.0.frecency_score.total_cmp(&a.0.frecency_score))
                    .then_with(|| a.2.cmp(&b.2))
            });

            for (c, orig, _) in pinned_base.into_iter().chain(normal_base) {
                if seen.insert(c.text.clone()) {
                    priority_overlay.push((c, orig));
                }
            }
            priority_overlay.truncate(page_size.max(5));
            self.set_current_candidates(priority_overlay);
        } else {
            let mut pinned_base = Vec::new();
            let mut normal_base = Vec::new();
            for (c, orig, idx) in base_pairs.drain(..) {
                if c.pinned {
                    pinned_base.push((c, orig, idx));
                } else {
                    normal_base.push((c, orig, idx));
                }
            }
            pinned_base.sort_by(|a, b| {
                b.0.frecency_score
                    .total_cmp(&a.0.frecency_score)
                    .then_with(|| a.2.cmp(&b.2))
            });
            normal_base.sort_by(|a, b| {
                b.0.frecency_score
                    .total_cmp(&a.0.frecency_score)
                    .then_with(|| a.2.cmp(&b.2))
            });
            let ordered: Vec<(CandidateItem, CandidateOrigin)> = pinned_base
                .into_iter()
                .chain(normal_base)
                .map(|(c, orig, _)| (c, orig))
                .take(page_size.max(5))
                .collect();
            self.set_current_candidates(ordered);
        }

        self.current_page = PageInfo {
            page_index: snap.page_index,
            page_size,
            has_prev: snap.has_prev,
            has_next: snap.has_next,
        };

        self.empty_action(None)
    }

    fn set_current_candidates(&mut self, pairs: Vec<(CandidateItem, CandidateOrigin)>) {
        self.current_candidates.clear();
        self.candidate_origins.clear();
        for (i, (mut item, orig)) in pairs.into_iter().enumerate() {
            item.index = i;
            self.current_candidates.push(item);
            self.candidate_origins.push(orig);
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn process_key_native_rime(
        &mut self,
        assets_dir: &Path,
        keycode: u32,
        modifiers: u32,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        if self.rime_session_id.is_none() && self.schema == SchemaKind::RimeIce {
            imported.corpus();
        }
        let sid = self.ensure_rime_session(assets_dir)?;

        let is_backspace = keycode == 0x08 || keycode == 0xFF08;
        let is_escape = keycode == 0x1B || keycode == 0xFF1B;
        let is_space = keycode == 0x20;
        let is_return = keycode == 0x0D || keycode == 0xFF0D;

        // Space commit: handle overlay, PureRust, or Rime candidate #0 directly
        if is_space && !self.raw_input.is_empty() {
            if let Some(origin) = self.candidate_origins.first().cloned() {
                match origin {
                    CandidateOrigin::Overlay {
                        canonical,
                        commit_text,
                        is_clipboard,
                    } => {
                        if !is_clipboard {
                            retention.record_selection(&canonical, 0.0, 0.001, 1.0);
                        }
                        let prefix = self.confirmed_non_ascii_prefix();
                        let final_commit = if !is_clipboard && !prefix.is_empty() {
                            format!("{}{}", self.convert_opencc(&prefix), commit_text)
                        } else {
                            commit_text
                        };
                        self.clear_composition_state();
                        return Ok(self.empty_action(Some(final_commit)));
                    }
                    CandidateOrigin::PureRust {
                        canonical,
                        commit_text,
                    } => {
                        retention.record_selection(&canonical, 0.0, 0.001, 1.0);
                        let prefix = self.confirmed_non_ascii_prefix();
                        let final_commit = if !prefix.is_empty() {
                            format!("{}{}", self.convert_opencc(&prefix), commit_text)
                        } else {
                            commit_text
                        };
                        self.clear_composition_state();
                        return Ok(self.empty_action(Some(final_commit)));
                    }
                    CandidateOrigin::Rime {
                        canonical,
                        rime_page_idx,
                        ..
                    } => {
                        let snap = rime_ffi::with_runtime(
                            assets_dir,
                            &self.config.cache_dir,
                            &self.config.data_dir,
                            |rt| rt.select_candidate_on_current_page(sid, rime_page_idx),
                        )?;
                        retention.record_selection(&canonical, 0.0, 0.001, 1.0);
                        return Ok(self.apply_rime_snapshot(
                            snap, is_return, false, clipboard, imported, retention,
                        ));
                    }
                }
            }
        }

        // Digit '1'..='5' selection while composing: respect overlay offset & Rime menu mapping
        if !self.raw_input.is_empty() {
            if let Some(ch) = char::from_u32(keycode) {
                if ch.is_ascii_digit() && ch != '0' {
                    let digit = ch.to_digit(10).unwrap_or(0) as usize;
                    let page_size = self.current_page.page_size.max(5);
                    if (1..=page_size).contains(&digit) {
                        if let Some(origin) = self.candidate_origins.get(digit - 1).cloned() {
                            match origin {
                                CandidateOrigin::Overlay {
                                    canonical,
                                    commit_text,
                                    is_clipboard,
                                } => {
                                    if !is_clipboard {
                                        retention.record_selection(&canonical, 0.0, 0.001, 1.0);
                                    }
                                    let prefix = self.confirmed_non_ascii_prefix();
                                    let final_commit = if !is_clipboard && !prefix.is_empty() {
                                        format!("{}{}", self.convert_opencc(&prefix), commit_text)
                                    } else {
                                        commit_text
                                    };
                                    self.clear_composition_state();
                                    return Ok(self.empty_action(Some(final_commit)));
                                }
                                CandidateOrigin::Rime {
                                    canonical,
                                    rime_page_idx,
                                    ..
                                } => {
                                    let snap = rime_ffi::with_runtime(
                                        assets_dir,
                                        &self.config.cache_dir,
                                        &self.config.data_dir,
                                        |rt| rt.select_candidate_on_current_page(sid, rime_page_idx),
                                    )?;
                                    retention.record_selection(&canonical, 0.0, 0.001, 1.0);
                                    return Ok(self.apply_rime_snapshot(
                                        snap, false, false, clipboard, imported, retention,
                                    ));
                                }
                                CandidateOrigin::PureRust {
                                    canonical,
                                    commit_text,
                                } => {
                                    retention.record_selection(&canonical, 0.0, 0.001, 1.0);
                                    let prefix = self.confirmed_non_ascii_prefix();
                                    let final_commit = if !prefix.is_empty() {
                                        format!("{}{}", self.convert_opencc(&prefix), commit_text)
                                    } else {
                                        commit_text
                                    };
                                    self.clear_composition_state();
                                    return Ok(self.empty_action(Some(final_commit)));
                                }
                            }
                        }
                    }
                }
            }
        }

        if is_backspace {
            self.raw_input.pop();
        } else if is_escape {
            self.raw_input.clear();
        } else if let Some(ch) = char::from_u32(keycode) {
            if ch.is_ascii_alphabetic() || ch == '\'' {
                self.raw_input.push(ch.to_ascii_lowercase());
            } else if !ch.is_control() && self.raw_input.is_empty() && self.composing.is_empty() {
                let snap = rime_ffi::with_runtime(
                    assets_dir,
                    &self.config.cache_dir,
                    &self.config.data_dir,
                    |rt| rt.process_key(sid, keycode, modifiers),
                )?;
                if let Some(committed) = snap.commit_text {
                    let converted = self.convert_opencc(&committed);
                    return Ok(self.empty_action(Some(converted)));
                }
                return Ok(self.empty_action(Some(ch.to_string())));
            }
        }

        let snap = rime_ffi::with_runtime(
            assets_dir,
            &self.config.cache_dir,
            &self.config.data_dir,
            |rt| rt.process_key(sid, keycode, modifiers),
        )?;

        Ok(self.apply_rime_snapshot(
            snap, is_return, is_escape, clipboard, imported, retention,
        ))
    }

    fn process_key_pure_rust(
        &mut self,
        keycode: u32,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        let page_size = TOKENS.ergonomics.desktop_candidate_page_size as usize;

        if keycode == 0x08 || keycode == 0xFF08 {
            self.raw_input.pop();
            self.pure_rust.page_index = 0;
        } else if keycode == 0x1B || keycode == 0xFF1B {
            self.clear_composition_state();
            return Ok(self.empty_action(None));
        } else if keycode == 0x20 {
            if !self.raw_input.is_empty() {
                if let Some(origin) = self.candidate_origins.first().cloned() {
                    let is_clip = matches!(
                        origin,
                        CandidateOrigin::Overlay {
                            is_clipboard: true,
                            ..
                        }
                    );
                    if !is_clip {
                        retention.record_selection(origin.canonical(), 0.0, 0.001, 1.0);
                    }
                }
                let commit = self
                    .current_candidates
                    .first()
                    .map(|c| c.text.clone())
                    .unwrap_or_else(|| std::mem::take(&mut self.raw_input));
                self.clear_composition_state();
                return Ok(self.empty_action(Some(commit)));
            }
            return Ok(self.empty_action(Some(" ".to_string())));
        } else if keycode == 0x0D || keycode == 0xFF0D {
            let committed = if self.raw_input.is_empty() {
                None
            } else {
                Some(self.raw_input.clone())
            };
            self.clear_composition_state();
            return Ok(self.empty_action(committed));
        } else if keycode == 0xFF55 || keycode == ('-' as u32) {
            if !self.raw_input.is_empty() && self.pure_rust.page_index > 0 {
                self.pure_rust.page_index -= 1;
            }
        } else if keycode == 0xFF56 || keycode == ('=' as u32) {
            if !self.raw_input.is_empty() && self.current_page.has_next {
                self.pure_rust.page_index += 1;
            }
        } else if let Some(ch) = char::from_u32(keycode) {
            if ch.is_ascii_alphabetic() || ch == '\'' {
                self.raw_input.push(ch.to_ascii_lowercase());
                self.pure_rust.page_index = 0;
            } else if ch.is_ascii_digit() && !self.raw_input.is_empty() {
                let digit = ch.to_digit(10).unwrap_or(0) as usize;
                if (1..=page_size).contains(&digit) {
                    if let Some(cand) = self.current_candidates.get(digit - 1).cloned() {
                        if let Some(origin) = self.candidate_origins.get(digit - 1).cloned() {
                            let is_clip = matches!(
                                origin,
                                CandidateOrigin::Overlay {
                                    is_clipboard: true,
                                    ..
                                }
                            );
                            if !is_clip {
                                retention.record_selection(origin.canonical(), 0.0, 0.001, 1.0);
                            }
                        }
                        self.clear_composition_state();
                        return Ok(self.empty_action(Some(cand.text)));
                    }
                }
            } else if !ch.is_control() && self.raw_input.is_empty() {
                return Ok(self.empty_action(Some(ch.to_string())));
            }
        }

        if self.raw_input.is_empty() {
            self.clear_composition_state();
            return Ok(self.empty_action(None));
        }

        let base_pairs = self.pure_rust.query_all_candidates_with_canonical(
            &self.raw_input,
            self.schema,
            self.opencc_mode,
            Some(imported),
            Some(retention),
        );

        let (mut merged, mut seen) =
            self.build_overlay_candidates(clipboard, imported, retention, false);
        for (item, canonical) in base_pairs {
            let converted = self.convert_opencc(&canonical);
            if seen.insert(converted.clone()) {
                let mut item = item;
                item.text = converted.clone();
                let origin = CandidateOrigin::PureRust {
                    canonical,
                    commit_text: converted,
                };
                merged.push((item, origin));
            }
        }

        let total_pages = merged.len().div_ceil(page_size).max(1);
        if self.pure_rust.page_index >= total_pages {
            self.pure_rust.page_index = 0;
        }
        let start = self.pure_rust.page_index * page_size;
        let end = (start + page_size).min(merged.len());
        let page_pairs: Vec<(CandidateItem, CandidateOrigin)> =
            merged.into_iter().skip(start).take(end - start).collect();

        self.composing = self.raw_input.clone();
        self.cursor_utf8 = self.composing.len();
        self.set_current_candidates(page_pairs);
        self.current_page = PageInfo {
            page_index: self.pure_rust.page_index,
            page_size,
            has_prev: self.pure_rust.page_index > 0,
            has_next: self.pure_rust.page_index + 1 < total_pages,
        };
        Ok(self.empty_action(None))
    }

    fn build_overlay_candidates(
        &self,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
        include_embedded_sentence: bool,
    ) -> (Vec<(CandidateItem, CandidateOrigin)>, HashSet<String>) {
        let mut overlay: Vec<(CandidateItem, CandidateOrigin)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        if let Some(clip) = clipboard.find_shortcut(&self.raw_input) {
            if seen.insert(clip.content.clone()) {
                let item = CandidateItem {
                    index: 0,
                    text: clip.content.clone(),
                    comment: format!("📌 {}", self.raw_input),
                    pinned: true,
                    is_clipboard_shortcut: true,
                    shortcut_code: clip.shortcut_code.clone(),
                    frecency_score: 1000.0,
                };
                let origin = CandidateOrigin::Overlay {
                    canonical: clip.content.clone(),
                    commit_text: clip.content.clone(),
                    is_clipboard: true,
                };
                overlay.push((item, origin));
            }
        }

        if let Some((_, macro_items)) = expand_time_macro(&self.raw_input) {
            for (idx, (val, label)) in macro_items.into_iter().enumerate() {
                if seen.insert(val.clone()) {
                    let item = CandidateItem {
                        index: 0,
                        text: val.clone(),
                        comment: format!("🕒 {label}"),
                        pinned: false,
                        is_clipboard_shortcut: false,
                        shortcut_code: None,
                        frecency_score: 500.0 - (idx as f64),
                    };
                    let origin = CandidateOrigin::Overlay {
                        canonical: val.clone(),
                        commit_text: val,
                        is_clipboard: true,
                    };
                    overlay.push((item, origin));
                }
            }
        }

        if self.schema == SchemaKind::DoublePinyinFlypy {
            for hint in &self.lookup_hints {
                if hint.xiaohe_code != self.raw_input || retention.erased_words.contains(&hint.ch) {
                    continue;
                }
                let text = self.convert_opencc(&hint.ch);
                if seen.insert(text.clone()) {
                    overlay.push((CandidateItem {
                        index: 0, text: text.clone(), comment: hint.toned_pinyin.clone(),
                        pinned: retention.pinned_words.contains(&hint.ch),
                        frecency_score: retention.decayed_score_at(&hint.ch, RetentionEngine::now_epoch_ms()),
                        is_clipboard_shortcut: false, shortcut_code: None,
                    }, CandidateOrigin::Overlay {canonical: hint.ch.clone(), commit_text: text, is_clipboard: false}));
                }
            }
        }

        if !imported.is_empty() {
            let compact: String = self
                .raw_input
                .chars()
                .filter(|c| c.is_ascii_alphabetic())
                .collect();
            let imp_matches = match self.schema {
                SchemaKind::RimeIce => imported.query_quanpin(&compact),
                SchemaKind::DoublePinyinFlypy => imported.query_xiaohe(&compact),
            };
            for imp in imp_matches {
                if retention.erased_words.contains(&imp.text) {
                    continue;
                }
                let converted = self.convert_opencc(&imp.text);
                if seen.insert(converted.clone()) {
                    let item = CandidateItem {
                        index: 0,
                        text: converted.clone(),
                        comment: imp.toned_pinyin.clone(),
                        pinned: retention.pinned_words.contains(&imp.text),
                        is_clipboard_shortcut: false,
                        shortcut_code: None,
                        frecency_score: (imp.weight as f64) + 50.0,
                    };
                    let origin = CandidateOrigin::Overlay {
                        canonical: imp.text,
                        commit_text: converted,
                        is_clipboard: false,
                    };
                    overlay.push((item, origin));
                }
            }

            // On native-rime, also inject sentence-embedded imported hotwords via pure-Rust DAG
            if include_embedded_sentence
                && imported.has_embedded_hotword_key(
                    &compact,
                    self.schema == SchemaKind::DoublePinyinFlypy,
                )
            {
                let decoded = self.pure_rust.query_all_candidates_with_canonical(
                    &self.raw_input,
                    self.schema,
                    OpenCcMode::Simplified,
                    Some(imported),
                    Some(retention),
                );
                for (item, canonical) in decoded
                    .into_iter()
                    .filter(|(_, c)| imported.contains_hotword(c) && !retention.erased_words.contains(c))
                    .take(3)
                {
                    let converted = self.convert_opencc(&canonical);
                    if seen.insert(converted.clone()) {
                        let ov_item = CandidateItem {
                            index: 0,
                            text: converted.clone(),
                            comment: item.comment,
                            pinned: retention.pinned_words.contains(&canonical),
                            is_clipboard_shortcut: false,
                            shortcut_code: None,
                            frecency_score: item.frecency_score + 50.0,
                        };
                        let origin = CandidateOrigin::Overlay {
                            canonical,
                            commit_text: converted,
                            is_clipboard: false,
                        };
                        overlay.push((ov_item, origin));
                    }
                }
            }
        }

        (overlay, seen)
    }

    pub(crate) fn select_candidate_on_backend(
        &mut self,
        index: usize,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> Result<Option<String>, VitralError> {
        let origin = self
            .candidate_origins
            .get(index)
            .cloned()
            .ok_or_else(|| VitralError::NotFound(format!("candidate index {index} out of range")))?;

        #[cfg(not(all(target_os = "linux", feature = "native-rime")))]
        let _ = (clipboard, imported, retention);

        match origin {
            CandidateOrigin::Overlay {
                commit_text,
                is_clipboard,
                ..
            } => {
                let prefix = self.confirmed_non_ascii_prefix();
                let final_commit = if !is_clipboard && !prefix.is_empty() {
                    format!("{}{}", self.convert_opencc(&prefix), commit_text)
                } else {
                    commit_text
                };
                self.clear_composition_state();
                Ok(Some(final_commit))
            }
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            CandidateOrigin::Rime {
                rime_page_idx,
                commit_text,
                ..
            } => {
                if let (Some(sid), Some(ref assets_dir)) =
                    (self.rime_session_id, &self.resolved_assets_dir)
                {
                    let snap = rime_ffi::with_runtime(
                        assets_dir,
                        &self.config.cache_dir,
                        &self.config.data_dir,
                        |rt| rt.select_candidate_on_current_page(sid, rime_page_idx),
                    )?;
                    if let Some(committed) = snap.commit_text {
                        self.clear_composition_state();
                        return Ok(Some(self.convert_opencc(&committed)));
                    }
                    if !snap.preedit.is_empty() || !snap.candidates.is_empty() {
                        let _ = self.apply_rime_snapshot(
                            snap, false, false, clipboard, imported, retention,
                        );
                        return Ok(None);
                    }
                }
                self.clear_composition_state();
                Ok(Some(commit_text))
            }
            CandidateOrigin::PureRust { commit_text, .. } => {
                let prefix = self.confirmed_non_ascii_prefix();
                let final_commit = if !prefix.is_empty() {
                    format!("{}{}", self.convert_opencc(&prefix), commit_text)
                } else {
                    commit_text
                };
                self.clear_composition_state();
                Ok(Some(final_commit))
            }
        }
    }

    pub(crate) fn pin_candidate_in_state(
        &mut self,
        index: usize,
        retention: &mut RetentionEngine,
    ) -> Result<(), VitralError> {
        if index >= self.current_candidates.len() {
            return Err(VitralError::NotFound(format!(
                "candidate index {index} out of range"
            )));
        }
        let canonical = self
            .candidate_origins
            .get(index)
            .map(CandidateOrigin::canonical)
            .map(str::to_string)
            .unwrap_or_else(|| self.current_candidates[index].text.clone());
        retention.pin_word(&canonical);
        let mut item = self.current_candidates.remove(index);
        item.pinned = true;
        self.current_candidates.insert(0, item);
        if index < self.candidate_origins.len() {
            let orig = self.candidate_origins.remove(index);
            self.candidate_origins.insert(0, orig);
        }
        for (i, c) in self.current_candidates.iter_mut().enumerate() {
            c.index = i;
        }
        Ok(())
    }

    pub(crate) fn erase_candidate_in_state(
        &mut self,
        index: usize,
        retention: &mut RetentionEngine,
    ) -> Result<(), VitralError> {
        if index >= self.current_candidates.len() {
            return Err(VitralError::NotFound(format!(
                "candidate index {index} out of range"
            )));
        }
        let canonical = self
            .candidate_origins
            .get(index)
            .map(CandidateOrigin::canonical)
            .map(str::to_string)
            .unwrap_or_else(|| self.current_candidates[index].text.clone());
        retention.erase_word(&canonical);
        self.current_candidates.remove(index);
        if index < self.candidate_origins.len() {
            self.candidate_origins.remove(index);
        }
        for (i, c) in self.current_candidates.iter_mut().enumerate() {
            c.index = i;
        }
        Ok(())
    }

    pub fn empty_action(&self, commit_text: Option<String>) -> InputAction {
        let page_size = self.current_page.page_size.max(1);
        let page_items: Vec<CandidateItem> = self
            .current_candidates
            .iter()
            .take(page_size)
            .cloned()
            .collect();
        let macro_expanded = if commit_text.is_none() {
            expand_time_macro(&self.raw_input)
                .and_then(|(_, items)| items.into_iter().next().map(|(v, _)| v))
        } else {
            None
        };
        InputAction {
            lang_mode: self.lang_mode,
            schema: self.schema.as_str().to_string(),
            preedit: PreeditState {
                text: self.composing.clone(),
                cursor_utf8: self.cursor_utf8.min(self.composing.len()),
                retroactive_slots: Vec::new(),
            },
            commit_text,
            candidates: page_items,
            page: self.current_page.clone(),
            paired_punctuation: None,
            cursor_delta: 0,
            macro_expanded,
            voice_trigger: false,
        }
    }
}
