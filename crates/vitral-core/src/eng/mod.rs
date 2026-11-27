pub(crate) mod english;
pub mod gram;
#[cfg(any(feature = "native-onnx", test))]
mod neural_rerank;
#[cfg(all(target_os = "linux", feature = "native-rime"))]
pub mod opencc_ffi;
pub mod pure_rust;
pub mod quanpin;
pub mod spatial;
pub use spatial::{SpatialAlt, TouchKeyHint};
#[cfg(all(target_os = "linux", feature = "native-rime"))]
mod phrase_rank;
#[cfg(all(target_os = "linux", feature = "native-rime"))]
pub mod rime_ffi;

use crate::clip::ClipboardManager;
use crate::error::VitralError;
use crate::hw::ids_exta::IdsIndex;
use crate::imp::polyphone::default_assets_dir_candidates;
use crate::imp::xiaohe;
use crate::imp::ImportedLexiconIndex;
use crate::ret::{expand_time_macro, match_paired_punctuation, RetentionEngine};
use crate::sync::RelayDevice;
use pinyin::{ToPinyin, ToPinyinMulti};
use pure_rust::{
    personal_boost, pure_rust_opencc, DecodedCandidate, PureRustEngine, PERSONAL_PRIORITY_THRESHOLD,
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use vitral_tokens::TOKENS;

pub const MOD_LANG_TOGGLE: u32 = 1 << 30;
pub const MOD_VOICE_HOLD: u32 = 1 << 29;

const KEY_HOME: u32 = 0xff50;
const KEY_LEFT: u32 = 0xff51;
const KEY_RIGHT: u32 = 0xff53;
const KEY_END: u32 = 0xff57;
const KEY_DELETE: u32 = 0xffff;
const FILTER_MARKER: char = '`';
const MAX_FILTER_LEN: usize = 12;

const DEV_APPS: &[&str] = &[
    "alacritty", "kitty", "wezterm", "wezterm-gui", "foot", "konsole",
    "terminal", "gnome-terminal", "gnome-terminal-server", "xfce4-terminal", "tilix",
    "xterm", "urxvt", "windowsterminal", "wt", "cmd", "powershell", "pwsh", "termux",
    "mintty", "nvim", "vim", "gvim", "tmux", "code", "code-insiders", "code - insiders",
];
const IDE_APPS: &[&str] = &[
    "codium", "cursor", "zed", "idea", "pycharm", "clion", "goland", "webstorm",
    "rider", "rustrover", "android studio", "android-studio", "studio64", "devenv", "sublime_text",
    "neovide", "emacs",
];
const SENSITIVE_APPS: &[&str] = &[
    "1password", "onepassword", "keepass", "keepassx", "keepassxc", "bitwarden", "enpass", "polkit",
    "polkit-gnome-authentication-agent-1", "polkit-kde-authentication-agent-1",
    "gnome-keyring", "gnome-keyring-daemon", "kwallet", "kwalletd", "kwalletd5", "kwalletd6",
    "kwalletmanager", "kwalletmanager5", "kwalletmanager6",
    "tor browser", "tor-browser", "incognito", "private browsing",
];

fn app_matches(app_id: &str, builtins: &[&str], custom: &[String]) -> bool {
    let lower = app_id.to_lowercase();
    let filename = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    let filename = filename.strip_suffix(".exe").unwrap_or(filename);
    builtins.iter().any(|name| filename.split('.').any(|part| {
        part == *name || part.strip_suffix("64") == Some(*name)
    })) || custom.iter().any(|rule| {
        let rule = rule.trim().to_lowercase();
        !rule.is_empty() && lower.contains(&rule)
    })
}

pub(crate) fn is_dev_app(app_id: &str) -> bool {
    app_matches(app_id, DEV_APPS, &[]) || app_matches(app_id, IDE_APPS, &[])
}

pub fn is_privacy_app(app_id: &str, custom: &[String]) -> bool {
    if app_matches(app_id.trim(), SENSITIVE_APPS, custom) {
        return true;
    }
    let lower = app_id.to_lowercase();
    ["incognito", "private browsing"].iter().any(|label| {
        lower.match_indices(label).any(|(start, _)| {
            !lower[..start].ends_with(char::is_alphanumeric)
                && !lower[start + label.len()..].starts_with(char::is_alphanumeric)
        })
    })
}

fn clamp_cursor(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn map_zh_punctuation(ch: char) -> Option<&'static str> {
    match ch {
        ',' | '，' => Some("，"),
        '.' | '。' => Some("。"),
        '?' | '？' => Some("？"),
        '!' | '！' => Some("！"),
        ';' | '；' => Some("；"),
        ':' | '：' => Some("："),
        '\\' | '、' => Some("、"),
        _ => None,
    }
}

fn toned_syllable_tone(toned: &str) -> Option<u8> {
    for ch in toned.chars() {
        match ch {
            'ā' | 'ē' | 'ī' | 'ō' | 'ū' | 'ǖ' => return Some(b'1'),
            'á' | 'é' | 'í' | 'ó' | 'ú' | 'ǘ' => return Some(b'2'),
            'ǎ' | 'ě' | 'ǐ' | 'ǒ' | 'ǔ' | 'ǚ' => return Some(b'3'),
            'à' | 'è' | 'ì' | 'ò' | 'ù' | 'ǜ' => return Some(b'4'),
            _ => {}
        }
    }
    None
}

fn candidate_reading<'a>(
    item: &'a CandidateItem,
    word: &str,
    imported: &ImportedLexiconIndex,
) -> Cow<'a, str> {
    let mut syllables = item.comment.split_whitespace();
    if syllables.clone().count() == word.chars().count()
        && syllables.all(|s| xiaohe::canonical_syllable_set().contains(xiaohe::normalize_syllable(s).as_str()))
    {
        Cow::Borrowed(&item.comment)
    } else {
        Cow::Owned(crate::imp::polyphone::PolyphoneAnnotator::new(
            std::sync::Arc::clone(imported.corpus()),
        ).annotate_text(word).untoned)
    }
}

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TypoCorrectionLevel {
    Off,
    Conservative,
    #[default]
    Standard,
    Aggressive,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_enabled")]
    pub prediction_enabled: bool,
    #[serde(default = "default_enabled")]
    pub prediction_skip_dev_apps: bool,
    #[serde(default = "default_prediction_delay")]
    pub prediction_delay_ms: u64,
    #[serde(default = "default_enabled")]
    pub profile_distill_enabled: bool,
    #[serde(default = "default_voice_cleanup_mode")]
    pub voice_cleanup_mode: String,
    #[serde(default)]
    pub privacy_apps: Vec<String>,
    #[serde(default = "crate::shortcuts::defaults")]
    pub shortcuts: Vec<crate::shortcuts::ShortcutBinding>,
    #[serde(default = "default_enabled")]
    pub linux_english_mode: bool,
    #[serde(default = "default_enabled")]
    pub adaptive_learning: bool,
    #[serde(default)]
    pub fuzzy_pinyin: bool,
    #[serde(default)]
    pub fuzzy_pinyin_pairs: Vec<String>,
    #[serde(default)]
    pub undo_commit_on_backspace: bool,
    #[serde(default = "default_typo_correction_level")]
    pub typo_correction_level: String,
    #[serde(default)]
    pub syllable_split: bool,
    #[serde(default = "default_enabled")]
    pub auto_english_for_dev_apps: bool,
    #[serde(default)]
    pub default_english_apps: Vec<String>,
    #[serde(default = "default_enabled")]
    pub disable_paired_punct_in_ide: bool,
    #[serde(default)]
    pub disable_paired_punct_apps: Vec<String>,
    #[serde(default = "default_quote_style")]
    pub quote_style: String,
    #[serde(default)]
    pub auto_micro_space: bool,
    #[serde(default)]
    pub data_dir: String,
    #[serde(default)]
    pub cache_dir: String,
    #[serde(default)]
    pub assets_dir: String,
    #[serde(default)]
    pub tokens_path: String,
    #[serde(default)]
    pub model_catalog_url: Option<String>,
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
    #[serde(default = "default_enabled")]
    pub ai_prefer_local: bool,
    #[serde(default)]
    pub known_devices: Vec<RelayDevice>,
}

impl EngineConfig {
    pub(crate) fn validate_voice_cleanup_mode(&self) -> Result<(), VitralError> {
        if !matches!(self.voice_cleanup_mode.as_str(), "off" | "light" | "polish") {
            return Err(VitralError::InvalidArgument(
                "voice_cleanup_mode must be off, light or polish".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn validated_typo_correction_level(&self) -> Result<TypoCorrectionLevel, VitralError> {
        match self.typo_correction_level.as_str() {
            "off" => Ok(TypoCorrectionLevel::Off),
            "conservative" => Ok(TypoCorrectionLevel::Conservative),
            "standard" => Ok(TypoCorrectionLevel::Standard),
            "aggressive" => Ok(TypoCorrectionLevel::Aggressive),
            _ => Err(VitralError::InvalidArgument(
                "typo_correction_level must be off, conservative, standard or aggressive".into(),
            )),
        }
    }

    pub fn validate_prediction(&self) -> Result<(), VitralError> {
        if !(150..=2000).contains(&self.prediction_delay_ms) {
            return Err(VitralError::InvalidArgument("prediction_delay_ms must be 150..2000".into()));
        }
        Ok(())
    }

    pub(crate) fn validated_fuzzy_pairs(&self) -> Result<quanpin::FuzzyPairs, VitralError> {
        quanpin::FuzzyPairs::from_names(&self.fuzzy_pinyin_pairs)
    }

    pub(crate) fn validate_quote_style(&self) -> Result<(), VitralError> {
        if !matches!(self.quote_style.as_str(), "curly" | "corner") {
            return Err(VitralError::InvalidArgument("quote_style must be curly or corner".into()));
        }
        Ok(())
    }

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
        let config: Self = serde_json::from_value(value).map_err(|e| VitralError::Parse(e.to_string()))?;
        if let Some(url) = config.model_catalog_url.as_deref().filter(|s| !s.is_empty()) { crate::models::validate_url(url)?; }
        config.validate_prediction()?;
        config.validate_voice_cleanup_mode()?;
        config.validated_typo_correction_level()?;
        config.validate_quote_style()?;
        config.validated_fuzzy_pairs()?;
        Ok(config)
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
            .field("typo_correction_level", &self.typo_correction_level)
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

fn default_prediction_delay() -> u64 { 380 }

fn default_voice_cleanup_mode() -> String {
    "light".into()
}

fn default_typo_correction_level() -> String {
    "standard".into()
}

fn default_enabled() -> bool { true }

fn default_quote_style() -> String {
    "curly".into()
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
            prediction_enabled: true,
            prediction_skip_dev_apps: default_enabled(),
            prediction_delay_ms: default_prediction_delay(),
            profile_distill_enabled: true,
            voice_cleanup_mode: default_voice_cleanup_mode(),
            privacy_apps: Vec::new(),
            shortcuts: crate::shortcuts::defaults(),
            linux_english_mode: true,
            adaptive_learning: true,
            fuzzy_pinyin: false,
            fuzzy_pinyin_pairs: Vec::new(),
            undo_commit_on_backspace: false,
            typo_correction_level: default_typo_correction_level(),
            syllable_split: false,
            auto_english_for_dev_apps: true,
            default_english_apps: Vec::new(),
            disable_paired_punct_in_ide: true,
            disable_paired_punct_apps: Vec::new(),
            quote_style: default_quote_style(),
            auto_micro_space: false,
            data_dir: String::new(),
            cache_dir: String::new(),
            assets_dir: String::new(),
            tokens_path: String::new(),
            model_catalog_url: None,
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
            ai_prefer_local: true,
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

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputAction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_action: Option<crate::shortcuts::ShortcutAction>,
    pub lang_mode: LangMode,
    pub schema: String,
    pub preedit: PreeditState,
    pub commit_text: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) clipboard_commit: bool,
    #[serde(default)]
    pub undo_commit_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_commit_text: Option<String>,
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
    Components {
        canonical: String,
        commit_text: String,
        features: [f64; 9],
    },
    Overlay {
        features: [f64; 9],
        canonical: String,
        commit_text: String,
        is_clipboard: bool,
        imported: bool,
    },
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    Rime {
        features: [f64; 9],
        canonical: String,
        rime_page_idx: usize,
    },
    PureRust {
        consumed_len: usize,
        lm_score: f64,
        #[cfg(any(feature = "native-onnx", test))]
        rank_score: f64,
        spatial_guarded: bool,
        features: [f64; 9],
        canonical: String,
        commit_text: String,
    },
}

impl CandidateOrigin {
    fn features(&self) -> &[f64; 9] {
        match self {
            Self::Components { features, .. }
            | Self::Overlay { features, .. }
            | Self::PureRust { features, .. } => features,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            Self::Rime { features, .. } => features,
        }
    }

    fn set_features(&mut self, value: [f64; 9]) {
        match self {
            Self::Components { features, .. }
            | Self::Overlay { features, .. }
            | Self::PureRust { features, .. } => *features = value,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            Self::Rime { features, .. } => *features = value,
        }
    }

    fn canonical(&self) -> &str {
        match self {
            CandidateOrigin::Components { canonical, .. }
            | CandidateOrigin::Overlay { canonical, .. }
            | CandidateOrigin::PureRust { canonical, .. } => canonical,
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            CandidateOrigin::Rime { canonical, .. } => canonical,
        }
    }

    fn has_personal_priority(&self, retention: &RetentionEngine, adaptive: bool) -> bool {
        let mut features = *self.features();
        features[6] = 0.0;
        features[8] = 0.0;
        retention.score_candidate_features(&features, adaptive) >= PERSONAL_PRIORITY_THRESHOLD
    }
}

impl DecodedCandidate {
    fn into_portable_pair(self) -> (CandidateItem, CandidateOrigin) {
        let origin = CandidateOrigin::PureRust {
            consumed_len: self.consumed_len,
            lm_score: self.lm_score,
            #[cfg(any(feature = "native-onnx", test))]
            rank_score: 0.0,
            spatial_guarded: self.spatial_guarded,
            features: [0.0; 9],
            canonical: self.canonical,
            commit_text: self.item.text.clone(),
        };
        (self.item, origin)
    }
}

struct UndoCommit {
    text: String,
    previous_char: Option<char>,
    touch_hints: Vec<TouchKeyHint>,
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    native: bool,
}

struct PendingUndoRegret {
    word: String,
    code: String,
    context: Option<String>,
}

pub struct EngineState {
    pub config: EngineConfig,
    pub resolved_assets_dir: Option<PathBuf>,
    pub model_paths: crate::models::ModelPaths,
    pub schema: SchemaKind,
    pub opencc_mode: OpenCcMode,
    pub lang_mode: LangMode,
    pub active_app_id: Option<String>,
    pub(crate) active_app_no_learning: bool,
    pub last_committed_word: Option<String>,
    pub last_committed_code: Option<String>,
    pub last_committed_ms: u64,
    pub last_committed_char: Option<char>,
    undo_commit: Option<UndoCommit>,
    pending_undo_regret: Option<PendingUndoRegret>,
    pub regret_backspaces: usize,
    last_commit_context: Option<String>,
    last_commit_window: Vec<char>,
    context_window: Vec<char>,
    #[cfg(feature = "native-onnx")]
    semantic_epoch: u64,
    #[cfg(feature = "native-onnx")]
    semantic_requested: u64,
    cached_context_vec: Option<[i8; 32]>,
    cached_mlm_priors: HashMap<char, f32>,
    #[cfg(feature = "native-onnx")]
    neural_rerank_cache: neural_rerank::Cache,
    #[cfg(feature = "native-onnx")]
    pending_neural_rerank: Option<neural_rerank::Pending>,
    pub(crate) candidate_features: Vec<[f64; 9]>,
    last_commit_features: Option<[f64; 9]>,
    pub app_lang_memory: HashMap<String, LangMode>,
    pub composing: String,
    pub(crate) raw_input: String,
    raw_touch_hints: Vec<TouchKeyHint>,
    raw_cursor: usize,
    pending_phrase_text: String,
    pending_phrase_code: String,
    pending_phrase_selections: usize,
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
    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    rime_input_start: Option<usize>,
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
        }
        if config.data_dir.trim().is_empty() {
            if let Some(ref sd) = config.state_dir {
                if !sd.trim().is_empty() {
                    config.data_dir = sd.clone();
                }
            }
        }
        let resolved_assets_dir = resolve_assets_dir(&config.assets_dir);
        let mut pure_rust = PureRustEngine::with_corpus_cache(
            resolved_assets_dir.as_deref(),
            (!config.data_dir.is_empty()).then(|| Path::new(&config.data_dir)),
        );
        pure_rust.fuzzy_pinyin = config.fuzzy_pinyin;
        pure_rust.fuzzy_pairs = config.validated_fuzzy_pairs().unwrap_or(quanpin::FuzzyPairs::NONE);
        pure_rust.typo_correction_level = config.validated_typo_correction_level().unwrap_or_default();
        pure_rust.syllable_split = config.syllable_split;
        let mut engine = Self {
            schema: config.schema,
            opencc_mode: config.opencc_mode,
            model_paths: crate::models::ModelPaths::bundled(resolved_assets_dir.as_deref()),
            resolved_assets_dir,
            config,
            lang_mode: LangMode::Zh,
            active_app_id: None,
            active_app_no_learning: false,
            last_committed_word: None,
            last_committed_code: None,
            last_committed_ms: 0,
            last_committed_char: None,
            undo_commit: None,
            pending_undo_regret: None,
            regret_backspaces: 0,
            last_commit_context: None,
            last_commit_window: Vec::new(),
            context_window: Vec::new(),
            #[cfg(feature = "native-onnx")]
            semantic_epoch: 0,
            #[cfg(feature = "native-onnx")]
            semantic_requested: 0,
            cached_context_vec: None,
            cached_mlm_priors: HashMap::new(),
            #[cfg(feature = "native-onnx")]
            neural_rerank_cache: neural_rerank::Cache::default(),
            #[cfg(feature = "native-onnx")]
            pending_neural_rerank: None,
            candidate_features: Vec::new(),
            last_commit_features: None,
            app_lang_memory: HashMap::new(),
            composing: String::new(),
            raw_input: String::new(),
            raw_touch_hints: Vec::new(),
            raw_cursor: 0,
            pending_phrase_text: String::new(),
            pending_phrase_code: String::new(),
            pending_phrase_selections: 0,
            cursor_utf8: 0,
            current_candidates: Vec::new(),
            current_page: PageInfo {
                page_index: 0,
                page_size: 0,
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
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            rime_input_start: None,
        };
        engine.current_page.page_size = engine.candidate_page_size();
        engine
    }

    fn candidate_page_size(&self) -> usize {
        if self.config.platform == "android" {
            24
        } else {
            TOKENS.ergonomics.desktop_candidate_page_size as usize
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
        if self.is_passthrough_active() {
            return Ok(self.build_passthrough_action());
        }
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let (Some(sid), Some(assets), Some(_)) = (
            self.rime_session_id, self.resolved_assets_dir.clone(), self.rime_input_start,
        ) {
            let snap = rime_ffi::with_runtime(&assets, &self.config.cache_dir, &self.config.data_dir, |rt| rt.snapshot(sid))?;
            return Ok(self.apply_rime_snapshot(snap, false, false, clipboard, imported, retention));
        }
        self.process_key_pure_rust(0, clipboard, imported, retention)
    }

    pub(crate) fn speech_context_before(&self) -> String { self.context_window.iter().collect() }

    pub(crate) fn reset_commit_context(&mut self) {
        self.context_window.clear();
        self.refresh_semantic_context();
        self.last_commit_features = None;
        self.undo_commit = None;
        self.last_committed_word = None;
        self.last_committed_code = None;
        self.last_commit_context = None;
        self.last_commit_window.clear();
        self.regret_backspaces = 0;
    }

    /// Host deletion feedback counts Unicode characters, including whole-word deletion.
    pub fn notify_deleted_chars(&mut self, count: usize) {
        if count > 0 {
            self.undo_commit = None;
            self.last_committed_char = None;
        }
        if self.raw_input.is_empty()
            && self.last_committed_word.is_some()
            && RetentionEngine::now_epoch_ms().saturating_sub(self.last_committed_ms) <= 5000
        {
            self.regret_backspaces = self.regret_backspaces.saturating_add(count);
        }
    }

    fn previous_context(&self, now_ms: u64) -> Option<&str> {
        (now_ms.saturating_sub(self.last_committed_ms) <= 6000)
            .then_some(self.last_committed_word.as_deref())
            .flatten()
    }

    fn prefix_prediction_context(&self, now_ms: u64) -> Option<String> {
        let previous = self.previous_context(now_ms)?;
        let mut tail: Vec<char> = if self.context_window.is_empty() {
            previous.chars().rev().take(4).collect()
        } else {
            self.context_window.iter().rev().take(4).copied().collect()
        };
        tail.reverse();
        tail.iter()
            .all(|ch| crate::ret::is_cjk(*ch))
            .then(|| tail.into_iter().collect())
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

    fn is_clipboard_shortcut_candidate(&self, index: usize) -> bool {
        self.current_candidates.get(index).is_some_and(|item| item.is_clipboard_shortcut)
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
        self.undo_commit = None;
        self.settle_undo_regret(Some(canonical), retention);
        if self.is_active_privacy_app() {
            self.reset_commit_context();
            return;
        }
        let fully_deleted =
            self.last_committed_word.as_ref().is_some_and(|w| {
                self.regret_backspaces == w.chars().count() && self.regret_backspaces > 0
            }) && now_ms.saturating_sub(self.last_committed_ms) <= 6000;
        let correction = self.config.adaptive_learning
            && fully_deleted
            && self.last_committed_code.as_deref() == Some(code)
            && self
                .last_committed_word
                .as_deref()
                .is_some_and(|w| w != canonical);
        let prev = if fully_deleted {
            self.context_window.clone_from(&self.last_commit_window);
            self.refresh_semantic_context();
            self.last_commit_context.clone()
        } else {
            self.previous_context(now_ms).map(str::to_string)
        };
        if prev.is_none() {
            self.context_window.clear();
        }
        let before_window = self.context_window.clone();
        let feedback = if fully_deleted { None } else { feedback };
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
                && self.pending_phrase_selections == 0
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
                        retention.observe_phrase_composition(&combined, &xiaohe, &ann.untoned, now_ms);
                    }
                }
            }
        }
        if self.config.adaptive_learning
            && (2..=8).contains(&canonical.chars().count())
            && canonical.chars().all(crate::ret::is_cjk)
            && (retention.frecency.get(canonical).copied().unwrap_or(0.0) >= 1.5
                || retention.user_phrases.contains_key(canonical)
                || retention.pinned_words.contains(canonical))
        {
            let ann = crate::imp::polyphone::PolyphoneAnnotator::new(std::sync::Arc::clone(
                imported.corpus(),
            ))
            .annotate_text(canonical);
            let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&ann.untoned);
            retention.register_typed_phrase(canonical, &ann.untoned, &xiaohe, now_ms);
        }
        self.last_commit_context = prev;
        self.last_commit_window = before_window;
        self.last_committed_word = Some(canonical.to_string());
        self.last_committed_code = Some(code.to_string());
        self.last_committed_ms = now_ms;
        self.regret_backspaces = 0;
        if !canonical.is_empty() && canonical.chars().all(crate::ret::is_cjk) {
            self.extend_commit_context(canonical);
            self.last_commit_features = Some(chosen);
        } else {
            self.reset_commit_context();
        }
    }

    fn extend_commit_context(&mut self, text: &str) {
        self.context_window.extend(text.chars());
        if self.context_window.len() > 16 {
            self.context_window.drain(..self.context_window.len() - 16);
        }
        self.refresh_semantic_context();
    }

    fn learn_current_candidate(
        &mut self,
        canonical: &str,
        selected_index: usize,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) {
        let code = self.candidate_code_at(selected_index).to_string();
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

    pub(crate) fn refresh_semantic_context(&mut self) {
        self.invalidate_neural_rerank();
        #[cfg(feature = "native-onnx")]
        {
            self.semantic_epoch = self.semantic_epoch.wrapping_add(1);
        }
        self.cached_context_vec = None;
        self.cached_mlm_priors.clear();
        if !self.config.adaptive_learning || self.context_window.is_empty() {
            return;
        }
        self.cached_context_vec = Some(RetentionEngine::project_context_fallback(
            &self.context_window.iter().collect::<String>(),
        ));
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn take_semantic_context(
        &mut self,
    ) -> Option<(u64, Vec<char>, crate::models::ModelPaths)> {
        if self.semantic_requested == self.semantic_epoch
            || !self.config.adaptive_learning
            || self.context_window.len() < 2
            || self.model_paths.reranker.is_none()
        {
            return None;
        }
        self.semantic_requested = self.semantic_epoch;
        Some((
            self.semantic_epoch,
            self.context_window.clone(),
            self.model_paths.clone(),
        ))
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn is_semantic_epoch(&self, epoch: u64) -> bool {
        self.semantic_epoch == epoch
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn cancel_pending_semantic_context(&mut self) {
        self.semantic_epoch = self.semantic_epoch.wrapping_add(1);
        self.semantic_requested = self.semantic_epoch;
    }

    fn invalidate_neural_rerank(&mut self) {
        #[cfg(feature = "native-onnx")]
        {
            self.pending_neural_rerank = None;
        }
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn clear_neural_rerank_cache(&mut self) {
        self.invalidate_neural_rerank();
        self.neural_rerank_cache.invalidate();
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn take_neural_rerank(&mut self) -> Option<neural_rerank::Pending> {
        self.pending_neural_rerank.take()
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn apply_neural_rerank(&mut self, task: neural_rerank::Pending, scores: Vec<f64>) {
        let cache_generation = self
            .neural_rerank_cache
            .bind(self.model_paths.reranker.as_deref());
        if self.model_paths.reranker != task.model_paths.reranker
            || cache_generation != task.cache_generation
        {
            return;
        }
        // Only populate the cache; never mutate already-displayed candidates behind the host UI.
        self.neural_rerank_cache.store(&task.pool, task.missing, scores);
    }

    #[cfg(feature = "native-onnx")]
    pub(crate) fn apply_semantic_context(
        &mut self,
        epoch: u64,
        vector: [i8; 32],
        priors: HashMap<char, f32>,
    ) {
        if self.semantic_epoch == epoch {
            self.cached_context_vec = Some(vector);
            self.cached_mlm_priors = priors;
        }
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
            self.base_code(),
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

    fn promote_prefix_prediction(
        &self,
        pairs: &mut Vec<(CandidateItem, CandidateOrigin)>,
        word: &str,
        prediction_score: f64,
        retention: &RetentionEngine,
        now_ms: u64,
    ) {
        let Some(index) = pairs
            .iter()
            .position(|(_, origin)| origin.canonical() == word)
        else {
            return;
        };
        let context = self.prefix_prediction_context(now_ms);
        let predicted_pinned = pairs[index].0.pinned;
        // Cached language-model priors may refine ordinary candidates, but must not
        // undo the explicit context/prefix match. Preserve stronger personal choices
        // already ahead of the prediction without demoting it behind lower groups.
        let at = pairs[..index]
            .iter()
            .rposition(|(item, origin)| {
                item.pinned
                    || item.is_clipboard_shortcut
                    || item.quick_paste_id.is_some()
                    || item.function_action.is_some()
                    || matches!(
                        origin,
                        CandidateOrigin::Overlay {
                            is_clipboard: true,
                            ..
                        }
                    )
                    || (!predicted_pinned && {
                        let personal_score = retention.contextual_score_at(
                            origin.canonical(),
                            self.base_code(),
                            context.as_deref(),
                            self.active_app_id.as_deref(),
                            now_ms,
                            self.config.adaptive_learning,
                        );
                        personal_score >= PERSONAL_PRIORITY_THRESHOLD
                            && personal_score > prediction_score.max(0.0)
                    })
            })
            .map_or(0, |index| index + 1);
        if at < index {
            let predicted = pairs.remove(index);
            pairs.insert(at, predicted);
        }
    }

    pub(crate) fn candidate_code_at(&self, index: usize) -> &str {
        let code = self.base_code();
        let consumed_len = match self.candidate_origins.get(index) {
            Some(CandidateOrigin::PureRust { consumed_len, .. }) => *consumed_len,
            _ => code.len(),
        };
        &code[..consumed_len]
    }

    #[cfg(test)]
    pub(crate) fn canonical_at(&self, index: usize) -> Option<&str> {
        self.candidate_origins.get(index).map(CandidateOrigin::canonical)
    }

    fn clear_pending_phrase(&mut self) {
        self.pending_phrase_text.clear();
        self.pending_phrase_code.clear();
        self.pending_phrase_selections = 0;
    }

    fn track_phrase_selection(
        &mut self,
        canonical: &str,
        code: &str,
        finished: bool,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) {
        if self.is_active_privacy_app() || canonical.is_empty() || !canonical.chars().all(crate::ret::is_cjk) {
            self.clear_pending_phrase();
            return;
        }
        // A whole-word choice by itself is not a manual composition.
        if finished && self.pending_phrase_selections == 0 {
            return;
        }
        self.pending_phrase_text.push_str(canonical);
        if self.pending_phrase_selections > 0 {
            self.pending_phrase_code.push(' ');
        }
        self.pending_phrase_code.push_str(code);
        self.pending_phrase_selections += 1;
        if finished
            && self.config.adaptive_learning
            && self.pending_phrase_selections >= 2
            && (2..=8).contains(&self.pending_phrase_text.chars().count())
        {
            let word = self.pending_phrase_text.clone();
            let code = &self.pending_phrase_code;
            let compact = code.replace(['\'', ' '], "");
            let now = RetentionEngine::now_epoch_ms();
            let ann = crate::imp::polyphone::PolyphoneAnnotator::new(std::sync::Arc::clone(
                imported.corpus(),
            ))
            .annotate_text(&word);
            let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&ann.untoned);
            let acronym = crate::ret::UserPhrase::derive_acronym(&word, &ann.untoned, &xiaohe);
            let syllables: Vec<_> = ann.untoned.split_whitespace().collect();
            let parts: Vec<_> = code.split(['\'', ' ']).filter(|part| !part.is_empty()).collect();
            let full_code = if self.schema == SchemaKind::RimeIce {
                syllables.concat()
            } else {
                xiaohe.clone()
            };
            let used_acronym = !acronym.is_empty() && compact != full_code
                && (compact == acronym
                    || (parts.len() == word.chars().count() && parts.len() == syllables.len()
                        && parts.iter().zip(&syllables).all(|(part, syllable)| {
                            (part.len() == 1 || matches!(*part, "zh" | "ch" | "sh"))
                                && syllable.starts_with(part)
                        })));
            if used_acronym {
                retention.register_typed_phrase(&word, &ann.untoned, &xiaohe, now);
                retention.record_acronym_selection(&word, &acronym, &compact, now);
                return;
            }
            let (quanpin, xiaohe) = if self.schema == SchemaKind::RimeIce {
                (
                    if full_code == compact {
                        ann.untoned
                    } else {
                        code.replace('\'', " ")
                    },
                    xiaohe,
                )
            } else {
                (ann.untoned, compact)
            };
            retention.register_typed_phrase(&word, &quanpin, &xiaohe, now);
        }
    }

    pub(crate) fn clear_composition_state(&mut self) {
        self.invalidate_neural_rerank();
        self.pending_undo_regret = None;
        self.clear_pending_phrase();
        self.pure_rust.confirmed_prefix.clear();
        self.pure_rust.confirming_initials = false;
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        self.clear_native_composition();
        self.composing.clear();
        self.raw_input.clear();
        self.raw_touch_hints.clear();
        self.raw_cursor = 0;
        self.cursor_utf8 = 0;
        self.current_candidates.clear();
        self.candidate_origins.clear();
        self.candidate_features.clear();
        self.current_page.page_index = 0;
        self.current_page.has_prev = false;
        self.current_page.has_next = false;
        self.pure_rust.page_index = 0;
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn clear_native_composition(&mut self) {
        self.rime_first_page.clear();
        self.rime_overlay_page = 0;
        self.rime_first_has_next = false;
        self.rime_input_start = None;
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

    pub(crate) fn is_active_privacy_app(&self) -> bool {
        self.active_app_no_learning
            || self.active_app_id.as_deref().is_some_and(|app| is_privacy_app(app, &self.config.privacy_apps))
    }

    pub fn on_app_focus(&mut self, app_id: &str) -> Result<AppFocusState, VitralError> {
        self.on_app_focus_with_learning_flag(app_id, false)
    }

    pub(crate) fn on_app_focus_with_learning_flag(
        &mut self,
        app_id: &str,
        no_personalized_learning: bool,
    ) -> Result<AppFocusState, VitralError> {
        if app_id.trim().is_empty() {
            return Err(VitralError::InvalidArgument("app_id cannot be empty".into()));
        }
        let was_private = self.is_active_privacy_app();
        let app_changed = self.active_app_id.as_deref() != Some(app_id);
        self.active_app_id = Some(app_id.to_string());
        self.active_app_no_learning = no_personalized_learning;
        let private = self.is_active_privacy_app();
        let privacy_changed = was_private != private || (private && app_changed);
        if app_changed || privacy_changed {
            self.reset_commit_context();
            self.last_committed_char = None;
            self.pending_undo_regret = None;
            self.clear_pending_phrase();
        }
        let prev_lang = self.lang_mode;
        let remembered = self.app_lang_memory.contains_key(app_id);
        let lang_mode = *self.app_lang_memory.entry(app_id.to_string()).or_insert_with(|| {
            let builtins = if self.config.auto_english_for_dev_apps { DEV_APPS } else { &[] };
            if app_matches(app_id, builtins, &self.config.default_english_apps) {
                LangMode::En
            } else {
                LangMode::Zh
            }
        });
        self.lang_mode = lang_mode;
        if privacy_changed || prev_lang != lang_mode || lang_mode == LangMode::En {
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
        self.undo_commit = None;
        self.pending_undo_regret = None;
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
        self.undo_commit = None;
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

    pub(crate) fn set_syllable_split(&mut self, enabled: bool) -> Result<(), VitralError> {
        if self.config.syllable_split == enabled {
            return Ok(());
        }
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if self.schema == SchemaKind::RimeIce && !self.is_active_privacy_app() {
            if let Some(assets) = self
                .resolved_assets_dir
                .clone()
                .filter(|p| p.join("rime").is_dir())
            {
                let sid = self.ensure_rime_session(&assets)?;
                let restore = !enabled && !self.raw_input.is_empty() && !self.is_passthrough_active()
                    && self.candidate_filter().is_none();
                rime_ffi::with_runtime(
                    &assets,
                    &self.config.cache_dir,
                    &self.config.data_dir,
                    |rt| {
                        rt.clear_composition(sid);
                        // Restore native input before switching back, without committing or learning.
                        if restore {
                            for ch in self.raw_input.chars() {
                                rt.process_key(sid, ch as u32, 0)?;
                            }
                            rt.set_caret_pos(sid, self.raw_cursor)?;
                        }
                        Ok(())
                    },
                )?;
                self.rime_input_start = restore.then_some(0);
            }
            self.rime_first_page.clear();
            self.rime_overlay_page = 0;
        }
        self.invalidate_neural_rerank();
        self.config.syllable_split = enabled;
        self.pure_rust.syllable_split = enabled;
        self.pure_rust.page_index = 0;
        self.current_page.page_index = 0;
        Ok(())
    }

    pub fn set_opencc_mode(&mut self, mode: OpenCcMode) {
        if self.opencc_mode != mode {
            self.invalidate_neural_rerank();
        }
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
        self.model_paths = crate::models::ModelPaths::from_config(&self.config, self.resolved_assets_dir.as_deref())?;
        self.pure_rust.set_model_paths(&self.model_paths);
        #[cfg(feature = "native-onnx")]
        self.clear_neural_rerank_cache();
        self.refresh_semantic_context();
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        { self.rime_session_id = None; rime_ffi::clear_runtime(); }

        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let Some(root) = self.resolved_assets_dir.clone().filter(|root| {
            !self.is_active_privacy_app() && root.join("rime").is_dir()
        }) {
            let _ = self.ensure_rime_session(&root)?;
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
        self.process_touch_key(
            keycode, modifiers, swipe_dir, TouchKeyHint::default(), clipboard, imported, retention,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process_touch_key(
        &mut self,
        keycode: u32,
        modifiers: u32,
        swipe_dir: SwipeDir,
        hint: TouchKeyHint,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        self.invalidate_neural_rerank();
        let hint = hint.for_key(u8::try_from(keycode).unwrap_or_default().to_ascii_lowercase());
        // Fcitx uses X11 keysyms; normalize editing keys before interpreting
        // printable text. In particular, 0xff08 is BackSpace, not Unicode '（'.
        let keycode = match keycode {
            0xff08 => 0x08,
            0xff0d => 0x0d,
            0xff1b => 0x1b,
            _ => keycode,
        };
        if modifiers == 0 && swipe_dir == SwipeDir::None && self.lang_mode == LangMode::Zh {
            let can_filter = || !self.raw_input.is_empty()
                && !self.is_passthrough_active() && self.component_code().is_none();
            match keycode {
                0x5b | 0x5d if can_filter() => {
                    if let Some(action) = self.commit_word_edge(keycode == 0x5d, imported, retention) {
                        return Ok(action);
                    }
                }
                0x60 if can_filter() && self.candidate_filter().is_none() => {
                    #[cfg(all(target_os = "linux", feature = "native-rime"))]
                    if self.rime_input_start.is_some() {
                        self.clear_native_composition();
                    }
                    self.append_filter_key(FILTER_MARKER);
                    return self.process_key_pure_rust(0, clipboard, imported, retention);
                }
                0x31..=0x35 if self.is_filter_tone_key(keycode) => {
                    self.append_filter_key(char::from(keycode as u8));
                    return self.process_key_pure_rust(0, clipboard, imported, retention);
                }
                _ => {}
            }
        }
        let is_cursor_key = matches!(keycode, KEY_LEFT | KEY_RIGHT | KEY_HOME | KEY_END | KEY_DELETE);
        if matches!(keycode, 0x08 | 0x1b) { self.clear_pending_phrase(); }
        if keycode == 0x1b {
            self.reset_commit_context();
            self.last_committed_char = None;
            self.pending_undo_regret = None;
        }
        if keycode == 0x08 && self.raw_input.is_empty() && self.composing.is_empty() {
            if modifiers == 0 && swipe_dir == SwipeDir::None && self.lang_mode == LangMode::Zh {
                if let Some(action) = self.undo_candidate_commit(clipboard, imported, retention)? {
                    return Ok(action);
                }
            }
            self.notify_deleted_chars(1);
        }
        if keycode == 0x0d {
            if self.config.adaptive_learning && !self.is_active_privacy_app() {
                retention.record_english_casing(&self.raw_input);
            }
            self.reset_commit_context();
        }
        if keycode == 0x20 && self.raw_input.is_empty() && self.composing.is_empty() {
            self.last_committed_char = None;
        }
        if !is_cursor_key && char::from_u32(keycode).is_some_and(|c| {
            !c.is_control() && !c.is_ascii_alphanumeric() && c != '\'' && !crate::ret::is_cjk(c)
        }) && (keycode != 0x20 || self.raw_input.is_empty())
        {
            self.reset_commit_context();
        }
        if modifiers & MOD_LANG_TOGGLE != 0 {
            self.toggle_lang_mode();
            return Ok(self.empty_action(None));
        }
        if is_cursor_key && self.raw_input.is_empty() && self.composing.is_empty() {
            self.undo_commit = None;
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
            if let Some(ch) = char::from_u32(keycode).filter(|_| !is_cursor_key) {
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

        if let Some(act) = self.handle_passthrough_or_paired_punct(keycode, retention)? {
            return Ok(act);
        }

        // Xiaohe requires an apostrophe because uu is the code for shu.
        if self.raw_input.is_empty() && self.schema == SchemaKind::RimeIce && keycode == 'u' as u32 {
            return self.process_key_pure_rust_with_touch(keycode, hint, clipboard, imported, retention);
        }
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if self.schema == SchemaKind::DoublePinyinFlypy && self.rime_input_start == Some(0)
            && self.raw_input == "u" && keycode == '\'' as u32
        {
            self.clear_composition_state();
            self.insert_raw_char('u', TouchKeyHint::default());
            return self.process_key_pure_rust_with_touch(keycode, hint, clipboard, imported, retention);
        }

        if self.config.syllable_split && self.schema == SchemaKind::RimeIce {
            return self.process_key_pure_rust_with_touch(keycode, hint, clipboard, imported, retention);
        }

        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if !self.is_active_privacy_app() {
            if let Some(assets_dir) = self.resolved_assets_dir.clone() {
                if !hint.is_empty() && self.rime_input_start.is_some() {
                    self.clear_native_composition();
                } else if hint.is_empty()
                    && assets_dir.join("rime").is_dir()
                    && (self.rime_input_start.is_some()
                        || (self.raw_input.is_empty()
                            && !(self.schema == SchemaKind::DoublePinyinFlypy
                                && self.config.fuzzy_pinyin)))
                {
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

        self.process_key_pure_rust_with_touch(keycode, hint, clipboard, imported, retention)
    }

    fn commit_word_edge(
        &mut self,
        tail: bool,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Option<InputAction> {
        let (index, item) = self.current_candidates.iter().enumerate().find(|(index, item)| {
            item.function_action.is_none() && item.quick_paste_id.is_none()
                && !item.is_clipboard_shortcut && !self.is_clipboard_candidate(*index)
        })?;
        let word = self.candidate_origins.get(index)?.canonical();
        let edge = |text: &str| {
            let mut chars = text.chars();
            let first = chars.next().filter(|ch| crate::ret::is_cjk(*ch))?;
            let last = chars.next_back().filter(|ch| crate::ret::is_cjk(*ch))?;
            chars.all(crate::ret::is_cjk).then_some(if tail { last } else { first })
        };
        let original = edge(word)?.to_string();
        let displayed = edge(&item.text)?;
        let displayed_text = displayed.to_string();
        let same_character = self.convert_opencc(&original) == displayed_text;
        let canonical = if same_character { original } else { displayed_text };
        let mut committed = self.confirmed_non_ascii_prefix();
        committed.push(displayed);
        let now_ms = RetentionEngine::now_epoch_ms();
        if self.config.adaptive_learning {
            let reading = if same_character {
                candidate_reading(item, word, imported)
            } else {
                // Regional phrase conversion may substitute a different character.
                Cow::Borrowed(displayed.to_pinyin().map_or("", |p| p.plain()))
            };
            let mut syllables = reading.split_whitespace();
            let syllable = if tail { syllables.next_back() } else { syllables.next() };
            let code = syllable.map_or_else(String::new, |syllable| match self.schema {
                SchemaKind::RimeIce => xiaohe::normalize_syllable(syllable),
                SchemaKind::DoublePinyinFlypy => xiaohe::syllable_to_xiaohe(syllable),
            });
            self.record_candidate_commit(&canonical, &code, None, imported, retention, now_ms);
        } else {
            self.settle_undo_regret(Some(&canonical), retention);
            if self.previous_context(now_ms).is_none() {
                self.context_window.clear();
            }
            self.last_commit_context = self.previous_context(now_ms).map(str::to_owned);
            self.last_commit_window.clone_from(&self.context_window);
            self.last_commit_features = None;
            self.last_committed_code = None;
            self.last_committed_word = Some(canonical.clone());
            self.last_committed_ms = now_ms;
            self.regret_backspaces = 0;
            self.extend_commit_context(&canonical);
        }
        self.clear_composition_state();
        Some(self.commit_action(Some(committed), true))
    }

    fn undo_candidate_commit(
        &mut self,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<Option<InputAction>, VitralError> {
        let now = RetentionEngine::now_epoch_ms();
        if !self.config.undo_commit_on_backspace || now.saturating_sub(self.last_committed_ms) > 5000 {
            return Ok(None);
        }
        let Some(undo) = self.undo_commit.take() else {
            return Ok(None);
        };
        let (Some(word), Some(code)) = (
            self.last_committed_word.take(), self.last_committed_code.take(),
        ) else {
            return Ok(None);
        };
        let context = self.last_commit_context.take();
        self.context_window = std::mem::take(&mut self.last_commit_window);
        self.refresh_semantic_context();
        self.last_commit_features = None;
        self.last_committed_char = undo.previous_char;
        self.regret_backspaces = 0;

        let mut action = self.restore_candidate_code(
            code.clone(), &undo, clipboard, imported, retention,
        )?;
        self.pending_undo_regret = Some(PendingUndoRegret { word, code, context });
        action.undo_commit_chars = undo.text.chars().count();
        action.undo_commit_text = Some(undo.text);
        Ok(Some(action))
    }

    pub(crate) fn settle_undo_regret(&mut self, selected: Option<&str>, retention: &mut RetentionEngine) {
        if let Some(regret) = self.pending_undo_regret.take() {
            if selected != Some(regret.word.as_str()) && !self.is_active_privacy_app() {
                retention.penalize_selection(
                    &regret.code,
                    &regret.word,
                    regret.context.as_deref(),
                    self.active_app_id.as_deref(),
                    RetentionEngine::now_epoch_ms(),
                );
            }
        }
    }

    fn restore_candidate_code(
        &mut self,
        code: String,
        undo: &UndoCommit,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if undo.native && !self.is_active_privacy_app() {
            if let (Some(sid), Some(assets)) = (self.rime_session_id, &self.resolved_assets_dir) {
                let snapshot = rime_ffi::with_runtime(
                    assets, &self.config.cache_dir, &self.config.data_dir, |rt| {
                        rt.clear_composition(sid);
                        for ch in code.chars() {
                            rt.process_key(sid, ch as u32, 0)?;
                        }
                        rt.snapshot(sid)
                    },
                );
                if let Ok(snapshot) = snapshot {
                    if snapshot.commit_text.is_none() && snapshot.raw_input == code {
                        return Ok(self.apply_rime_snapshot(
                            snapshot, false, false, clipboard, imported, retention,
                        ));
                    }
                }
                self.clear_composition_state();
            }
        }
        self.set_raw_input(code);
        let len = self.raw_touch_hints.len().min(undo.touch_hints.len());
        self.raw_touch_hints[..len].copy_from_slice(&undo.touch_hints[..len]);
        self.raw_cursor = self.raw_input.len();
        self.process_key_pure_rust(0, clipboard, imported, retention)
    }

    pub fn set_preedit_cursor(
        &mut self,
        cursor_utf8: usize,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        let cursor = clamp_cursor(&self.composing, cursor_utf8);
        if cursor == self.cursor_utf8 {
            return Ok(self.empty_action(None));
        }
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        if let (Some(sid), Some(assets), Some(start)) = (
            self.rime_session_id, &self.resolved_assets_dir, self.rime_input_start,
        ) {
            let target = start + self.raw_cursor_for_rime_preedit(cursor);
            let snap = rime_ffi::with_runtime(
                assets, &self.config.cache_dir, &self.config.data_dir,
                |rt| rt.set_caret_pos(sid, target),
            )?;
            return Ok(self.apply_rime_snapshot(snap, false, false, clipboard, imported, retention));
        }
        #[cfg(not(all(target_os = "linux", feature = "native-rime")))]
        let _ = (clipboard, imported, retention);
        let prefix_len = self.preedit_prefix_len();
        self.raw_cursor = clamp_cursor(&self.raw_input, cursor.saturating_sub(prefix_len));
        self.cursor_utf8 = prefix_len + self.raw_cursor;
        Ok(self.empty_action(None))
    }

    fn move_raw_cursor(&mut self, keycode: u32) -> bool {
        self.raw_cursor = match keycode {
            KEY_LEFT => self.previous_raw_boundary(),
            KEY_RIGHT => self.raw_input[self.raw_cursor..]
                .chars().next().map_or(self.raw_cursor, |ch| self.raw_cursor + ch.len_utf8()),
            KEY_HOME => 0,
            KEY_END => self.raw_input.len(),
            _ => return false,
        };
        self.cursor_utf8 = self.preedit_prefix_len() + self.raw_cursor;
        true
    }

    fn preedit_prefix_len(&self) -> usize {
        self.composing.strip_suffix(&self.raw_input).map_or(0, str::len)
    }

    fn previous_raw_boundary(&self) -> usize {
        self.raw_input[..self.raw_cursor].char_indices().next_back().map_or(0, |(index, _)| index)
    }

    pub fn raw_input(&self) -> &str {
        &self.raw_input
    }

    pub(crate) fn set_raw_input(&mut self, raw_input: String) {
        self.invalidate_neural_rerank();
        self.raw_input = raw_input;
        self.raw_cursor = clamp_cursor(&self.raw_input, self.raw_cursor);
        self.raw_touch_hints.clear();
        self.raw_touch_hints.resize(self.raw_input.len(), TouchKeyHint::default());
    }

    fn insert_raw_char(&mut self, ch: char, hint: TouchKeyHint) {
        self.raw_input.insert(self.raw_cursor, ch);
        let width = ch.len_utf8();
        self.raw_touch_hints.splice(
            self.raw_cursor..self.raw_cursor,
            std::iter::once(hint).chain(std::iter::repeat_n(TouchKeyHint::default(), width - 1)),
        );
        self.raw_cursor += width;
    }

    fn append_filter_key(&mut self, ch: char) {
        let keep_cursor = ch == FILTER_MARKER && self.raw_cursor < self.raw_input.len();
        self.raw_input.push(ch);
        self.raw_touch_hints.push(TouchKeyHint::default());
        if !keep_cursor {
            self.raw_cursor = self.raw_input.len();
        }
        self.pure_rust.page_index = 0;
    }

    fn base_code(&self) -> &str {
        self.raw_input.split_once(FILTER_MARKER).map_or(&self.raw_input, |(code, _)| code)
    }

    fn candidate_filter(&self) -> Option<&str> {
        self.raw_input.split_once(FILTER_MARKER).map(|(_, filter)| filter)
    }

    pub(crate) fn is_filter_tone_key(&self, keycode: u32) -> bool {
        (0x31..=0x35).contains(&keycode) && self.raw_input.ends_with(FILTER_MARKER)
            && self.raw_input.len() > 1 && !self.is_passthrough_active()
    }

    fn delete_raw_char(&mut self, backward: bool) -> bool {
        if backward {
            if self.raw_cursor == 0 {
                return false;
            }
            self.raw_cursor = self.previous_raw_boundary();
        }
        if self.raw_cursor == self.raw_input.len() {
            return false;
        }
        let ch = self.raw_input.remove(self.raw_cursor);
        self.raw_touch_hints.drain(self.raw_cursor..self.raw_cursor + ch.len_utf8());
        true
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

    pub(crate) fn is_passthrough_active(&self) -> bool {
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
        let page_size = self.candidate_page_size();
        self.composing = self.raw_input.clone();
        self.cursor_utf8 = self.raw_cursor;
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
            imported: false,
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
        retention: &mut RetentionEngine,
    ) -> Result<Option<InputAction>, VitralError> {
        let is_backspace = keycode == 0x08 || keycode == 0xFF08;
        let is_escape = keycode == 0x1B || keycode == 0xFF1B;
        let is_space = keycode == 0x20;
        let is_return = keycode == 0x0D || keycode == 0xFF0D;

        if self.is_passthrough_active() {
            if self.move_raw_cursor(keycode) {
                return Ok(Some(self.empty_action(None)));
            }
            if is_escape {
                self.clear_composition_state();
                return Ok(Some(self.empty_action(None)));
            }
            if is_backspace || keycode == KEY_DELETE {
                if !self.delete_raw_char(is_backspace) {
                    return Ok(Some(self.empty_action(None)));
                }
                if self.raw_input.is_empty() {
                    self.clear_composition_state();
                    return Ok(Some(self.empty_action(None)));
                }
                return Ok(Some(self.build_passthrough_action()));
            }
            if is_space || is_return {
                let committed = self.raw_input.clone();
                self.settle_undo_regret(None, retention);
                self.clear_composition_state();
                return Ok(Some(self.commit_action(Some(committed), false)));
            }
            if let Some(ch) = char::from_u32(keycode) {
                if !ch.is_control() {
                    self.insert_raw_char(ch, TouchKeyHint::default());
                    return Ok(Some(self.build_passthrough_action()));
                }
            }
            return Ok(Some(self.build_passthrough_action()));
        }

        if let Some(ch) = char::from_u32(keycode)
            .filter(|_| !matches!(keycode, KEY_LEFT | KEY_RIGHT | KEY_HOME | KEY_END | KEY_DELETE))
        {
            if !ch.is_control() && self.should_enter_passthrough(ch) {
                self.insert_raw_char(ch, TouchKeyHint::default());
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
                #[cfg(all(target_os = "linux", feature = "native-rime"))]
                {
                    self.rime_input_start = None;
                }
                return Ok(Some(self.build_passthrough_action()));
            }

            let single_quote = matches!(ch, '\'' | '‘' | '『');
            if !single_quote || (self.raw_input.is_empty() && self.composing.is_empty()) {
                let corner = self.config.quote_style == "corner";
                let pair = match ch {
                    '"' | '“' => Some(if corner { "「」" } else { "“”" }),
                    '\'' | '‘' => Some(if corner { "『』" } else { "‘’" }),
                    '『' => Some("『』"),
                    _ => match_paired_punctuation(ch),
                };
                if let Some(pair) = pair {
                    let disable_pair = self.active_app_id.as_deref().is_some_and(|app| {
                        app_matches(app, &[], &self.config.disable_paired_punct_apps)
                            || (self.config.disable_paired_punct_in_ide && is_dev_app(app))
                    });
                    let prefix = if !self.raw_input.is_empty() {
                        self.current_candidates.first().map(|c| c.text.clone())
                    } else {
                        None
                    };
                    let clipboard_commit = prefix.is_some() && self.is_clipboard_shortcut_candidate(0);
                    if clipboard_commit {
                        self.pending_undo_regret = None;
                    } else {
                        self.settle_undo_regret(None, retention);
                    }
                    self.clear_composition_state();
                    let mut committed = prefix.unwrap_or_default();
                    if disable_pair {
                        committed.extend(pair.chars().take(1));
                        let mut act = self.commit_action(Some(committed), !clipboard_commit);
                        act.clipboard_commit = clipboard_commit;
                        return Ok(Some(act));
                    }
                    committed.push_str(pair);
                    let mut act = self.commit_action(Some(committed), !clipboard_commit);
                    act.clipboard_commit = clipboard_commit;
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

    pub(crate) fn raw_commit_text(&self) -> String {
        let mut text = if self.is_passthrough_active() {
            String::new()
        } else {
            self.confirmed_non_ascii_prefix()
        };
        text.push_str(&self.raw_input);
        text
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn rime_cursor_in_raw_prefix(&self, raw: &str, mut display: &str, target: usize) -> usize {
        // Work back from Rime's caret. Spaces may be added for display and
        // each expanded Xiaohe syllable may correspond to just two raw bytes.
        let mut end = raw.len();
        while target < display.len() {
            let remaining = &raw[..end];
            if display.ends_with([' ', '\'']) {
                if remaining.ends_with([' ', '\'']) {
                    end -= 1;
                }
                display = &display[..display.len() - 1];
                continue;
            }

            let start = display.rfind([' ', '\'']).map_or(0, |index| index + 1);
            let syllable = &display[start..];
            let mut width = syllable.len();
            if self.schema == SchemaKind::DoublePinyinFlypy {
                let code = crate::imp::xiaohe::syllable_to_xiaohe(syllable);
                if !code.is_empty() && remaining.ends_with(&code) {
                    width = code.len();
                }
            }
            let raw_start = clamp_cursor(raw, end.saturating_sub(width));
            if target >= start {
                return clamp_cursor(raw, raw_start + (target - start).min((end - raw_start).saturating_sub(1)));
            }
            end = raw_start;
            display = &display[..start];
        }
        end
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn raw_cursor_for_rime_preedit(&self, cursor: usize) -> usize {
        let prefix_end = self.composing.find(|c: char| c.is_ascii()).unwrap_or(self.composing.len());
        if cursor <= prefix_end {
            return 0;
        }
        if cursor >= self.cursor_utf8 {
            // librime appends the untranslated raw suffix after the caret.
            return clamp_cursor(&self.raw_input, self.raw_cursor + cursor - self.cursor_utf8);
        }
        self.rime_cursor_in_raw_prefix(
            &self.raw_input[..self.raw_cursor], &self.composing[prefix_end..self.cursor_utf8],
            cursor - prefix_end,
        )
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn sync_rime_preedit(&mut self, snap: &rime_ffi::RimeSnapshot) {
        let (preedit, cursor) = if snap.preedit.is_empty() {
            (&snap.raw_input, snap.raw_cursor)
        } else {
            (&snap.preedit, snap.cursor_utf8)
        };
        let cursor = clamp_cursor(preedit, cursor);
        let caret = clamp_cursor(&snap.raw_input, snap.raw_cursor);
        let prefix_end = preedit.find(|c: char| c.is_ascii()).unwrap_or(preedit.len());
        let (prefix, tail) = preedit.split_at(prefix_end);
        let start = if prefix.is_empty() {
            0
        } else {
            self.rime_cursor_in_raw_prefix(&snap.raw_input[..caret], &tail[..cursor.saturating_sub(prefix_end)], 0)
        };
        self.rime_input_start = Some(start);
        self.set_raw_input(snap.raw_input[start..].to_owned());
        self.raw_cursor = caret - start;
        self.pure_rust.confirming_initials = false;
        self.pure_rust.confirmed_prefix = prefix.to_owned();

        let converted = self.convert_opencc(prefix);
        self.cursor_utf8 = if cursor < prefix_end {
            let chars = prefix[..cursor].chars().count();
            converted.char_indices().nth(chars).map_or(converted.len(), |(index, _)| index)
        } else {
            converted.len() + cursor - prefix_end
        };
        self.composing = converted + tail;
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
            || (snap.preedit.is_empty() && snap.candidates.is_empty() && snap.raw_input.is_empty())
        {
            self.clear_composition_state();
            return self.empty_action(None);
        }

        self.sync_rime_preedit(&snap);

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
                    rime_page_idx: rime_idx,
                };
                (item, origin, rime_idx * 2)
            })
            .collect();

        for (item, origin, rank) in &mut base_pairs {
            self.score_pair(item, origin, retention, now_ms, *rank / 2);
        }
        if snap.page_index == 0 {
            let context = self.prefix_prediction_context(now_ms);
            let decoded = std::cell::OnceCell::new();
            let simplified = std::cell::OnceCell::new();
            let converted = std::cell::OnceCell::new();
            let candidates = |mode| {
                // Deduplication uses display text, so each OpenCC mode keeps its own ranking.
                let ranked = if mode == OpenCcMode::Simplified { &simplified } else { &converted };
                ranked.get_or_init(|| {
                    let decoded = decoded.get_or_init(|| self.pure_rust.decode_candidates(
                        &self.raw_input, self.schema, Some(imported), Some(retention), None,
                    ));
                    self.pure_rust.rank_candidates(
                        decoded, &self.raw_input, self.schema, mode, Some(imported),
                        Some(retention), context.as_deref(), self.active_app_id.as_deref(),
                        self.config.adaptive_learning, None,
                    )
                })
            };
            let (mut priority_overlay, mut seen) =
                self.build_overlay_candidates(clipboard, imported, retention, true);
            // Learned full readings share Frecency ranking with native alternatives.
            // Automatically registered dictionary words must not become permanent
            // priority overlays merely because they were selected twice.
            priority_overlay.retain(|(item, origin)| {
                if matches!(item.comment.as_str(), "习惯" | "剪贴板")
                    && !matches!(origin, CandidateOrigin::Overlay { imported: true, .. })
                {
                    seen.remove(&item.text);
                    let rank = base_pairs.len() * 2;
                    base_pairs.push((item.clone(), origin.clone(), rank));
                    false
                } else {
                    true
                }
            });
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
                let (pr, _) = candidates(OpenCcMode::Simplified);
                let mut added_full_alt = 0usize;
                for (
                    offset,
                    DecodedCandidate {
                        item: pr_item,
                        canonical,
                        consumed_len,
                        lm_score,
                        ..
                    },
                ) in pr.iter().enumerate()
                {
                    if *consumed_len != self.raw_input.len()
                        || retention.erased_words.contains(canonical)
                    {
                        continue;
                    }
                    let frec = self.contextual_score(retention, canonical, now_ms);
                    let pinned = retention.pinned_words.contains(canonical);
                    let is_full_alt =
                        need_full_span_alt && added_full_alt == 0 && canonical.chars().count() >= max_rime_len
                            && (self.schema == SchemaKind::DoublePinyinFlypy
                                || pr_item.comment.split_whitespace().collect::<String>() == self.raw_input.replace(['\'', ' '], ""));
                    if frec <= 0.0 && !pinned && !is_full_alt {
                        continue;
                    }
                    let converted = if self.opencc_mode == OpenCcMode::Simplified {
                        pr_item.text.clone()
                    } else {
                        self.convert_opencc(canonical)
                    };
                    if seen.contains(&converted) || !seen_base.insert(converted.clone()) {
                        continue;
                    }
                    let item = CandidateItem {
                        index: 0,
                        text: converted.clone(),
                        comment: pr_item.comment.clone(),
                        pinned,
                        is_clipboard_shortcut: false,
                        quick_paste_id: None,
                        function_action: None,
                        shortcut_code: None,
                        frecency_score: frec,
                    };
                    let origin = CandidateOrigin::PureRust {
                        consumed_len: *consumed_len,
                        lm_score: *lm_score,
                        #[cfg(any(feature = "native-onnx", test))]
                        rank_score: 0.0,
                        spatial_guarded: false,
                        features: [0.0; 9],
                        canonical: canonical.clone(),
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
                                consumed_len: self.raw_input.len(),
                                lm_score: score,
                                #[cfg(any(feature = "native-onnx", test))]
                                rank_score: 0.0,
                                spatial_guarded: false,
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
                    // The learned overlay already matched the whole input code.
                    // Its recorded reading can differ from the dictionary annotation.
                    full_span.extend(
                        base_pairs.iter()
                            .filter(|(item, _, _)| item.comment == "习惯")
                            .map(|(_, origin, _)| origin.canonical().to_owned()),
                    );
                }
            }

            for (item, origin, rank) in &mut base_pairs {
                if *origin.features() == [0.0; 9] {
                    self.score_pair(item, origin, retention, now_ms, *rank);
                }
            }
            let corpus = self.pure_rust.get_corpus();
            let leading_weight = base_pairs.iter()
                .filter_map(|(_, origin, _)| corpus.word_weight(origin.canonical()))
                .max()
                .unwrap_or(0);
            for (item, origin, _) in &mut base_pairs {
                let word = origin.canonical();
                if origin.features()[5] <= 0.0 {
                    item.frecency_score = retention.protect_dictionary_rank(
                        word, item.frecency_score, corpus.word_weight(word).unwrap_or(0), leading_weight,
                    );
                }
            }
            // Exact imports and sentences containing imports share the native
            // candidate list so pins and established habits can outrank them.
            base_pairs.retain(|(item, _, _)| !seen.contains(&item.text));
            let compare_full_span = !full_span.is_empty();
            priority_overlay.retain(|(item, origin)| {
                if matches!(origin, CandidateOrigin::Overlay { imported: true, .. }) {
                    seen.remove(&item.text);
                    if compare_full_span {
                        full_span.insert(origin.canonical().to_owned());
                    }
                    let rank = base_pairs.len() * 2;
                    base_pairs.push((item.clone(), origin.clone(), rank));
                    false
                } else {
                    true
                }
            });
            base_pairs.sort_by(|a, b| {
                full_span
                    .contains(b.1.canonical())
                    .cmp(&full_span.contains(a.1.canonical()))
                    .then_with(|| b.0.pinned.cmp(&a.0.pinned))
                    .then_with(|| {
                        b.0.frecency_score
                            .max(0.0)
                            .total_cmp(&a.0.frecency_score.max(0.0))
                    })
                    .then_with(|| a.2.cmp(&b.2))
            });

            let (abbreviations, mut merged): (Vec<_>, Vec<_>) = priority_overlay
                .into_iter()
                .partition(|(item, _)| item.comment == "简拼");
            for (c, orig, _) in base_pairs {
                if seen.insert(c.text.clone()) {
                    merged.push((c, orig));
                }
            }
            let abbreviation_at = merged.iter().take_while(|(item, origin)| {
                item.pinned || item.frecency_score > 0.0
                    || matches!(origin, CandidateOrigin::Overlay { .. })
            }).count();
            merged.splice(abbreviation_at..abbreviation_at, abbreviations);
            let mut priority_overlay = merged;
            if (self.config.fuzzy_pinyin && self.schema == SchemaKind::RimeIce)
                || english::lookup_core_english(&self.raw_input, Some(retention)).is_some()
            {
                let (decoded, _) = candidates(self.opencc_mode);
                let decoded_pinned = decoded.iter().take_while(|c| c.item.pinned).count();
                let english_at = decoded.iter().position(|c| c.item.comment == "英文");
                let priority_count = priority_overlay
                    .iter()
                    .take_while(|(c, _)| {
                        c.quick_paste_id.is_some()
                            || c.function_action.is_some()
                            || c.is_clipboard_shortcut
                    })
                    .count();
                let mut fuzzy_added = 0;
                for (rank, candidate) in decoded.iter().enumerate() {
                    let english = candidate.item.comment == "英文";
                    let fuzzy = self.config.fuzzy_pinyin
                        && self.schema == SchemaKind::RimeIce
                        && candidate.canonical.chars().all(crate::ret::is_cjk)
                        && candidate.consumed_len == self.raw_input.len()
                        && candidate.item.comment.replace(' ', "") != self.raw_input
                        && fuzzy_added < 3;
                    if !(english || fuzzy) {
                        continue;
                    }
                    if english {
                        priority_overlay
                            .retain(|(_, origin)| origin.canonical() != candidate.canonical);
                        seen.remove(&candidate.item.text);
                    }
                    if !seen.insert(candidate.item.text.clone()) {
                        continue;
                    }
                    let origin = CandidateOrigin::PureRust {
                        consumed_len: candidate.consumed_len,
                        lm_score: candidate.lm_score,
                        #[cfg(any(feature = "native-onnx", test))]
                        rank_score: 0.0,
                        spatial_guarded: false,
                        features: [0.0; 9],
                        canonical: candidate.canonical.clone(),
                        commit_text: candidate.item.text.clone(),
                    };
                    let mut item = candidate.item.clone();
                    let mut origin = origin;
                    self.score_pair(&mut item, &mut origin, retention, now_ms, rank);
                    let insert_at = if english {
                        let pinned_end = priority_overlay
                            .iter()
                            .rposition(|(c, _)| {
                                c.quick_paste_id.is_some()
                                    || c.function_action.is_some()
                                    || c.is_clipboard_shortcut
                                    || c.pinned
                            })
                            .map_or(0, |index| index + 1);
                        if item.pinned {
                            pinned_end
                        } else {
                            pinned_end + english_at.unwrap_or(0).saturating_sub(decoded_pinned)
                        }
                    } else {
                        priority_count + fuzzy_added + 1
                    };
                    priority_overlay.insert(
                        insert_at.min(priority_overlay.len()),
                        (item, origin),
                    );
                    if fuzzy {
                        fuzzy_added += 1;
                    }
                }
            }
            let empty_spatial = HashSet::new();
            self.insert_corrections(&mut priority_overlay, imported, retention, now_ms, 0, &empty_spatial);
            if let Some(context) = context.as_deref() {
                let active = if self.opencc_mode == OpenCcMode::Simplified { &simplified } else { &converted };
                // If no other branch decoded, first check for a prefix worth ranking.
                let prediction = active.get().or_else(|| self.pure_rust.contextual_prefix_candidate(
                    &self.raw_input,
                    self.schema,
                    Some(imported),
                    Some(retention),
                    context,
                    self.config.adaptive_learning,
                ).map(|_| candidates(self.opencc_mode)));
                if let Some((decoded, Some((word, prediction_score)))) = prediction {
                    // The portable ranking already guarded against explicitly typed longer sentences.
                    if let Some(candidate) = decoded.first() {
                        let mut item = candidate.item.clone();
                        let mut origin = CandidateOrigin::PureRust {
                            consumed_len: candidate.consumed_len,
                            lm_score: candidate.lm_score,
                            #[cfg(any(feature = "native-onnx", test))]
                            rank_score: 0.0,
                            spatial_guarded: false,
                            features: [0.0; 9],
                            canonical: candidate.canonical.clone(),
                            commit_text: item.text.clone(),
                        };
                        self.score_pair(&mut item, &mut origin, retention, now_ms, 0);
                        priority_overlay.retain(|(_, other)| other.canonical() != word);
                        priority_overlay.push((item, origin));
                        self.promote_prefix_prediction(
                            &mut priority_overlay,
                            word,
                            *prediction_score,
                            retention,
                            now_ms,
                        );
                    }
                }
            }

            // Keep words displaced by the added first-page candidates available for paging.
            let extra = priority_overlay.iter().filter(|(item, _)| {
                item.quick_paste_id.is_some() || item.function_action.is_some()
                    || matches!(item.comment.as_str(), "简拼" | "纠错")
            }).count();
            priority_overlay.truncate(page_size + extra);
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
        self.invalidate_neural_rerank();
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
        let is_escape = keycode == 0x1B || keycode == 0xFF1B;
        let is_space = keycode == 0x20;
        let is_return = keycode == 0x0D || keycode == 0xFF0D;

        // All selection entry points share partial-selection bookkeeping.
        let selection = if is_space && !self.raw_input.is_empty() {
            Some(0)
        } else if !self.raw_input.is_empty() {
            char::from_u32(keycode)
                .and_then(|ch| ch.to_digit(10))
                .filter(|digit| (1..=self.current_page.page_size as u32).contains(digit))
                .map(|digit| digit as usize - 1)
        } else {
            None
        };
        if let Some(index) = selection.filter(|i| *i < self.candidate_origins.len()) {
            return self.select_candidate_action(index, clipboard, imported, retention);
        }

        if self.raw_input.is_empty() && self.composing.is_empty() {
            if let Some(ch) = char::from_u32(keycode).filter(|ch| {
                !ch.is_control() && !ch.is_ascii_alphabetic() && *ch != '\''
            }) {
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
        if let Some(committed) = snap.commit_text.as_deref() {
            self.settle_undo_regret(Some(committed), retention);
        }
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
        self.process_key_pure_rust_with_touch(
            keycode, TouchKeyHint::default(), clipboard, imported, retention,
        )
    }

    fn process_key_pure_rust_with_touch(
        &mut self,
        keycode: u32,
        hint: TouchKeyHint,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        let page_size = self.candidate_page_size();
        self.raw_cursor = clamp_cursor(&self.raw_input, self.raw_cursor);
        if self.move_raw_cursor(keycode) {
            return Ok(self.empty_action(None));
        }

        // process_key normalizes X11 Escape; U+FF1B here is the Chinese semicolon.
        if matches!(keycode, 0x08 | 0xff08 | KEY_DELETE) {
            let backward = keycode != KEY_DELETE;
            let removed_at = if backward { self.previous_raw_boundary() } else { self.raw_cursor };
            let removing_marker = self.raw_input[removed_at..].starts_with(FILTER_MARKER);
            if !self.delete_raw_char(backward) {
                return Ok(self.empty_action(None));
            }
            if removing_marker {
                self.raw_input.truncate(self.raw_cursor);
                self.raw_touch_hints.truncate(self.raw_cursor);
            }
            self.pure_rust.page_index = 0;
        } else if keycode == 0x1B {
            self.clear_composition_state();
            return Ok(self.empty_action(None));
        } else if keycode == 0x20 {
            if !self.raw_input.is_empty() {
                if !self.candidate_origins.is_empty() {
                    return self.select_candidate_action(0, clipboard, imported, retention);
                }
                let committed = self.raw_commit_text();
                self.settle_undo_regret(None, retention);
                self.clear_composition_state();
                return Ok(self.empty_action(Some(committed)));
            }
            return Ok(self.empty_action(Some(" ".to_string())));
        } else if keycode == 0x0D || keycode == 0xFF0D {
            let committed = if self.raw_input.is_empty() {
                None
            } else {
                self.settle_undo_regret(None, retention);
                Some(self.raw_commit_text())
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
            if ch.is_ascii_lowercase() && self.candidate_filter().is_some() {
                if self.candidate_filter().is_some_and(|filter| filter.len() < MAX_FILTER_LEN) {
                    self.append_filter_key(ch);
                }
            } else if ch.is_ascii_alphabetic()
                || (ch == '\'' && (self.candidate_filter().is_none() || self.raw_cursor <= self.base_code().len()))
            {
                self.insert_raw_char(ch.to_ascii_lowercase(), hint);
                self.pure_rust.page_index = 0;
            } else if ch.is_ascii_digit() && !self.raw_input.is_empty() {
                let digit = ch.to_digit(10).unwrap_or(0) as usize;
                if (1..=page_size.min(9)).contains(&digit)
                    && self.current_candidates.get(digit - 1).is_some()
                {
                    return self.select_candidate_action(
                        digit - 1,
                        clipboard,
                        imported,
                        retention,
                    );
                }
            } else if let Some(punctuation) = map_zh_punctuation(ch) {
                let clipboard_commit = !self.raw_input.is_empty() && self.is_clipboard_shortcut_candidate(0);
                let mut text = if self.raw_input.is_empty() {
                    String::new()
                } else if !self.candidate_origins.is_empty() {
                    self.select_candidate_on_backend(0, clipboard, imported, retention)?
                        .unwrap_or_default()
                } else {
                    self.settle_undo_regret(None, retention);
                    self.raw_input.clone()
                };
                self.clear_composition_state();
                text.push_str(punctuation);
                let mut act = self.commit_action(Some(text), !clipboard_commit);
                act.clipboard_commit = clipboard_commit;
                return Ok(act);
            } else if !ch.is_control() && self.raw_input.is_empty() {
                let text = ch.to_string();
                return Ok(self.empty_action(Some(text)));
            }
        }

        if self.base_code().is_empty() {
            self.clear_composition_state();
            return Ok(self.empty_action(None));
        }

        if let Some(code) = self.component_code() {
            let ids = crate::hw::ids_exta::IdsIndex::load_cached(self.resolved_assets_dir.as_deref());
            let candidates = ids.lookup_component_code(code, imported.corpus());
            let mut seen = HashSet::new();
            let mut pairs: Vec<_> = candidates.into_iter().filter_map(|ch| {
                let canonical = ch.to_string();
                if retention.erased_words.contains(&canonical) {
                    return None;
                }
                let text = self.convert_opencc(&canonical);
                if !seen.insert(text.clone()) {
                    return None;
                }
                let comment = imported.char_phonetics(ch).into_iter().next()
                    .map_or_else(String::new, |(pinyin, xiaohe)| format!("{pinyin} · {xiaohe}"));
                let item = CandidateItem {
                    index: 0,
                    text: text.clone(),
                    comment,
                    pinned: retention.pinned_words.contains(&canonical),
                    is_clipboard_shortcut: false,
                    quick_paste_id: None,
                    function_action: None,
                    shortcut_code: None,
                    frecency_score: 0.0,
                };
                Some((item, CandidateOrigin::Components {
                    canonical, commit_text: text, features: [0.0; 9],
                }))
            }).collect();
            if self.schema == SchemaKind::RimeIce {
                let dictionary = self.pure_rust.english_dictionary();
                let raw = self.raw_input.as_str();
                let retained = Some(&*retention);
                let exact = dictionary.lookup_english(raw, retained);
                let completions = if pairs.is_empty() && exact.is_none() {
                    dictionary.complete_english(raw, retained, 2)
                } else {
                    Vec::new()
                };
                let english = exact.into_iter()
                    .map(|word| (word, word.to_owned(), "英文"));
                let mixed = dictionary.lookup_cn_en(raw, self.schema, retained)
                    .map(|word| (word, self.convert_opencc(word), "中英"));
                let completions = completions.into_iter()
                    .map(|word| (word, word.to_owned(), "英文"));
                let mut language: Vec<_> = english.chain(mixed).chain(completions)
                    .filter_map(|(word, text, comment)| {
                        DecodedCandidate::language(word, text, comment, raw.len(), retained)
                    })
                    .filter(|candidate| seen.insert(candidate.item.text.clone()))
                    .map(DecodedCandidate::into_portable_pair)
                    .collect();
                language.sort_by_key(|(item, _)| !item.pinned);
                let now_ms = RetentionEngine::now_epoch_ms();
                for (rank, (item, origin)) in language.iter_mut().enumerate() {
                    self.score_pair(item, origin, retention, now_ms, pairs.len() + rank);
                }
                pairs.extend(language);
            }
            return Ok(self.show_portable_page(pairs));
        }

        let (code, filter) = self.raw_input.split_once(FILTER_MARKER).map_or(
            (self.raw_input.as_str(), None),
            |(code, filter)| (code, Some(filter)),
        );
        let now_ms = RetentionEngine::now_epoch_ms();
        let prefix_context = self.prefix_prediction_context(now_ms);
        let touch_hints = (self.raw_touch_hints.iter().any(|hint| !hint.is_empty())
            && !self.is_passthrough_active())
            .then(|| &self.raw_touch_hints[..code.len()]);
        let (base_pairs, contextual_prediction) = self.pure_rust.query_candidates_with_prediction(
            code,
            self.schema,
            self.opencc_mode,
            Some(imported),
            Some(retention),
            prefix_context.as_deref(),
            self.active_app_id.as_deref(),
            self.config.adaptive_learning,
            touch_hints,
        );

        let corpus = self.pure_rust.get_corpus();
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        let xiaohe_fuzzy = flypy && self.pure_rust.fuzzy_pinyin;
        let leading_weight = base_pairs.iter()
            .filter(|c| c.consumed_len == code.len() && c.spatial_edits == 0)
            .filter_map(|c| corpus.word_weight(&c.canonical))
            .max()
            .unwrap_or(0);
        let (overlays, mut seen) =
            self.build_overlay_candidates(clipboard, imported, retention, false);
        // Overlay-only exact matches start at the best regular full reading.
        // An exact import gets its explicit boost once, in the overlay.
        let best_lm = base_pairs.iter()
            .filter(|c| c.consumed_len == code.len() && c.spatial_edits == 0 && !matches!(c.item.comment.as_str(), "英文" | "中英")
                && imported.lookup_imported_score(&c.canonical).is_none())
            .map(|c| c.lm_score)
            .chain(overlays.iter().filter_map(|(_, origin)| match origin {
                CandidateOrigin::PureRust { lm_score, .. } => Some(*lm_score),
                _ => None,
            }))
            .reduce(f64::max)
            .unwrap_or(0.0);
        let candidate_group = |origin: &CandidateOrigin| match origin {
            // Functional commands, macros and explicit paste shortcuts retain their order.
            CandidateOrigin::Overlay {
                is_clipboard: true, ..
            } => 0,
            CandidateOrigin::PureRust { consumed_len, .. }
                if *consumed_len < code.len() =>
            {
                2
            }
            _ => 1,
        };
        let normalized_input = crate::imp::xiaohe::normalize_syllable(code);
        let single_query = if flypy {
            code.len() == 2
        } else {
            crate::imp::xiaohe::canonical_syllable_set().contains(normalized_input.as_str())
        };
        let rank = |item: CandidateItem, origin: CandidateOrigin, base_match: Option<&DecodedCandidate>| {
            let group = candidate_group(&origin);
            let mut key = (group, false, false, false);
            let mut score = 0.0;
            if group == 1 {
                let mut chars = origin.canonical().chars();
                let single_char = chars.next().filter(|_| chars.next().is_none());
                let reading = if matches!(origin, CandidateOrigin::Overlay { is_clipboard: false, .. }) {
                    base_match
                        .map(|c| c.item.comment.as_str())
                        .or_else(|| {
                            let ch = single_char?;
                            corpus.char_readings(ch).map(|(reading, _)| reading).find(|reading| {
                                if flypy {
                                    quanpin::matches_xiaohe_reading(reading, code)
                                } else {
                                    *reading == normalized_input
                                }
                            })
                        })
                        .unwrap_or(&item.comment)
                } else {
                    &item.comment
                };
                let exact_xiaohe = (xiaohe_fuzzy || (flypy && single_query))
                    && quanpin::matches_xiaohe_reading(reading, code);
                let exact_single = single_query && single_char.is_some()
                    && if flypy {
                        exact_xiaohe
                    } else {
                        crate::imp::xiaohe::normalize_syllable(reading) == normalized_input
                    };
                let exact_overlay = matches!(origin, CandidateOrigin::Overlay { is_clipboard: false, .. })
                    && !matches!(item.comment.as_str(), "续写" | "简拼");
                let prefer_exact_xiaohe = xiaohe_fuzzy && (exact_xiaohe || exact_overlay);
                key = (group, !prefer_exact_xiaohe, !item.pinned, !exact_single);
                let lm_score = match &origin {
                    CandidateOrigin::PureRust { lm_score, .. } => *lm_score,
                    CandidateOrigin::Overlay { imported: false, .. } => {
                        base_match.map_or(best_lm, |c| c.lm_score)
                    }
                    _ => best_lm,
                };
                let features = origin.features();
                let weights = &retention.reranker.weights;
                let prior = features[6] * weights[6] + features[8] * weights[8];
                // Use non-personal priors only for close scores.
                score = lm_score + personal_boost(item.frecency_score - prior) + 0.05 * prior;
            }
            #[cfg(any(feature = "native-onnx", test))]
            let origin = {
                let mut origin = origin;
                if let CandidateOrigin::PureRust { rank_score, .. } = &mut origin {
                    *rank_score = score;
                }
                origin
            };
            (item, origin, key, score)
        };
        let mut merged: Vec<_> = overlays.into_iter().map(|(item, origin)| {
            let base_match = match &origin {
                CandidateOrigin::Overlay { is_clipboard: false, .. } => {
                    base_pairs.iter()
                        .find(|c| c.item.text == item.text && c.consumed_len == code.len())
                }
                _ => None,
            };
            rank(item, origin, base_match)
        }).collect();
        let base_pinned = base_pairs.iter().take_while(|c| c.item.pinned).count();
        let spatial_texts: HashSet<String> = base_pairs
            .iter()
            .filter(|c| c.spatial_edits > 0 && c.consumed_len == code.len())
            .map(|c| c.item.text.clone())
            .collect();
        let mut language_candidates = Vec::new();
        for (base_rank, candidate) in base_pairs.into_iter().enumerate() {
            if seen.insert(candidate.item.text.clone()) {
                let consumed_len = candidate.consumed_len;
                let (mut item, mut origin) = candidate.into_portable_pair();
                self.score_pair(&mut item, &mut origin, retention, now_ms, base_rank);
                // Semantic recall in a relevant context has its own evidence.
                if consumed_len == code.len() && origin.features()[5] <= 0.0 {
                    let word = origin.canonical();
                    item.frecency_score = retention.protect_dictionary_rank(
                        word, item.frecency_score, corpus.word_weight(word).unwrap_or(0), leading_weight,
                    );
                }
                if matches!(item.comment.as_str(), "英文" | "中英") && !item.pinned {
                    language_candidates.push((base_rank.saturating_sub(base_pinned), item, origin));
                } else {
                    merged.push(rank(item, origin, None));
                }
            }
        }

        merged.sort_by(|(_, _, a_key, a_score), (_, _, b_key, b_score)| {
            a_key.cmp(b_key).then_with(|| b_score.total_cmp(a_score))
        });
        let mut merged: Vec<_> = merged.into_iter()
            .map(|(item, origin, _, _)| (item, origin))
            .collect();
        let has_sentence = merged.iter().any(|(item, origin)| {
            matches!(origin, CandidateOrigin::PureRust { consumed_len, .. }
                if *consumed_len == code.len())
                && item.comment.split_whitespace().count() > 1
                && (self.schema == SchemaKind::DoublePinyinFlypy
                    || item
                        .comment
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .eq(code.chars().filter(|ch| ch.is_ascii_alphabetic())))
        });
        if has_sentence {
            if let Some(partial_at) = merged
                .iter()
                .position(|(_, origin)| candidate_group(origin) == 2)
            {
                let priority_count = merged
                    .iter()
                    .take_while(|(_, origin)| candidate_group(origin) == 0)
                    .count();
                let priority_words = merged[priority_count..partial_at]
                    .iter()
                    .take_while(|(item, origin)| {
                        item.pinned
                            || item.comment == "简拼"
                            || origin.has_personal_priority(retention, self.config.adaptive_learning)
                    })
                    .count();
                // With Xiaohe fuzzy matching, exact readings may precede the last pin.
                let priority_words = merged[priority_count..partial_at]
                    .iter()
                    .rposition(|(item, _)| item.pinned)
                    .map_or(priority_words, |index| priority_words.max(index + 1));
                // Keep personal matches and abbreviations ahead of selectable prefixes.
                let head_count = (priority_count + priority_words.max(2)).min(partial_at);
                let partials = merged.split_off(partial_at);
                let rest_full = merged.split_off(head_count);
                let mut prefixes = Vec::new();
                let mut singles = Vec::new();
                let mut remaining = Vec::new();
                let mut lengths = HashSet::new();
                for pair in partials {
                    let CandidateOrigin::PureRust {
                        consumed_len,
                        canonical,
                        ..
                    } = &pair.1
                    else {
                        unreachable!();
                    };
                    if canonical.chars().count() > 1 && lengths.insert(*consumed_len) {
                        prefixes.push(pair);
                    } else if canonical.chars().count() == 1
                        && pair.0.comment.split_whitespace().count() == 1
                        && singles.len() < 2
                    {
                        singles.push(pair);
                    } else {
                        remaining.push(pair);
                    }
                }
                merged.extend(prefixes);
                merged.extend(singles);
                merged.extend(rest_full);
                merged.extend(remaining);
            }
        }
        let mut correction_at = merged.iter()
            .rposition(|(item, _)| item.pinned && matches!(item.comment.as_str(), "英文" | "中英"))
            .map_or(0, |index| index + 1);
        if !language_candidates.is_empty() {
            let priority = merged.iter()
                .take_while(|(_, origin)| candidate_group(origin) == 0)
                .count();
            let short_conflict = code.len() <= 4
                && english::lookup_core_english(code, Some(retention)).is_none();
            if short_conflict {
                let abbreviations = merged.iter().any(|(item, _)| item.comment == "简拼");
                for (at, item, _) in &mut language_candidates {
                    if item.comment == "英文" {
                        if abbreviations {
                            *at = (*at).max(3);
                        }
                        if *at >= 2 {
                            // Keep the leading Chinese readings ahead of typo guesses.
                            correction_at = correction_at.max(priority + 2);
                        }
                    }
                }
            }
        }
        self.insert_corrections(&mut merged, imported, retention, now_ms, correction_at, &spatial_texts);
        let priority = merged.iter()
            .rposition(|(item, origin)| {
                candidate_group(origin) == 0 || (candidate_group(origin) == 1 && item.pinned)
            })
            .map_or(0, |index| index + 1);
        let mut next_at = priority;
        for (at, item, origin) in language_candidates {
            // Apply the decoder's language positions after shortcuts and pinned words.
            let at = (priority + at).max(next_at).min(merged.len());
            merged.insert(at, (item, origin));
            next_at = at + 1;
        }
        if let Some((word, score)) = contextual_prediction {
            self.promote_prefix_prediction(&mut merged, &word, score, retention, now_ms);
        }
        let guarded = |origin: &CandidateOrigin| matches!(
            origin, CandidateOrigin::PureRust { spatial_guarded: true, .. }
        );
        if merged.iter().any(|(_, origin)| guarded(origin)) {
            let exact = merged.iter().position(|(item, origin)| {
                !guarded(origin) && candidate_group(origin) == 1 && match origin {
                    CandidateOrigin::Overlay { .. } => !matches!(item.comment.as_str(), "续写" | "简拼"),
                    _ => {
                        let syllables: Vec<_> = item.comment.split_whitespace().map(str::to_owned).collect();
                        quanpin::matches_exact_reading(&syllables, code, flypy)
                    }
                }
            });
            if let Some(exact) = exact {
                // Personal and semantic boosts can change the order after the beam.
                merged[..=exact].sort_by_key(|(_, origin)| guarded(origin));
            }
        }
        if let Some(filter) = filter.filter(|filter| !filter.is_empty()) {
            self.filter_candidate_pairs(&mut merged, code, filter, imported);
        }
        #[cfg(feature = "native-onnx")]
        let pending = {
            let cache_generation = self
                .neural_rerank_cache
                .bind(self.model_paths.reranker.as_deref());
            if self.model_paths.reranker.is_some()
                && filter.is_none()
                && !self.is_passthrough_active()
            {
                self.neural_rerank_pool(&merged, code, retention, now_ms)
                    .and_then(|pool| {
                        let missing = pool
                            .apply_cached(&self.neural_rerank_cache, |a, b| merged.swap(a, b))
                            .err()?;
                        Some((cache_generation, pool, missing))
                    })
            } else {
                None
            }
        };
        let action = self.show_portable_page(merged);
        #[cfg(feature = "native-onnx")]
        if let Some((cache_generation, pool, missing)) = pending {
            self.pending_neural_rerank = Some(neural_rerank::Pending {
                model_paths: self.model_paths.clone(),
                cache_generation,
                pool,
                missing,
            });
        }
        Ok(action)
    }

    #[cfg(any(feature = "native-onnx", test))]
    fn neural_rerank_pool(
        &self,
        merged: &[(CandidateItem, CandidateOrigin)],
        code: &str,
        retention: &RetentionEngine,
        now_ms: u64,
    ) -> Option<neural_rerank::Pool> {
        let left_context = if self.previous_context(now_ms).is_some() {
            self.context_window.as_slice()
        } else {
            &[]
        };
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        let candidates = merged.iter().map(|(item, origin)| {
            let CandidateOrigin::PureRust {
                consumed_len,
                spatial_guarded,
                canonical,
                rank_score,
                ..
            } = origin else {
                return None;
            };
            if *consumed_len != code.len()
                || *spatial_guarded
                || item.pinned
                || item.is_clipboard_shortcut
                || item.function_action.is_some()
                || item.quick_paste_id.is_some()
                || item.shortcut_code.is_some()
                || matches!(item.comment.as_str(), "英文" | "中英" | "纠错" | "续写" | "简拼")
                || origin.has_personal_priority(retention, self.config.adaptive_learning)
            {
                return None;
            }
            let syllables: Vec<_> = item.comment.split_whitespace().map(str::to_owned).collect();
            quanpin::matches_exact_reading(&syllables, code, flypy).then_some(
                neural_rerank::Candidate {
                    text: canonical,
                    base_score: *rank_score,
                },
            )
        });
        neural_rerank::Pool::new(candidates, left_context)
    }

    fn filter_candidate_pairs(
        &self,
        pairs: &mut Vec<(CandidateItem, CandidateOrigin)>,
        code: &str,
        filter: &str,
        imported: &ImportedLexiconIndex,
    ) {
        let tone = filter.as_bytes().first().copied().filter(|b| (b'1'..=b'5').contains(b));
        let aux = &filter[usize::from(tone.is_some())..];
        let ids = (!aux.is_empty()).then(|| IdsIndex::load_cached(self.resolved_assets_dir.as_deref()));
        let annotator = tone.map(|_| {
            crate::imp::polyphone::PolyphoneAnnotator::new(std::sync::Arc::clone(imported.corpus()))
        });
        let first_part = code.split(['\'', ' ']).find(|part| !part.is_empty()).unwrap_or("");
        let normalized = xiaohe::normalize_syllable(first_part);
        let single_quanpin = xiaohe::canonical_syllable_set().contains(normalized.as_str());
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        let matches_first_syllable = |syllable: &str, is_phrase: bool| {
            let spelled = if flypy {
                first_part.get(..2)
                    .and_then(|first| xiaohe::xiaohe_reverse_map().get(first))
                    .is_some_and(|readings| readings.contains(&syllable))
            } else if single_quanpin && !is_phrase {
                normalized == syllable
            } else {
                first_part.get(..syllable.len())
                    .is_some_and(|first| xiaohe::normalize_syllable(first) == syllable)
            };
            spelled || (is_phrase && [2, 1].into_iter().any(|len| {
                first_part.get(..len).is_some_and(|initial| {
                    quanpin::is_initial_token(initial) && syllable.starts_with(initial)
                })
            }))
        };
        pairs.retain(|(item, origin)| {
            if matches!(origin, CandidateOrigin::Overlay { is_clipboard: true, .. }) {
                return false;
            }
            let word = origin.canonical();
            let mut chars = word.chars();
            let Some(first) = chars.next() else { return false };
            if !crate::ret::is_cjk(first) || !chars.clone().all(crate::ret::is_cjk) {
                return false;
            }
            let is_phrase = !chars.as_str().is_empty();
            if let Some(ids) = &ids {
                let mut displayed = item.text.chars();
                if !ids.matches_aux_code(first, aux)
                    && !chars.next_back().is_some_and(|last| ids.matches_aux_code(last, aux))
                    && !displayed.next().is_some_and(|first| ids.matches_aux_code(first, aux))
                    && !displayed.next_back().is_some_and(|last| ids.matches_aux_code(last, aux))
                {
                    return false;
                }
            }
            tone.is_none_or(|target| {
                let reading = candidate_reading(item, word, imported);
                let Some(raw_syllable) = reading.split_whitespace().next() else { return false };
                let syllable = xiaohe::normalize_syllable(raw_syllable);
                if !matches_first_syllable(&syllable, is_phrase) {
                    return false;
                }
                if is_phrase {
                    if let Some(explicit_tone) = toned_syllable_tone(raw_syllable) {
                        return explicit_tone == target;
                    }
                    if let Some(ann_toned) = annotator
                        .as_ref()
                        .and_then(|a| a.annotate_text(word).toned.split_whitespace().next().map(str::to_owned))
                    {
                        if xiaohe::normalize_syllable(&ann_toned) == syllable {
                            return toned_syllable_tone(&ann_toned).unwrap_or(b'5') == target;
                        }
                    }
                }
                first.to_pinyin_multi().is_some_and(|readings| readings.into_iter().any(|p| {
                    xiaohe::normalize_syllable(p.plain()) == syllable
                        && p.with_tone_num_end().as_bytes().last().copied()
                            .filter(|b| (b'1'..=b'4').contains(b)).unwrap_or(b'5') == target
                }))
            })
        });
    }

    fn component_code(&self) -> Option<&str> {
        let prefix = match self.schema {
            SchemaKind::RimeIce => "u",
            SchemaKind::DoublePinyinFlypy => "u'",
        };
        self.raw_input.strip_prefix(prefix)
            .filter(|code| code.len() >= 2 && code.bytes().all(|byte| byte.is_ascii_lowercase()))
    }

    fn show_portable_page(&mut self, merged: Vec<(CandidateItem, CandidateOrigin)>) -> InputAction {
        let page_size = self.candidate_page_size();
        let total_pages = merged.len().div_ceil(page_size).max(1);
        if self.pure_rust.page_index >= total_pages {
            self.pure_rust.page_index = 0;
        }
        let start = self.pure_rust.page_index * page_size;
        let end = (start + page_size).min(merged.len());
        let page_pairs: Vec<(CandidateItem, CandidateOrigin)> =
            merged.into_iter().skip(start).take(end - start).collect();

        let prefix = self.confirmed_non_ascii_prefix();
        self.cursor_utf8 = prefix.len() + self.raw_cursor;
        self.composing = prefix + &self.raw_input;
        self.set_current_candidates(page_pairs);
        self.current_page = PageInfo {
            page_index: self.pure_rust.page_index,
            page_size,
            has_prev: self.pure_rust.page_index > 0,
            has_next: self.pure_rust.page_index + 1 < total_pages,
        };
        self.empty_action(None)
    }

    fn recover_transposition(
        &self,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> Vec<(CandidateItem, CandidateOrigin)> {
        let level = self.pure_rust.typo_correction_level;
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        let input = self.base_code();
        if !quanpin::can_correct(input, flypy, level) {
            return Vec::new();
        }
        let corpus = imported.corpus();
        let exact = quanpin::exact_word_match(input, flypy, corpus, Some(imported), Some(retention));
        if !exact.allows(level) {
            return Vec::new();
        }
        let mut exact_words = HashSet::new();
        if exact.weight > 0 {
            quanpin::visit_exact_words(input, flypy, corpus, Some(imported), Some(retention),
                |word, _, _, _| { exact_words.insert(word.to_owned()); });
        }
        let mut ranked = Vec::new();
        let odd_flypy = flypy && input.bytes().filter(u8::is_ascii_lowercase).count() % 2 != 0;
        for (quality, code) in quanpin::correction_codes(input, flypy, level) {
            for (item, canonical, weight) in self.pure_rust.query_correction_candidates(
                &code, self.schema, imported, retention,
            ) {
                if exact_words.contains(&canonical) || !exact.supports(level, quality, weight) {
                    continue;
                }
                let whole_phrase = odd_flypy && canonical.chars().count() > 1;
                // For a missing or extra key, favor edits that preserve the first key.
                let leading_edit = odd_flypy && code.as_bytes().first() != input.as_bytes().first();
                let score = item.frecency_score + (weight as f64).ln_1p()
                    - f64::from(quality) - if leading_edit { 0.30 } else { 0.0 };
                ranked.push((item, canonical, score, whole_phrase, quality));
            }
        }
        ranked.sort_by(|a, b| {
            b.0.pinned.cmp(&a.0.pinned)
                .then_with(|| b.3.cmp(&a.3))
                .then_with(|| if exact.weight == 0 { a.4.cmp(&b.4) } else { std::cmp::Ordering::Equal })
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| a.1.cmp(&b.1))
        });
        let mut seen = HashSet::new();
        ranked.into_iter().filter_map(|(mut item, canonical, score, _, _)| {
            let commit_text = self.convert_opencc(&canonical);
            if !seen.insert(commit_text.clone()) {
                return None;
            }
            item.text = commit_text.clone();
            item.comment = "纠错".into();
            Some((
                item,
                CandidateOrigin::PureRust {
                    consumed_len: input.len(),
                    lm_score: score,
                    #[cfg(any(feature = "native-onnx", test))]
                    rank_score: 0.0,
                    spatial_guarded: false,
                    canonical,
                    commit_text,
                    features: [0.0; 9],
                },
            ))
        }).take(2).collect()
    }

    fn insert_corrections(
        &self,
        merged: &mut Vec<(CandidateItem, CandidateOrigin)>,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
        now_ms: u64,
        earliest_at: usize,
        spatial_texts: &HashSet<String>,
    ) {
        let mut recovered = self.recover_transposition(imported, retention);
        recovered.retain(|(correction, _)| {
            !merged.iter().any(|(item, origin)| {
                item.text == correction.text
                    && (matches!(origin, CandidateOrigin::Overlay { is_clipboard: true, .. })
                        || spatial_texts.contains(&item.text))
            })
        });
        if recovered.is_empty() {
            return;
        }
        merged.retain(|(item, origin)| {
            matches!(origin, CandidateOrigin::Overlay { is_clipboard: true, .. })
                || spatial_texts.contains(&item.text)
                || !recovered.iter().any(|(correction, _)| correction.text == item.text)
        });
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        let mut exact_words = HashSet::new();
        quanpin::visit_exact_words(self.base_code(), flypy, imported.corpus(), Some(imported), Some(retention),
            |word, _, _, _| { exact_words.insert(word.to_owned()); });
        let first_word = merged.iter().take_while(|(item, origin)| {
            item.pinned
                || origin.has_personal_priority(retention, self.config.adaptive_learning)
                || item.comment == "简拼"
                || matches!(origin, CandidateOrigin::Overlay { is_clipboard: true, .. })
        }).count();
        // Keep a valid exact word or top 2D-touch-decoded candidate ahead of blind edit corrections.
        let at = merged[first_word..].iter()
            .position(|(item, origin)| {
                exact_words.contains(origin.canonical()) || spatial_texts.contains(&item.text)
            })
            .map_or(first_word, |index| first_word + index + 1)
            .max(earliest_at)
            .min(merged.len());
        for (rank, (item, origin)) in recovered.iter_mut().enumerate() {
            self.score_pair(item, origin, retention, now_ms, at + rank);
        }
        merged.splice(at..at, recovered);
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

        let quick_paste = (self.config.platform == "android" && self.raw_input.is_empty())
            .then(|| clipboard.visible_quick_paste())
            .flatten();
        if let Some(clip) = quick_paste {
            overlay.push((
                CandidateItem {
                    index: 0,
                    text: clip.content.clone(),
                    comment: "快捷粘贴".into(),
                    pinned: false,
                    is_clipboard_shortcut: false,
                    quick_paste_id: Some(clip.id.clone()),
                    function_action: None,
                    shortcut_code: None,
                    frecency_score: 0.0,
                },
                CandidateOrigin::Overlay {
                    features: [0.0; 9],
                    canonical: clip.content.clone(),
                    commit_text: clip.content.clone(),
                    is_clipboard: true,
                    imported: false,
                },
            ));
        }

        let code = self.base_code();
        let shortcut_code = self.candidate_filter().is_none_or(str::is_empty).then_some(code);
        for binding in &self.config.shortcuts {
            if !binding.text.is_empty() && shortcut_code == Some(binding.text.as_str())
                && self.config.shortcut_enabled(binding.action) {
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
                        imported: false,
                    },
                ));
            }
        }

        if let Some(clip) = shortcut_code.and_then(|code| clipboard.find_shortcut(code)) {
            if seen.insert(clip.content.clone()) {
                let item = CandidateItem {
                    index: 0,
                    text: clip.content.clone(),
                    comment: format!("📌 {code}"),
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
                    imported: false,
                };
                overlay.push((item, origin));
            }
        }

        if let Some((_, macro_items)) = shortcut_code.and_then(expand_time_macro) {
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
                        imported: false,
                    };
                    overlay.push((item, origin));
                }
            }
        }

        let compact = code.replace(['\'', ' '], "");
        let flypy = self.schema == SchemaKind::DoublePinyinFlypy;
        let allow_abbreviation = if flypy {
            !matches!(compact.len(), 2 | 4)
                || quanpin::exact_word_match(
                    code, true, imported.corpus(), Some(imported), Some(retention),
                ).weight < quanpin::HIGH_DICTIONARY_WEIGHT
        } else {
            !crate::imp::xiaohe::canonical_syllable_set().contains(compact.as_str())
        };
        let now_ms = RetentionEngine::now_epoch_ms();
        if self.config.adaptive_learning {
            let full_code = if flypy { &compact } else { code };
            let phrases = retention.query_user_phrases(full_code, flypy);
            let mut learned: Vec<_> = phrases
                .into_iter()
                .map(|p| {
                    let decay = if p.updated_ms == 0 {
                        1.0
                    } else {
                        let age_sec = now_ms.saturating_sub(p.updated_ms) as f64 / 1000.0;
                        0.5_f64.powf(age_sec / retention.half_life_sec.max(1.0))
                    };
                    (
                        p.word.as_str(),
                        "习惯",
                        self.contextual_score(retention, &p.word, now_ms) + decay,
                    )
                })
                .collect();
            if allow_abbreviation {
                for phrase in retention.query_acronym_phrases(&compact) {
                    learned.push((
                        phrase.word.as_str(),
                        "简拼",
                        self.contextual_score(retention, &phrase.word, now_ms) + 1.0,
                    ));
                }
            }
            let continuations = self
                .previous_context(now_ms)
                .filter(|_| !self.pure_rust.confirming_initials)
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
            learned.sort_by(|a, b| b.2.total_cmp(&a.2));
            for (canonical, comment, score) in learned {
                if self.config.syllable_split && self.schema == SchemaKind::RimeIce {
                    let reading = crate::imp::polyphone::PolyphoneAnnotator::new(
                        std::sync::Arc::clone(imported.corpus()),
                    ).annotate_text(canonical);
                    if reading.untoned.split_whitespace().any(quanpin::can_zero_initial_split) {
                        continue;
                    }
                }
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
                            imported: false,
                        },
                    ));
                }
            }
        }

        if (include_embedded_sentence || flypy) && allow_abbreviation && (2..=4).contains(&compact.len()) {
            for (word, score) in quanpin::ranked_abbreviations(
                &compact, imported.corpus(), self.pure_rust.get_gram(), Some(retention),
            ) {
                let text = self.convert_opencc(word);
                if seen.insert(text.clone()) {
                    overlay.push((
                        CandidateItem {
                            index: 0,
                            text: text.clone(),
                            comment: "简拼".into(),
                            pinned: retention.pinned_words.contains(word),
                            frecency_score: self.contextual_score(retention, word, now_ms),
                            is_clipboard_shortcut: false,
                            shortcut_code: None,
                            quick_paste_id: None,
                            function_action: None,
                        },
                        CandidateOrigin::PureRust {
                            consumed_len: code.len(),
                            lm_score: score,
                            #[cfg(any(feature = "native-onnx", test))]
                            rank_score: 0.0,
                            spatial_guarded: false,
                            features: [0.0; 9],
                            canonical: word.to_owned(),
                            commit_text: text,
                        },
                    ));
                }
            }
        }

        if self.schema == SchemaKind::DoublePinyinFlypy {
            for hint in &self.lookup_hints {
                if hint.xiaohe_code != code || retention.erased_words.contains(&hint.ch) {
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
                            imported: false,
                        },
                    ));
                }
            }
        }

        if !imported.is_empty() {
            let compact: String = code.chars()
                .filter(|c| c.is_ascii_alphabetic())
                .collect();
            let imp_matches = match self.schema {
                SchemaKind::RimeIce => imported.query_quanpin(&compact),
                SchemaKind::DoublePinyinFlypy => imported.query_xiaohe(&compact),
            };
            for imp in imp_matches {
                if self.config.syllable_split && self.schema == SchemaKind::RimeIce
                    && imp.pinyin.split_whitespace().any(quanpin::can_zero_initial_split)
                { continue; }
                if retention.erased_words.contains(&imp.text) {
                    continue;
                }
                let converted = self.convert_opencc(&imp.text);
                let import_score = 2.0 + f64::from(imp.weight).max(1.0).ln();
                if seen.insert(converted.clone()) {
                    let item = CandidateItem {
                        index: 0,
                        text: converted.clone(),
                        comment: if self.config.syllable_split && self.schema == SchemaKind::RimeIce {
                            imp.pinyin.clone()
                        } else { imp.toned_pinyin.clone() },
                        pinned: retention.pinned_words.contains(&imp.text),
                        is_clipboard_shortcut: false,
                        quick_paste_id: None,
                        function_action: None,
                        shortcut_code: None,
                        frecency_score: import_score
                            + self.contextual_score(retention, &imp.text, now_ms),
                    };
                    let origin = CandidateOrigin::Overlay {
                        features: [0.0; 9],
                        canonical: imp.text,
                        commit_text: converted,
                        is_clipboard: false,
                        imported: true,
                    };
                    overlay.push((item, origin));
                } else {
                    for (item, origin) in &mut overlay {
                        if item.text != converted {
                            continue;
                        }
                        if let CandidateOrigin::Overlay {
                            is_clipboard: false,
                            imported: added @ false,
                            ..
                        } = origin
                        {
                            item.frecency_score += import_score;
                            *added = true;
                            break;
                        }
                    }
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
                    code,
                    self.schema,
                    OpenCcMode::Simplified,
                    Some(imported),
                    Some(retention),
                    self.prefix_prediction_context(RetentionEngine::now_epoch_ms())
                        .as_deref(),
                    self.active_app_id.as_deref(),
                    self.config.adaptive_learning,
                );
                for DecodedCandidate {
                    item, canonical, ..
                } in decoded
                    .into_iter()
                    .filter(|c| {
                        c.consumed_len == code.len()
                            && imported.contains_hotword(&c.canonical)
                            && !retention.erased_words.contains(&c.canonical)
                    })
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
                            frecency_score: self.contextual_score(retention, &canonical, now_ms)
                                + 2.0,
                        };
                        let origin = CandidateOrigin::Overlay {
                            features: [0.0; 9],
                            canonical,
                            commit_text: converted,
                            is_clipboard: false,
                            imported: true,
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

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    fn apply_rime_selection(
        &mut self,
        snapshot: rime_ffi::RimeSnapshot,
        canonical: &str,
        index: usize,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Option<String> {
        let before = self.raw_input.clone();
        if let Some(committed) = snapshot.commit_text {
            let code = format!("{}{before}", self.pending_phrase_code.replace(' ', ""));
            let feedback = (self.pending_phrase_selections == 0 && canonical == committed)
                .then(|| self.selection_feedback(index))
                .flatten();
            self.record_candidate_commit(
                &committed, &code, feedback, imported, retention, RetentionEngine::now_epoch_ms(),
            );
            self.track_phrase_selection(canonical, &before, true, imported, retention);
            self.clear_composition_state();
            return Some(self.convert_opencc(&committed));
        }
        if !snapshot.preedit.is_empty() || !snapshot.candidates.is_empty() {
            let confirmed = self.pure_rust.confirmed_prefix.len();
            self.apply_rime_snapshot(snapshot, false, false, clipboard, imported, retention);
            if self.pure_rust.confirmed_prefix.len() > confirmed {
                self.settle_undo_regret(Some(canonical), retention);
                let compact = |code: &str| code.replace(['\'', ' '], "");
                let before = compact(&before);
                let tail = compact(&self.raw_input);
                if let Some(consumed) = before.strip_suffix(&tail).filter(|s| !s.is_empty()) {
                    self.track_phrase_selection(canonical, consumed, false, imported, retention);
                }
            }
        } else {
            self.clear_composition_state();
        }
        None
    }

    pub(crate) fn select_candidate_action(
        &mut self,
        index: usize,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<InputAction, VitralError> {
        let clipboard_commit = self.is_clipboard_shortcut_candidate(index);
        let allow_micro_space = !self.is_passthrough_active() && !clipboard_commit;
        let eligible = allow_micro_space && self.component_code().is_none() && !self.raw_input.is_empty()
            && self.raw_input.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'\'')
            && self.current_candidates.get(index).is_some_and(|item| {
                !item.text.is_empty() && item.text.chars().all(crate::ret::is_cjk)
                    && item.function_action.is_none() && item.quick_paste_id.is_none()
            });
        let previous_char = self.last_committed_char;
        let touch_hints = if eligible && self.raw_touch_hints.iter().any(|hint| !hint.is_empty()) {
            self.raw_touch_hints[..self.candidate_code_at(index).len()].to_vec()
        } else {
            Vec::new()
        };
        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        let native = self.rime_input_start.is_some();
        let committed = self.select_candidate_on_backend(index, clipboard, imported, retention)?;
        let mut action = self.commit_action(committed, allow_micro_space);
        action.clipboard_commit = clipboard_commit;
        if eligible && self.last_committed_code.as_ref().is_some_and(|code| !code.is_empty()) {
            self.undo_commit = action.commit_text.clone().map(|text| UndoCommit {
                text,
                previous_char,
                touch_hints,
                #[cfg(all(target_os = "linux", feature = "native-rime"))]
                native,
            });
        }
        Ok(action)
    }

    pub(crate) fn select_candidate_on_backend(
        &mut self,
        index: usize,
        clipboard: &ClipboardManager,
        imported: &ImportedLexiconIndex,
        retention: &mut RetentionEngine,
    ) -> Result<Option<String>, VitralError> {
        let origin = self.candidate_origins.get(index).cloned().ok_or_else(|| {
            VitralError::NotFound(format!("candidate index {index} out of range"))
        })?;

        match origin {
            CandidateOrigin::Components { canonical, commit_text, .. } => {
                self.settle_undo_regret(Some(&canonical), retention);
                self.clear_composition_state();
                self.reset_commit_context();
                Ok(Some(commit_text))
            }
            CandidateOrigin::Overlay {
                commit_text,
                is_clipboard,
                canonical,
                ..
            } => {
                let prefix = self.confirmed_non_ascii_prefix();
                let final_commit = if !is_clipboard && !prefix.is_empty() {
                    format!("{}{}", self.convert_opencc(&prefix), commit_text)
                } else {
                    commit_text
                };
                if !is_clipboard {
                    self.learn_current_candidate(&canonical, index, imported, retention);
                    let code = self.base_code().to_owned();
                    self.track_phrase_selection(&canonical, &code, true, imported, retention);
                } else {
                    if self.is_clipboard_shortcut_candidate(index) {
                        self.pending_undo_regret = None;
                    } else {
                        self.settle_undo_regret(Some(&canonical), retention);
                    }
                    self.reset_commit_context();
                }
                self.clear_composition_state();
                Ok(Some(final_commit))
            }
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            CandidateOrigin::Rime {
                rime_page_idx,
                canonical,
                ..
            } => {
                if self.is_active_privacy_app() {
                    self.clear_composition_state();
                    return Ok(None);
                }
                let (Some(sid), Some(assets_dir)) =
                    (self.rime_session_id, &self.resolved_assets_dir)
                else {
                    return Ok(None);
                };
                let snapshot = rime_ffi::with_runtime(
                    assets_dir,
                    &self.config.cache_dir,
                    &self.config.data_dir,
                    |rt| rt.select_candidate_on_current_page(sid, rime_page_idx),
                )?;
                Ok(self.apply_rime_selection(snapshot, &canonical, index, clipboard, imported, retention))
            }
            CandidateOrigin::PureRust {
                commit_text,
                consumed_len,
                canonical,
                ..
            } => {
                self.learn_current_candidate(&canonical, index, imported, retention);
                let prefix = self.confirmed_non_ascii_prefix();
                let final_commit = if !prefix.is_empty() {
                    format!("{}{}", self.convert_opencc(&prefix), commit_text)
                } else {
                    commit_text
                };
                let base_code = self.base_code();
                let base_len = base_code.len();
                let remaining = base_code[consumed_len..].trim_start_matches(['\'', ' ']);
                let remaining_start = base_len - remaining.len();
                let remaining = remaining.to_owned();
                let code = self.raw_input[..consumed_len].to_string();
                let confirming_initials = quanpin::is_initial_token(&code)
                    && (self.pure_rust.confirming_initials
                        || quanpin::is_initial_sequence(base_code));
                self.track_phrase_selection(
                    &canonical,
                    &code,
                    remaining.is_empty(),
                    imported,
                    retention,
                );
                let confirmed = format!("{}{}", self.pure_rust.confirmed_prefix, canonical);
                let pending = (
                    std::mem::take(&mut self.pending_phrase_text),
                    std::mem::take(&mut self.pending_phrase_code),
                    self.pending_phrase_selections,
                );
                let mut remaining_hints = self.raw_touch_hints.split_off(remaining_start);
                remaining_hints.truncate(base_len - remaining_start);
                self.clear_composition_state();
                if !remaining.is_empty() {
                    self.pure_rust.confirmed_prefix = confirmed;
                    self.pure_rust.confirming_initials = confirming_initials;
                    (
                        self.pending_phrase_text,
                        self.pending_phrase_code,
                        self.pending_phrase_selections,
                    ) = pending;
                    self.raw_input = remaining;
                    self.raw_touch_hints = remaining_hints;
                    self.raw_cursor = self.raw_input.len();
                    self.process_key_pure_rust(0, clipboard, imported, retention)?;
                }
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
        self.invalidate_neural_rerank();
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
        self.invalidate_neural_rerank();
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
        self.commit_action(commit_text, true)
    }

    pub(crate) fn commit_action(&mut self, mut commit_text: Option<String>, allow_micro_space: bool) -> InputAction {
        if allow_micro_space && self.config.auto_micro_space && self.lang_mode == LangMode::Zh {
            if let (Some(previous), Some(text)) = (self.last_committed_char, commit_text.as_mut()) {
                let starts_cjk = text.chars().next().is_some_and(crate::ret::is_cjk);
                let english_word = !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphabetic());
                if (previous.is_ascii_alphanumeric() && starts_cjk)
                    || (crate::ret::is_cjk(previous) && english_word)
                {
                    text.insert(0, ' ');
                }
            }
        }
        if let Some(ch) = commit_text.as_deref().and_then(|text| text.chars().next_back()) {
            self.undo_commit = None;
            self.last_committed_char = (!ch.is_whitespace()).then_some(ch);
            if !crate::ret::is_cjk(ch) {
                self.reset_commit_context();
            }
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
                cursor_utf8: clamp_cursor(&self.composing, self.cursor_utf8),
                retroactive_slots: Vec::new(),
            },
            commit_text,
            clipboard_commit: false,
            undo_commit_chars: 0,
            undo_commit_text: None,
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

    fn touch_hint(primary: u8, alternative: u8) -> TouchKeyHint {
        TouchKeyHint::new(primary, [Some(SpatialAlt { ch: alternative, penalty: 1.0 }), None])
    }

    #[test]
    fn precision_word_edge_commits_and_learns_the_character_reading() {
        for (schema, word_code, tail_code) in [
            (SchemaKind::RimeIce, "yisheng", "sheng"),
            (SchemaKind::DoublePinyinFlypy, "yiug", "ug"),
        ] {
            let (mut e, clip, imp, mut r) = fixture();
            e.set_schema(schema);
            for (key, ch, code) in [('[', "医", "yi"), (']', "生", tail_code)] {
                type_code(&mut e, word_code, &clip, &imp, &mut r);
                assert_eq!(e.canonical_at(0), Some("医生"));
                let action = e.process_key(key as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
                assert_eq!(action.commit_text.as_deref(), Some(ch));
                assert!(action.preedit.text.is_empty());
                assert_eq!(e.last_committed_char, ch.chars().next());
                assert_eq!(e.context_window.last().copied(), ch.chars().next());
                assert!(r.code_preferences[code].contains_key(ch));
                assert!(!r.code_preferences.contains_key(word_code));
            }
            let paired = e.process_key('[' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(paired.commit_text.as_deref(), Some("[]"));
            e.config.disable_paired_punct_in_ide = true;
            e.config.auto_english_for_dev_apps = false;
            e.on_app_focus("idea").unwrap();
            let unpaired = e.process_key('[' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(unpaired.commit_text.as_deref(), Some("["));
            offer(&mut e, "你", "ni");
            let single = e.process_key('[' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(single.commit_text.as_deref(), Some("你["));
            e.config.adaptive_learning = false;
            offer(&mut e, "医生", word_code);
            let seq = r.crdt_seq;
            let edge = e.process_key(']' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(edge.commit_text.as_deref(), Some("生"));
            assert_eq!(e.context_window, ['生']);
            assert_eq!(r.crdt_seq, seq);
        }
    }

    #[test]
    fn precision_word_edge_learns_the_taiwan_standard_substitution() {
        for adaptive in [true, false] {
            let (mut e, clip, imp, mut r) = fixture();
            e.config.adaptive_learning = adaptive;
            e.set_opencc_mode(OpenCcMode::TaiwanStandard);
            type_code(&mut e, "ruanjian", &clip, &imp, &mut r);
            assert_eq!(e.canonical_at(0), Some("软件"));
            assert_eq!(e.current_candidates[0].text, "軟體");

            let action = e.process_key(']' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(action.commit_text.as_deref(), Some("體"));
            assert_eq!(e.context_window, ['體']);
            assert_eq!(e.last_committed_word.as_deref(), Some("體"));
            assert_eq!(r.code_preferences.get("ti").is_some_and(|words| words.contains_key("體")), adaptive);
            assert!(!r.code_preferences.contains_key("jian"));
            assert!(!r.frecency.contains_key("件"));
        }
    }

    #[test]
    fn precision_tone_filter_keeps_homophones_and_restores_number_selection() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let (mut e, clip, imp, mut r) = fixture();
            e.set_schema(schema);
            for (tone, example) in [('1', "妈"), ('3', "马"), ('4', "骂"), ('5', "吗")] {
                type_code(&mut e, &format!("ma`{tone}"), &clip, &imp, &mut r);
                assert_eq!(e.raw_input, format!("ma`{tone}"));
                assert!(e.current_candidates.iter().any(|item| item.text == example));
                let first = e.current_candidates[0].text.clone();
                assert!(first.chars().next().unwrap().to_pinyin_multi().unwrap().into_iter().any(|p| {
                    p.plain() == "ma" && if tone == '5' {
                        !p.with_tone_num_end().ends_with(|ch: char| ch.is_ascii_digit())
                    } else {
                        p.with_tone_num_end().ends_with(tone)
                    }
                }));
                let key = if tone == '1' { '1' } else { ' ' };
                let action = e.process_key(key as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
                assert_eq!(action.commit_text.as_deref(), Some(first.as_str()));
                assert!(r.code_preferences["ma"].contains_key(&first));
            }
            assert_eq!(r.code_preferences.len(), 1);
            type_code(&mut e, "de`4", &clip, &imp, &mut r);
            assert!(!e.current_candidates.iter().any(|item| matches!(item.text.as_str(), "的" | "地")));
        }
    }

    #[test]
    fn precision_tone_filter_keeps_decoded_segmentation_and_abbreviation() {
        for (code, word, tone) in [("xian", "西安", '1'), ("nh", "你好", '3')] {
            for (digit, matches) in [(tone, true), ('2', false)] {
                let (mut e, clip, imp, mut r) = fixture();
                type_code(&mut e, &format!("{code}`{digit}"), &clip, &imp, &mut r);
                loop {
                    if e.current_candidates.iter().any(|item| item.text == word) {
                        assert!(matches, "{code}`{digit} should exclude {word}");
                        break;
                    }
                    if !e.current_page.has_next {
                        assert!(!matches, "{code}`{digit} should retain {word}");
                        break;
                    }
                    e.process_key(0xff56, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
                }
            }
        }
    }

    #[test]
    fn precision_aux_filter_uses_bundled_radicals_and_backspace_restores_order() {
        let assets = default_assets_dir_candidates().unwrap();
        let (mut e, clip, imp, mut r) = fixture();
        for (schema, base, suffix, expected) in [
            (SchemaKind::RimeIce, "de", "b", "的"),
            (SchemaKind::RimeIce, "de", "bai", "的"),
            (SchemaKind::RimeIce, "de", "t", "地"),
            (SchemaKind::RimeIce, "de", "tu", "地"),
            (SchemaKind::RimeIce, "de", "5b", "的"),
            (SchemaKind::RimeIce, "yisheng", "niuheng", "医生"),
            (SchemaKind::RimeIce, "shi", "hengjinniao", "䴓"),
            (SchemaKind::DoublePinyinFlypy, "mk", "ry", "明"),
            (SchemaKind::DoublePinyinFlypy, "mk", "riyue", "明"),
        ] {
            e.resolved_assets_dir = None;
            e.set_schema(schema);
            type_code(&mut e, base, &clip, &imp, &mut r);
            let unfiltered = e.current_candidates.clone();
            let page = e.current_page.clone();
            e.resolved_assets_dir = Some(assets.clone());
            type_code(&mut e, &format!("`{suffix}"), &clip, &imp, &mut r);
            assert_eq!(e.raw_input, format!("{base}`{suffix}"));
            assert_eq!(e.canonical_at(0), Some(expected), "{base}`{suffix}");
            for _ in 0..=suffix.len() {
                e.process_key(0x08, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            }
            assert_eq!(e.raw_input, base);
            assert_eq!(e.current_page.has_next, page.has_next);
            assert_eq!(
                e.current_candidates.iter().map(|item| &item.text).collect::<Vec<_>>(),
                unfiltered.iter().map(|item| &item.text).collect::<Vec<_>>(),
            );
            e.clear_composition_state();
        }
        e.resolved_assets_dir = None;
        type_code(&mut e, "mk`1abcdefghijkl", &clip, &imp, &mut r);
        assert_eq!(e.raw_input, "mk`1abcdefghijk");
        let raw = e.process_key(0x0d, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
        assert_eq!(raw.commit_text.as_deref(), Some("mk`1abcdefghijk"));
    }

    #[test]
    fn precision_aux_filter_matches_the_displayed_traditional_character() {
        for suffix in ["deng", "1deng"] {
            let (mut e, clip, imp, mut r) = fixture();
            e.set_opencc_mode(OpenCcMode::Traditional);
            type_code(&mut e, "fa", &clip, &imp, &mut r);
            e.resolved_assets_dir = default_assets_dir_candidates();
            type_code(&mut e, &format!("`{suffix}"), &clip, &imp, &mut r);
            assert!(e.current_candidates.iter().any(|item| item.text == "發"));
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn precision_keys_leave_native_rime_composition_cleanly() {
        let (mut e, clip, imp, mut r) = fixture();
        e.resolved_assets_dir = resolve_assets_dir("");
        for (schema, code) in [(SchemaKind::RimeIce, "yisheng"), (SchemaKind::DoublePinyinFlypy, "yiug")] {
            e.set_schema(schema);
            for (key, expected) in [('[', "医"), (']', "生")] {
                type_code(&mut e, code, &clip, &imp, &mut r);
                assert!(e.rime_input_start.is_some());
                assert_eq!(e.canonical_at(0), Some("医生"));
                let action = e.process_key(key as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
                assert_eq!(action.commit_text.as_deref(), Some(expected));
                assert!(e.rime_input_start.is_none());
                assert!(e.raw_input.is_empty());
            }
            type_code(&mut e, "ma", &clip, &imp, &mut r);
            assert!(e.rime_input_start.is_some());
            e.process_key(KEY_LEFT, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            let cursor = e.raw_cursor;
            type_code(&mut e, "`", &clip, &imp, &mut r);
            assert!(e.rime_input_start.is_none());
            assert_eq!((e.raw_input.as_str(), e.raw_cursor), ("ma`", cursor));
            type_code(&mut e, "1", &clip, &imp, &mut r);
            assert_eq!(e.canonical_at(0), Some("妈"));
            let commit = e.process_key(0x20, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(commit.commit_text.as_deref(), Some("妈"));
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn space_with_empty_filter_retains_native_confirmed_preedit() {
        for (mode, reading, prefix, displayed) in [
            (OpenCcMode::Simplified, "ni", "你", "你"),
            (OpenCcMode::Traditional, "ma", "妈", "媽"),
        ] {
            let (mut e, clip, imp, mut r) = fixture();
            e.set_opencc_mode(mode);
            let preedit = format!("{prefix}hao");
            let raw_input = format!("{reading}hao");
            e.sync_rime_preedit(&rime_ffi::RimeSnapshot {
                commit_text: None,
                cursor_utf8: preedit.len(),
                preedit,
                raw_cursor: raw_input.len(),
                raw_input,
                page_index: 0,
                page_size: 5,
                has_prev: false,
                has_next: false,
                candidates: Vec::new(),
            });
            assert_eq!(e.pure_rust.confirmed_prefix, prefix);
            assert_eq!(e.raw_input, "hao");

            type_code(&mut e, "`zzzz", &clip, &imp, &mut r);
            assert!(e.current_candidates.is_empty());
            assert_eq!(e.composing, format!("{displayed}hao`zzzz"));
            let action = e.process_key(0x20, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(action.commit_text, Some(format!("{displayed}hao`zzzz")));
            assert!(action.preedit.text.is_empty());
        }
    }

    #[test]
    fn touch_keys_recover_one_or_two_neighbor_misses_in_both_schemas() {
        for (schema, intended, typed, expected) in [
            (SchemaKind::DoublePinyinFlypy, "woxliipkgo", "woxkiipkgo", "我想吃苹果"),
            (SchemaKind::DoublePinyinFlypy, "woxliipkgo", "woxkiipkgp", "我想吃苹果"),
            (SchemaKind::RimeIce, "nihao", "nihso", "你好"),
            (SchemaKind::RimeIce, "nihao", "njhso", "你好"),
        ] {
            let (mut engine, clipboard, imported, mut retention) = fixture();
            engine.set_schema(schema);
            // Spatial evidence also works at a level that disables Xiaohe typo guesses.
            engine.pure_rust.typo_correction_level = TypoCorrectionLevel::Conservative;
            for (primary, correct) in typed.bytes().zip(intended.bytes()) {
                engine.process_touch_key(
                    u32::from(primary), 0, SwipeDir::None, touch_hint(primary, correct),
                    &clipboard, &imported, &mut retention,
                ).unwrap();
            }
            let top = engine.current_candidates.first().map(|c| c.text.as_str());
            assert_eq!(top, Some(expected), "{typed} for {intended}");
        }
    }

    #[test]
    fn android_touch_aishi_ranks_exact_bundled_word_first_across_pages() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        for level in [TypoCorrectionLevel::Standard, TypoCorrectionLevel::Aggressive] {
            let (mut engine, clipboard, imported, mut retention) = fixture();
            engine.config.platform = "android".into();
            engine.pure_rust.reload_assets(Some(&assets));
            engine.pure_rust.typo_correction_level = level;
            for (primary, alternatives) in b"aishi".iter().copied()
                .zip([*b"qz", *b"uo", *b"ad", *b"uy", *b"uo"])
            {
                let hint = TouchKeyHint::new(
                    primary, alternatives.map(|ch| Some(SpatialAlt { ch, penalty: 1.0 })),
                );
                engine.process_touch_key(
                    u32::from(primary), 0, SwipeDir::None, hint, &clipboard, &imported, &mut retention,
                ).unwrap();
            }
            assert_eq!(engine.current_candidates[0].text, "碍事", "{:?}", engine.current_candidates);
            loop {
                assert!(engine.current_candidates.iter().all(|c| !matches!(c.text.as_str(), "起诉" | "爱啊" | "唉阿姨")), "{:?}", engine.current_candidates);
                if !engine.current_page.has_next {
                    break;
                }
                engine.process_key('=' as u32, 0, SwipeDir::None, &clipboard, &imported, &mut retention).unwrap();
            }
        }
    }

    #[test]
    fn touch_keys_respect_exact_word_and_disabled_corrections() {
        let (mut engine, clipboard, imported, mut retention) = fixture();
        engine.pure_rust.typo_correction_level = TypoCorrectionLevel::Aggressive;
        retention.pin_word("修饰");
        for (primary, alternative) in b"diushi".iter().copied().zip(b"xiushi".iter().copied()) {
            engine.process_touch_key(
                u32::from(primary), 0, SwipeDir::None, touch_hint(primary, alternative),
                &clipboard, &imported, &mut retention,
            ).unwrap();
        }
        assert_eq!(engine.current_candidates[0].text, "丢失");
        assert!(engine.candidate_origins.iter().any(|origin| matches!(
            origin, CandidateOrigin::PureRust { spatial_guarded: true, .. }
        )));

        engine.clear_composition_state();
        engine.pure_rust.typo_correction_level = TypoCorrectionLevel::Off;
        for (primary, alternative) in b"njhso".iter().copied().zip(b"nihao".iter().copied()) {
            engine.process_touch_key(
                u32::from(primary), 0, SwipeDir::None, touch_hint(primary, alternative),
                &clipboard, &imported, &mut retention,
            ).unwrap();
        }
        assert!(engine.current_candidates.iter().all(|candidate| candidate.text != "你好"));
    }

    #[test]
    fn touch_hints_follow_cursor_edits_partial_selection_escape_and_undo() {
        let (mut engine, clipboard, imported, mut retention) = fixture();
        engine.config.undo_commit_on_backspace = true;
        let s_to_a = touch_hint(b's', b'a');
        let o_to_i = touch_hint(b'o', b'i');
        let empty = TouchKeyHint::default();
        for (key, hint) in [
            (u32::from(b'n'), empty), (u32::from(b'o'), o_to_i), (u32::from(b'\''), empty),
            (u32::from(b'h'), empty), (u32::from(b's'), s_to_a), (u32::from(b'o'), empty),
        ] {
            engine.process_touch_key(key, 0, SwipeDir::None, hint, &clipboard, &imported, &mut retention).unwrap();
        }
        assert_eq!(engine.raw_touch_hints, [empty, o_to_i, empty, empty, s_to_a, empty]);
        for (key, code, hints) in [
            (KEY_HOME, "no'hso", vec![empty, o_to_i, empty, empty, s_to_a, empty]),
            (KEY_RIGHT, "no'hso", vec![empty, o_to_i, empty, empty, s_to_a, empty]),
            (u32::from(b's'), "nso'hso", vec![empty, s_to_a, o_to_i, empty, empty, s_to_a, empty]),
            (0x08, "no'hso", vec![empty, o_to_i, empty, empty, s_to_a, empty]),
            (KEY_DELETE, "n'hso", vec![empty, empty, empty, s_to_a, empty]),
            (u32::from(b'i'), "ni'hso", vec![empty, empty, empty, empty, s_to_a, empty]),
        ] {
            let hint = if key == u32::from(b's') { s_to_a } else { empty };
            engine.process_touch_key(key, 0, SwipeDir::None, hint, &clipboard, &imported, &mut retention).unwrap();
            assert_eq!(engine.raw_input, code);
            assert_eq!(engine.raw_touch_hints, hints);
        }
        let prefix = loop {
            if let Some(index) = engine.candidate_origins.iter().position(|origin| matches!(
                origin, CandidateOrigin::PureRust { consumed_len: 2, .. }
            )) {
                break index;
            }
            assert!(engine.current_page.has_next, "selectable first syllable");
            engine.process_key(0xff56, 0, SwipeDir::None, &clipboard, &imported, &mut retention).unwrap();
        };
        engine.select_candidate_action(prefix, &clipboard, &imported, &mut retention).unwrap();
        assert_eq!(engine.raw_input, "hso");
        assert_eq!(engine.raw_touch_hints, [empty, s_to_a, empty]);
        let last = engine.current_candidates.iter().position(|c| c.text == "好").unwrap();
        engine.select_candidate_action(last, &clipboard, &imported, &mut retention).unwrap();
        assert!(engine.raw_touch_hints.is_empty());
        engine.process_key(0x08, 0, SwipeDir::None, &clipboard, &imported, &mut retention).unwrap();
        assert_eq!(engine.raw_input, "hso");
        assert_eq!(engine.raw_touch_hints, [empty, s_to_a, empty]);
        engine.process_key(0x1b, 0, SwipeDir::None, &clipboard, &imported, &mut retention).unwrap();
        assert!(engine.raw_input.is_empty());
        assert!(engine.raw_touch_hints.is_empty());
    }

    #[test]
    fn pure_rust_edits_in_place_and_decodes_the_corrected_spelling() {
        for (schema, typo, tail) in [
            (SchemaKind::RimeIce, "nixxao", "ao"),
            (SchemaKind::DoublePinyinFlypy, "nixxc", "c"),
        ] {
            let (mut e, clip, imp, mut ret) = fixture();
            e.schema = schema;
            e.pure_rust.typo_correction_level = TypoCorrectionLevel::Off;
            let mut key = |key| e.process_key(key, 0, SwipeDir::None, &clip, &imp, &mut ret).unwrap();
            let typed = typo.bytes().map(|ch| key(ch.into())).last().unwrap();
            assert_eq!(typed.preedit.cursor_utf8, typo.len());
            assert_eq!(key(KEY_HOME).preedit.cursor_utf8, 0);
            assert_eq!(key(0xff08).preedit.text, typo);
            assert_eq!(key(KEY_END).preedit.cursor_utf8, typo.len());
            let moved = (0..=tail.len()).map(|_| key(KEY_LEFT)).last().unwrap();
            assert_eq!(moved.preedit.cursor_utf8, 3);
            assert_eq!(moved.preedit.text, typo);
            assert_eq!(
                moved.candidates.iter().map(|c| &c.text).collect::<Vec<_>>(),
                typed.candidates.iter().map(|c| &c.text).collect::<Vec<_>>(),
            );

            let backspaced = key(0x08);
            assert_eq!(backspaced.preedit.text, format!("nix{tail}"));
            assert_eq!(backspaced.preedit.cursor_utf8, 2);
            let deleted = key(KEY_DELETE);
            assert_eq!(deleted.preedit.text, format!("ni{tail}"));
            assert_eq!(deleted.preedit.cursor_utf8, 2);
            let fixed = key('h' as u32);
            assert_eq!(fixed.preedit.text, format!("nih{tail}"));
            assert_eq!(fixed.preedit.cursor_utf8, 3);
            assert_eq!(fixed.candidates[0].text, "你好");
            assert_eq!(key(0x20).commit_text.as_deref(), Some("你好"));
        }
    }

    #[test]
    fn passthrough_cursor_and_edits_stay_on_utf8_boundaries() {
        let (mut e, clip, imp, mut ret) = fixture();
        type_code(&mut e, "A猫B", &clip, &imp, &mut ret);
        let action = e.set_preedit_cursor(2, &clip, &imp, &mut ret).unwrap();
        assert_eq!(action.preedit.cursor_utf8, 1);
        assert_eq!(action.candidates[0].comment, "En/Token");
        assert_eq!(e.set_preedit_cursor(usize::MAX, &clip, &imp, &mut ret).unwrap().preedit.cursor_utf8, 5);

        let mut key = |key| e.process_key(key, 0, SwipeDir::None, &clip, &imp, &mut ret).unwrap();
        assert_eq!(key(KEY_LEFT).preedit.cursor_utf8, 4);
        assert_eq!(key(KEY_LEFT).preedit.cursor_utf8, 1);
        let inserted = key('é' as u32);
        assert_eq!(inserted.preedit.cursor_utf8, 3);
        assert_eq!(inserted.candidates[0].text, "Aé猫B");
        let deleted = key(KEY_DELETE);
        assert_eq!(deleted.preedit.cursor_utf8, 3);
        assert_eq!(deleted.preedit.text, "AéB");
        assert_eq!(key(0xff08).preedit.text, "AB");
        assert_eq!(key(KEY_RIGHT).preedit.cursor_utf8, 2);
        assert_eq!(key(KEY_DELETE).preedit.text, "AB");
        assert_eq!(key(KEY_HOME).preedit.cursor_utf8, 0);
        assert_eq!(key(KEY_DELETE).preedit.text, "B");
        let empty = key(KEY_DELETE);
        assert_eq!(empty.preedit.cursor_utf8, 0);
        assert!(empty.preedit.text.is_empty() && empty.candidates.is_empty());
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_rime_cursor_edits_raw_input_and_matches_preedit() {
        let (mut e, clip, imp, mut ret) = fixture();
        e.resolved_assets_dir = Some(resolve_assets_dir("").unwrap());
        type_code(&mut e, "nixao", &clip, &imp, &mut ret);
        assert!(e.rime_input_start.is_some());
        let key = |e: &mut EngineState, key, ret: &mut RetentionEngine| {
            e.process_key(key, 0, SwipeDir::None, &clip, &imp, ret).unwrap()
        };
        key(&mut e, KEY_LEFT, &mut ret);
        assert_eq!((e.raw_input.as_str(), e.raw_cursor), ("nixao", 4));
        assert_eq!(e.cursor_utf8, e.composing.len() - 1);
        key(&mut e, KEY_HOME, &mut ret);
        assert_eq!(e.cursor_utf8, 0);
        assert_eq!(key(&mut e, KEY_END, &mut ret).preedit.cursor_utf8, e.composing.len());

        let jumped = e.set_preedit_cursor(2, &clip, &imp, &mut ret).unwrap();
        assert_eq!(e.raw_cursor, 2);
        assert_eq!(&jumped.preedit.text[jumped.preedit.cursor_utf8..], "xao");
        key(&mut e, KEY_RIGHT, &mut ret);
        assert_eq!(e.raw_cursor, 3);
        key(&mut e, KEY_LEFT, &mut ret);
        let deleted = key(&mut e, KEY_DELETE, &mut ret);
        assert_eq!((e.raw_input.as_str(), e.raw_cursor), ("niao", 2));
        assert_eq!(&deleted.preedit.text[deleted.preedit.cursor_utf8..], "ao");
        let inserted = key(&mut e, 'h' as u32, &mut ret);
        assert_eq!((e.raw_input.as_str(), e.raw_cursor), ("nihao", 3));
        assert_eq!(&inserted.preedit.text[inserted.preedit.cursor_utf8..], "ao");
        let finished = key(&mut e, KEY_END, &mut ret);
        assert_eq!(finished.preedit.cursor_utf8, finished.preedit.text.len());
        assert!(finished.candidates.iter().any(|c| c.text == "你好"));
        key(&mut e, 0x1b, &mut ret);

        e.set_schema(SchemaKind::DoublePinyinFlypy);
        type_code(&mut e, "nihc", &clip, &imp, &mut ret);
        let display = e.composing.find("hao").unwrap() + 1;
        let jumped = e.set_preedit_cursor(display, &clip, &imp, &mut ret).unwrap();
        assert_eq!((e.raw_input.as_str(), e.raw_cursor), ("nihc", 3));
        assert_eq!(&jumped.preedit.text[jumped.preedit.cursor_utf8..], "c");
    }

    fn offer(engine: &mut EngineState, word: &str, code: &str) {
        engine.set_raw_input(code.into());
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
                consumed_len: code.len(),
                lm_score: 0.0,
                #[cfg(any(feature = "native-onnx", test))]
                rank_score: 0.0,
                spatial_guarded: false,
                features: [0.0; 9],
                canonical: word.into(),
                commit_text: word.into(),
            },
        )]);
    }

    #[test]
    fn partial_selection_keeps_canonical_context_without_duplicate_commits() {
        for keyboard in [false, true] {
            let (mut e, clip, imp, mut r) = fixture();
            offer(&mut e, "云", "yunli");
            if let CandidateOrigin::PureRust { consumed_len, .. } = &mut e.candidate_origins[0] {
                *consumed_len = 3;
            }
            let first = if keyboard {
                e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap()
                    .commit_text
            } else {
                e.select_candidate_on_backend(0, &clip, &imp, &mut r)
                    .unwrap()
            };
            assert_eq!(first.as_deref(), Some("云"));
            assert_eq!(e.raw_input, "li");
            assert_eq!(e.pure_rust.confirmed_prefix, "云");
            offer(&mut e, "璃", "li");
            if keyboard {
                e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
            } else {
                e.select_candidate_on_backend(0, &clip, &imp, &mut r)
                    .unwrap();
            }
            assert_eq!(e.speech_context_before(), "云璃");
            assert!(!r
                .context_bigrams
                .get("璃")
                .is_some_and(|b| b.contains_key("云璃")));
            assert_eq!(r.user_phrases["云璃"].count, 1);
            assert!(e.pure_rust.confirmed_prefix.is_empty());
            offer(&mut e, "云", "yunli");
            if let CandidateOrigin::PureRust { consumed_len, .. } = &mut e.candidate_origins[0] {
                *consumed_len = 3;
            }
            e.select_candidate_on_backend(0, &clip, &imp, &mut r)
                .unwrap();
            offer(&mut e, "璃", "li");
            e.select_candidate_on_backend(0, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(r.user_phrases["云璃"].count, 2);
        }
        let (mut e, clip, imp, mut r) = fixture();
        e.config.adaptive_learning = false;
        offer(&mut e, "我要", "woyaoquchifan");
        if let CandidateOrigin::PureRust { consumed_len, .. } = &mut e.candidate_origins[0] {
            *consumed_len = 5;
        }
        e.select_candidate_on_backend(0, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(e.raw_input, "quchifan");
        assert_eq!(e.pure_rust.confirmed_prefix, "我要");
        e.last_committed_ms = 0;
        e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
        assert_eq!(e.pure_rust.confirmed_prefix, "我要");
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_confirmed_prefix_retracts_when_preedit_returns_to_pinyin() {
        let (mut e, _, _, _) = fixture();
        let mut snap = rime_ffi::RimeSnapshot {
            commit_text: None,
            preedit: "我要qu chi fan".into(),
            cursor_utf8: "我要qu chi fan".len(),
            raw_input: "woyaoquchifan".into(),
            raw_cursor: "woyaoquchifan".len(),
            page_index: 0,
            page_size: 5,
            has_prev: false,
            has_next: false,
            candidates: Vec::new(),
        };
        e.sync_rime_preedit(&snap);
        assert_eq!(e.pure_rust.confirmed_prefix, "我要");
        assert_eq!(e.raw_input, "quchifan");
        assert_eq!(e.raw_cursor, 8);
        assert_eq!(e.raw_cursor_for_rime_preedit("我要qu ".len()), 2);
        snap.preedit = "woyaoquchifan".into();
        snap.cursor_utf8 = 2;
        snap.raw_cursor = 2;
        e.sync_rime_preedit(&snap);
        assert!(e.pure_rust.confirmed_prefix.is_empty());
        assert_eq!(e.raw_input, "woyaoquchifan");
        assert_eq!(e.raw_cursor, 2);
    }

    #[test]
    fn multi_character_regret_requires_exact_deletion_and_restores_context() {
        for deleted in [1, 2, 3] {
            let (mut e, clip, imp, mut r) = fixture();
            for (word, code) in [("请", "qing"), ("指导", "zhidao")] {
                offer(&mut e, word, code);
                e.select_candidate_on_backend(0, &clip, &imp, &mut r)
                    .unwrap();
            }
            e.notify_deleted_chars(deleted);
            offer(&mut e, "知道", "zhidao");
            e.select_candidate_on_backend(0, &clip, &imp, &mut r)
                .unwrap();
            if deleted == 2 {
                assert_eq!(e.speech_context_before(), "请知道");
                assert!(r.context_bigrams["请"].contains_key("知道"));
                assert!(!r
                    .context_bigrams
                    .get("指导")
                    .is_some_and(|b| b.contains_key("知道")));
                assert!(r.frecency["指导"] <= 0.5);
            } else {
                assert!(r.frecency["指导"] > 0.5);
            }
        }
    }

    #[test]
    fn frequent_polyphones_keep_priority_only_for_the_learned_reading() {
        let (mut e, clip, imp, _) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        for (word, learned, other) in [
            ("的", "de", "di"),
            ("会", "hui", "kuai"),
            ("说", "shuo", "shui"),
            ("了", "le", "liao"),
        ] {
            let mut r = RetentionEngine::default();
            let now = RetentionEngine::now_epoch_ms();
            for _ in 0..30 {
                r.record_contextual_selection(word, learned, None, None, now, 1.0, true);
            }
            for (code, matches) in [(other, false), (learned, true)] {
                e.clear_composition_state();
                e.set_raw_input(code.into());
                let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
                assert!(!action.candidates.is_empty());
                assert_eq!(
                    action.candidates[0].text == word,
                    matches,
                    "{code}: {:?}",
                    action.candidates
                );
            }
        }
    }

    #[test]
    fn rarely_selected_polyphone_reading_keeps_dictionary_priority_in_both_schemas() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let (mut engine, clipboard, imported, mut ret) = fixture();
            engine.schema = schema;
            let encode = |pinyin: &str| {
                if schema == SchemaKind::RimeIce {
                    pinyin.to_string()
                } else {
                    crate::imp::xiaohe::syllable_to_xiaohe(pinyin)
                }
            };
            let now = RetentionEngine::now_epoch_ms();
            let hui = encode("hui");
            let kuai = encode("kuai");
            for _ in 0..100 {
                ret.record_contextual_selection("会", &hui, None, None, now, 1.0, true);
            }
            ret.record_contextual_selection("会", &kuai, None, None, now, 1.0, true);
            assert!(ret.reading_frecency_at("会", "hui", schema == SchemaKind::RimeIce, now) > 90.0);
            assert_eq!(ret.reading_frecency_at("会", "kuai", schema == SchemaKind::RimeIce, now), 0.0);
            engine.set_raw_input(kuai);
            let action = engine.process_key_pure_rust(0, &clipboard, &imported, &mut ret).unwrap();
            assert_eq!(action.candidates[0].text, "快", "{schema:?}");
        }
    }

    #[test]
    fn real_phrase_selection_cannot_override_a_complete_syllable_with_its_acronym() {
        let (mut engine, clipboard, imported, mut ret) = fixture();
        for _ in 0..2 {
            engine.set_raw_input("fangan".into());
            let action = engine.process_key_pure_rust(0, &clipboard, &imported, &mut ret).unwrap();
            let index = action.candidates.iter().position(|c| c.text == "方案").unwrap();
            assert_eq!(
                engine.select_candidate_on_backend(index, &clipboard, &imported, &mut ret).unwrap().as_deref(),
                Some("方案"),
            );
            engine.reset_commit_context();
        }
        assert_eq!(ret.query_acronym_phrases("fa")[0].word, "方案");
        engine.set_raw_input("fa".into());
        let action = engine.process_key_pure_rust(0, &clipboard, &imported, &mut ret).unwrap();
        assert_eq!(action.candidates[0].text.chars().count(), 1);
        assert!(action.candidates.iter().all(|c| c.text != "方案"));
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_selection_learns_only_after_commit_and_tracks_the_consumed_prefix() {
        let (mut engine, clipboard, imported, mut ret) = fixture();
        let snapshot = |preedit: &str, commit_text: Option<&str>| rime_ffi::RimeSnapshot {
            commit_text: commit_text.map(str::to_owned),
            preedit: preedit.into(),
            cursor_utf8: preedit.len(),
            raw_input: "ni'hao".into(),
            raw_cursor: "ni'hao".len(),
            page_index: 0,
            page_size: 5,
            has_prev: false,
            has_next: false,
            candidates: Vec::new(),
        };
        for commit in [false, true] {
            engine.set_raw_input("ni'hao".into());
            assert_eq!(
                engine.apply_rime_selection(snapshot("你hao", None), "你", 0, &clipboard, &imported, &mut ret),
                None,
            );
            assert_eq!(engine.raw_input, "hao");
            assert_eq!(engine.pending_phrase_code, "ni");
            assert!(engine.speech_context_before().is_empty());
            assert!(ret.code_preferences.is_empty());
            if commit {
                assert_eq!(
                    engine.apply_rime_selection(snapshot("", Some("你好")), "好", 0, &clipboard, &imported, &mut ret).as_deref(),
                    Some("你好"),
                );
            } else {
                engine.process_key(27, 0, SwipeDir::None, &clipboard, &imported, &mut ret).unwrap();
                assert!(engine.pending_phrase_code.is_empty());
                assert!(ret.frecency.is_empty());
            }
        }
        assert_eq!(engine.speech_context_before(), "你好");
        assert!(ret.code_preferences["nihao"].contains_key("你好"));
        assert!(!ret.code_preferences.contains_key("hao"));
    }

    #[test]
    fn frequent_abbreviations_do_not_override_legal_syllables() {
        let (mut e, clip, imp, mut r) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        let now = RetentionEngine::now_epoch_ms();
        for (word, code) in [
            ("方案", "fangan"),
            ("平安", "pingan"),
            ("答案", "daan"),
            ("第二", "dier"),
            ("公安", "gongan"),
        ] {
            for _ in 0..30 {
                r.record_contextual_selection(word, code, None, None, now, 1.0, true);
            }
        }
        for (code, excluded) in [
            ("fa", "方案"),
            ("pa", "平安"),
            ("da", "答案"),
            ("de", "第二"),
            ("ga", "公安"),
        ] {
            e.clear_composition_state();
            e.set_raw_input(code.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            assert_eq!(
                action.candidates[0].text.chars().count(),
                1,
                "{code}: {:?}",
                action.candidates
            );
            assert_ne!(action.candidates[0].text, excluded);
        }
        for code in ["nh", "srf", "wsm"] {
            e.clear_composition_state();
            e.set_raw_input(code.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            assert!(
                action.candidates.iter().any(|c| {
                    c.text.chars().count() >= 2
                        && c.comment.split_whitespace().filter_map(|s| s.chars().next()).collect::<String>() == code
                }),
                "{code}: {:?}",
                action.candidates
            );
        }
    }

    #[test]
    fn nonpersonal_priors_preserve_selectable_sentence_prefixes() {
        let (mut e, clip, imp, mut r) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        for prior in [6, 8] {
            e.clear_composition_state();
            r.reranker.weights[8] = if prior == 8 { 1.0 } else { 0.0 };
            if prior == 6 {
                e.cached_mlm_priors = e
                    .pure_rust
                    .get_corpus()
                    .character_keys()
                    .map(|ch| (ch, 0.5))
                    .collect();
            }
            e.set_raw_input("woyaoquka".into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            assert!(e.candidate_origins.iter().take(2).all(|origin| matches!(
                origin,
                CandidateOrigin::PureRust {
                    consumed_len: 9,
                    ..
                }
            )));
            for features in e.candidate_features.iter().take(2) {
                assert!(features[prior] > 0.0);
                assert_eq!(&features[..6], &[0.0; 6]);
                assert_eq!(features[7], 0.0);
            }
            assert_eq!(
                action.candidates[2].text, "我要去",
                "prior={prior}: {:?}",
                action.candidates
            );
            assert!(
                matches!(e.candidate_origins[2], CandidateOrigin::PureRust { consumed_len, .. } if consumed_len < e.raw_input.len())
            );
        }
    }

    #[test]
    fn personal_priority_uses_only_weighted_personal_features() {
        let (mut e, _, _, mut r) = fixture();
        offer(&mut e, "去", "qu");
        let origin = &mut e.candidate_origins[0];
        let mut features = [0.0; 9];
        features[0] = 0.3;
        features[6] = 1.0;
        features[8] = 1.0;
        origin.set_features(features);
        r.reranker.weights[8] = 2.0;
        assert!(!origin.has_personal_priority(&r, true));

        r.reranker.weights[0] = 2.0;
        assert!(origin.has_personal_priority(&r, true));
        assert!(!origin.has_personal_priority(&r, false));

        features[0] = 0.6;
        origin.set_features(features);
        r.reranker.weights[0] = 0.5;
        assert!(!origin.has_personal_priority(&r, true));
        assert!(origin.has_personal_priority(&r, false));
    }

    #[test]
    fn portable_xiaohe_exact_reading_precedes_fuzzy_pin() {
        let (_, clip, imp, mut r) = fixture();
        let mut e = EngineState::new(EngineConfig {
            schema: SchemaKind::DoublePinyinFlypy,
            fuzzy_pinyin: true,
            fuzzy_pinyin_pairs: vec!["sh_s".into()],
            typo_correction_level: "off".into(),
            ..Default::default()
        });
        r.pin_word("是");
        e.set_raw_input("si".into());
        let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
        assert_eq!(action.candidates[0].comment, "si");
        assert!(!action.candidates[0].pinned);
        loop {
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            if let Some(candidate) = action.candidates.iter().find(|c| c.text == "是") {
                assert_eq!(candidate.comment, "shi");
                assert!(candidate.pinned);
                break;
            }
            assert!(action.page.has_next, "missing fuzzy candidate 是");
            e.pure_rust.page_index += 1;
        }
    }

    #[test]
    fn portable_xiaohe_exact_readings_precede_a_pinned_english_word() {
        let (mut e, clip, imp, mut r) = fixture();
        e.set_schema(SchemaKind::DoublePinyinFlypy);
        e.pure_rust.fuzzy_pinyin = true;
        e.pure_rust.fuzzy_pairs = quanpin::FuzzyPairs::from_names(&["sh_s".into()]).unwrap();
        e.pure_rust.typo_correction_level = TypoCorrectionLevel::Off;
        let assets = default_assets_dir_candidates().unwrap();
        e.pure_rust.reload_assets(Some(&assets));
        r.pin_word("so");

        type_code(&mut e, "so", &clip, &imp, &mut r);
        assert!(quanpin::matches_xiaohe_reading(&e.current_candidates[0].comment, "so"));
        loop {
            for candidate in &e.current_candidates {
                if candidate.text == "so" {
                    assert!(candidate.pinned);
                    assert_eq!(candidate.comment, "英文");
                    return;
                }
                assert!(quanpin::matches_xiaohe_reading(&candidate.comment, "so"), "{candidate:?}");
            }
            assert!(e.current_page.has_next, "missing pinned English candidate");
            e.process_key(0xFF56, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
        }
    }

    #[test]
    fn weak_history_and_nonpersonal_priors_preserve_portable_ranking() {
        let (mut e, clip, imp, mut r) = fixture();
        let now = RetentionEngine::now_epoch_ms();
        let six_half_lives_ms = (r.half_life_sec * 6_000.0) as u64;
        r.record_selection_at("妮", now - six_half_lives_ms, 3.2);
        e.cached_mlm_priors.insert('妮', 1.0);
        e.config.platform = "android".into();
        e.set_raw_input("ni".into());
        let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
        assert_eq!(action.candidates[0].text, "你");
        let rare = action.candidates.iter().position(|c| c.text == "妮").unwrap();
        assert!(e.candidate_features[rare][6] > 0.0);
        assert!((e.candidate_features[rare][0] - 0.05).abs() < 0.001);

        // Learn a positive base-rank weight with no personal evidence for the typo.
        let mut preferred = [0.0; 9];
        preferred[8] = 1.0;
        assert!(r.update_reranker_pairwise(&preferred, &[0.0; 9], now));
        assert!(r.reranker.weights[8] > 0.0);
        for (code, expected) in [("ni", "你"), ("nihoa", "你好")] {
            e.set_raw_input(code.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            assert_eq!(action.candidates[0].text, expected, "{code}: {:?}", action.candidates);
            assert!(e.candidate_features[0][8] > 0.0);
        }
    }

    #[test]
    fn imported_weight_competes_with_pins_and_established_habits() {
        let (mut e, clip, mut imp, mut r) = fixture();
        e.set_raw_input("nihao".into());
        let run = |e: &mut EngineState, imp: &ImportedLexiconIndex, r: &mut RetentionEngine| {
            let portable = e.process_key_pure_rust(0, &clip, imp, r).unwrap().candidates;
            #[cfg(all(target_os = "linux", feature = "native-rime"))]
            {
                let snapshot = rime_ffi::RimeSnapshot {
                    commit_text: None,
                    preedit: "nihao".into(),
                    cursor_utf8: 5,
                    raw_input: "nihao".into(),
                    raw_cursor: 5,
                    page_index: 0,
                    page_size: 5,
                    has_prev: false,
                    has_next: false,
                    candidates: vec![rime_ffi::RimeRawCandidate {
                        text: "你好".into(),
                        comment: String::new(),
                    }],
                };
                let native = e.apply_rime_snapshot(snapshot, false, false, &clip, imp, r);
                assert_eq!(native.candidates[0].text, portable[0].text, "{:?}", native.candidates);
            }
            portable
        };
        let entry = |text: &str, weight| crate::imp::ImportedLexiconEntry {
            text: text.into(),
            pinyin: "ni hao".into(),
            toned_pinyin: "nǐ hǎo".into(),
            xiaohe: "nihc".into(),
            weight,
        };
        imp.inject_hotwords(&[entry("你号", 700)]);
        assert_eq!(run(&mut e, &imp, &mut r)[0].text, "你号");
        imp.inject_hotwords(&[entry("妮好", 100_000)]);
        let untrained = run(&mut e, &imp, &mut r);
        assert_eq!(untrained[0].text, "妮好");
        assert!(untrained.iter().any(|c| c.text == "你号"));
        r.pin_word("你好");
        assert_eq!(run(&mut e, &imp, &mut r)[0].text, "你好");
        r.pinned_words.remove("你好");
        let now = RetentionEngine::now_epoch_ms();
        for _ in 0..24 {
            r.record_contextual_selection("你好", "nihao", None, None, now, 1.0, true);
        }
        let habitual = run(&mut e, &imp, &mut r);
        assert_eq!(habitual[0].text, "你好");
        assert!(habitual.iter().any(|c| c.text == "妮好"));
    }

    #[test]
    fn late_homophones_are_reachable_on_portable_candidate_pages() {
        let (mut e, clip, _, mut r) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        let imp = ImportedLexiconIndex::new(Some(&assets));
        for (schema, code, syllable) in [
            (SchemaKind::RimeIce, "shi", "shi"),
            (SchemaKind::RimeIce, "yi", "yi"),
            (SchemaKind::DoublePinyinFlypy, "ui", "shi"),
        ] {
            let late_word = {
                let mut bucket = e.pure_rust.get_corpus().spaced_pinyin_words(syllable);
                assert!(bucket.len() > 48);
                bucket.nth(48).unwrap().0.to_owned()
            };
            e.clear_composition_state();
            e.schema = schema;
            e.set_raw_input(code.into());
            let mut action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            let mut seen = HashSet::new();
            loop {
                seen.extend(action.candidates.iter().map(|c| c.text.clone()));
                if !action.page.has_next {
                    break;
                }
                assert!(action.page.page_index < 60);
                action = e
                    .process_key_pure_rust(0xFF56, &clip, &imp, &mut r)
                    .unwrap();
            }
            assert!(seen.len() > 48, "{code}: {}", seen.len());
            assert!(seen.contains(&late_word), "{code}: missing {late_word}");
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_merge_keeps_personal_prefixes_behind_full_readings() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let imp = ImportedLexiconIndex::new(Some(&assets));
        for (schema, code, prefix, prefix_code, full, native_prefix) in [
            (SchemaKind::RimeIce, "dibu", "地", "di", "底部", Some("地")),
            (
                SchemaKind::DoublePinyinFlypy,
                "woycquka",
                "我要",
                "woyc",
                "我要去卡",
                None,
            ),
        ] {
            let (mut e, clip, _, mut r) = fixture();
            e.schema = schema;
            e.pure_rust = PureRustEngine::new(Some(&assets));
            e.set_raw_input(code.into());
            let now = RetentionEngine::now_epoch_ms();
            r.record_contextual_selection(prefix, prefix_code, None, None, now, 100.0, true);
            r.pin_word(prefix);
            let mut candidates = vec![rime_ffi::RimeRawCandidate {
                text: full.into(),
                comment: String::new(),
            }];
            if let Some(word) = native_prefix {
                candidates.push(rime_ffi::RimeRawCandidate {
                    text: word.into(),
                    comment: String::new(),
                });
            }
            let snapshot = rime_ffi::RimeSnapshot {
                commit_text: None,
                preedit: code.into(),
                cursor_utf8: code.len(),
                raw_input: code.into(),
                raw_cursor: code.len(),
                page_index: 0,
                page_size: 5,
                has_prev: false,
                has_next: false,
                candidates,
            };
            let action = e.apply_rime_snapshot(snapshot, false, false, &clip, &imp, &r);
            assert_eq!(
                action.candidates[0].text, full,
                "{code}: {:?}",
                action.candidates
            );
            assert!(!e.candidate_origins.iter().any(|origin| matches!(origin, CandidateOrigin::PureRust { consumed_len, .. } if *consumed_len < code.len())));
            if native_prefix.is_none() {
                assert!(!action.candidates.iter().any(|c| c.text == prefix));
            } else {
                assert!(action
                    .candidates
                    .iter()
                    .any(|c| c.text == prefix && c.pinned));
            }
        }
    }

    #[test]
    fn personal_single_characters_do_not_displace_dibu_words() {
        let (mut e, clip, imp, mut r) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        e.config.platform = "android".into();
        let now = RetentionEngine::now_epoch_ms();
        for (word, code, count) in [("的", "de", 30), ("地", "di", 10), ("不", "bu", 20)] {
            for _ in 0..count {
                r.record_contextual_selection(word, code, None, None, now, 1.0, true);
            }
        }
        for pinned in [false, true] {
            if pinned {
                r.pin_word("的");
            }
            e.set_raw_input("dibu".into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            let words: Vec<_> = action.candidates.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(words[0], "底部", "pinned={pinned}: {words:?}");
            for word in ["底部", "地步"] {
                let rank = words.iter().position(|w| *w == word).unwrap();
                for other in ["的", "地", "的不", "的部", "的步"] {
                    if let Some(other_rank) = words.iter().position(|w| *w == other) {
                        assert!(rank < other_rank, "pinned={pinned}: {words:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn personal_prefixes_do_not_displace_full_sentences_or_typo_corrections() {
        let (mut e, clip, imp, mut r) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        let now = RetentionEngine::now_epoch_ms();
        for (word, code, count) in [("似乎", "sihu", 10), ("时", "shi", 15)] {
            for _ in 0..count {
                r.record_contextual_selection(word, code, None, None, now, 1.0, true);
            }
        }
        for code in ["sihuyoudianwenti", "sihuyoudianwrnti"] {
            e.set_raw_input(code.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            assert_eq!(
                action.candidates[0].text, "似乎有点问题",
                "{code}: {:?}",
                action.candidates
            );
            assert!(matches!(
                e.candidate_origins[0],
                CandidateOrigin::PureRust { consumed_len, .. } if consumed_len == code.len()
            ));
        }
    }

    #[test]
    fn personal_full_matches_preserve_selectable_sentence_prefixes() {
        let (mut e, clip, imp, _) = fixture();
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        e.pure_rust = PureRustEngine::new(Some(&assets));
        e.config.platform = "android".into();
        let code = "woyaoquka";
        let full_words: Vec<_> = e
            .pure_rust
            .query_all_candidates_contextual(
                code,
                e.schema,
                e.opencc_mode,
                Some(&imp),
                None,
                None,
                None,
                true,
            )
            .into_iter()
            .filter(|c| c.consumed_len == code.len())
            .take(3)
            .map(|c| c.canonical)
            .collect();
        assert_eq!(full_words.len(), 3);
        for personal_count in [0, 1, 3] {
            let mut r = RetentionEngine::default();
            let now = RetentionEngine::now_epoch_ms();
            r.record_selection_at("我要", now, 10.0);
            r.record_selection_at("我", now, 15.0);
            r.pin_word("我");
            for word in full_words.iter().take(personal_count) {
                r.record_selection_at(word, now, 5.0);
            }
            e.set_raw_input(code.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            let head_count = personal_count.max(2);
            assert!(e.candidate_origins.iter().take(head_count).all(|origin| matches!(
                origin, CandidateOrigin::PureRust { consumed_len, .. } if *consumed_len == code.len()
            )), "personal_count={personal_count}: {:?}", action.candidates);
            for word in full_words.iter().take(personal_count) {
                assert!(action
                    .candidates
                    .iter()
                    .take(head_count)
                    .any(|c| &c.text == word));
            }
            for word in ["我要去", "我要", "我", "握"] {
                assert!(
                    action.candidates.iter().any(|c| c.text == word),
                    "personal_count={personal_count}: {:?}",
                    action.candidates
                );
            }
        }
    }

    #[test]
    fn portable_homophones_span_many_five_item_desktop_pages() {
        let (mut e, clip, imp, mut r) = fixture();
        for platform in ["", "linux", "windows"] {
            e.config.platform = platform.into();
            for (schema, code, minimum) in [
                (SchemaKind::RimeIce, "shi", 40),
                (SchemaKind::DoublePinyinFlypy, "ui", 40),
                (SchemaKind::RimeIce, "yishi", 13),
            ] {
                e.clear_composition_state();
                e.schema = schema;
                e.set_raw_input(code.into());
                let mut action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
                assert_eq!(action.page.page_size, 5);
                assert_eq!(action.candidates.len(), 5);
                assert!(action.page.has_next);
                let first_page = action.candidates.clone();
                let ignored = e
                    .process_key_pure_rust('6' as u32, &clip, &imp, &mut r)
                    .unwrap();
                assert!(ignored.commit_text.is_none());
                assert!(ignored
                    .candidates
                    .iter()
                    .map(|c| (&c.text, &c.comment, c.index))
                    .eq(first_page.iter().map(|c| (&c.text, &c.comment, c.index))));
                let mut seen = HashSet::new();
                loop {
                    assert_eq!(action.page.page_size, 5);
                    assert!(action.candidates.len() <= 5);
                    for candidate in &action.candidates {
                        assert!(seen.insert(candidate.text.clone()), "{platform} {code}");
                    }
                    if !action.page.has_next {
                        break;
                    }
                    assert!(seen.len() < 300, "pagination did not terminate");
                    action = e
                        .process_key_pure_rust(0xFF56, &clip, &imp, &mut r)
                        .unwrap();
                    assert!(action.page.has_prev);
                }
                assert!(seen.len() >= minimum, "{platform} {code}: {}", seen.len());
            }
        }
    }

    #[test]
    fn android_pages_have_twenty_four_items_and_select_beyond_five() {
        for (platform, expected_size) in [("android", 24), ("linux", 5), ("windows", 5), ("", 5)] {
            let mut e = EngineState::new(EngineConfig {
                platform: platform.into(),
                schema: SchemaKind::RimeIce,
                ..Default::default()
            });
            assert_eq!(e.current_page.page_size, expected_size);
            e.set_raw_input("github".into());
            assert_eq!(e.build_passthrough_action().page.page_size, expected_size);
        }
        let (mut e, clip, imp, mut r) = fixture();
        e.config.platform = "android".into();
        for (schema, code) in [
            (SchemaKind::RimeIce, "shi"),
            (SchemaKind::DoublePinyinFlypy, "ui"),
        ] {
            e.schema = schema;
            e.set_raw_input(code.into());
            let first = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            assert_eq!(first.page.page_size, 24);
            assert_eq!(first.candidates.len(), 24);
            assert!(first.page.has_next);
            let second = e
                .process_key_pure_rust(0xFF56, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(second.page.page_size, 24);
            assert_eq!(second.page.page_index, 1);
            assert!(second.page.has_prev);
            assert!(second
                .candidates
                .iter()
                .all(|c| !first.candidates.iter().any(|p| p.text == c.text)));
            let back = e
                .process_key_pure_rust(0xFF55, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(back.page.page_index, 0);
            assert!(!back.page.has_prev);
            assert!(back
                .candidates
                .iter()
                .map(|c| (&c.text, &c.comment, c.index))
                .eq(first
                    .candidates
                    .iter()
                    .map(|c| (&c.text, &c.comment, c.index))));
            let expected = first.candidates[10].text.clone();
            assert_eq!(
                e.select_candidate_on_backend(10, &clip, &imp, &mut r)
                    .unwrap(),
                Some(expected)
            );
            assert!(e.raw_input.is_empty());
            e.set_raw_input(code.into());
            let first = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
            let expected = first.candidates[8].text.clone();
            let selected = e
                .process_key_pure_rust('9' as u32, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(selected.commit_text, Some(expected));
            assert!(e.raw_input.is_empty());
        }
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn semantic_context_rejects_old_results_and_deduplicates_requests() {
        let (mut engine, _, imported, mut retention) = fixture();
        engine.model_paths.reranker = Some(PathBuf::from("test-reranker"));
        let now = RetentionEngine::now_epoch_ms();
        engine.record_candidate_commit("你好", "nihao", None, &imported, &mut retention, now);
        let (old, _, _) = engine.take_semantic_context().unwrap();
        assert!(engine.take_semantic_context().is_none());

        engine.record_candidate_commit("世界", "shijie", None, &imported, &mut retention, now);
        let fallback = engine.cached_context_vec.unwrap();
        let (current, context, _) = engine.take_semantic_context().unwrap();
        assert_eq!(context.iter().collect::<String>(), "你好世界");
        engine.apply_semantic_context(old, [1; 32], HashMap::from([('测', 0.5)]));
        assert_eq!(engine.cached_context_vec, Some(fallback));
        assert!(engine.cached_mlm_priors.is_empty());
        engine.apply_semantic_context(current, [2; 32], HashMap::from([('试', 0.5)]));
        assert_eq!(engine.cached_context_vec, Some([2; 32]));
        assert_eq!(engine.cached_mlm_priors.get(&'试'), Some(&0.5));

        engine.reset_commit_context();
        engine.apply_semantic_context(current, [2; 32], HashMap::new());
        assert!(engine.cached_context_vec.is_none());
        assert!(engine.take_semantic_context().is_none());
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn real_semantic_context_is_filled_after_commit() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let config = EngineConfig {
            assets_dir: assets.display().to_string(),
            pure_offline: true,
            ..Default::default()
        };
        let core = crate::VitralCore::init(&serde_json::to_string(&config).unwrap()).unwrap();
        {
            let mut st = core.input.lock();
            assert!(st.engine.model_paths.reranker.is_some());
            offer(&mut st.engine, "你好", "nihao");
        }
        assert_eq!(
            core.select_candidate(0).unwrap().commit_text.as_deref(),
            Some("你好")
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let st = core.input.lock();
                assert!(st.engine.cached_context_vec.is_some());
                if !st.engine.cached_mlm_priors.is_empty() {
                    assert!(!st.semantic_worker_running);
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "semantic inference did not complete");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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

    fn assert_personal_acronym(
        e: &mut EngineState,
        clip: &ClipboardManager,
        imp: &ImportedLexiconIndex,
        r: &mut RetentionEngine,
        word: &str,
        acronym: &str,
    ) {
        e.reset_commit_context();
        type_code(e, acronym, clip, imp, r);
        assert!(
            e.current_candidates
                .iter()
                .take(3)
                .any(|c| c.text == word && c.comment == "简拼"),
            "PureRust {:?}: {acronym} -> {:?}",
            e.schema,
            e.current_candidates
        );
        e.clear_composition_state();

        #[cfg(all(target_os = "linux", feature = "native-rime"))]
        {
            // Exercise the native overlay merge with a synthetic Rime snapshot.
            e.set_raw_input(acronym.into());
            let snapshot = rime_ffi::RimeSnapshot {
                commit_text: None,
                preedit: acronym.into(),
                cursor_utf8: acronym.len(),
                raw_input: acronym.into(),
                raw_cursor: acronym.len(),
                page_index: 0,
                page_size: 5,
                has_prev: false,
                has_next: false,
                candidates: vec![rime_ffi::RimeRawCandidate {
                    text: "词".into(),
                    comment: String::new(),
                }],
            };
            let action = e.apply_rime_snapshot(snapshot, false, false, clip, imp, r);
            assert!(
                action
                    .candidates
                    .iter()
                    .take(3)
                    .any(|c| c.text == word && c.comment == "简拼"),
                "native overlay {:?}: {acronym} -> {:?}",
                e.schema,
                action.candidates
            );
            e.clear_composition_state();
        }
    }

    #[test]
    fn spaced_typed_commits_gain_personal_acronyms_in_both_schemas() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            for (word, pinyin, acronym) in [
                ("测试词组", "ce shi ci zu", "cscz"),
                ("我要打卡", "wo yao da ka", "wydk"),
                ("我要打卡新词组", "wo yao da ka xin ci zu", "wydkxcz"),
                (
                    "我要打卡你好世界",
                    "wo yao da ka ni hao shi jie",
                    "wydknhsj",
                ),
            ] {
                for interval_ms in [5_000, 60_000, 3_600_000] {
                    let (mut e, clip, imp, mut r) = fixture();
                    e.schema = schema;
                    let code = if schema == SchemaKind::RimeIce {
                        pinyin.split_whitespace().collect::<String>()
                    } else {
                        crate::imp::xiaohe::pinyin_phrase_to_xiaohe(pinyin)
                    };
                    let now = RetentionEngine::now_epoch_ms();
                    e.record_candidate_commit(word, &code, None, &imp, &mut r, now);
                    assert!(!r.user_phrases.contains_key(word));
                    e.record_candidate_commit(word, &code, None, &imp, &mut r, now + interval_ms);
                    assert!(r.frecency[word] < 2.0);
                    let phrase = r.user_phrases.get(word).unwrap();
                    assert_eq!(phrase.source, "typed");
                    assert_eq!(phrase.count, 2);
                    assert_eq!(phrase.acronym, acronym);
                    assert_eq!(phrase.updated_ms, now + interval_ms);
                    assert_personal_acronym(&mut e, &clip, &imp, &mut r, word, acronym);
                }
            }
        }
    }

    #[test]
    fn composed_phrase_gains_count_and_acronym_after_later_commit() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let (mut e, clip, imp, mut r) = fixture();
            e.schema = schema;
            let codes = if schema == SchemaKind::RimeIce {
                ["wo", "yao", "da", "ka"]
            } else {
                ["wo", "yk", "da", "ka"]
            };
            for (i, (word, code)) in ["我", "要", "打", "卡"].into_iter().zip(codes).enumerate() {
                e.track_phrase_selection(word, code, i == 3, &imp, &mut r);
            }
            let phrase = &r.user_phrases["我要打卡"];
            assert_eq!(phrase.source, "typed");
            assert_eq!(phrase.count, 1);
            let now = phrase.updated_ms + 10_000;
            // Legacy entries may have no stored acronym yet.
            r.user_phrases.get_mut("我要打卡").unwrap().acronym.clear();
            r.frecency.insert("我要打卡".into(), 0.1);
            e.record_candidate_commit("我要打卡", &codes.concat(), None, &imp, &mut r, now);
            assert!(r.frecency["我要打卡"] < 1.5);
            let phrase = &r.user_phrases["我要打卡"];
            assert_eq!(phrase.count, 2);
            assert_eq!(phrase.updated_ms, now);
            assert_eq!(phrase.acronym, "wydk");
            // Count must keep the acronym eligible after frecency decays.
            r.frecency.insert("我要打卡".into(), 0.0);
            assert_eq!(r.query_acronym_phrases("wydk")[0].word, "我要打卡");
            assert_personal_acronym(&mut e, &clip, &imp, &mut r, "我要打卡", "wydk");
        }
    }

    #[test]
    fn contextual_prefix_prediction_commits_in_both_schemas() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            for (context, code, expected) in [
                ("正", "q", "确"),
                ("输入", "f", "法"),
                ("人工", "z", "智能"),
                ("人工", "zn", "智能"),
                ("没", "g", "关系"),
                ("没", "gx", "关系"),
            ] {
                let (mut e, clip, imp, mut r) = fixture();
                e.schema = schema;
                // Load bundled resources before starting the six-second context window.
                type_code(&mut e, "q", &clip, &imp, &mut r);
                e.clear_composition_state();
                offer(&mut e, context, "test");
                e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
                type_code(&mut e, code, &clip, &imp, &mut r);
                assert_eq!(
                    e.canonical_at(0),
                    Some(expected),
                    "{schema:?}: {context} + {code}"
                );
                assert!(
                    matches!(&e.candidate_origins[0], CandidateOrigin::PureRust { consumed_len, .. } if *consumed_len == code.len())
                );
                let selected = e
                    .process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
                assert_eq!(selected.commit_text.as_deref(), Some(expected));
                assert_eq!(e.speech_context_before(), format!("{context}{expected}"));
                assert!(r.context_bigrams[context].contains_key(expected));
            }
        }
    }

    #[test]
    fn contextual_prefix_prediction_preserves_pins_and_stronger_personal_words() {
        let (mut e, clip, imp, mut r) = fixture();
        type_code(&mut e, "q", &clip, &imp, &mut r);
        e.clear_composition_state();
        offer(&mut e, "正", "zheng");
        e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        let now = RetentionEngine::now_epoch_ms();
        let six_half_lives_ms = (r.half_life_sec * 6_000.0) as u64;
        r.record_selection_at("去", now - six_half_lives_ms, 3.2);
        assert!((r.decayed_score_at("去", now) - 0.05).abs() < 1e-9);
        type_code(&mut e, "q", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some("确"));
        e.clear_composition_state();

        r.context_bigrams.insert(
            "正".into(),
            HashMap::from([("确".into(), 8.0), ("情绪".into(), 1.0)]),
        );
        r.pinned_words.insert("确".into());
        type_code(&mut e, "q", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some("确"));
        assert!(e.current_candidates[0].pinned);
        e.clear_composition_state();
        r.pinned_words.clear();
        r.record_selection_at("情绪", RetentionEngine::now_epoch_ms(), 100.0);
        type_code(&mut e, "q", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some("情绪"));
    }

    #[test]
    fn contextual_prefix_prediction_uses_accumulated_context_and_resets() {
        let (mut e, clip, imp, mut r) = fixture();
        type_code(&mut e, "q", &clip, &imp, &mut r);
        e.clear_composition_state();
        for word in ["输", "入"] {
            offer(&mut e, word, "test");
            e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap();
        }
        type_code(&mut e, "f", &clip, &imp, &mut r);
        assert_eq!(e.canonical_at(0), Some("法"));
        let (mut expired, _, imp, mut r) = fixture();
        let now = RetentionEngine::now_epoch_ms();
        expired.record_candidate_commit("人", "ren", None, &imp, &mut r, now.saturating_sub(7000));
        expired.record_candidate_commit("工", "gong", None, &imp, &mut r, now);
        assert_eq!(expired.speech_context_before(), "工");
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let (mut baseline, clip, imp, mut r) = fixture();
            baseline.schema = schema;
            type_code(&mut baseline, "q", &clip, &imp, &mut r);
            let expected = baseline.canonical_at(0).unwrap().to_string();
            assert_ne!(expected, "确");
            for reset_key in [0x0d, '.' as u32, '。' as u32, 0x1b] {
                let (mut e, clip, imp, mut r) = fixture();
                e.schema = schema;
                offer(&mut e, "正", "zheng");
                e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
                e.process_key(reset_key, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
                assert!(e.context_window.is_empty());
                type_code(&mut e, "q", &clip, &imp, &mut r);
                assert_eq!(
                    e.canonical_at(0),
                    Some(expected.as_str()),
                    "{schema:?}: {reset_key}"
                );
                e.clear_composition_state();
                offer(&mut e, "正", "zheng");
                e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
                type_code(&mut e, "q", &clip, &imp, &mut r);
                assert_eq!(e.canonical_at(0), Some("确"));
                e.process_key(reset_key, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap();
                assert!(
                    e.context_window.is_empty(),
                    "composing reset: {schema:?}: {reset_key}"
                );
            }
            let (mut e, clip, imp, mut r) = fixture();
            e.schema = schema;
            offer(&mut e, "正", "zheng");
            e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap();
            e.last_committed_ms = RetentionEngine::now_epoch_ms().saturating_sub(6001);
            type_code(&mut e, "q", &clip, &imp, &mut r);
            assert_eq!(e.canonical_at(0), Some(expected.as_str()));
        }
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
    fn single_character_continuations_compete_with_exact_homophones() {
        let (mut e, clip, imp, mut r) = fixture();
        let now = RetentionEngine::now_epoch_ms();
        for word in ["泥", "妮"] {
            r.record_contextual_selection(word, "ni", Some("看"), None, now, 8.0, true);
        }
        offer(&mut e, "看", "kan");
        e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        type_code(&mut e, "ni", &clip, &imp, &mut r);

        let candidates = &e.current_candidates;
        let last_regular = candidates
            .iter()
            .rposition(|c| c.text.chars().count() == 1 && c.comment == "ni")
            .expect("expected a regular exact homophone");
        for word in ["泥", "妮"] {
            let position = candidates
                .iter()
                .position(|c| c.text == word && c.comment == "续写")
                .unwrap_or_else(|| panic!("missing continuation {word}: {candidates:?}"));
            assert!(position < last_regular, "{word}: {candidates:?}");
        }
    }

    #[test]
    fn xiaohe_fuzzy_pinned_prefix_continuation_follows_exact_single_char() {
        let (mut e, clip, imp, mut r) = fixture();
        e.config.platform = "android".into();
        e.set_schema(SchemaKind::DoublePinyinFlypy);
        e.config.fuzzy_pinyin = true;
        e.pure_rust.fuzzy_pinyin = true;
        e.pure_rust.fuzzy_pairs = quanpin::FuzzyPairs::from_names(&["sh_s".into()]).unwrap();
        e.pure_rust.typo_correction_level = TypoCorrectionLevel::Off;
        let now = RetentionEngine::now_epoch_ms();
        r.record_contextual_selection("你", "ni", Some("看"), None, now, 8.0, true);
        r.record_contextual_selection("你好", "nihc", Some("看"), None, now, 1.0, true);
        r.pin_word("你好");
        offer(&mut e, "看", "kj");
        e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        e.set_raw_input("ni".into());
        e.raw_cursor = 2;
        let action = e.process_key_pure_rust(0, &clip, &imp, &mut r).unwrap();
        let exact_pos = action
            .candidates
            .iter()
            .position(|c| c.text == "你")
            .expect("missing exact single char");
        let regular_exact = action
            .candidates
            .iter()
            .rposition(|c| c.text.chars().count() == 1 && c.comment == "ni")
            .expect("missing regular exact single char");
        let prefix_pos = action
            .candidates
            .iter()
            .position(|c| c.text == "你好" && c.comment == "续写");
        assert_eq!(exact_pos, 0, "{:?}", action.candidates);
        if let Some(prefix_pos) = prefix_pos {
            assert!(prefix_pos > regular_exact, "{:?}", action.candidates);
        }
    }

    #[test]
    fn correction_levels_keep_exact_words_and_offer_distinct_alternatives() {
        let (mut e, clip, imp, mut ret) = fixture();
        for (level, raw, exact, expected) in [
            (TypoCorrectionLevel::Off, "nuhao", "怒号", false),
            (TypoCorrectionLevel::Conservative, "nuhao", "怒号", false),
            (TypoCorrectionLevel::Standard, "nuhao", "怒号", true),
            (TypoCorrectionLevel::Standard, "nigao", "你搞", false),
            (TypoCorrectionLevel::Aggressive, "nigao", "你搞", true),
        ] {
            e.clear_composition_state();
            e.pure_rust.typo_correction_level = level;
            e.set_raw_input(raw.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut ret).unwrap();
            let corrections = action.candidates.iter().filter(|item| item.comment == "纠错");
            assert_eq!(action.candidates[0].text, exact, "{level:?}: {:?}", action.candidates);
            assert_eq!(corrections.clone().any(|item| item.text == "你好"), expected,
                "{level:?}: {:?}", action.candidates);
            assert!(corrections.count() <= 2);
        }
        ret.pin_word("怒号");
        e.set_raw_input("nuhao".into());
        assert!(e.recover_transposition(&imp, &ret).is_empty());

        e.pure_rust.typo_correction_level = TypoCorrectionLevel::Conservative;
        e.set_raw_input("nihoa".into());
        let alternatives = e.recover_transposition(&imp, &ret);
        assert_eq!(alternatives.len(), 2);
        assert_ne!(alternatives[0].0.text, alternatives[1].0.text);
    }

    #[test]
    fn short_quanpin_and_odd_xiaohe_codes_can_be_corrected() {
        for (schema, raw, corrected) in [
            (SchemaKind::RimeIce, "tna", "谈"),
            (SchemaKind::RimeIce, "nui", "牛"),
            (SchemaKind::DoublePinyinFlypy, "nhc", "你好"),
            (SchemaKind::DoublePinyinFlypy, "nixhc", "你好"),
        ] {
            let (mut e, clip, imp, mut ret) = fixture();
            e.schema = schema;
            e.set_raw_input(raw.into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut ret).unwrap();
            assert!(action.candidates.iter().any(|item| item.text == corrected && item.comment == "纠错"),
                "{raw}: {:?}", action.candidates);
            e.pure_rust.typo_correction_level = TypoCorrectionLevel::Off;
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut ret).unwrap();
            assert!(action.candidates.iter().all(|item| item.comment != "纠错"));
        }
    }

    #[test]
    fn bundled_abbreviations_stay_on_the_first_page_in_both_schemas() {
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            let (mut e, clip, imp, mut ret) = fixture();
            e.schema = schema;
            e.config.adaptive_learning = false;
            e.set_raw_input("zsh".into());
            let action = e.process_key_pure_rust(0, &clip, &imp, &mut ret).unwrap();
            assert!(action.candidates.iter().take(3).any(|item| item.text == "早上好"),
                "{schema:?}: {:?}; abbreviation scores: {:?}", action.candidates,
                quanpin::ranked_abbreviations("zsh", imp.corpus(), e.pure_rust.get_gram(), None));
        }
    }

    #[test]
    fn segmented_abbreviation_selection_immediately_learns_the_full_reading() {
        for (schema, code) in [
            (SchemaKind::RimeIce, "z's'h"),
            (SchemaKind::DoublePinyinFlypy, "z's'h"),
            (SchemaKind::RimeIce, "zsh"),
            (SchemaKind::DoublePinyinFlypy, "zsh"),
        ] {
            let (mut e, clip, imp, mut ret) = fixture();
            e.schema = schema;
            type_code(&mut e, code, &clip, &imp, &mut ret);
            for word in ["早", "上", "好"] {
                let selected = loop {
                    if let Some(index) = e.current_candidates.iter().position(|item| {
                        item.text == word && e.candidate_code_at(item.index).len() == 1
                    }) {
                        break Some(index);
                    }
                    if !e.current_page.has_next {
                        break None;
                    }
                    e.process_key_pure_rust('=' as u32, &clip, &imp, &mut ret).unwrap();
                };
                let index = selected.unwrap_or_else(|| panic!("{schema:?} {}: missing {word}; {:?}",
                    e.raw_input, e.current_candidates));
                let commit = e.select_candidate_on_backend(index, &clip, &imp, &mut ret).unwrap();
                assert_eq!(commit.as_deref(), Some(word));
            }
            let learned = &ret.user_phrases["早上好"];
            assert_eq!(learned.quanpin, "zao shang hao");
            assert_eq!(learned.xiaohe, "zcuhhc");
            assert_eq!(learned.acronym, "zsh");
            assert_eq!(ret.code_preferences["zsh"]["早上好"], 1.0);
            assert_eq!(ret.query_acronym_phrases("zsh")[0].word, "早上好");
            type_code(&mut e, "zsh", &clip, &imp, &mut ret);
            assert_eq!(e.canonical_at(0), Some("早上好"));
            assert_eq!(e.current_candidates[0].comment, "简拼");
        }
    }

    #[test]
    fn flypy_segmented_initials_digraph_recall_and_clipboard_shortcut_survive_correction() {
        // High #1: Flypy z's keeps initial selection on remaining 's' (shang's flypy is uh).
        let (mut e, mut clip, imp, mut ret) = fixture();
        e.schema = SchemaKind::DoublePinyinFlypy;
        type_code(&mut e, "z's", &clip, &imp, &mut ret);
        for word in ["早", "上"] {
            let index = loop {
                if let Some(i) = e.current_candidates.iter().position(|item| {
                    item.text == word && e.candidate_code_at(item.index).len() == 1
                }) {
                    break i;
                }
                assert!(e.current_page.has_next, "missing {word} in {:?}", e.current_candidates);
                e.process_key_pure_rust('=' as u32, &clip, &imp, &mut ret).unwrap();
            };
            assert_eq!(e.select_candidate_on_backend(index, &clip, &imp, &mut ret).unwrap().as_deref(), Some(word));
        }

        // Low #1: z'sh'h learned preference recalls on zshh as well as zsh.
        e.schema = SchemaKind::RimeIce;
        type_code(&mut e, "z'sh'h", &clip, &imp, &mut ret);
        for word in ["早", "上", "好"] {
            let index = loop {
                if let Some(i) = e.current_candidates.iter().position(|item| item.text == word) {
                    break i;
                }
                assert!(e.current_page.has_next, "missing {word} in {:?}", e.current_candidates);
                e.process_key_pure_rust('=' as u32, &clip, &imp, &mut ret).unwrap();
            };
            assert_eq!(e.select_candidate_on_backend(index, &clip, &imp, &mut ret).unwrap().as_deref(), Some(word));
        }
        assert!(ret.query_acronym_phrases("zshh").iter().any(|p| p.word == "早上好"));

        // High #2: clipboard shortcut matching a typo correction keeps its shortcut identity.
        let pushed = clip.push(crate::clip::ClipboardPayload {
            id: Some("clip-tan".into()),
            content: "谈".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 1000,
        });
        clip.pin(&pushed.ring[0].id, Some("tna")).unwrap();
        type_code(&mut e, "tna", &clip, &imp, &mut ret);
        let first = &e.current_candidates[0];
        assert_eq!(first.text, "谈");
        assert!(first.is_clipboard_shortcut);
        assert_eq!(first.shortcut_code.as_deref(), Some("tna"));

        // Low #2: aggressive typo correction is a superset of standard for low-weight exact words.
        let low_exact = quanpin::ExactWordMatch { weight: 100, personal: false };
        assert!(low_exact.supports(TypoCorrectionLevel::Standard, 1, 50));
        assert!(low_exact.supports(TypoCorrectionLevel::Aggressive, 1, 50));
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
        assert!(!e.current_candidates.iter().any(|c| c.text == "你好"));
        e.clear_composition_state();
        let mut fresh = RetentionEngine::default();
        type_code(&mut e, "nihoa", &clip, &imp, &mut fresh);
        assert!(e.current_candidates.iter().any(|c| c.text == "你好"));
        e.clear_composition_state();
        e.schema = SchemaKind::DoublePinyinFlypy;
        e.set_raw_input("nich".into());
        let recovered = e.recover_transposition(&imp, &fresh).remove(0);
        assert_eq!(recovered.1.canonical(), "你好");
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_page_zero_promotes_contextual_prefix_with_portable_origin() {
        for (schema, mode, display) in [
            (SchemaKind::RimeIce, OpenCcMode::Simplified, "确"),
            (SchemaKind::RimeIce, OpenCcMode::Traditional, "確"),
            (SchemaKind::DoublePinyinFlypy, OpenCcMode::Simplified, "确"),
        ] {
            let (mut e, clip, imp, mut r) = fixture();
            e.schema = schema;
            e.set_opencc_mode(mode);
            e.config.fuzzy_pinyin = true;
            e.pure_rust.fuzzy_pinyin = true;
            type_code(&mut e, "q", &clip, &imp, &mut r);
            e.clear_composition_state();
            e.record_candidate_commit(
                "正",
                "zheng",
                None,
                &imp,
                &mut r,
                RetentionEngine::now_epoch_ms(),
            );
            let now = RetentionEngine::now_epoch_ms();
            let six_half_lives_ms = (r.half_life_sec * 6_000.0) as u64;
            r.record_selection_at("去", now - six_half_lives_ms, 3.2);
            e.set_raw_input("q".into());
            let snapshot = rime_ffi::RimeSnapshot {
                commit_text: None,
                preedit: "q".into(),
                cursor_utf8: 1,
                raw_input: "q".into(),
                raw_cursor: 1,
                page_index: 0,
                page_size: 5,
                has_prev: false,
                has_next: false,
                candidates: vec![rime_ffi::RimeRawCandidate {
                    text: "去".into(),
                    comment: String::new(),
                }],
            };
            e.pure_rust.decode_calls.set(0);
            let action = e.apply_rime_snapshot(snapshot, false, false, &clip, &imp, &r);
            assert_eq!(e.pure_rust.decode_calls.get(), 1);
            assert_eq!(action.candidates[0].text, display);
            assert!(
                matches!(&e.candidate_origins[0], CandidateOrigin::PureRust { consumed_len: 1, canonical, .. } if canonical == "确")
            );
            let commit = e
                .process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap();
            assert_eq!(commit.commit_text.as_deref(), Some(display));
            assert_eq!(e.speech_context_before(), "正确");
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_page_zero_recovers_transposition_with_portable_commit_origin() {
        let (mut e, clip, imp, r) = fixture();
        // Synthetic native snapshot exercises merging without creating a Rime session.
        e.set_raw_input("nihoa".into());
        let snapshot = rime_ffi::RimeSnapshot {
            commit_text: None,
            preedit: "nihoa".into(),
            cursor_utf8: 5,
            raw_input: "nihoa".into(),
            raw_cursor: 5,
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
        e.set_raw_input("nihoa".into());
        assert!(!e.recover_transposition(&imp, &r).is_empty());
        let measure = |query: &mut dyn FnMut()| {
            let start = std::time::Instant::now();
            for _ in 0..100 {
                query();
            }
            start.elapsed() / 100
        };
        let typo = measure(&mut || {
            std::hint::black_box(e.recover_transposition(&imp, &r));
        });
        e.set_raw_input("d".into());
        let continuation = measure(&mut || {
            std::hint::black_box(e.build_overlay_candidates(&clip, &imp, &r, false));
        });
        e.set_raw_input("xmgl".into());
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
    fn chinese_punctuation_commits_composition_and_learns_selection() {
        let punctuation = [
            (',', "，"),
            ('.', "。"),
            ('?', "？"),
            ('!', "！"),
            (';', "；"),
            (':', "："),
            ('\\', "、"),
            ('，', "，"),
            ('。', "。"),
            ('？', "？"),
            ('！', "！"),
            ('；', "；"),
            ('：', "："),
            ('、', "、"),
        ];
        for (ch, expected) in punctuation {
            let (mut e, clip, imp, mut r) = fixture();
            assert_eq!(
                e.process_key_pure_rust(ch as u32, &clip, &imp, &mut r)
                    .unwrap()
                    .commit_text
                    .as_deref(),
                Some(expected),
            );
            type_code(&mut e, "nihao", &clip, &imp, &mut r);
            assert_eq!(
                e.process_key_pure_rust(ch as u32, &clip, &imp, &mut r)
                    .unwrap()
                    .commit_text,
                Some(format!("你好{expected}")),
            );
            assert!(r.decayed_score_at("你好", RetentionEngine::now_epoch_ms()) > 0.0);
            assert!(e.raw_input.is_empty() && e.composing.is_empty());
            assert!(e.current_candidates.is_empty() && e.candidate_origins.is_empty());
            assert_eq!(e.last_committed_char, expected.chars().last());
        }
        for ch in [',', '，'] {
            let (mut e, clip, imp, mut r) = fixture();
            type_code(&mut e, "nihao", &clip, &imp, &mut r);
            assert_eq!(
                e.process_key(ch as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
                    .unwrap()
                    .commit_text
                    .as_deref(),
                Some("你好，"),
            );
            assert!(r.decayed_score_at("你好", RetentionEngine::now_epoch_ms()) > 0.0);
            assert!(e.raw_input.is_empty() && e.current_candidates.is_empty());
        }
        let (mut e, clip, imp, mut r) = fixture();
        offer(&mut e, "我", "woyao");
        if let CandidateOrigin::PureRust { consumed_len, .. } = &mut e.candidate_origins[0] {
            *consumed_len = 2;
        }
        assert_eq!(
            e.process_key(',' as u32, 0, SwipeDir::None, &clip, &imp, &mut r)
                .unwrap()
                .commit_text
                .as_deref(),
            Some("我，"),
        );
        assert!(
            e.raw_input.is_empty() && e.composing.is_empty() && e.current_candidates.is_empty()
        );
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
    fn app_rules_preserve_language_memory_and_control_paired_punctuation() {
        let (mut e, clip, imp, mut r) = fixture();
        let terminal = r"C:\Windows\System32\PowerShell.EXE";
        assert_eq!(e.on_app_focus(terminal).unwrap().lang_mode, LangMode::En);
        e.toggle_lang_mode();
        let remembered = e.on_app_focus(terminal).unwrap();
        assert!(remembered.remembered);
        assert_eq!(remembered.lang_mode, LangMode::Zh);
        let mut key = |e: &mut EngineState, ch| {
            e.process_key(ch as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap()
        };
        let single = key(&mut e, '(');
        assert_eq!(single.commit_text.as_deref(), Some("("));
        assert!(single.paired_punctuation.is_none());
        assert_eq!(single.cursor_delta, 0);
        e.config.disable_paired_punct_in_ide = false;
        assert_eq!(key(&mut e, '(').commit_text.as_deref(), Some("()"));

        e.config.auto_english_for_dev_apps = false;
        e.config.default_english_apps = vec!["My-Editor".into()];
        e.config.disable_paired_punct_apps = vec!["dev.scratch".into()];
        assert_eq!(e.on_app_focus("kitty").unwrap().lang_mode, LangMode::Zh);
        assert_eq!(e.on_app_focus("/usr/bin/my-editor-preview").unwrap().lang_mode, LangMode::En);
        assert_eq!(e.on_app_focus("Dev.Scratch.Preview").unwrap().lang_mode, LangMode::Zh);
        assert_eq!(key(&mut e, '[').commit_text.as_deref(), Some("["));
        e.config.disable_paired_punct_in_ide = true;
        e.on_app_focus("/opt/idea/bin/idea64.exe").unwrap();
        assert_eq!(key(&mut e, '{').commit_text.as_deref(), Some("{"));
    }

    #[test]
    fn quote_style_handles_pairs_single_fallback_and_pinyin_separator() {
        let (mut e, clip, imp, mut r) = fixture();
        let mut key = |e: &mut EngineState, ch| {
            e.process_key(ch as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap()
        };
        assert_eq!(key(&mut e, '"').commit_text.as_deref(), Some("“”"));
        assert_eq!(key(&mut e, '\'').commit_text.as_deref(), Some("‘’"));
        e.config = EngineConfig::parse_json(r#"{"quote_style":"corner"}"#).unwrap();
        for (ch, pair) in [('"', "「」"), ('\'', "『』")] {
            let action = key(&mut e, ch);
            assert_eq!(action.commit_text.as_deref(), Some(pair));
            assert_eq!(action.paired_punctuation.as_deref(), Some(pair));
            assert_eq!(action.cursor_delta, -1);
        }
        key(&mut e, 'x');
        assert!(key(&mut e, '\'').commit_text.is_none());
        assert!(e.raw_input.ends_with('\''));
        e.clear_composition_state();
        e.on_app_focus("pycharm").unwrap();
        assert_eq!(key(&mut e, '"').commit_text.as_deref(), Some("「"));
        assert!(EngineConfig::parse_json(r#"{"quote_style":"straight"}"#).is_err());
    }

    #[test]
    fn micro_space_applies_to_mixed_language_commits_and_skips_passthrough() {
        let (mut e, clip, imp, mut r) = fixture();
        let mut key = |e: &mut EngineState, ch| {
            e.process_key(ch as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap()
        };
        assert_eq!(key(&mut e, '7').commit_text.as_deref(), Some("7"));
        offer(&mut e, "中文", "zhongwen");
        assert_eq!(key(&mut e, ' ').commit_text.as_deref(), Some("中文"));
        e.config.auto_micro_space = true;
        offer(&mut e, "OpenAI", "openai");
        assert_eq!(key(&mut e, ' ').commit_text.as_deref(), Some(" OpenAI"));
        offer(&mut e, "中文", "zhongwen");
        assert_eq!(key(&mut e, ' ').commit_text.as_deref(), Some(" 中文"));
        for ch in "Rust".chars() {
            key(&mut e, ch);
        }
        assert_eq!(key(&mut e, ' ').commit_text.as_deref(), Some("Rust"));
        offer(&mut e, "中文", "zhongwen");
        assert_eq!(key(&mut e, ' ').commit_text.as_deref(), Some(" 中文"));
        e.toggle_lang_mode();
        assert_eq!(key(&mut e, 'a').commit_text.as_deref(), Some("a"));
    }

    #[test]
    fn deleting_a_committed_digit_clears_micro_spacing_context() {
        let (mut e, clip, imp, mut r) = fixture();
        e.config.auto_micro_space = true;
        for host_feedback in [false, true] {
            e.process_key('7' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            if host_feedback {
                e.notify_deleted_chars(1);
            } else {
                e.process_key(0x08, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            }
            offer(&mut e, "中文", "zhongwen");
            let action = e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(action.commit_text.as_deref(), Some("中文"));
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
            (SchemaKind::RimeIce, p.quanpin.replace(' ', "")),
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
    fn default_config_does_not_restore_committed_candidate() {
        assert!(!EngineConfig::default().undo_commit_on_backspace);
        let config: EngineConfig = serde_json::from_str("{}").unwrap();
        assert!(!config.undo_commit_on_backspace);
        let (mut e, clip, imp, mut r) = fixture();
        offer(&mut e, "你好", "nihao");
        e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
        let action = e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
        assert_eq!(action.undo_commit_chars, 0);
        assert!(action.undo_commit_text.is_none());
        assert!(action.preedit.text.is_empty());
    }

    #[test]
    fn undo_candidate_commit_restores_quanpin_and_xiaohe_once() {
        for (schema, code) in [
            (SchemaKind::RimeIce, "nihao"),
            (SchemaKind::DoublePinyinFlypy, "nihc"),
        ] {
            let (mut e, clip, imp, mut r) = fixture();
            e.config.undo_commit_on_backspace = true;
            e.set_schema(schema);
            e.config.auto_micro_space = true;
            e.last_committed_char = Some('1');
            offer(&mut e, "你好", code);
            let commit = e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
            let original = commit.commit_text.unwrap();
            assert_eq!(original, " 你好");

            let undo = e.process_key(0xff08, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(undo.undo_commit_text.as_deref(), Some(original.as_str()));
            assert_eq!(undo.undo_commit_chars, 3);
            assert_eq!(undo.preedit.text, code);
            assert_eq!(undo.preedit.cursor_utf8, code.len());
            assert!(!undo.candidates.is_empty());
            assert!(e.last_committed_word.is_none());
            assert!(e.last_committed_code.is_none());
            assert!(r.code_preferences[code].contains_key("你好"));

            let edit = e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(edit.preedit.text, &code[..code.len() - 1]);
            assert!(edit.undo_commit_text.is_none());
            assert_eq!(edit.undo_commit_chars, 0);
        }
    }

    #[test]
    fn undo_regret_is_saved_only_after_a_different_selection_or_raw_commit() {
        for (key, next_word, penalized) in [
            (0x1b, None, false),
            (0x20, Some("你好"), false),
            (0x20, Some("拟好"), true),
            (0x0d, None, true),
        ] {
            let (mut e, clip, imp, mut r) = fixture();
            e.config.undo_commit_on_backspace = true;
            e.set_schema(SchemaKind::RimeIce);
            offer(&mut e, "前", "qian");
            e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
            offer(&mut e, "你好", "nihao");
            e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
            let before = serde_json::to_value(&r).unwrap();
            let session_cache = r.session_cache.clone();
            let undo = e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(undo.undo_commit_text.as_deref(), Some("你好"));
            assert_eq!(serde_json::to_value(&r).unwrap(), before);
            assert_eq!(r.session_cache, session_cache);

            if let Some(word) = next_word {
                offer(&mut e, word, "nihao");
            }
            let action = e.process_key(key, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert!(e.pending_undo_regret.is_none());
            assert!(action.preedit.text.is_empty());
            if penalized {
                assert!(r.frecency["你好"] <= 0.5);
                assert!(!r.code_preferences["nihao"].contains_key("你好"));
                assert_eq!(r.context_bigrams["前"]["你好"], 0.5);
            } else if key == 0x1b {
                assert_eq!(serde_json::to_value(&r).unwrap(), before);
                assert_eq!(r.session_cache, session_cache);
            } else {
                assert!(r.frecency["你好"] > 1.0);
                assert!(r.code_preferences["nihao"].contains_key("你好"));
            }
        }
    }

    #[test]
    fn idle_space_and_host_deletion_clear_micro_space_boundary() {
        for deleted_by_host in [false, true] {
            let (mut e, clip, imp, mut r) = fixture();
            e.config.auto_micro_space = true;
            assert_eq!(e.empty_action(Some("A".into())).commit_text.as_deref(), Some("A"));
            if deleted_by_host {
                e.notify_deleted_chars(1);
            } else {
                let space = e.process_key(32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
                assert_eq!(space.commit_text.as_deref(), Some(" "));
            }
            assert_eq!(e.last_committed_char, None);
            offer(&mut e, "你好", "nihc");
            let commit = e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
            assert_eq!(commit.commit_text.as_deref(), Some("你好"));
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn privacy_focus_clears_native_composition_and_resumes_rime_on_exit() {
        let (mut e, clip, imp, mut retention) = fixture();
        e.resolved_assets_dir = resolve_assets_dir("");
        e.set_schema(SchemaKind::RimeIce);
        e.on_app_focus("editor").unwrap();
        type_code(&mut e, "ni", &clip, &imp, &mut retention);
        assert!(e.rime_input_start.is_some());

        for app in ["org.keepassxc.KeePassXC", "editor"] {
            let before = retention.crdt_seq;
            e.on_app_focus_with_learning_flag(app, app == "editor").unwrap();
            assert!(e.is_active_privacy_app());
            assert!(e.rime_input_start.is_none());
            assert!(e.raw_input.is_empty());
            assert!(e.current_candidates.is_empty());

            type_code(&mut e, "nihao", &clip, &imp, &mut retention);
            assert!(e.rime_input_start.is_none());
            assert!(e.candidate_origins.iter().all(|origin| !matches!(origin, CandidateOrigin::Rime { .. })));
            let index = e.current_candidates.iter().position(|c| c.text == "你好").unwrap();
            assert_eq!(
                e.select_candidate_action(index, &clip, &imp, &mut retention).unwrap().commit_text.as_deref(),
                Some("你好")
            );
            assert_eq!(retention.crdt_seq, before);
            assert!(e.speech_context_before().is_empty());
            type_code(&mut e, "ni", &clip, &imp, &mut retention);
            e.on_app_focus("editor").unwrap();
            assert!(e.raw_input.is_empty());
            type_code(&mut e, "ni", &clip, &imp, &mut retention);
            assert!(e.rime_input_start.is_some());
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn undo_candidate_commit_replays_native_rime_input() {
        let (mut e, clip, imp, mut r) = fixture();
        e.config.undo_commit_on_backspace = true;
        e.resolved_assets_dir = resolve_assets_dir("");
        for (schema, code) in [
            (SchemaKind::RimeIce, "nihao"),
            (SchemaKind::DoublePinyinFlypy, "nihc"),
        ] {
            e.set_schema(schema);
            type_code(&mut e, code, &clip, &imp, &mut r);
            assert!(e.rime_input_start.is_some());
            let index = e.current_candidates.iter().position(|c| c.text == "你好").unwrap();
            let commit = e.select_candidate_action(index, &clip, &imp, &mut r).unwrap();
            assert_eq!(commit.commit_text.as_deref(), Some("你好"));
            let undo = e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(undo.undo_commit_text.as_deref(), Some("你好"));
            assert_eq!(e.raw_input, code);
            assert_eq!(e.raw_cursor, code.len());
            assert!(e.rime_input_start.is_some());
            assert!(!undo.candidates.is_empty());
        }
    }

    #[test]
    fn undo_candidate_commit_excludes_clipboard_and_punctuation() {
        let (mut e, mut clip, imp, mut r) = fixture();
        e.config.undo_commit_on_backspace = true;
        let state = clip.push(crate::clip::ClipboardPayload {
            id: None,
            content: "剪贴板".into(),
            mime_type: "text/plain".into(),
            source_device: "test".into(),
            timestamp_ms: 1,
        });
        clip.pin(&state.ring[0].id, Some("ksb")).unwrap();

        offer(&mut e, "你好", "nihao");
        e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
        type_code(&mut e, "ksb", &clip, &imp, &mut r);
        let shortcut = e.current_candidates.iter().position(|c| c.is_clipboard_shortcut).unwrap();
        let paste = e.select_candidate_action(shortcut, &clip, &imp, &mut r).unwrap();
        assert_eq!(paste.commit_text.as_deref(), Some("剪贴板"));
        assert_eq!(e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap().undo_commit_chars, 0);

        offer(&mut e, "你好", "nihao");
        e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
        e.process_key(',' as u32, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
        assert_eq!(e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap().undo_commit_chars, 0);

        offer(&mut e, "你好", "nihao");
        e.select_candidate_action(0, &clip, &imp, &mut r).unwrap();
        e.last_committed_ms = RetentionEngine::now_epoch_ms().saturating_sub(5001);
        assert_eq!(e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap().undo_commit_chars, 0);
    }

    #[test]
    fn component_lookup_accepts_only_the_schema_prefix_and_skips_learning() {
        for (schema, prefix) in [
            (SchemaKind::RimeIce, "u"),
            (SchemaKind::DoublePinyinFlypy, "u'"),
        ] {
            let (mut e, clip, imp, mut r) = fixture();
            e.set_schema(schema);
            e.resolved_assets_dir = resolve_assets_dir("");
            if schema == SchemaKind::DoublePinyinFlypy {
                for code in ["u", "ui", "uo", "un", "ua", "ue", "uu"] {
                    type_code(&mut e, code, &clip, &imp, &mut r);
                    assert!(e.component_code().is_none(), "{code}");
                    assert!(!e.current_candidates.is_empty(), "{code}");
                    e.clear_composition_state();
                }
                for (code, word) in [("uuru", "输入"), ("uujv", "数据"), ("uufa", "书法")] {
                    type_code(&mut e, code, &clip, &imp, &mut r);
                    assert!(e.component_code().is_none(), "{code}");
                    assert!(e.current_candidates.iter().any(|c| c.text == word), "{code}: {:?}", e.current_candidates);
                    e.clear_composition_state();
                }
            }
            for (code, word, comment) in [
                ("riyue", "明", "míng · mk"),
                ("huohuo", "炎", "yán · yj"),
                ("huohuohuo", "焱", "yàn · yj"),
                ("shuishuishui", "淼", "miǎo · mn"),
            ] {
                let raw = format!("{prefix}{code}");
                type_code(&mut e, &raw, &clip, &imp, &mut r);
                let candidate = e.current_candidates.iter().find(|c| c.text == word)
                    .unwrap_or_else(|| panic!("{raw}: {:?}", e.current_candidates));
                assert_eq!(candidate.comment, comment);
                let seq = r.crdt_seq;
                let commit = e.select_candidate_action(candidate.index, &clip, &imp, &mut r).unwrap();
                assert_eq!(commit.commit_text.as_deref(), Some(word));
                assert_eq!(r.crdt_seq, seq);
                assert!(!r.frecency.contains_key(word));
                assert!(e.undo_commit.is_none());
            }
            let raw = format!("{prefix}riyue");
            type_code(&mut e, &raw, &clip, &imp, &mut r);
            let edited = e.process_key(8, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(edited.preedit.text, &raw[..raw.len() - 1]);
            let result = e.process_key(13, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
            assert_eq!(result.commit_text.as_deref(), Some(edited.preedit.text.as_str()));
        }
    }

    #[test]
    fn quanpin_component_prefix_also_recalls_english_and_mixed_words() {
        let (mut e, clip, imp, mut r) = fixture();
        let assets = default_assets_dir_candidates().unwrap();
        e.pure_rust.reload_assets(Some(&assets));
        e.resolved_assets_dir = Some(assets);
        for (code, word, comment) in [
            ("upan", "U盘", "中英"),
            ("user", "user", "英文"),
            ("update", "update", "英文"),
            ("upda", "update", "英文"),
            ("url", "URL", "英文"),
        ] {
            type_code(&mut e, code, &clip, &imp, &mut r);
            assert!(e.current_candidates.iter().any(|c| c.text == word && c.comment == comment),
                "{code}: {:?}", e.current_candidates);
            e.clear_composition_state();
        }

        e.set_opencc_mode(OpenCcMode::Traditional);
        r.pin_word("U盤");
        type_code(&mut e, "upan", &clip, &imp, &mut r);
        let mixed = &e.current_candidates[0];
        assert_eq!(mixed.text, "U盤");
        assert!(mixed.pinned);
        e.erase_candidate_in_state(mixed.index, &mut r).unwrap();
        e.clear_composition_state();
        type_code(&mut e, "upan", &clip, &imp, &mut r);
        assert!(!e.current_candidates.iter().any(|c| c.text == "U盤"));

        e.clear_composition_state();
        r.record_english_casing("URiyue");
        r.pin_word("URiyue");
        type_code(&mut e, "uriyue", &clip, &imp, &mut r);
        let first = &e.current_candidates[0];
        assert_eq!(first.text, "明");
        assert!(matches!(e.candidate_origins[0], CandidateOrigin::Components { .. }));
        while !e.current_candidates.iter().any(|c| c.text == "URiyue" && c.pinned) {
            assert!(e.current_page.has_next, "missing pinned URiyue");
            e.process_key(0xFF56, 0, SwipeDir::None, &clip, &imp, &mut r).unwrap();
        }
    }

    #[test]
    fn a_rare_homophone_needs_repeated_selection_or_a_pin_to_overtake() {
        let (mut e, clip, imp, mut r) = fixture();
        e.config.platform = "android".into();
        e.pure_rust.typo_correction_level = TypoCorrectionLevel::Off;
        let now = RetentionEngine::now_epoch_ms();
        let candidate_rank = |e: &mut EngineState, r: &mut RetentionEngine| {
            e.clear_composition_state();
            e.set_raw_input("ni".into());
            e.raw_cursor = 2;
            let action = e.process_key_pure_rust(0, &clip, &imp, r).unwrap();
            action.candidates
        };
        let rank = |candidates: &[CandidateItem], word| {
            candidates.iter().position(|c| c.text == word).unwrap()
        };
        let initial = candidate_rank(&mut e, &mut r);
        let common = rank(&initial, "你");
        let rare = rank(&initial, "妮");
        assert!(common < rare);
        e.select_candidate_action(rare, &clip, &imp, &mut r).unwrap();
        let once = candidate_rank(&mut e, &mut r);
        assert!(rank(&once, "你") < rank(&once, "妮"), "{once:?}");

        r.pin_word("妮");
        assert_eq!(candidate_rank(&mut e, &mut r)[0].text, "妮");
        r.pinned_words.remove("妮");
        for _ in 0..3 {
            r.record_contextual_selection("妮", "ni", None, None, now, 1.0, true);
        }
        assert_eq!(candidate_rank(&mut e, &mut r)[0].text, "妮");
    }

    #[test]
    fn backspace_regret_corrects_original_context_and_preference() {
        let (mut e, clip, imp, mut r) = fixture();
        e.config.undo_commit_on_backspace = false;
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
    fn portable_mixed_language_order_survives_candidate_merging() {
        let (mut engine, clipboard, _, mut retention) = fixture();
        let assets = default_assets_dir_candidates().unwrap();
        engine.pure_rust.reload_assets(Some(&assets));
        let imported = ImportedLexiconIndex::new(Some(&assets));
        let query = |engine: &mut EngineState, retention: &mut RetentionEngine, code: &str| {
            engine.clear_composition_state();
            type_code(engine, code, &clipboard, &imported, retention);
            engine.current_candidates.clone()
        };
        for code in ["rug", "can", "he", "you"] {
            let candidates = query(&mut engine, &mut retention, code);
            if code == "rug" {
                assert_eq!(candidates[0].text, "如果", "{candidates:?}");
            }
            let english_at = candidates.iter().position(|c| c.text == code && c.comment == "英文");
            assert_eq!(english_at, Some(3), "{code}: {candidates:?}");
            assert!(candidates[..2].iter().all(|c| c.text.chars().all(crate::ret::is_cjk)));
        }
        for (code, word) in [("api", "API"), ("gith", "GitHub"), ("pyth", "Python")] {
            let candidates = query(&mut engine, &mut retention, code);
            assert_eq!(candidates[0].text, word, "{code}: {candidates:?}");
            assert_eq!(candidates[0].comment, "英文");
        }
        let completions = query(&mut engine, &mut retention, "pyth");
        assert_eq!(completions[1].text, "pythonic");
        for (schema, code) in [(SchemaKind::RimeIce, "xguang"), (SchemaKind::DoublePinyinFlypy, "xgl")] {
            engine.schema = schema;
            let candidates = query(&mut engine, &mut retention, code);
            assert_eq!(candidates[0].text, "X光");
            assert_eq!(candidates[0].comment, "中英");
        }
        engine.schema = SchemaKind::RimeIce;
        query(&mut engine, &mut retention, "rug");
        engine.pin_candidate_in_state(3, &mut retention).unwrap();
        let pinned = query(&mut engine, &mut retention, "rug");
        assert_eq!(pinned[0].text, "rug");
        assert!(pinned[0].pinned);
        retention.pinned_words.remove("rug");

        query(&mut engine, &mut retention, "Rug");
        let committed = engine.process_key(13, 0, SwipeDir::None, &clipboard, &imported, &mut retention).unwrap();
        assert_eq!(committed.commit_text.as_deref(), Some("Rug"));
        let learned = query(&mut engine, &mut retention, "rug");
        assert_eq!(learned[0].text, "Rug");
        assert_eq!(learned[0].comment, "英文");
        retention.pin_word("如果");
        let learned = query(&mut engine, &mut retention, "rug");
        assert_eq!(learned[0].text, "如果");
        assert!(learned[0].pinned);
        assert_eq!(learned[1].text, "Rug");
        assert!(!learned[1].pinned);
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn native_english_candidate_respects_pinned_order() {
        let (mut engine, clipboard, _, mut retention) = fixture();
        let assets = default_assets_dir_candidates().unwrap();
        engine.pure_rust.reload_assets(Some(&assets));
        let imported = ImportedLexiconIndex::new(Some(&assets));
        let apply = |engine: &mut EngineState, retention: &RetentionEngine| {
            let snapshot = rime_ffi::RimeSnapshot {
                commit_text: None,
                preedit: "ai".into(),
                cursor_utf8: 2,
                raw_input: "ai".into(),
                raw_cursor: 2,
                page_index: 0,
                page_size: 5,
                has_prev: false,
                has_next: false,
                candidates: ["爱", "AI", "矮", "哎"]
                    .map(|text| rime_ffi::RimeRawCandidate {
                        text: text.into(),
                        comment: String::new(),
                    })
                    .to_vec(),
            };
            engine
                .apply_rime_snapshot(snapshot, false, false, &clipboard, &imported, retention)
                .candidates
        };

        retention.pin_word("AI");
        let pinned_ai = apply(&mut engine, &retention);
        assert_eq!(pinned_ai[0].text, "AI", "{pinned_ai:?}");
        assert!(pinned_ai[0].pinned);

        retention.pinned_words.clear();
        retention.pin_word("爱");
        retention.pin_word("矮");
        let pinned_zh = apply(&mut engine, &retention);
        assert_eq!(pinned_zh[0].text, "爱", "{pinned_zh:?}");
        assert_eq!(pinned_zh[1].text, "矮", "{pinned_zh:?}");
        assert!(pinned_zh[0].pinned && pinned_zh[1].pinned);
        let ai_pos = pinned_zh
            .iter()
            .position(|c| c.text == "AI" && c.comment == "英文")
            .expect("expected AI candidate");
        assert!(ai_pos >= 2, "{pinned_zh:?}");
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
    }

    #[test]
    fn public_set_raw_input_syncs_touch_hints_and_native_touch_uses_spatial_lattice() {
        let (mut e, clip, imp, mut r) = fixture();
        e.set_raw_input("ni".into());
        assert_eq!(e.raw_input(), "ni");
        // Delete at cursor 0 must never panic on synced raw_touch_hints.
        e.process_key(KEY_DELETE, 0, SwipeDir::None, &clip, &imp, &mut r)
            .unwrap();
        assert_eq!(e.raw_input(), "i");
        e.clear_composition_state();

        // Even with real bundled Rime assets present on Linux, non-empty TouchKeyHint routes to spatial lattice.
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let cfg = EngineConfig {
            schema: SchemaKind::RimeIce,
            assets_dir: assets.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let mut native_eng = EngineState::new(cfg);
        let native_imp = ImportedLexiconIndex::new(Some(&assets));
        let mut native_ret = RetentionEngine::default();
        for (key, alt) in [(b'b', Some(b'n')), (b'i', None), (b'h', None), (b's', Some(b'a')), (b'o', None)] {
            let hint = alt.map_or_else(TouchKeyHint::default, |ch| {
                TouchKeyHint::new(key, [Some(SpatialAlt { ch, penalty: 1.0 }), None])
            });
            native_eng
                .process_touch_key(u32::from(key), 0, SwipeDir::None, hint, &clip, &native_imp, &mut native_ret)
                .unwrap();
        }
        assert_eq!(
            native_eng.current_candidates.first().map(|c| c.text.as_str()),
            Some("你好"),
            "{:?}",
            native_eng.current_candidates
        );
    }

    #[test]
    fn phrase_tone_filter_disambiguates_same_syllable_polyphone_tones() {
        let (mut e, clip, imp, mut r) = fixture();
        e.set_schema(SchemaKind::RimeIce);
        type_code(&mut e, "haoke`4", &clip, &imp, &mut r);
        assert!(
            e.current_candidates.iter().any(|c| c.text == "好客"),
            "haoke`4 should keep 好客 (hào kè): {:?}",
            e.current_candidates
        );
        e.clear_composition_state();
        type_code(&mut e, "haoke`3", &clip, &imp, &mut r);
        assert!(
            !e.current_candidates.iter().any(|c| c.text == "好客"),
            "haoke`3 must exclude 好客 (hào kè): {:?}",
            e.current_candidates
        );
    }
}
