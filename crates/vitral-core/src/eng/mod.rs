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
    #[serde(default = "crate::shortcuts::defaults")]
    pub shortcuts: Vec<crate::shortcuts::ShortcutBinding>,
    #[serde(default = "default_enabled")]
    pub linux_english_mode: bool,
    #[serde(default = "default_enabled")]
    pub adaptive_learning: bool,
    #[serde(default = "default_enabled")]
    pub learn_from_clipboard: bool,
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
    pub fn shortcut_enabled(&self, action: crate::shortcuts::ShortcutAction) -> bool {
        !(cfg!(target_os = "linux") && !self.linux_english_mode && action == crate::shortcuts::ShortcutAction::ToggleLanguage)
    }

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

fn default_enabled() -> bool { true }

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
            shortcuts: crate::shortcuts::defaults(),
            linux_english_mode: true,
            adaptive_learning: true,
            learn_from_clipboard: true,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_paste_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_action: Option<crate::shortcuts::ShortcutAction>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_action: Option<crate::shortcuts::ShortcutAction>,
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
        features: [f64; 9],
        canonical: String,
        commit_text: String,
        is_clipboard: bool,
    },
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    Rime {
        features: [f64; 9],
        canonical: String,
        commit_text: String,
        rime_page_idx: usize,
    },
    PureRust {
        features: [f64; 9],
        canonical: String,
        commit_text: String,
    },
}

impl CandidateOrigin {
    fn features(&self) -> &[f64; 9] {
        match self {
            Self::Overlay { features, .. } | Self::PureRust { features, .. } => features,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            Self::Rime { features, .. } => features,
        }
    }

    fn set_features(&mut self, value: [f64; 9]) {
        match self {
            Self::Overlay { features, .. } | Self::PureRust { features, .. } => *features = value,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            Self::Rime { features, .. } => *features = value,
        }
    }

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
    pub last_committed_word: Option<String>,
    pub last_committed_code: Option<String>,
    pub last_committed_ms: u64,
    pub last_committed_char: Option<char>,
    pub regret_backspaces: usize,
    last_commit_context: Option<String>,
    context_window: Vec<char>,
    cached_context_vec: Option<[i8; 32]>,
    cached_mlm_priors: HashMap<char, f32>,
    pub(crate) candidate_features: Vec<[f64; 9]>,
    last_commit_features: Option<[f64; 9]>,
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
    rime_first_page: Vec<(CandidateItem, CandidateOrigin)>,
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    rime_overlay_page: usize,
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    rime_first_has_next: bool,
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
            last_committed_word: None,
            last_committed_code: None,
            last_committed_ms: 0,
            last_committed_char: None,
            regret_backspaces: 0,
            last_commit_context: None,
            context_window: Vec::new(),
            cached_context_vec: None,
            cached_mlm_priors: HashMap::new(),
            candidate_features: Vec::new(),
            last_commit_features: None,
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
            rime_first_page: Vec::new(),
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            rime_overlay_page: 0,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            rime_first_has_next: false,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            rime_session_id: None,
        }
    }

    pub(crate) fn refresh_candidates(
        &mut self, clipboard: &ClipboardManager, imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        if self.raw_input.is_empty() && self.composing.is_empty() {
            let (pairs, _) = self.build_overlay_candidates(clipboard, imported, retention, false);
            self.set_current_candidates(pairs.into_iter().filter(|(c, _)| c.quick_paste_id.is_some()).collect());
            return Ok(self.empty_action(None));
        }
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let (Some(sid), Some(assets)) = (self.rime_session_id, self.resolved_assets_dir.clone()) {
            let snap = rime_ffi::with_runtime(&assets, &self.config.cache_dir, &self.config.data_dir, |rt| rt.snapshot(sid))?;
            return Ok(self.apply_rime_snapshot(snap, false, false, clipboard, imported, retention));
        }
        self.process_key_pure_rust(0, clipboard, imported, retention)
    }

    pub(crate) fn reset_commit_context(&mut self) {
        self.context_window.clear();
        self.cached_context_vec = None;
        self.cached_mlm_priors.clear();
        self.last_commit_features = None;
        self.last_committed_word = None;
        self.last_committed_code = None;
        self.last_commit_context = None;
        self.regret_backspaces = 0;
    }

    fn previous_context(&self, now_ms: u64) -> Option<&str> {
        (now_ms.saturating_sub(self.last_committed_ms) <= 6000)
            .then_some(self.last_committed_word.as_deref())
            .flatten()
    }

    pub(crate) fn is_clipboard_candidate(&self, index: usize) -> bool {
        matches!(
            self.candidate_origins.get(index),
            Some(CandidateOrigin::Overlay {
                is_clipboard: true,
                ..
            })
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_candidate_commit(
        &mut self,
        canonical: &str,
        code: &str,
        feedback: Option<([f64; 9], Option<[f64; 9]>)>,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
        now_ms: u64,
    ) {
        let prev = self.previous_context(now_ms).map(str::to_string);
        let correction = self.config.adaptive_learning
            && self.regret_backspaces > 0
            && self.last_committed_code.as_deref() == Some(code)
            && self
                .last_committed_word
                .as_deref()
                .is_some_and(|w| w != canonical)
            && now_ms.saturating_sub(self.last_committed_ms) <= 6000;
        let chosen = feedback.map(|f| f.0).unwrap_or_else(|| {
            retention.extract_candidate_features(
                canonical,
                code,
                prev.as_deref(),
                self.active_app_id.as_deref(),
                now_ms,
                self.config.adaptive_learning,
                self.cached_context_vec.as_ref(),
                Some(&self.cached_mlm_priors),
                None,
            )
        });
        if self.config.adaptive_learning {
            let skipped = if correction {
                self.last_commit_features
            } else {
                feedback.and_then(|f| f.1)
            };
            if let Some(skipped) = skipped {
                retention.update_reranker_pairwise(&chosen, &skipped, now_ms);
            }
            if !canonical.is_empty() && canonical.chars().all(crate::ret::is_cjk) {
                if let Some(vector) = self.cached_context_vec {
                    retention.record_semantic_memory(
                        vector,
                        canonical,
                        self.active_app_id.as_deref(),
                        now_ms,
                    );
                }
            }
        }
        if correction {
            retention.record_correction(
                code,
                self.last_committed_word.as_deref().unwrap(),
                canonical,
                self.last_commit_context.as_deref(),
                self.active_app_id.as_deref(),
                now_ms,
            );
        } else {
            retention.record_contextual_selection(
                canonical,
                code,
                prev.as_deref(),
                self.active_app_id.as_deref(),
                now_ms,
                1.0,
                self.config.adaptive_learning,
            );
            if self.config.adaptive_learning
                && self.regret_backspaces == 0
                && now_ms.saturating_sub(self.last_committed_ms) <= 4500
            {
                if let Some(previous) = prev.as_deref() {
                    let combined = format!("{previous}{canonical}");
                    if (2..=6).contains(&combined.chars().count())
                        && combined.chars().all(crate::ret::is_cjk)
                    {
                        let ann = crate::imp::polyphone::PolyphoneAnnotator::new(
                            std::sync::Arc::clone(imported.corpus()),
                        )
                        .annotate_text(&combined);
                        let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&ann.untoned);
                        retention.observe_phrase_composition(
                            &combined,
                            &xiaohe,
                            &ann.untoned,
                            now_ms,
                        );
                    }
                }
            }
        }
        if self.config.adaptive_learning
            && (2..=6).contains(&canonical.chars().count())
            && canonical.chars().all(crate::ret::is_cjk)
            && (retention.frecency.get(canonical).copied().unwrap_or(0.0) >= 2.0
                || retention.pinned_words.contains(canonical))
            && !retention
                .user_phrases
                .get(canonical)
                .is_some_and(|p| p.source == "typed")
        {
            let ann = crate::imp::polyphone::PolyphoneAnnotator::new(std::sync::Arc::clone(
                imported.corpus(),
            ))
            .annotate_text(canonical);
            let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&ann.untoned);
            retention.register_typed_phrase(canonical, &ann.untoned, &xiaohe, now_ms);
        }
        if !correction {
            self.last_commit_context = prev;
        }
        self.last_committed_word = Some(canonical.to_string());
        self.last_committed_code = Some(code.to_string());
        self.last_committed_ms = now_ms;
        self.regret_backspaces = 0;
        if !canonical.is_empty() && canonical.chars().all(crate::ret::is_cjk) {
            self.context_window.extend(canonical.chars());
            if self.context_window.len() > 16 {
                self.context_window.drain(..self.context_window.len() - 16);
            }
            self.refresh_semantic_context();
            self.last_commit_features = Some(chosen);
        } else {
            self.reset_commit_context();
        }
    }

    fn learn_current_candidate(
        &mut self,
        canonical: &str,
        selected_index: usize,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) {
        let code = self.raw_input.clone();
        self.record_candidate_commit(
            canonical,
            &code,
            self.selection_feedback(selected_index),
            imported,
            retention,
            RetentionEngine::now_epoch_ms(),
        );
    }

    pub(crate) fn selection_feedback(&self, index: usize) -> Option<([f64; 9], Option<[f64; 9]>)> {
        self.candidate_features.get(index).copied().map(|chosen| {
            let skipped = (index > 0 && !self.is_clipboard_candidate(0))
                .then(|| self.candidate_features.first().copied())
                .flatten();
            (chosen, skipped)
        })
    }

    fn refresh_semantic_context(&mut self) {
        self.cached_context_vec = None;
        self.cached_mlm_priors.clear();
        if !self.config.adaptive_learning || self.context_window.is_empty() {
            return;
        }
        if self.resolved_assets_dir.is_some() && self.context_window.len() >= 2 {
            let bundle = crate::hw::onnx_models::HwOnnxBundle::load_cached(
                self.resolved_assets_dir.as_deref(),
            );
            if bundle.has_reranker_onnx() {
                if let Some((vector, priors)) = bundle.encode_rbt3_context(&self.context_window) {
                    self.cached_context_vec = Some(vector);
                    self.cached_mlm_priors = priors;
                    return;
                }
            }
        }
        self.cached_context_vec = Some(RetentionEngine::project_context_fallback(
            &self.context_window.iter().collect::<String>(),
        ));
    }

    fn features_for(
        &self,
        retention: &RetentionEngine,
        word: &str,
        now_ms: u64,
        rank: Option<usize>,
    ) -> [f64; 9] {
        retention.extract_candidate_features(
            word,
            &self.raw_input,
            self.previous_context(now_ms),
            self.active_app_id.as_deref(),
            now_ms,
            self.config.adaptive_learning,
            self.cached_context_vec.as_ref(),
            Some(&self.cached_mlm_priors),
            rank,
        )
    }

    fn contextual_score(&self, retention: &RetentionEngine, word: &str, now_ms: u64) -> f64 {
        retention.score_candidate_features(
            &self.features_for(retention, word, now_ms, None),
            self.config.adaptive_learning,
        )
    }

    fn score_pair(
        &self,
        item: &mut CandidateItem,
        origin: &mut CandidateOrigin,
        retention: &RetentionEngine,
        now_ms: u64,
        rank: usize,
    ) {
        if matches!(
            origin,
            CandidateOrigin::Overlay {
                is_clipboard: true,
                ..
            }
        ) {
            return;
        }
        let features = self.features_for(retention, origin.canonical(), now_ms, Some(rank));
        let score = retention.score_candidate_features(&features, self.config.adaptive_learning);
        if matches!(origin, CandidateOrigin::Overlay { .. }) {
            item.frecency_score +=
                score - self.contextual_score(retention, origin.canonical(), now_ms);
        } else {
            item.frecency_score = score;
        }
        origin.set_features(features);
    }

    pub(crate) fn canonical_at(&self, index: usize) -> Option<&str> {
        self.candidate_origins.get(index).map(CandidateOrigin::canonical)
    }

    pub(crate) fn clear_composition_state(&mut self) {
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            self.rime_first_page.clear();
            self.rime_overlay_page = 0;
        }
        self.composing.clear();
        self.raw_input.clear();
        self.cursor_utf8 = 0;
        self.current_candidates.clear();
        self.candidate_origins.clear();
        self.candidate_features.clear();
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
        if self.active_app_id.as_deref() != Some(app_id) {
            self.reset_commit_context();
            self.last_committed_char = None;
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
        // Fcitx uses X11 keysyms; normalize editing keys before interpreting
        // printable text. In particular, 0xff08 is BackSpace, not Unicode '（'.
        let keycode = match keycode {
            0xff08 => 0x08,
            0xff0d => 0x0d,
            0xff1b => 0x1b,
            _ => keycode,
        };
        let now_ms = RetentionEngine::now_epoch_ms();
        if keycode == 0x1b {
            self.reset_commit_context();
            self.last_committed_char = None;
        }
        if keycode == 0x08
            && self.raw_input.is_empty()
            && self.last_committed_word.is_some()
            && now_ms.saturating_sub(self.last_committed_ms) <= 5000
        {
            self.regret_backspaces = self.regret_backspaces.saturating_add(1);
        }
        if keycode == 0x0d {
            if self.config.adaptive_learning {
                retention.record_english_casing(&self.raw_input);
            }
            self.reset_commit_context();
        }
        if char::from_u32(keycode).is_some_and(|c| {
            !c.is_control() && !c.is_ascii_alphanumeric() && c != '\'' && !crate::ret::is_cjk(c)
        }) && (keycode != 0x20 || self.raw_input.is_empty())
        {
            self.reset_commit_context();
        }
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

        if self.raw_input.is_empty() && self.composing.is_empty()
            && matches!(keycode, 0x2e | 0x3a)
            && self.last_committed_char.is_some_and(|c| c.is_ascii_digit())
        {
            return Ok(self.empty_action(char::from_u32(keycode).map(|c| c.to_string())));
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
            quick_paste_id: None,
            function_action: None,
            shortcut_code: None,
            frecency_score: 0.0,
        };
        let origin = CandidateOrigin::Overlay {
            features: [0.0; 9],
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
                let frec = self.contextual_score(retention, &c.text, now_ms);
                let pinned = retention.pinned_words.contains(&c.text);
                let item = CandidateItem {
                    index: 0,
                    text: converted.clone(),
                    comment: c.comment,
                    pinned,
                    is_clipboard_shortcut: false,
                    quick_paste_id: None,
                    function_action: None,
                    shortcut_code: None,
                    frecency_score: frec,
                };
                let origin = CandidateOrigin::Rime {
                    features: [0.0; 9],
                    canonical: c.text,
                    commit_text: converted,
                    rime_page_idx: rime_idx,
                };
                (item, origin, rime_idx * 2)
            })
            .collect();

        for (item, origin, rank) in &mut base_pairs {
            self.score_pair(item, origin, retention, now_ms, *rank / 2);
        }
        if snap.page_index == 0 {
            let (mut priority_overlay, mut seen) =
                self.build_overlay_candidates(clipboard, imported, retention, true);
            // Learned full readings share Frecency ranking with native alternatives.
            // Automatically registered dictionary words must not become permanent
            // priority overlays merely because they were selected twice.
            priority_overlay.retain(|(item, origin)| {
                if matches!(item.comment.as_str(), "习惯" | "剪贴板") {
                    seen.remove(&item.text);
                    let rank = base_pairs.len() * 2;
                    base_pairs.push((item.clone(), origin.clone(), rank));
                    false
                } else {
                    true
                }
            });
            if let Some((mut item, mut origin)) = self.recover_transposition(
                base_pairs
                    .iter()
                    .map(|(_, origin, _)| origin.canonical())
                    .chain(
                        priority_overlay
                            .iter()
                            .map(|(_, origin)| origin.canonical()),
                    ),
                imported,
                retention,
            ) {
                self.score_pair(&mut item, &mut origin, retention, now_ms, 0);
                base_pairs.retain(|(_, other, _)| other.canonical() != origin.canonical());
                priority_overlay.retain(|(_, other)| other.canonical() != origin.canonical());
                seen.insert(item.text.clone());
                priority_overlay.push((item, origin));
            }

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
                let pr = self.pure_rust.query_all_candidates_contextual(
                    &self.raw_input,
                    self.schema,
                    OpenCcMode::Simplified,
                    Some(imported),
                    Some(retention),
                    self.previous_context(RetentionEngine::now_epoch_ms()),
                    self.active_app_id.as_deref(),
                    self.config.adaptive_learning,
                );
                let mut added_full_alt = 0usize;
                for (offset, (pr_item, canonical)) in pr.into_iter().enumerate() {
                    if retention.erased_words.contains(&canonical) {
                        continue;
                    }
                    let frec = self.contextual_score(retention, &canonical, now_ms);
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
                        quick_paste_id: None,
                        function_action: None,
                        shortcut_code: None,
                        frecency_score: frec,
                    };
                    let origin = CandidateOrigin::PureRust {
                        features: [0.0; 9],
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
                                quick_paste_id: None,
                                function_action: None,
                                shortcut_code: None,
                                frecency_score: self.contextual_score(retention, &word, now_ms),
                            },
                            CandidateOrigin::PureRust {
                                features: [0.0; 9],
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

            for (item, origin, rank) in &mut base_pairs {
                if *origin.features() == [0.0; 9] {
                    self.score_pair(item, origin, retention, now_ms, *rank);
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
                    .max(0.0)
                    .total_cmp(&a.0.frecency_score.max(0.0))
                    .then_with(|| a.2.cmp(&b.2))
            });
            normal_base.sort_by(|a, b| {
                full_span
                    .contains(b.1.canonical())
                    .cmp(&full_span.contains(a.1.canonical()))
                    .then_with(|| {
                        b.0.frecency_score
                            .max(0.0)
                            .total_cmp(&a.0.frecency_score.max(0.0))
                    })
                    .then_with(|| a.2.cmp(&b.2))
            });

            for (c, orig, _) in pinned_base.into_iter().chain(normal_base) {
                if seen.insert(c.text.clone()) {
                    priority_overlay.push((c, orig));
                }
            }
            // Keep the existing first-page ranking, plus the word displaced by Quick Paste.
            priority_overlay.truncate(page_size + priority_overlay.iter().filter(|(c, _)| c.quick_paste_id.is_some() || c.function_action.is_some()).count());
            self.rime_first_page = priority_overlay;
            self.rime_overlay_page = 0;
            self.rime_first_has_next = snap.has_next;
            self.set_current_candidates(self.rime_first_page.iter().take(page_size).cloned().collect());
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
                    .max(0.0)
                    .total_cmp(&a.0.frecency_score.max(0.0))
                    .then_with(|| a.2.cmp(&b.2))
            });
            normal_base.sort_by(|a, b| {
                b.0.frecency_score
                    .max(0.0)
                    .total_cmp(&a.0.frecency_score.max(0.0))
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
            has_next: snap.has_next || (snap.page_index == 0 && self.rime_first_page.len() > page_size),
        };

        self.empty_action(None)
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn show_rime_overlay_page(&mut self) -> InputAction {
        let size = self.current_page.page_size;
        let start = self.rime_overlay_page * size;
        self.set_current_candidates(self.rime_first_page.iter().skip(start).take(size).cloned().collect());
        self.current_page.page_index = self.rime_overlay_page;
        self.current_page.has_prev = start > 0;
        self.current_page.has_next = start + size < self.rime_first_page.len() || self.rime_first_has_next;
        self.empty_action(None)
    }

    fn set_current_candidates(&mut self, pairs: Vec<(CandidateItem, CandidateOrigin)>) {
        self.current_candidates.clear();
        self.candidate_origins.clear();
        self.candidate_features.clear();
        for (i, (mut item, orig)) in pairs.into_iter().enumerate() {
            item.index = i;
            self.candidate_features.push(*orig.features());
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

        // Overlay candidates share the first backend page. Page through its
        // overflow before advancing Rime so no displaced word disappears.
        let page_size = self.current_page.page_size;
        let last_overlay = self.rime_first_page.len().saturating_sub(1) / page_size;
        let backward = keycode == 0xff55;
        let forward = keycode == 0xff56;
        if self.current_page.page_index <= last_overlay && (backward || forward) {
            if backward && self.rime_overlay_page > 0 {
                self.rime_overlay_page -= 1;
                return Ok(self.show_rime_overlay_page());
            }
            if forward && self.rime_overlay_page < last_overlay {
                self.rime_overlay_page += 1;
                return Ok(self.show_rime_overlay_page());
            }
        }
        let returning_to_first = backward && self.current_page.page_index == last_overlay + 1;
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
                        ..
                    } => {
                        if !is_clipboard {
                            self.learn_current_candidate(&canonical, 0, imported, retention);
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
                        ..
                    } => {
                        self.learn_current_candidate(&canonical, 0, imported, retention);
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
                        self.learn_current_candidate(&canonical, 0, imported, retention);
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
                                    ..
                                } => {
                                    if !is_clipboard {
                                        self.learn_current_candidate(
                                            &canonical,
                                            digit - 1,
                                            imported,
                                            retention,
                                        );
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
                                    self.learn_current_candidate(
                                        &canonical,
                                        digit - 1,
                                        imported,
                                        retention,
                                    );
                                    return Ok(self.apply_rime_snapshot(
                                        snap, false, false, clipboard, imported, retention,
                                    ));
                                }
                                CandidateOrigin::PureRust {
                                    canonical,
                                    commit_text,
                                    ..
                                } => {
                                    self.learn_current_candidate(
                                        &canonical,
                                        digit - 1,
                                        imported,
                                        retention,
                                    );
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

        let backend_page = snap.page_index;
        let mut action = self.apply_rime_snapshot(
            snap, is_return, is_escape, clipboard, imported, retention,
        );
        if returning_to_first && backend_page == 0 {
            self.rime_overlay_page = self.rime_first_page.len().saturating_sub(1) / page_size;
            action = self.show_rime_overlay_page();
        } else if backend_page > 0 {
            self.current_page.page_index = backend_page + last_overlay;
            action.page = self.current_page.clone();
        }
        Ok(action)
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
                        self.learn_current_candidate(origin.canonical(), 0, imported, retention);
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
                                self.learn_current_candidate(
                                    origin.canonical(),
                                    digit - 1,
                                    imported,
                                    retention,
                                );
                            }
                        }
                        self.clear_composition_state();
                        return Ok(self.empty_action(Some(cand.text)));
                    }
                }
            } else if !ch.is_control() && self.raw_input.is_empty() {
                let text = match ch {
                    '.' => "。".into(),
                    ':' => "：".into(),
                    _ => ch.to_string(),
                };
                return Ok(self.empty_action(Some(text)));
            }
        }

        if self.raw_input.is_empty() {
            self.clear_composition_state();
            return Ok(self.empty_action(None));
        }

        let base_pairs = self.pure_rust.query_all_candidates_contextual(
            &self.raw_input,
            self.schema,
            self.opencc_mode,
            Some(imported),
            Some(retention),
            self.previous_context(RetentionEngine::now_epoch_ms()),
            self.active_app_id.as_deref(),
            self.config.adaptive_learning,
        );

        let (mut merged, mut seen) =
            self.build_overlay_candidates(clipboard, imported, retention, false);
        let now_ms = RetentionEngine::now_epoch_ms();
        for (base_rank, (item, canonical)) in base_pairs.into_iter().enumerate() {
            let converted = self.convert_opencc(&canonical);
            if seen.insert(converted.clone()) {
                let mut item = item;
                item.text = converted.clone();
                let origin = CandidateOrigin::PureRust {
                    features: [0.0; 9],
                    canonical,
                    commit_text: converted,
                };
                let mut origin = origin;
                self.score_pair(&mut item, &mut origin, retention, now_ms, base_rank);
                merged.push((item, origin));
            }
        }

        merged.sort_by(|a, b| {
            // Negative personal scores remove a boost; dictionary order remains the tie-breaker.
            // Functional commands, macros and explicit paste shortcuts retain their order.
            let priority = |origin: &CandidateOrigin| {
                matches!(
                    origin,
                    CandidateOrigin::Overlay {
                        is_clipboard: true,
                        ..
                    }
                )
            };
            match (priority(&a.1), priority(&b.1)) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => b.0.pinned.cmp(&a.0.pinned).then_with(|| {
                    b.0.frecency_score
                        .max(0.0)
                        .total_cmp(&a.0.frecency_score.max(0.0))
                }),
            }
        });
        if let Some(mut pair) = self.recover_transposition(
            merged.iter().map(|(_, origin)| origin.canonical()),
            imported,
            retention,
        ) {
            self.score_pair(&mut pair.0, &mut pair.1, retention, now_ms, 0);
            merged.retain(|(_, origin)| origin.canonical() != pair.1.canonical());
            let first_word = merged
                .iter()
                .take_while(|(_, origin)| {
                    matches!(
                        origin,
                        CandidateOrigin::Overlay {
                            is_clipboard: true,
                            ..
                        }
                    )
                })
                .count();
            merged.insert(first_word, pair);
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

    fn matches_full_reading(
        &self,
        word: &str,
        code: &str,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> bool {
        if word.chars().count() < 2
            || !word.chars().all(crate::ret::is_cjk)
            || !imported.matches_reading(word, code, self.schema == SchemaKind::DoublePinyinFlypy)
        {
            return false;
        }
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        if retention.user_phrases.get(word).is_some_and(|p| {
            (if flypy { &p.xiaohe } else { &p.quanpin }) == code || p.acronym == code
        }) {
            return true;
        }
        let imported_matches = if flypy {
            imported.query_xiaohe(code)
        } else {
            imported.query_quanpin(code)
        };
        if imported_matches.iter().any(|p| p.text == word) {
            return true;
        }
        let matches = |pinyin: &str| {
            if flypy {
                crate::imp::xiaohe::pinyin_phrase_to_xiaohe(pinyin) == code
            } else {
                pinyin.split_whitespace().collect::<String>() == code
            }
        };
        if let Some((syllables, _)) = imported.corpus().word_readings.get(word) {
            return matches(&syllables.join(" "));
        }
        let ann = crate::imp::polyphone::PolyphoneAnnotator::new(std::sync::Arc::clone(
            imported.corpus(),
        ))
        .annotate_text(word);
        matches(&ann.untoned)
    }

    fn recover_transposition<'a>(
        &self,
        current_words: impl Iterator<Item = &'a str>,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> Option<(CandidateItem, CandidateOrigin)> {
        if !self.config.adaptive_learning
            || self.raw_input.len() < 4
            || !self.raw_input.bytes().all(|b| b.is_ascii_lowercase())
            || current_words
                .into_iter()
                .any(|word| self.matches_full_reading(word, &self.raw_input, imported, retention))
        {
            return None;
        }
        let mut swapped = self.raw_input.as_bytes().to_vec();
        let mut best: Option<(CandidateItem, String, f64)> = None;
        let mut attempts = 0;
        for i in 0..swapped.len() - 1 {
            if swapped[i] == swapped[i + 1] {
                continue;
            }
            if attempts == 12 {
                break;
            }
            attempts += 1;
            swapped.swap(i, i + 1);
            // All bytes are lowercase ASCII, including after the swap.
            let code = std::str::from_utf8(&swapped).expect("ASCII pinyin");
            let recovered = self
                .pure_rust
                .query_correction_candidates(code, self.schema, imported, retention)
                .into_iter()
                .next();
            if let Some((item, canonical, score)) = recovered {
                if best.as_ref().is_none_or(|(old, _, old_score)| {
                    (item.pinned && !old.pinned)
                        || (item.pinned == old.pinned && score > *old_score)
                }) {
                    best = Some((item, canonical, score));
                }
            }
            swapped.swap(i, i + 1);
        }
        best.map(|(mut item, canonical, _)| {
            let commit_text = self.convert_opencc(&canonical);
            item.text = commit_text.clone();
            item.comment = "纠错".into();
            (
                item,
                CandidateOrigin::PureRust {
                    canonical,
                    commit_text,
                    features: [0.0; 9],
                },
            )
        })
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

        let paste = clipboard.quick_paste_state();
        if let Some(clip) = paste.quick_paste.filter(|_| {
            self.config.platform == "android"
                && paste.candidate_visible
                && self.raw_input.is_empty()
        }) {
            overlay.push((
                CandidateItem {
                    index: 0,
                    text: clip.content.clone(),
                    comment: "快捷粘贴".into(),
                    pinned: false,
                    is_clipboard_shortcut: false,
                    quick_paste_id: Some(clip.id),
                    function_action: None,
                    shortcut_code: None,
                    frecency_score: 0.0,
                },
                CandidateOrigin::Overlay {
                    features: [0.0; 9],
                    canonical: clip.content.clone(),
                    commit_text: clip.content,
                    is_clipboard: true,
                },
            ));
        }

        for binding in &self.config.shortcuts {
            if !binding.text.is_empty() && binding.text == self.raw_input && self.config.shortcut_enabled(binding.action) {
                let label = binding.action.label().to_string();
                overlay.push((
                    CandidateItem {
                        index: 0,
                        text: label.clone(),
                        comment: "功能".into(),
                        pinned: false,
                        is_clipboard_shortcut: false,
                        quick_paste_id: None,
                        function_action: Some(binding.action),
                        shortcut_code: None,
                        frecency_score: 0.0,
                    },
                    CandidateOrigin::Overlay {
                        features: [0.0; 9],
                        canonical: label.clone(),
                        commit_text: label,
                        is_clipboard: true,
                    },
                ));
            }
        }

        if let Some(clip) = clipboard.find_shortcut(&self.raw_input) {
            if seen.insert(clip.content.clone()) {
                let item = CandidateItem {
                    index: 0,
                    text: clip.content.clone(),
                    comment: format!("📌 {}", self.raw_input),
                    pinned: true,
                    is_clipboard_shortcut: true,
                    quick_paste_id: None,
                    function_action: None,
                    shortcut_code: clip.shortcut_code.clone(),
                    frecency_score: 1000.0,
                };
                let origin = CandidateOrigin::Overlay {
                    features: [0.0; 9],
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
                        quick_paste_id: None,
                        function_action: None,
                        shortcut_code: None,
                        frecency_score: 500.0 - (idx as f64),
                    };
                    let origin = CandidateOrigin::Overlay {
                        features: [0.0; 9],
                        canonical: val.clone(),
                        commit_text: val,
                        is_clipboard: true,
                    };
                    overlay.push((item, origin));
                }
            }
        }

        if self.config.adaptive_learning {
            let compact: String = self
                .raw_input
                .chars()
                .filter(|c| c.is_ascii_alphabetic())
                .collect();
            let now_ms = RetentionEngine::now_epoch_ms();
            let phrases = retention
                .query_user_phrases(&compact, self.schema == SchemaKind::DoublePinyinFlypy);
            let mut learned: Vec<_> = phrases
                .into_iter()
                .map(|p| {
                    let clipboard = p.source == "clipboard";
                    (
                        p.word.as_str(),
                        if clipboard { "剪贴板" } else { "习惯" },
                        self.contextual_score(retention, &p.word, now_ms)
                            + if clipboard { 0.35 } else { 1.0 },
                    )
                })
                .collect();
            for phrase in retention.query_acronym_phrases(&compact) {
                learned.push((
                    phrase.word.as_str(),
                    "简拼",
                    self.contextual_score(retention, &phrase.word, now_ms) + 1.0,
                ));
            }
            let continuations = self
                .previous_context(now_ms)
                .map(|previous| {
                    retention.query_context_continuations(
                        previous,
                        &compact,
                        self.schema == SchemaKind::DoublePinyinFlypy,
                        now_ms,
                        |word| {
                            let ann = crate::imp::polyphone::PolyphoneAnnotator::new(
                                std::sync::Arc::clone(imported.corpus()),
                            )
                            .annotate_text(word);
                            let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&ann.untoned);
                            (ann.untoned, xiaohe)
                        },
                    )
                })
                .unwrap_or_default();
            for (word, _) in &continuations {
                learned.push((
                    word.as_str(),
                    "续写",
                    self.contextual_score(retention, word, now_ms) + 1.5,
                ));
            }
            if let Some(word) = retention.query_user_english(&compact) {
                learned.push((
                    word,
                    "英文",
                    self.contextual_score(retention, word, now_ms) + 1.0,
                ));
            }
            learned.sort_by(|a, b| b.2.total_cmp(&a.2));
            for (canonical, comment, score) in learned {
                let text = self.convert_opencc(canonical);
                if seen.insert(text.clone()) {
                    overlay.push((
                        CandidateItem {
                            index: 0,
                            text: text.clone(),
                            comment: comment.into(),
                            pinned: retention.pinned_words.contains(canonical),
                            frecency_score: score,
                            is_clipboard_shortcut: false,
                            shortcut_code: None,
                            quick_paste_id: None,
                            function_action: None,
                        },
                        CandidateOrigin::Overlay {
                            features: [0.0; 9],
                            canonical: canonical.to_string(),
                            commit_text: text,
                            is_clipboard: false,
                        },
                    ));
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
                    overlay.push((
                        CandidateItem {
                            index: 0,
                            text: text.clone(),
                            comment: hint.toned_pinyin.clone(),
                            pinned: retention.pinned_words.contains(&hint.ch),
                            frecency_score: self.contextual_score(
                                retention,
                                &hint.ch,
                                RetentionEngine::now_epoch_ms(),
                            ),
                            is_clipboard_shortcut: false,
                            shortcut_code: None,
                            quick_paste_id: None,
                            function_action: None,
                        },
                        CandidateOrigin::Overlay {
                            features: [0.0; 9],
                            canonical: hint.ch.clone(),
                            commit_text: text,
                            is_clipboard: false,
                        },
                    ));
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
                        quick_paste_id: None,
                        function_action: None,
                        shortcut_code: None,
                        frecency_score: (imp.weight as f64)
                            + 50.0
                            + self.contextual_score(
                                retention,
                                &imp.text,
                                RetentionEngine::now_epoch_ms(),
                            ),
                    };
                    let origin = CandidateOrigin::Overlay {
                        features: [0.0; 9],
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
                let decoded = self.pure_rust.query_all_candidates_contextual(
                    &self.raw_input,
                    self.schema,
                    OpenCcMode::Simplified,
                    Some(imported),
                    Some(retention),
                    self.previous_context(RetentionEngine::now_epoch_ms()),
                    self.active_app_id.as_deref(),
                    self.config.adaptive_learning,
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
                            quick_paste_id: None,
                            function_action: None,
                            shortcut_code: None,
                            frecency_score: item.frecency_score + 50.0,
                        };
                        let origin = CandidateOrigin::Overlay {
                            features: [0.0; 9],
                            canonical,
                            commit_text: converted,
                            is_clipboard: false,
                        };
                        overlay.push((ov_item, origin));
                    }
                }
            }
        }

        let now_ms = RetentionEngine::now_epoch_ms();
        for (rank, (item, origin)) in overlay.iter_mut().enumerate() {
            self.score_pair(item, origin, retention, now_ms, rank);
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
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if !self.rime_first_page.is_empty()
            && self.current_page.page_index <= self.rime_first_page.len().saturating_sub(1) / self.current_page.page_size
        {
            if let Some(pos) = self.rime_first_page.iter().position(|(item, origin)| item.quick_paste_id.is_none() && origin.canonical() == canonical) {
                let mut pair = self.rime_first_page.remove(pos);
                pair.0.pinned = true;
                let first_word = usize::from(self.rime_first_page.first().is_some_and(|(c, _)| c.quick_paste_id.is_some()));
                self.rime_first_page.insert(first_word, pair);
                self.show_rime_overlay_page();
                return Ok(());
            }
        }
        let mut item = self.current_candidates.remove(index);
        item.pinned = true;
        let first_word = usize::from(self.current_candidates.first().is_some_and(|c| c.quick_paste_id.is_some()));
        self.current_candidates.insert(first_word, item);
        if index < self.candidate_origins.len() {
            let orig = self.candidate_origins.remove(index);
            self.candidate_origins.insert(first_word, orig);
            let features = self.candidate_features.remove(index);
            self.candidate_features.insert(first_word, features);
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
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if !self.rime_first_page.is_empty()
            && self.current_page.page_index <= self.rime_first_page.len().saturating_sub(1) / self.current_page.page_size
        {
            self.rime_first_page.retain(|(item, origin)| item.quick_paste_id.is_some() || origin.canonical() != canonical);
            self.rime_overlay_page = self.rime_overlay_page.min(self.rime_first_page.len().saturating_sub(1) / self.current_page.page_size);
            self.show_rime_overlay_page();
            return Ok(());
        }
        self.current_candidates.remove(index);
        if index < self.candidate_origins.len() {
            self.candidate_origins.remove(index);
            self.candidate_features.remove(index);
        }
        for (i, c) in self.current_candidates.iter_mut().enumerate() {
            c.index = i;
        }
        Ok(())
    }

    pub fn empty_action(&mut self, commit_text: Option<String>) -> InputAction {
        if let Some(ch) = commit_text.as_deref().and_then(|text| text.chars().next_back()) {
            self.last_committed_char = Some(ch);
        }
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
            function_action: None,
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

#[cfg(test)]
mod personalization_tests {
    use super::*;

    fn fixture() -> (
        EngineState,
        ClipboardManager,
        ImportedLexiconIndex,
        RetentionEngine,
    ) {
        let mut engine = EngineState::new(EngineConfig {
            schema: SchemaKind::RimeIce,
            ..Default::default()
        });
        engine.resolved_assets_dir = None;
        (
            engine,
            ClipboardManager::new(50),
            ImportedLexiconIndex::new(None),
            RetentionEngine::default(),
        )
    }

    fn type_code(
        engine: &mut EngineState,
        code: &str,
        clip: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        r: &mut RetentionEngine,
    ) {
        for ch in code.chars() {
            engine
                .process_key(ch as u32, 0, SwipeDir::None, clip, imported, r)
                .unwrap();
        }
    }

    fn offer(engine: &mut EngineState, word: &str, code: &str) {
        engine.raw_input = code.into();
        engine.set_current_candidates(vec![(
            CandidateItem {
                index: 0,
                text: word.into(),
                comment: String::new(),
                pinned: false,
                is_clipboard_shortcut: false,
                quick_paste_id: None,
                function_action: None,
                shortcut_code: None,
                frecency_score: 0.0,
            },
            CandidateOrigin::PureRust {
                features: [0.0; 9],
                canonical: word.into(),
                commit_text: word.into(),
            },
        )]);
    }

    #[test]
    fn phase3_commit_feedback_semantic_recall_and_cached_hot_queries() {
        let (mut e, clip, imp, mut r) = fixture();
        offer(&mut e, "项目进度安排", "xiangmujinduanpai");
        e.process_key(0x20, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        let context = e.cached_context_vec.unwrap();
        type_code(&mut e, "shuru", &clip, &imp, &mut r);
        assert_eq!(
            e.cached_context_vec,
            Some(context),
            "typing must only read the context cache"
        );
        assert_eq!(e.current_candidates.len(), e.candidate_features.len());
        let target = e.canonical_at(1).unwrap().to_string();
        assert!(e.selection_feedback(1).unwrap().1.is_some());
        let committed = e
            .process_key('2' as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(committed.commit_text.as_deref(), Some(target.as_str()));
        assert!(r.reranker.updates > 0);
        assert!(r.semantic_memories.iter().any(|m| m.word == target));
        assert!(e.candidate_features.is_empty());
        e.reset_commit_context();
        offer(&mut e, "项目进度计划", "xiangmujindujihua");
        e.process_key(0x20, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        type_code(&mut e, "shuru", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some(target.as_str()));
        assert!(e.candidate_features[0][5] > 0.0);
        // Fill the personal datastore to its production bound before timing cached queries.
        for i in 0..512 {
            r.record_semantic_memory(
                context,
                &format!("词{i}"),
                None,
                RetentionEngine::now_epoch_ms(),
            );
        }
        let now = RetentionEngine::now_epoch_ms();
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            for (rank, candidate) in e.current_candidates.iter().enumerate() {
                std::hint::black_box(e.features_for(&r, &candidate.text, now, Some(rank)));
            }
        }
        let mean = start.elapsed() / 1000;
        eprintln!(
            "phase3 cached candidate query mean={mean:?}, memories={}",
            r.semantic_memories.len()
        );
        #[cfg(not(debug_assertions))]
        assert!(mean < std::time::Duration::from_micros(500), "{mean:?}");
        let start = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap());
        }
        let hot_mean = start.elapsed() / 100;
        eprintln!("phase3 full warm key query mean={hot_mean:?}");
        #[cfg(not(debug_assertions))]
        assert!(hot_mean < std::time::Duration::from_micros(500), "{hot_mean:?}");
        let before = r.reranker.updates;
        e.config.adaptive_learning = false;
        e.reset_commit_context();
        e.clear_composition_state();
        type_code(&mut e, "shuru", &clip, &imp, &mut r);
        e.process_key('2' as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(r.reranker.updates, before);
        assert!(e.cached_context_vec.is_none());
        e.process_key(0x1b, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert!(e.context_window.is_empty());
    }

    #[test]
    fn frequent_typed_phrases_gain_personal_acronyms() {
        let (mut e, clip, imp, mut r) = fixture();
        let now = RetentionEngine::now_epoch_ms();
        for _ in 0..2 {
            e.record_candidate_commit("项目管理", "xiangmuguanli", None, &imp, &mut r, now);
        }
        let phrase = r.user_phrases.get("项目管理").unwrap();
        assert_eq!(phrase.source, "typed");
        assert_eq!(phrase.acronym, "xmgl");
        type_code(&mut e, "xmgl", &clip, &imp, &mut r);
        assert!(e
            .current_candidates
            .iter()
            .any(|c| c.text == "项目管理" && c.comment == "简拼"));
        e.clear_composition_state();
        e.config.adaptive_learning = false;
        let (overlay, _) = e.build_overlay_candidates(&clip, &imp, &r, false);
        assert!(!overlay.iter().any(|(c, _)| c.comment == "简拼"));
    }

    #[test]
    fn one_key_context_continuation_commits_and_expires_without_idle_space_change() {
        let (mut e, clip, imp, mut r) = fixture();
        let now = RetentionEngine::now_epoch_ms();
        r.record_contextual_selection("打扰了", "daraole", Some("不好意思"), None, now, 1.0, true);
        offer(&mut e, "不好意思", "buhaoyisi");
        e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        type_code(&mut e, "d", &clip, &imp, &mut r);
        assert_eq!(e.current_candidates[0].text, "打扰了");
        assert_eq!(e.current_candidates[0].comment, "续写");
        let commit = e
            .process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(commit.commit_text.as_deref(), Some("打扰了"));
        assert_eq!(
            e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap()
                .commit_text
                .as_deref(),
            Some(" ")
        );
        e.last_committed_word = Some("不好意思".into());
        e.last_committed_ms = now.saturating_sub(6001);
        type_code(&mut e, "d", &clip, &imp, &mut r);
        assert!(!e.current_candidates.iter().any(|c| c.comment == "续写"));
    }

    #[test]
    fn adjacent_transposition_recovers_complete_words_but_preserves_valid_input() {
        let (mut e, clip, imp, mut r) = fixture();
        type_code(&mut e, "nihoa", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some("你好"));
        assert_eq!(e.current_candidates[0].comment, "纠错");
        assert!(
            matches!(&e.candidate_origins[0], CandidateOrigin::PureRust { canonical, .. } if canonical == "你好")
        );
        assert_eq!(
            e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap()
                .commit_text
                .as_deref(),
            Some("你好")
        );
        type_code(&mut e, "nihao", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some("你好"));
        assert!(!e.current_candidates.iter().any(|c| c.comment == "纠错"));
        e.clear_composition_state();
        r.erase_word("你好");
        type_code(&mut e, "nihoa", &clip, &imp, &mut r);
        assert!(!e.current_candidates.iter().any(|c| c.text == "你好"));
        e.clear_composition_state();
        e.config.adaptive_learning = false;
        type_code(&mut e, "nihoa", &clip, &imp, &mut r);
        assert!(!e.current_candidates.iter().any(|c| c.comment == "纠错"));
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_page_zero_recovers_transposition_with_portable_commit_origin() {
        let (mut e, clip, imp, r) = fixture();
        // Synthetic native snapshot exercises merging without creating a Rime session.
        e.raw_input = "nihoa".into();
        let snapshot = rime_ffi::RimeSnapshot {
            commit_text: None,
            preedit: "nihoa".into(),
            cursor_utf8: 5,
            page_index: 0,
            page_size: 5,
            has_prev: false,
            has_next: false,
            candidates: vec![rime_ffi::RimeRawCandidate {
                text: "你".into(),
                comment: String::new(),
            }],
        };
        let action = e.apply_rime_snapshot(snapshot, false, false, &clip, &imp, &r);
        assert_eq!(action.candidates[0].text, "你好");
        assert_eq!(action.candidates[0].comment, "纠错");
        assert_eq!(e.candidate_features[0][8], 1.0);
        assert!(
            matches!(&e.candidate_origins[0], CandidateOrigin::PureRust { canonical, .. } if canonical == "你好")
        );
    }

    #[test]
    fn fast_correction_lookup_respects_schema_imported_readings_and_erasure() {
        let (e, _, mut imp, mut r) = fixture();
        for (schema, code) in [
            (SchemaKind::RimeIce, "nihao"),
            (SchemaKind::DoublePinyinFlypy, "nihc"),
        ] {
            let words = e
                .pure_rust
                .query_correction_candidates(code, schema, &imp, &r);
            assert!(words.iter().any(|(_, word, _)| word == "你好"));
            assert!(words.len() <= 3);
        }
        imp.inject_hotwords(&[crate::imp::ImportedLexiconEntry {
            text: "你好".into(),
            pinyin: "ni hu".into(),
            toned_pinyin: "nǐ hǔ".into(),
            xiaohe: "nihu".into(),
            weight: 1000,
        }]);
        assert!(!e
            .pure_rust
            .query_correction_candidates("nihao", SchemaKind::RimeIce, &imp, &r)
            .iter()
            .any(|(_, word, _)| word == "你好"));
        assert!(e
            .pure_rust
            .query_correction_candidates("nihu", SchemaKind::DoublePinyinFlypy, &imp, &r)
            .iter()
            .any(|(_, word, _)| word == "你好"));
        r.erase_word("你好");
        assert!(!e
            .pure_rust
            .query_correction_candidates("nihu", SchemaKind::DoublePinyinFlypy, &imp, &r)
            .iter()
            .any(|(_, word, _)| word == "你好"));
        assert!(e
            .pure_rust
            .query_correction_candidates(&"x".repeat(49), SchemaKind::RimeIce, &imp, &r)
            .is_empty());
    }

    #[test]
    fn soft_efficiency_warm_query_latency() {
        let (mut e, clip, imp, mut r) = fixture();
        let now = RetentionEngine::now_epoch_ms();
        r.register_typed_phrase("项目管理", "xiang mu guan li", "xlmugrli", now);
        r.pin_word("项目管理");
        r.record_contextual_selection("打扰了", "daraole", Some("不好意思"), None, now, 2.0, true);
        e.last_committed_word = Some("不好意思".into());
        e.last_committed_ms = now;
        // Warm dictionary/model caches before measuring the added candidate work.
        e.raw_input = "nihoa".into();
        let fragments = e.pure_rust.query_all_candidates_with_canonical(
            &e.raw_input,
            e.schema,
            OpenCcMode::Simplified,
            Some(&imp),
            Some(&r),
        );
        assert!(e
            .recover_transposition(fragments.iter().map(|(_, word)| word.as_str()), &imp, &r)
            .is_some());
        let measure = |query: &mut dyn FnMut()| {
            let start = std::time::Instant::now();
            for _ in 0..100 {
                query();
            }
            start.elapsed() / 100
        };
        let typo = measure(&mut || {
            std::hint::black_box(e.recover_transposition(
                fragments.iter().map(|(_, word)| word.as_str()),
                &imp,
                &r,
            ));
        });
        e.raw_input = "d".into();
        let continuation = measure(&mut || {
            std::hint::black_box(e.build_overlay_candidates(&clip, &imp, &r, false));
        });
        e.raw_input = "xmgl".into();
        let acronym = measure(&mut || {
            std::hint::black_box(e.build_overlay_candidates(&clip, &imp, &r, false));
        });
        eprintln!("warm query averages: typo={typo:?}, continuation={continuation:?}, acronym={acronym:?}");
        #[cfg(not(debug_assertions))]
        for latency in [typo, continuation, acronym] {
            assert!(
                latency < std::time::Duration::from_micros(500),
                "{latency:?}"
            );
        }
    }

    #[test]
    fn digit_context_keeps_decimal_and_time_punctuation_half_width() {
        let (mut e, clip, imp, mut r) = fixture();
        for (digits, punct) in [("3", '.'), ("12", ':')] {
            for digit in digits.chars() {
                e.process_key(digit as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
            }
            let action = e
                .process_key(punct as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(action.commit_text, Some(punct.to_string()));
            assert_eq!(e.last_committed_char, Some(punct));
        }
        for (punct, expected) in [('.', "。"), (':', "：")] {
            offer(&mut e, "你好", "nihao");
            e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(e.last_committed_char, Some('好'));
            assert_eq!(
                e.process_key(punct as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap()
                    .commit_text
                    .as_deref(),
                Some(expected)
            );
        }
        e.empty_action(Some("12".into()));
        e.process_key(27, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(e.last_committed_char, None);
        e.empty_action(Some("3".into()));
        e.on_app_focus("chat").unwrap();
        assert_eq!(e.last_committed_char, None);
    }

    #[test]
    fn paired_context_promotes_follower_in_decoder() {
        let (mut e, clip, imp, mut r) = fixture();
        type_code(&mut e, "ni", &clip, &imp, &mut r);
        assert!(e.current_candidates.len() >= 2);
        let first = e.canonical_at(0).unwrap().to_string();
        let follower = e.canonical_at(1).unwrap().to_string();
        e.clear_composition_state();
        let now = RetentionEngine::now_epoch_ms();
        r.record_contextual_selection(&first, "ni", None, None, now, 1.0, true);
        r.record_contextual_selection(&follower, "ni", Some("前"), None, now, 1.0, true);
        offer(&mut e, "前", "qian");
        e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        type_code(&mut e, "ni", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some(follower.as_str()));
    }

    #[test]
    fn app_affinity_ranks_different_words_for_same_code() {
        let (mut e, clip, imp, mut r) = fixture();
        type_code(&mut e, "ni", &clip, &imp, &mut r);
        let code_word = e.canonical_at(0).unwrap().to_string();
        let chat_word = e.canonical_at(1).unwrap().to_string();
        e.clear_composition_state();
        let now = RetentionEngine::now_epoch_ms();
        r.record_contextual_selection(&code_word, "ni", None, Some("code"), now, 1.0, true);
        r.record_contextual_selection(&chat_word, "ni", None, Some("wechat"), now, 1.0, true);
        for (app, expected) in [("code", code_word), ("wechat", chat_word)] {
            e.clear_composition_state();
            e.on_app_focus(app).unwrap();
            e.lang_mode = LangMode::Zh;
            type_code(&mut e, "ni", &clip, &imp, &mut r);
            assert_eq!(e.canonical_at(0), Some(expected.as_str()));
        }
    }

    #[test]
    fn repeated_consecutive_commits_synthesize_phrase_for_both_schemas() {
        let (mut e, clip, imp, mut r) = fixture();
        for _ in 0..2 {
            for (word, code) in [("你", "ni"), ("好", "hao")] {
                offer(&mut e, word, code);
                e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
            }
        }
        let p = r
            .user_phrases
            .get("你好")
            .expect("two compositions promote a phrase")
            .clone();
        assert_eq!(p.source, "typed");
        for (schema, code) in [
            (SchemaKind::RimeIce, p.quanpin),
            (SchemaKind::DoublePinyinFlypy, p.xiaohe),
        ] {
            e.set_schema(schema);
            type_code(&mut e, &code, &clip, &imp, &mut r);
            assert!(e
                .current_candidates
                .iter()
                .any(|c| c.text == "你好" && c.comment == "习惯"));
        }
    }

    #[test]
    fn backspace_regret_corrects_original_context_and_preference() {
        let (mut e, clip, imp, mut r) = fixture();
        for (word, code) in [("前", "qian"), ("尼", "ni")] {
            offer(&mut e, word, code);
            e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap();
        }
        e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(e.regret_backspaces, 1);
        type_code(&mut e, "ni", &clip, &imp, &mut r);
        offer(&mut e, "你", "ni");
        e.process_key('1' as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert!(r.frecency["尼"] <= 0.5);
        assert_eq!(r.code_preferences["ni"]["你"], 2.5);
        assert!(!r.code_preferences["ni"].contains_key("尼"));
        assert_eq!(r.context_bigrams["前"]["你"], 2.5);
        assert!(!r.context_bigrams.contains_key("尼"));
        assert_eq!(e.regret_backspaces, 0);
        e.process_key(27, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert!(e.last_committed_word.is_none());
    }

    #[test]
    fn enter_learns_casing_and_disabled_learning_keeps_legacy_frecency() {
        let (mut e, clip, imp, mut r) = fixture();
        type_code(&mut e, "OpenAI", &clip, &imp, &mut r);
        let committed = e
            .process_key(13, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(committed.commit_text.as_deref(), Some("OpenAI"));
        assert_eq!(r.query_user_english("openai"), Some("OpenAI"));
        type_code(&mut e, "openai", &clip, &imp, &mut r);
        assert!(e
            .current_candidates
            .iter()
            .any(|c| c.text == "OpenAI" && c.comment == "英文"));
        e.clear_composition_state();
        e.config.adaptive_learning = false;
        offer(&mut e, "好", "hao");
        e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert!(r.frecency.contains_key("好"));
        assert!(r.code_preferences.is_empty());
        assert!(EngineConfig::parse_json("{}").unwrap().adaptive_learning);
        assert!(EngineConfig::parse_json("{}").unwrap().learn_from_clipboard);
    }
}
