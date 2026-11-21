use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};
use vitral_tokens::TOKENS;

pub const DEFAULT_FRECENCY_HALF_LIFE_SEC: f64 = 604_800.0;

fn default_half_life_sec() -> f64 {
    DEFAULT_FRECENCY_HALF_LIFE_SEC
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionCrdtDelta {
    pub schema_version: u32,
    pub device_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub frecency: HashMap<String, f64>,
    pub frecency_updated_ms: HashMap<String, u64>,
    pub pinned_words: Vec<String>,
    pub erased_words: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionEngine {
    pub frecency: HashMap<String, f64>,
    #[serde(default)]
    pub frecency_updated_ms: HashMap<String, u64>,
    pub pinned_words: HashSet<String>,
    pub erased_words: HashSet<String>,
    #[serde(default = "default_half_life_sec")]
    pub half_life_sec: f64,
    #[serde(default)]
    pub crdt_seq: u64,
}

impl Default for RetentionEngine {
    fn default() -> Self {
        Self {
            frecency: HashMap::new(),
            frecency_updated_ms: HashMap::new(),
            pinned_words: HashSet::new(),
            erased_words: HashSet::new(),
            half_life_sec: DEFAULT_FRECENCY_HALF_LIFE_SEC,
            crdt_seq: 0,
        }
    }
}

impl RetentionEngine {
    pub fn with_half_life(half_life_sec: f64) -> Self {
        let mut engine = Self::default();
        engine.set_half_life_sec(half_life_sec);
        engine
    }

    pub fn set_half_life_sec(&mut self, half_life_sec: f64) {
        self.half_life_sec = if !half_life_sec.is_finite() || half_life_sec <= 0.0 {
            DEFAULT_FRECENCY_HALF_LIFE_SEC
        } else {
            half_life_sec.max(1.0)
        };
    }

    pub fn now_epoch_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[inline]
    pub fn lambda(&self) -> f64 {
        std::f64::consts::LN_2 / self.half_life_sec.max(1.0)
    }

    pub fn decayed_score_at(&self, word: &str, now_ms: u64) -> f64 {
        if self.erased_words.contains(word) {
            return 0.0;
        }
        let Some(&s0) = self.frecency.get(word) else {
            return 0.0;
        };
        let last_ms = self.frecency_updated_ms.get(word).copied().unwrap_or(now_ms);
        let dt_sec = now_ms.saturating_sub(last_ms) as f64 / 1000.0;
        s0 * (-self.lambda() * dt_sec).exp()
    }

    pub fn decayed_score(&self, word: &str) -> f64 {
        self.decayed_score_at(word, Self::now_epoch_ms())
    }

    pub fn record_selection_at(&mut self, word: &str, now_ms: u64, boost: f64) -> f64 {
        self.erased_words.remove(word);
        let decayed = self.decayed_score_at(word, now_ms);
        let updated = decayed + boost;
        self.frecency.insert(word.to_string(), updated);
        self.frecency_updated_ms.insert(word.to_string(), now_ms);
        self.crdt_seq = self.crdt_seq.saturating_add(1);
        updated
    }

    pub fn record_selection(
        &mut self,
        word: &str,
        delta_seconds: f64,
        lambda: f64,
        boost: f64,
    ) -> f64 {
        let now_ms = Self::now_epoch_ms();
        if delta_seconds > 0.0 {
            self.erased_words.remove(word);
            let prev = self.frecency.get(word).copied().unwrap_or(0.0);
            let eff_lambda = if lambda.is_finite() && lambda > 0.0 {
                lambda
            } else {
                self.lambda()
            };
            let decayed = prev * (-eff_lambda * delta_seconds).exp();
            let updated = decayed + boost;
            self.frecency.insert(word.to_string(), updated);
            self.frecency_updated_ms.insert(word.to_string(), now_ms);
            self.crdt_seq = self.crdt_seq.saturating_add(1);
            updated
        } else {
            self.record_selection_at(word, now_ms, boost)
        }
    }

    pub fn pin_word(&mut self, word: &str) {
        self.erased_words.remove(word);
        self.pinned_words.insert(word.to_string());
        self.crdt_seq = self.crdt_seq.saturating_add(1);
    }

    pub fn erase_word(&mut self, word: &str) {
        self.pinned_words.remove(word);
        self.frecency.remove(word);
        self.frecency_updated_ms.remove(word);
        self.erased_words.insert(word.to_string());
        self.crdt_seq = self.crdt_seq.saturating_add(1);
    }

    pub fn export_crdt_delta(&self, device_id: &str) -> RetentionCrdtDelta {
        RetentionCrdtDelta {
            schema_version: 1,
            device_id: device_id.to_string(),
            seq: self.crdt_seq,
            timestamp_ms: Self::now_epoch_ms(),
            frecency: self.frecency.clone(),
            frecency_updated_ms: self.frecency_updated_ms.clone(),
            pinned_words: self.pinned_words.iter().cloned().collect(),
            erased_words: self.erased_words.iter().cloned().collect(),
        }
    }

    pub fn merge_crdt_delta(&mut self, delta: &RetentionCrdtDelta) {
        for word in &delta.erased_words {
            self.erase_word(word);
        }
        for word in &delta.pinned_words {
            self.pin_word(word);
        }
        for (word, &remote_score) in &delta.frecency {
            if self.erased_words.contains(word) {
                continue;
            }
            let remote_ts = delta
                .frecency_updated_ms
                .get(word)
                .copied()
                .unwrap_or(delta.timestamp_ms);
            let local_ts = self.frecency_updated_ms.get(word).copied().unwrap_or(0);
            let local_score = self.frecency.get(word).copied().unwrap_or(0.0);
            if remote_ts > local_ts || (remote_ts == local_ts && remote_score > local_score) {
                self.frecency.insert(word.clone(), remote_score);
                self.frecency_updated_ms.insert(word.clone(), remote_ts);
            }
        }
        self.crdt_seq = self.crdt_seq.max(delta.seq);
    }
}

/// Expand dynamic date (`rq`) / time (`sj`) macros via local wall clock.
pub fn expand_time_macro(raw: &str) -> Option<(&'static str, Vec<(String, String)>)> {
    let lower = raw.trim().to_ascii_lowercase();
    let now = Local::now();
    match lower.as_str() {
        "rq" => Some((
            "date",
            vec![
                (now.format("%Y-%m-%d").to_string(), "日期".to_string()),
                (now.format("%Y年%m月%d日").to_string(), "日期".to_string()),
                (now.format("%Y/%m/%d").to_string(), "日期".to_string()),
            ],
        )),
        "sj" => Some((
            "time",
            vec![
                (now.format("%H:%M:%S").to_string(), "时间".to_string()),
                (now.format("%H:%M").to_string(), "时间".to_string()),
                (now.format("%H时%M分").to_string(), "时间".to_string()),
            ],
        )),
        _ => None,
    }
}

/// Match opening bracket/quote character to auto-closed pair.
pub fn match_paired_punctuation(ch: char) -> Option<&'static str> {
    match ch {
        '(' => Some("()"),
        '（' => Some("（）"),
        '[' => Some("[]"),
        '{' => Some("{}"),
        '"' => Some("\"\""),
        '\'' => Some("''"),
        '“' => Some("“”"),
        '‘' => Some("‘’"),
        '<' => Some("<>"),
        '「' => Some("「」"),
        '《' => Some("《》"),
        _ => None,
    }
}

/// Check if a spacebar hold duration meets the design token voice trigger threshold (350ms).
pub fn is_voice_hold_triggered(hold_ms: u32) -> bool {
    hold_ms >= TOKENS.ergonomics.spacebar_voice_hold_threshold_ms
}
