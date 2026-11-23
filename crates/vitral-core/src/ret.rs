use chrono::Local;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use vitral_tokens::TOKENS;

pub const DEFAULT_FRECENCY_HALF_LIFE_SEC: f64 = 604_800.0;

fn default_half_life_sec() -> f64 {
    DEFAULT_FRECENCY_HALF_LIFE_SEC
}

pub const DEFAULT_RERANK_WEIGHTS: [f64; 9] = [1.0, 1.0, 1.4, 0.85, 1.1, 1.2, 0.65, 0.4, 0.0];

fn default_rerank_weights() -> [f64; 9] {
    DEFAULT_RERANK_WEIGHTS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonalReranker {
    #[serde(default = "default_rerank_weights")]
    pub weights: [f64; 9],
    #[serde(default)]
    pub updates: u64,
    #[serde(default)]
    pub updated_ms: u64,
}

impl Default for PersonalReranker {
    fn default() -> Self {
        Self {
            weights: DEFAULT_RERANK_WEIGHTS,
            updates: 0,
            updated_ms: 0,
        }
    }
}

impl PersonalReranker {
    pub fn score(&self, feats: &[f64; 9]) -> f64 {
        self.weights.iter().zip(feats).map(|(w, f)| w * f).sum()
    }

    pub fn update_pairwise(&mut self, chosen: &[f64; 9], skipped: &[f64; 9], now_ms: u64) -> bool {
        let loss = 0.5 - (self.score(chosen) - self.score(skipped));
        if loss <= 0.0 {
            return false;
        }
        let diff: [f64; 9] = std::array::from_fn(|i| (chosen[i] - skipped[i]).clamp(-2.0, 2.0));
        let norm_sq: f64 = diff.iter().map(|d| d * d).sum();
        if norm_sq <= 1e-9 {
            return false;
        }
        let eta = (loss / (norm_sq + 1.0)).min(0.18);
        for (i, w) in self.weights.iter_mut().enumerate() {
            *w += eta * diff[i] - 0.01 * (*w - DEFAULT_RERANK_WEIGHTS[i]);
            *w = if i == 8 {
                w.clamp(0.0, 2.0)
            } else {
                w.clamp(0.15, 4.0)
            };
        }
        self.updates = self.updates.saturating_add(1);
        self.updated_ms = now_ms.max(self.updated_ms.saturating_add(1));
        true
    }

    pub fn reset(&mut self, now_ms: u64) {
        *self = Self {
            updated_ms: now_ms,
            ..Self::default()
        };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticMemoryEntry {
    pub vec_i8: Vec<i8>,
    pub word: String,
    #[serde(default)]
    pub app_id: String,
    pub count: u16,
    pub updated_ms: u64,
}

// SplitMix64 supplies reproducible signs without storing a projection matrix.
pub(crate) fn project_token(vector: &mut [f32; 32], token: u64, weight: f32) {
    for (d, value) in vector.iter_mut().enumerate() {
        let mut hash = token.wrapping_add((d as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15));
        hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
        hash ^= hash >> 31;
        *value += if hash & 1 == 0 { weight } else { -weight };
    }
}

pub(crate) fn quantize_context(vector: [f32; 32]) -> [i8; 32] {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return [0; 32];
    }
    vector.map(|v| (v / norm * 127.0).round().clamp(-127.0, 127.0) as i8)
}

pub fn project_context_fallback(context: &str) -> [i8; 32] {
    let mut chars: Vec<_> = context
        .chars()
        .rev()
        .filter(|c| is_cjk(*c))
        .take(16)
        .collect();
    chars.reverse();
    let mut vector = [0.0; 32];
    for &ch in &chars {
        project_token(&mut vector, ch as u64, 1.0);
    }
    for pair in chars.windows(2) {
        project_token(&mut vector, (pair[0] as u64) << 32 | pair[1] as u64, 1.5);
    }
    quantize_context(vector)
}

fn cosine(a: &[i8], b: &[i8]) -> f64 {
    let dot: i32 = a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum();
    let norm = |v: &[i8]| v.iter().map(|&x| (x as i32).pow(2)).sum::<i32>() as f64;
    let denominator = (norm(a) * norm(b)).sqrt();
    if denominator == 0.0 {
        0.0
    } else {
        dot as f64 / denominator
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPhrase {
    pub word: String,
    pub xiaohe: String,
    pub quanpin: String,
    #[serde(default)]
    pub acronym: String,
    pub count: u32,
    pub updated_ms: u64,
    #[serde(default)]
    pub source: String,
}

impl UserPhrase {
    pub(crate) fn derive_acronym(word: &str, quanpin: &str, xiaohe: &str) -> String {
        if !valid_phrase(word) {
            return String::new();
        }
        let syllables: Vec<_> = quanpin.split_whitespace().collect();
        if syllables.len() == word.chars().count() {
            syllables
                .iter()
                .filter_map(|s| s.chars().next())
                .collect::<String>()
                .to_ascii_lowercase()
        } else {
            xiaohe
                .chars()
                .filter(char::is_ascii_alphabetic)
                .step_by(2)
                .collect::<String>()
                .to_ascii_lowercase()
        }
    }

    pub(crate) fn spaced_quanpin(&self) -> Option<Cow<'_, str>> {
        let chars = self.word.chars().count();
        if self.quanpin.split_whitespace().count() == chars {
            return Some(Cow::Borrowed(&self.quanpin));
        }

        // Older records stored compact pinyin. Recover its boundaries using the
        // corresponding Xiaohe pairs, which have explicit two-keystroke boundaries.
        if self.xiaohe.len() != chars * 2 || !self.xiaohe.is_ascii() {
            return None;
        }
        let mut remaining = self.quanpin.as_str();
        let mut syllables = Vec::with_capacity(chars);
        let readings = crate::imp::xiaohe::xiaohe_reverse_map();
        for offset in (0..self.xiaohe.len()).step_by(2) {
            let mut choices = readings.get(&self.xiaohe[offset..offset + 2])?
                .iter()
                .filter(|&&syllable| remaining.starts_with(syllable));
            let &syllable = choices.next()?;
            if choices.next().is_some() {
                return None;
            }
            remaining = &remaining[syllable.len()..];
            syllables.push(syllable);
        }
        remaining.is_empty().then(|| Cow::Owned(syllables.join(" ")))
    }

    pub(crate) fn matches_code(&self, code: &str, is_flypy: bool) -> bool {
        if is_flypy {
            return self.xiaohe == code;
        }
        if !code.contains([' ', '\'']) {
            return self.quanpin.bytes().filter(|&b| b != b' ').eq(code.bytes());
        }
        let Some(reading) = self.spaced_quanpin() else {
            return false;
        };
        let mut remaining = code;
        for syllable in reading.split_whitespace() {
            let Some(tail) = remaining.strip_prefix(syllable) else {
                return false;
            };
            remaining = tail.trim_start_matches([' ', '\'']);
        }
        remaining.is_empty()
    }

    fn effective_acronym(&self) -> Cow<'_, str> {
        if self.acronym.is_empty() {
            Cow::Owned(Self::derive_acronym(&self.word, "", &self.xiaohe))
        } else {
            Cow::Borrowed(&self.acronym)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersonalizationStats {
    pub vocab_count: usize,
    pub bigram_count: usize,
    pub phrase_count: usize,
    pub app_count: usize,
    pub english_count: usize,
    pub pinned_count: usize,
    pub erased_count: usize,
    #[serde(default)]
    pub reranker_updates: u64,
    #[serde(default)]
    pub semantic_memory_count: usize,
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
    #[serde(default)]
    pub context_bigrams: HashMap<String, HashMap<String, f64>>,
    #[serde(default)]
    pub app_frecency: HashMap<String, HashMap<String, f64>>,
    #[serde(default)]
    pub code_preferences: HashMap<String, HashMap<String, f64>>,
    #[serde(default)]
    pub user_phrases: HashMap<String, UserPhrase>,
    #[serde(default)]
    pub user_english: HashMap<String, String>,
    // Per-bucket clocks preserve corrections, including removed code preferences.
    #[serde(default)]
    pub habit_updated_ms: HashMap<String, u64>,
    #[serde(default)]
    pub learned_reset_ms: u64,
    #[serde(default)]
    pub reranker: Option<PersonalReranker>,
    #[serde(default)]
    pub semantic_memories: Vec<SemanticMemoryEntry>,
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
    #[serde(default)]
    pub context_bigrams: HashMap<String, HashMap<String, f64>>,
    #[serde(default)]
    pub app_frecency: HashMap<String, HashMap<String, f64>>,
    #[serde(default)]
    pub code_preferences: HashMap<String, HashMap<String, f64>>,
    #[serde(default)]
    pub user_phrases: HashMap<String, UserPhrase>,
    #[serde(default)]
    pub user_english: HashMap<String, String>,
    // Per-bucket clocks preserve corrections, including removed code preferences.
    #[serde(default)]
    pub habit_updated_ms: HashMap<String, u64>,
    #[serde(default)]
    pub learned_reset_ms: u64,
    #[serde(default)]
    pub reranker: PersonalReranker,
    #[serde(default)]
    pub semantic_memories: Vec<SemanticMemoryEntry>,
    #[serde(skip, default)]
    pub pending_phrase_pairs: HashMap<String, u32>,
    #[serde(skip, default)]
    pub session_cache: VecDeque<(String, String, u64)>,
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
            context_bigrams: HashMap::new(),
            app_frecency: HashMap::new(),
            code_preferences: HashMap::new(),
            user_phrases: HashMap::new(),
            user_english: HashMap::new(),
            pending_phrase_pairs: HashMap::new(),
            habit_updated_ms: HashMap::new(),
            learned_reset_ms: 0,
            reranker: PersonalReranker::default(),
            semantic_memories: Vec::new(),
            session_cache: VecDeque::with_capacity(32),
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
        let last_ms = self
            .frecency_updated_ms
            .get(word)
            .copied()
            .unwrap_or(now_ms);
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
        let updated_ms = now_ms
            .max(
                self.frecency_updated_ms
                    .get(word)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1),
            )
            .max(self.learned_reset_ms);
        self.frecency_updated_ms
            .insert(word.to_string(), updated_ms);
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

    pub fn contextual_score_at(
        &self,
        word: &str,
        code: &str,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        now_ms: u64,
        adaptive: bool,
    ) -> f64 {
        let mut feats = self.extract_candidate_features(
            word,
            code,
            prev_context,
            app_id,
            now_ms,
            adaptive,
            None,
            None,
            None,
        );
        // Legacy callers only use the original five signals.
        feats[7] = 0.0;
        self.score_candidate_features(&feats, adaptive)
    }

    pub fn has_mismatched_single_char_code(&self, word: &str, code: &str) -> bool {
        if word.chars().count() != 1 || code.is_empty() || !self.frecency.contains_key(word) {
            return false;
        }
        let code = normalize(code);
        if self
            .code_preferences
            .get(&code)
            .is_some_and(|words| words.contains_key(word))
        {
            return false;
        }
        self.code_preferences
            .iter()
            .any(|(other, words)| other != &code && words.contains_key(word))
    }

    fn code_preference_at(&self, word: &str, code: &str, now_ms: u64) -> f64 {
        let Some(weight) = self.code_preferences.get(code).and_then(|words| words.get(word)) else {
            return 0.0;
        };
        let last_ms = self.habit_updated_ms
            .get(&format!("c:{code}"))
            .copied()
            .unwrap_or(now_ms);
        weight * (-self.lambda() * (now_ms.saturating_sub(last_ms) as f64 / 1000.0)).exp()
    }

    fn single_char_reading_limit(
        &self,
        word: &str,
        code: &str,
        alternate: &str,
        global: f64,
        now_ms: u64,
    ) -> Option<f64> {
        if word.chars().count() != 1 || code.is_empty() || global <= 0.0 {
            return None;
        }
        let mut matching = self.code_preference_at(word, code, now_ms);
        if !alternate.is_empty() && alternate != code {
            matching += self.code_preference_at(word, alternate, now_ms);
        }
        if matching > 0.0 {
            // When other readings account for most of the history, require repeated
            // choices in this reading before personal ranking overrides the dictionary.
            return (global > matching + 1.0).then(|| (matching - 1.0).max(0.0));
        }
        self.has_mismatched_single_char_code(word, code).then_some(0.0)
    }

    pub(crate) fn reading_frecency_at(
        &self,
        word: &str,
        syllable: &str,
        flypy: bool,
        now_ms: u64,
    ) -> f64 {
        let global = self.decayed_score_at(word, now_ms);
        if word.chars().count() != 1 || global <= 0.0 {
            return global;
        }
        let xiaohe = crate::imp::xiaohe::syllable_to_xiaohe(syllable);
        let (code, alternate) = if flypy {
            (xiaohe.as_str(), syllable)
        } else {
            (syllable, xiaohe.as_str())
        };
        self.single_char_reading_limit(word, code, alternate, global, now_ms)
            .unwrap_or(global)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn extract_candidate_features(
        &self,
        word: &str,
        code: &str,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        now_ms: u64,
        adaptive: bool,
        query_vec: Option<&[i8; 32]>,
        mlm_priors: Option<&HashMap<char, f32>>,
        base_rank: Option<usize>,
    ) -> [f64; 9] {
        let mut feats = [0.0; 9];
        if self.erased_words.contains(word) {
            return feats;
        }
        feats[0] = self.decayed_score_at(word, now_ms);
        if !adaptive {
            return feats;
        }
        let lookup = |map: &HashMap<String, HashMap<String, f64>>, prefix: &str, key: &str| {
            let weight = map
                .get(key)
                .and_then(|words| words.get(word))
                .copied()
                .unwrap_or(0.0);
            if weight == 0.0 {
                return 0.0;
            }
            let last_ms = self
                .habit_updated_ms
                .get(&format!("{prefix}{key}"))
                .copied()
                .unwrap_or(now_ms);
            weight * (-self.lambda() * (now_ms.saturating_sub(last_ms) as f64 / 1000.0)).exp()
        };
        let app_key = normalize(app_id.unwrap_or_default());
        let topic_half_life = self.half_life_sec.min(600.0);
        let resonance = self
            .session_cache
            .iter()
            .filter_map(|(app, cached, timestamp)| {
                let dt_ms = now_ms.saturating_sub(*timestamp);
                (app == &app_key && cached == word && dt_ms as f64 <= topic_half_life * 1500.0)
                    .then(|| {
                        1.8 * (-std::f64::consts::LN_2 * dt_ms as f64 / (topic_half_life * 1000.0))
                            .exp()
                    })
            })
            .sum::<f64>()
            .min(3.6);
        feats[1] = resonance;
        feats[2] = lookup(
            &self.context_bigrams,
            "b:",
            &normalize(prev_context.unwrap_or_default()),
        );
        feats[3] = lookup(&self.app_frecency, "a:", &app_key);
        let code = normalize(code);
        feats[4] = self.code_preference_at(word, &code, now_ms);
        feats[5] = query_vec.map_or(0.0, |v| self.query_knn_semantic(v, word, app_id, now_ms));
        feats[6] = word
            .chars()
            .next()
            .and_then(|ch| mlm_priors?.get(&ch))
            .map_or(0.0, |p| {
                ((1.0 + *p as f64 * 30.0).ln() / 3.5).clamp(0.0, 1.0)
            });
        feats[7] = f64::from(self.user_phrases.contains_key(word));
        feats[8] = base_rank.map_or(0.0, |rank| 1.0 / (1.0 + rank as f64));
        if let Some(local) = self.single_char_reading_limit(word, &code, "", feats[0], now_ms) {
            let ratio = local / feats[0];
            for i in [0, 1, 3, 5] {
                feats[i] *= ratio;
            }
            feats[4] = feats[4].min(local);
        }
        feats
    }

    pub fn score_candidate_features(&self, features: &[f64; 9], adaptive: bool) -> f64 {
        if adaptive {
            self.reranker.score(features)
        } else {
            features[0]
        }
    }

    pub fn update_reranker_pairwise(
        &mut self,
        chosen: &[f64; 9],
        skipped: &[f64; 9],
        now_ms: u64,
    ) -> bool {
        if self
            .reranker
            .update_pairwise(chosen, skipped, now_ms.max(self.learned_reset_ms))
        {
            self.crdt_seq = self.crdt_seq.saturating_add(1);
            true
        } else {
            false
        }
    }

    pub fn project_context_fallback(context: &str) -> [i8; 32] {
        project_context_fallback(context)
    }

    pub fn record_semantic_memory(
        &mut self,
        context_vec: [i8; 32],
        word: &str,
        app_id: Option<&str>,
        now_ms: u64,
    ) {
        if word.is_empty() || self.erased_words.contains(word) || context_vec == [0; 32] {
            return;
        }
        let app = normalize(app_id.unwrap_or_default());
        if let Some(entry) = self.semantic_memories.iter_mut().find(|e| {
            e.word == word
                && e.app_id == app
                && e.vec_i8.len() == 32
                && cosine(&e.vec_i8, &context_vec) >= 0.85
        }) {
            let vector = std::array::from_fn(|i| {
                entry.vec_i8[i] as f32 * entry.count as f32 + context_vec[i] as f32
            });
            entry.vec_i8 = quantize_context(vector).to_vec();
            entry.count = entry.count.saturating_add(1);
            entry.updated_ms = now_ms.max(self.learned_reset_ms);
        } else {
            self.semantic_memories.push(SemanticMemoryEntry {
                vec_i8: context_vec.to_vec(),
                word: word.into(),
                app_id: app,
                count: 1,
                updated_ms: now_ms.max(self.learned_reset_ms),
            });
        }
        self.enforce_bounds();
        self.crdt_seq = self.crdt_seq.saturating_add(1);
    }

    pub fn query_knn_semantic(
        &self,
        query_vec: &[i8; 32],
        word: &str,
        app_id: Option<&str>,
        now_ms: u64,
    ) -> f64 {
        if self.erased_words.contains(word) || *query_vec == [0; 32] {
            return 0.0;
        }
        let app = normalize(app_id.unwrap_or_default());
        self.semantic_memories
            .iter()
            .filter(|e| e.word == word && e.vec_i8.len() == 32)
            .map(|e| {
                let cos = cosine(query_vec, &e.vec_i8);
                if cos <= 0.35 {
                    return 0.0;
                }
                let decay =
                    (-self.lambda() * (now_ms.saturating_sub(e.updated_ms) as f64 / 1000.0)).exp();
                let app_mult = if !app.is_empty() && app == e.app_id {
                    1.2
                } else {
                    0.85
                };
                ((cos - 0.35) / 0.65) * (1.0 + (e.count as f64).ln()) * decay * app_mult
            })
            .sum::<f64>()
            .clamp(0.0, 2.0)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_contextual_selection(
        &mut self,
        word: &str,
        code: &str,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        now_ms: u64,
        boost: f64,
        adaptive: bool,
    ) {
        self.record_selection_at(word, now_ms, boost);
        if !adaptive {
            return;
        }
        self.session_cache.push_back((
            normalize(app_id.unwrap_or_default()),
            word.to_string(),
            now_ms,
        ));
        if self.session_cache.len() > 32 {
            self.session_cache.pop_front();
        }
        let lambda = self.lambda();
        for (prefix, map, key) in [
            (
                "b:",
                &mut self.context_bigrams,
                normalize(prev_context.unwrap_or_default()),
            ),
            (
                "a:",
                &mut self.app_frecency,
                normalize(app_id.unwrap_or_default()),
            ),
            ("c:", &mut self.code_preferences, normalize(code)),
        ] {
            if key.is_empty() {
                continue;
            }
            let clock = self
                .habit_updated_ms
                .entry(format!("{prefix}{key}"))
                .or_insert(now_ms);
            let decay = (-lambda * (now_ms.saturating_sub(*clock) as f64 / 1000.0)).exp();
            let words = map.entry(key.clone()).or_default();
            for score in words.values_mut() {
                *score *= decay;
            }
            *words.entry(word.to_string()).or_default() += boost;
            *clock = now_ms
                .max(clock.saturating_add(1))
                .max(self.learned_reset_ms);
        }
        self.enforce_bounds();
    }

    pub fn record_correction(
        &mut self,
        code: &str,
        wrong_word: &str,
        right_word: &str,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        now_ms: u64,
    ) {
        if let Some(score) = self.frecency.get_mut(wrong_word) {
            *score *= 0.5;
        }
        let updated_ms = now_ms.max(
            self.frecency_updated_ms
                .get(wrong_word)
                .copied()
                .unwrap_or(0)
                .saturating_add(1),
        );
        self.frecency_updated_ms
            .insert(wrong_word.to_string(), updated_ms);
        if let Some(words) = self.code_preferences.get_mut(&normalize(code)) {
            words.remove(wrong_word);
        }
        for (map, key) in [
            (
                &mut self.context_bigrams,
                normalize(prev_context.unwrap_or_default()),
            ),
            (
                &mut self.app_frecency,
                normalize(app_id.unwrap_or_default()),
            ),
        ] {
            if let Some(score) = map.get_mut(&key).and_then(|w| w.get_mut(wrong_word)) {
                *score *= 0.5;
            }
        }
        self.record_contextual_selection(right_word, code, prev_context, app_id, now_ms, 2.5, true);
    }

    pub fn observe_phrase_composition(
        &mut self,
        combined_word: &str,
        xiaohe: &str,
        quanpin: &str,
        now_ms: u64,
    ) {
        let now_ms = now_ms.max(self.learned_reset_ms);
        if !valid_phrase(combined_word)
            || self.erased_words.contains(combined_word)
            || xiaohe.is_empty()
            || quanpin.is_empty()
        {
            return;
        }
        let count = self
            .pending_phrase_pairs
            .entry(combined_word.to_string())
            .or_default();
        *count = count.saturating_add(1);
        if *count >= 2 || self.user_phrases.contains_key(combined_word) {
            let count = self
                .user_phrases
                .get(combined_word)
                .map_or(*count, |p| p.count.saturating_add(1));
            self.user_phrases.insert(
                combined_word.to_string(),
                UserPhrase {
                    word: combined_word.to_string(),
                    xiaohe: normalize(xiaohe),
                    quanpin: quanpin
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_ascii_lowercase(),
                    acronym: UserPhrase::derive_acronym(combined_word, quanpin, xiaohe),
                    count,
                    updated_ms: now_ms,
                    source: "typed".into(),
                },
            );
            self.record_selection_at(combined_word, now_ms, 0.8);
        }
        // Pending observations are transient and bounded too.
        if self.pending_phrase_pairs.len() > 1024 {
            prune_counts(&mut self.pending_phrase_pairs, 1024);
        }
        self.enforce_bounds();
    }

    pub fn harvest_clipboard_text(
        &mut self,
        text: &str,
        annotate: impl Fn(&str) -> (String, String),
    ) {
        if text.is_empty()
            || text.len() > 4096
            || text.contains("://")
            || text.lines().any(secret_line)
        {
            return;
        }
        let now_ms = Self::now_epoch_ms().max(self.learned_reset_ms);
        let mut seen = HashSet::new();
        for phrase in text.split(|c: char| !is_cjk(c)).filter(|s| valid_phrase(s)) {
            if !seen.insert(phrase)
                || seen.len() > 8
                || self.erased_words.contains(phrase)
                || self.decayed_score_at(phrase, now_ms) > 0.35
                || self.user_phrases.contains_key(phrase)
            {
                continue;
            }
            let (quanpin, xiaohe) = annotate(phrase);
            if quanpin.is_empty() || xiaohe.is_empty() {
                continue;
            }
            self.user_phrases.insert(
                phrase.to_string(),
                UserPhrase {
                    word: phrase.to_string(),
                    quanpin: quanpin
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_ascii_lowercase(),
                    xiaohe: normalize(&xiaohe),
                    acronym: UserPhrase::derive_acronym(phrase, &quanpin, &xiaohe),
                    count: 1,
                    updated_ms: now_ms,
                    source: "clipboard".into(),
                },
            );
            if self.decayed_score_at(phrase, now_ms) < 0.35 {
                self.frecency.insert(phrase.to_string(), 0.35);
                self.frecency_updated_ms.insert(phrase.to_string(), now_ms);
            }
            self.crdt_seq = self.crdt_seq.saturating_add(1);
        }
        for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            self.record_english_casing(token);
        }
        self.enforce_bounds();
    }

    pub(crate) fn max_user_phrase_syllables(&self) -> usize {
        self.user_phrases
            .values()
            .filter(|p| !self.erased_words.contains(&p.word))
            .map(|p| p.word.chars().count())
            .max()
            .unwrap_or(0)
    }

    pub fn query_user_phrases(&self, compact_code: &str, is_flypy: bool) -> Vec<&UserPhrase> {
        if compact_code.is_empty() {
            return Vec::new();
        }
        let code = normalize(compact_code);
        let mut phrases: Vec<_> = self
            .user_phrases
            .values()
            .filter(|p| !self.erased_words.contains(&p.word) && p.matches_code(&code, is_flypy))
            .collect();
        phrases.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
        phrases
    }

    pub(crate) fn register_typed_phrase(
        &mut self,
        word: &str,
        quanpin: &str,
        xiaohe: &str,
        now_ms: u64,
    ) {
        if !valid_phrase(word)
            || self.erased_words.contains(word)
            || quanpin.is_empty()
            || xiaohe.is_empty()
        {
            return;
        }
        let frecency_count = self
            .frecency
            .get(word)
            .copied()
            .unwrap_or(1.0)
            .round()
            .max(1.0) as u32;
        let count = self
            .user_phrases
            .get(word)
            .map_or(frecency_count, |p| {
                p.count.saturating_add(1).max(frecency_count)
            });
        self.user_phrases.insert(
            word.into(),
            UserPhrase {
                word: word.into(),
                quanpin: quanpin
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_lowercase(),
                xiaohe: normalize(xiaohe),
                acronym: UserPhrase::derive_acronym(word, quanpin, xiaohe),
                count,
                updated_ms: now_ms.max(self.learned_reset_ms),
                source: "typed".into(),
            },
        );
        self.crdt_seq = self.crdt_seq.saturating_add(1);
        self.enforce_bounds();
    }

    pub fn query_acronym_phrases(&self, compact_code: &str) -> Vec<&UserPhrase> {
        let code = normalize(compact_code);
        if code.len() < 2 {
            return Vec::new();
        }
        let mut phrases: Vec<_> = self
            .user_phrases
            .values()
            .filter(|p| {
                !self.erased_words.contains(&p.word)
                    && p.effective_acronym() == code
                    && (p.count >= 2
                        || self.pinned_words.contains(&p.word)
                        || self.frecency.get(&p.word).copied().unwrap_or(0.0) >= 1.5)
            })
            .collect();
        phrases.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| {
                    self.frecency
                        .get(&b.word)
                        .copied()
                        .unwrap_or(0.0)
                        .total_cmp(&self.frecency.get(&a.word).copied().unwrap_or(0.0))
                })
                .then_with(|| a.word.cmp(&b.word))
        });
        phrases.truncate(3);
        phrases
    }

    pub fn query_context_continuations(
        &self,
        prev_context: &str,
        prefix: &str,
        is_flypy: bool,
        now_ms: u64,
        annotate_prefix: impl Fn(&str) -> (String, String),
    ) -> Vec<(String, f64)> {
        let context = normalize(prev_context);
        let prefix = normalize(prefix);
        if context.is_empty() || !(1..=3).contains(&prefix.len()) {
            return Vec::new();
        }
        let allow_acronym =
            is_flypy || !crate::imp::xiaohe::canonical_syllable_set().contains(prefix.as_str());
        let Some(followers) = self.context_bigrams.get(&context) else {
            return Vec::new();
        };
        let last_ms = self
            .habit_updated_ms
            .get(&format!("b:{context}"))
            .copied()
            .unwrap_or(now_ms);
        let decay = (-self.lambda() * now_ms.saturating_sub(last_ms) as f64 / 1000.0).exp();
        let mut matches: Vec<_> = followers
            .iter()
            .filter_map(|(word, weight)| {
                let weight = weight * decay;
                if self.erased_words.contains(word)
                    || word.is_empty()
                    || !word.chars().all(is_cjk)
                    || (!valid_phrase(word) && weight <= 0.25)
                {
                    return None;
                }
                let matches = if let Some(phrase) = self.user_phrases.get(word) {
                    let matches_code = if is_flypy {
                        phrase.xiaohe.starts_with(&prefix)
                    } else {
                        phrase.quanpin.bytes().filter(|&b| b != b' ').take(prefix.len()).eq(prefix.bytes())
                    };
                    matches_code || (allow_acronym && phrase.effective_acronym().starts_with(&prefix))
                } else {
                    let (quanpin, xiaohe) = annotate_prefix(word);
                    let acronym = UserPhrase::derive_acronym(word, &quanpin, &xiaohe);
                    let code = if is_flypy {
                        xiaohe
                    } else {
                        quanpin.split_whitespace().collect::<String>()
                    };
                    normalize(&code).starts_with(&prefix)
                        || (allow_acronym && acronym.starts_with(&prefix))
                };
                matches.then(|| (word.clone(), weight))
            })
            .collect();
        matches.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        matches.truncate(2);
        matches
    }

    pub fn query_user_english(&self, compact_code: &str) -> Option<&str> {
        self.user_english
            .get(&compact_code.to_ascii_lowercase())
            .filter(|word| !self.erased_words.contains(word.as_str()))
            .map(String::as_str)
    }

    pub fn record_english_casing(&mut self, raw: &str) {
        if !valid_english(raw) || self.erased_words.contains(raw) {
            return;
        }
        let key = raw.to_ascii_lowercase();
        if self.user_english.get(&key).is_some_and(|v| v == raw) {
            return;
        }
        self.user_english.insert(key.clone(), raw.to_string());
        let clock = self.habit_updated_ms.entry(format!("e:{key}")).or_default();
        *clock = Self::now_epoch_ms()
            .max(clock.saturating_add(1))
            .max(self.learned_reset_ms);
        self.crdt_seq = self.crdt_seq.saturating_add(1);
        self.enforce_bounds();
    }

    pub fn stats(&self) -> PersonalizationStats {
        PersonalizationStats {
            vocab_count: self.frecency.len(),
            bigram_count: self.context_bigrams.values().map(HashMap::len).sum(),
            phrase_count: self.user_phrases.len(),
            app_count: self.app_frecency.len(),
            english_count: self.user_english.len(),
            pinned_count: self.pinned_words.len(),
            erased_count: self.erased_words.len(),
            reranker_updates: self.reranker.updates,
            semantic_memory_count: self.semantic_memories.len(),
        }
    }

    pub fn clear_learned_habits(&mut self) {
        let latest = self
            .frecency_updated_ms
            .values()
            .chain(self.habit_updated_ms.values())
            .copied()
            .chain(self.user_phrases.values().map(|p| p.updated_ms))
            .chain(self.semantic_memories.iter().map(|e| e.updated_ms))
            .chain(std::iter::once(self.reranker.updated_ms))
            .max()
            .unwrap_or(0);
        self.learned_reset_ms = Self::now_epoch_ms()
            .max(latest)
            .max(self.learned_reset_ms)
            .saturating_add(1);
        self.reranker.reset(self.learned_reset_ms);
        self.semantic_memories.clear();
        self.frecency.clear();
        self.frecency_updated_ms.clear();
        self.context_bigrams.clear();
        self.app_frecency.clear();
        self.code_preferences.clear();
        self.user_phrases.clear();
        self.user_english.clear();
        self.pending_phrase_pairs.clear();
        self.session_cache.clear();
        self.habit_updated_ms.clear();
        self.crdt_seq = self.crdt_seq.saturating_add(1);
    }

    fn enforce_bounds(&mut self) {
        while self.semantic_memories.len() > 512 {
            let i = self
                .semantic_memories
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.count, e.updated_ms, &e.word, &e.app_id, &e.vec_i8))
                .map(|(i, _)| i)
                .unwrap();
            self.semantic_memories.remove(i);
        }
        prune_nested(&mut self.context_bigrams, 1024, 8);
        prune_nested(&mut self.app_frecency, 32, 256);
        prune_nested(&mut self.code_preferences, 2048, 6);
        while self.user_phrases.len() > 1024 {
            let key = self
                .user_phrases
                .iter()
                .min_by_key(|(k, p)| (p.count, p.updated_ms, *k))
                .map(|(k, _)| k.clone())
                .unwrap();
            self.user_phrases.remove(&key);
        }
        while self.user_english.len() > 512 {
            let key = self
                .user_english
                .keys()
                .min_by_key(|k| {
                    (
                        self.habit_updated_ms
                            .get(&format!("e:{k}"))
                            .copied()
                            .unwrap_or(0),
                        *k,
                    )
                })
                .cloned()
                .unwrap();
            self.user_english.remove(&key);
        }
        self.habit_updated_ms.retain(|key, _| {
            key.strip_prefix("b:")
                .is_some_and(|k| self.context_bigrams.contains_key(k))
                || key
                    .strip_prefix("a:")
                    .is_some_and(|k| self.app_frecency.contains_key(k))
                || key
                    .strip_prefix("c:")
                    .is_some_and(|k| self.code_preferences.contains_key(k))
                || key
                    .strip_prefix("e:")
                    .is_some_and(|k| self.user_english.contains_key(k))
        });
    }

    pub fn pin_word(&mut self, word: &str) {
        self.erased_words.remove(word);
        self.pinned_words.insert(word.to_string());
        self.crdt_seq = self.crdt_seq.saturating_add(1);
    }

    pub fn erase_word(&mut self, word: &str) {
        self.semantic_memories.retain(|e| e.word != word);
        self.session_cache.retain(|(_, cached, _)| cached != word);
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
            context_bigrams: self.context_bigrams.clone(),
            app_frecency: self.app_frecency.clone(),
            code_preferences: self.code_preferences.clone(),
            user_phrases: self.user_phrases.clone(),
            user_english: self.user_english.clone(),
            habit_updated_ms: self.habit_updated_ms.clone(),
            learned_reset_ms: self.learned_reset_ms,
            reranker: Some(self.reranker.clone()),
            semantic_memories: self.semantic_memories.clone(),
        }
    }

    pub fn merge_crdt_delta(&mut self, delta: &RetentionCrdtDelta) {
        for word in &delta.erased_words {
            self.erase_word(word);
        }
        for word in &delta.pinned_words {
            self.pin_word(word);
        }
        if delta.learned_reset_ms > self.learned_reset_ms {
            self.clear_learned_habits();
            self.learned_reset_ms = delta.learned_reset_ms;
            self.reranker.reset(self.learned_reset_ms);
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
            if remote_ts >= self.learned_reset_ms
                && (remote_ts > local_ts || (remote_ts == local_ts && remote_score > local_score))
            {
                self.frecency.insert(word.clone(), remote_score);
                self.frecency_updated_ms.insert(word.clone(), remote_ts);
            }
        }
        if delta.learned_reset_ms >= self.learned_reset_ms {
            for (prefix, local, remote) in [
                ("b:", &mut self.context_bigrams, &delta.context_bigrams),
                ("a:", &mut self.app_frecency, &delta.app_frecency),
                ("c:", &mut self.code_preferences, &delta.code_preferences),
            ] {
                for (key, values) in remote {
                    let clock_key = format!("{prefix}{key}");
                    let remote_ts = delta.habit_updated_ms.get(&clock_key).copied().unwrap_or(0);
                    let local_ts = self.habit_updated_ms.get(&clock_key).copied().unwrap_or(0);
                    if remote_ts < self.learned_reset_ms || remote_ts < local_ts {
                        continue;
                    }
                    let entry = local.entry(key.clone()).or_default();
                    if remote_ts > local_ts {
                        entry.clear();
                    }
                    for (word, weight) in values {
                        if weight.is_finite() && *weight > 0.0 && !self.erased_words.contains(word)
                        {
                            let score = entry.entry(word.clone()).or_default();
                            *score = score.max(*weight);
                        }
                    }
                    self.habit_updated_ms.insert(clock_key, remote_ts);
                }
            }
            for (word, phrase) in &delta.user_phrases {
                if !self.erased_words.contains(word)
                    && phrase.updated_ms >= self.learned_reset_ms
                    && valid_phrase(word)
                    && phrase.word == *word
                    && self.user_phrases.get(word).is_none_or(|p| {
                        (phrase.updated_ms, phrase.count, &phrase.source)
                            > (p.updated_ms, p.count, &p.source)
                    })
                {
                    self.user_phrases.insert(word.clone(), phrase.clone());
                }
            }
            for (key, value) in &delta.user_english {
                let clock_key = format!("e:{key}");
                let remote_ts = delta.habit_updated_ms.get(&clock_key).copied().unwrap_or(0);
                let local_ts = self.habit_updated_ms.get(&clock_key).copied().unwrap_or(0);
                if !self.erased_words.contains(value)
                    && remote_ts >= self.learned_reset_ms
                    && (remote_ts > local_ts
                        || (remote_ts == local_ts
                            && self.user_english.get(key).is_none_or(|v| value > v)))
                {
                    self.user_english.insert(key.clone(), value.clone());
                    self.habit_updated_ms.insert(clock_key, remote_ts);
                }
            }
        }
        if let Some(remote) = &delta.reranker {
            let valid = remote.weights.iter().enumerate().all(|(i, w)| {
                w.is_finite()
                    && if i == 8 {
                        (-0.5..=2.0).contains(w)
                    } else {
                        (0.15..=4.0).contains(w)
                    }
            });
            let tie_order = remote
                .weights
                .iter()
                .zip(self.reranker.weights)
                .map(|(a, b)| a.total_cmp(&b))
                .find(|o| !o.is_eq())
                .unwrap_or(std::cmp::Ordering::Equal);
            if valid
                && remote.updated_ms >= self.learned_reset_ms
                && ((remote.updated_ms, remote.updates)
                    > (self.reranker.updated_ms, self.reranker.updates)
                    || ((remote.updated_ms, remote.updates)
                        == (self.reranker.updated_ms, self.reranker.updates)
                        && tie_order.is_gt()))
            {
                self.reranker = remote.clone();
            }
        }
        let mut memories = self.semantic_memories.clone();
        memories.extend(
            delta
                .semantic_memories
                .iter()
                .filter(|e| {
                    e.vec_i8.len() == 32
                        && e.vec_i8.iter().any(|&v| v != 0)
                        && e.count > 0
                        && !e.word.is_empty()
                        && e.updated_ms >= self.learned_reset_ms
                        && !self.erased_words.contains(&e.word)
                })
                .cloned()
                .map(|mut e| {
                    e.app_id = normalize(&e.app_id);
                    e
                }),
        );
        // Newest representative wins; deterministic ties and ordering make replay idempotent.
        memories.sort_by(|a, b| {
            (b.updated_ms, b.count, &b.word, &b.app_id, &b.vec_i8).cmp(&(
                a.updated_ms,
                a.count,
                &a.word,
                &a.app_id,
                &a.vec_i8,
            ))
        });
        self.semantic_memories.clear();
        for entry in memories {
            if !self.semantic_memories.iter().any(|e| {
                e.word == entry.word
                    && e.app_id == entry.app_id
                    && cosine(&e.vec_i8, &entry.vec_i8) >= 0.85
            }) {
                self.semantic_memories.push(entry);
            }
        }
        self.enforce_bounds();
        self.crdt_seq = self.crdt_seq.max(delta.seq);
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(ch, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{20000}'..='\u{2fa1f}' | '\u{30000}'..='\u{323af}')
}

fn valid_phrase(word: &str) -> bool {
    (2..=8).contains(&word.chars().count()) && word.chars().all(is_cjk)
}

fn valid_english(raw: &str) -> bool {
    (3..=24).contains(&raw.len())
        && raw.starts_with(|c: char| c.is_ascii_alphabetic())
        && raw
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        && raw.bytes().any(|c| c.is_ascii_uppercase())
        && raw.bytes().any(|c| c.is_ascii_lowercase())
        && !raw.bytes().all(|c| c.is_ascii_hexdigit())
        && !secret_line(raw)
}

pub(crate) fn secret_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "sk-", "gho_", "ghp_", "bearer ", "password", "passwd", "token=", "secret=", "密码",
    ]
    .iter()
    .any(|s| lower.contains(s))
        || line
            .split(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '+' | '='))
            })
            .any(|run| {
                run.len() >= 24
                    && (run.bytes().collect::<HashSet<_>>().len() >= 12
                        || run.bytes().all(|c| c.is_ascii_hexdigit()))
            })
}

fn prune_counts(map: &mut HashMap<String, u32>, cap: usize) {
    while map.len() > cap {
        let key = map
            .iter()
            .min_by_key(|(k, v)| (**v, *k))
            .map(|(k, _)| k.clone())
            .unwrap();
        map.remove(&key);
    }
}

fn prune_nested(map: &mut HashMap<String, HashMap<String, f64>>, outer: usize, inner: usize) {
    for words in map.values_mut() {
        while words.len() > inner {
            let key = words
                .iter()
                .min_by(|a, b| a.1.total_cmp(b.1).then_with(|| a.0.cmp(b.0)))
                .map(|(k, _)| k.clone())
                .unwrap();
            words.remove(&key);
        }
    }
    while map.len() > outer {
        let key = map
            .iter()
            .min_by(|a, b| {
                a.1.values()
                    .sum::<f64>()
                    .total_cmp(&b.1.values().sum::<f64>())
                    .then_with(|| a.0.cmp(b.0))
            })
            .map(|(k, _)| k.clone())
            .unwrap();
        map.remove(&key);
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

#[cfg(test)]
mod personalization_tests {
    use super::*;

    #[test]
    fn single_character_history_is_isolated_by_reading() {
        let now = 1_000;
        let context = project_context_fallback("讨论项目进度安排");
        for (word, learned, other) in [
            ("的", "de", "di"),
            ("会", "hui", "kuai"),
            ("说", "shuo", "shui"),
            ("了", "le", "liao"),
            ("给", "gei", "ji"),
            ("没", "mei", "mo"),
            ("着", "zhe", "zhao"),
            ("和", "he", "huo"),
        ] {
            let mut r = RetentionEngine::default();
            r.record_contextual_selection(
                word,
                learned,
                Some("前文"),
                Some("editor"),
                now,
                100.0,
                true,
            );
            r.record_semantic_memory(context, word, Some("editor"), now);
            let features = |r: &RetentionEngine, code: &str, adaptive| {
                r.extract_candidate_features(
                    word,
                    code,
                    Some("前文"),
                    Some("editor"),
                    now,
                    adaptive,
                    Some(&context),
                    None,
                    Some(0),
                )
            };
            let matching = features(&r, &format!(" {} ", learned.to_uppercase()), true);
            for i in [0, 1, 2, 3, 4, 5] {
                assert!(matching[i] > 0.0, "{word}: {matching:?}");
            }
            let mismatching = features(&r, other, true);
            for i in [0, 1, 3, 4, 5] {
                assert_eq!(mismatching[i], 0.0, "{word}: {mismatching:?}");
            }
            assert_eq!(mismatching[2], matching[2]);
            assert_eq!(features(&r, other, false)[0], matching[0]);
            assert!(!r.has_mismatched_single_char_code(word, ""));
            r.record_contextual_selection(word, other, None, None, now, 1.0, true);
            assert!(!r.has_mismatched_single_char_code(word, other));
        }
        let mut r = RetentionEngine::default();
        r.record_selection_at("的", now, 100.0);
        assert!(!r.has_mismatched_single_char_code("的", "di"));
        r.record_contextual_selection("你好", "nihao", None, None, now, 100.0, true);
        assert!(!r.has_mismatched_single_char_code("你好", "nh"));
        r.code_preferences
            .entry("de".into())
            .or_default()
            .insert("地".into(), 1.0);
        assert!(!r.has_mismatched_single_char_code("地", "di"));
    }

    #[test]
    fn dictionary_rank_feedback_stays_nonnegative_and_counts_updates() {
        let mut reranker = PersonalReranker::default();
        let mut chosen = [0.0; 9];
        let mut skipped = [0.0; 9];
        chosen[8] = 0.5;
        skipped[8] = 1.0;
        for now in 1..=20 {
            assert!(reranker.update_pairwise(&chosen, &skipped, now));
            assert_eq!(reranker.weights[8], 0.0);
            assert_eq!(reranker.updates, now);
            assert_eq!(reranker.updated_ms, now);
        }
    }

    #[test]
    fn typed_phrase_updates_preserve_counts_after_frecency_decay() {
        let mut r = RetentionEngine::default();
        let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe("ce shi ci zu");
        r.register_typed_phrase("测试词组", "ce shi ci zu", &xiaohe, 1000);
        r.user_phrases.get_mut("测试词组").unwrap().count = 100;
        r.frecency.insert("测试词组".into(), 0.1);
        r.register_typed_phrase("测试词组", "ce shi ci zu", &xiaohe, 61_000);
        let phrase = &r.user_phrases["测试词组"];
        assert_eq!(phrase.count, 101);
        assert_eq!(phrase.updated_ms, 61_000);
        assert_eq!(phrase.acronym, "cscz");
        assert_eq!(r.query_acronym_phrases("cscz")[0].word, "测试词组");
        r.user_phrases.get_mut("测试词组").unwrap().count = u32::MAX;
        r.register_typed_phrase("测试词组", "ce shi ci zu", &xiaohe, 62_000);
        assert_eq!(r.user_phrases["测试词组"].count, u32::MAX);
    }

    #[test]
    fn eight_character_typed_phrase_survives_query_and_sync() {
        let mut r = RetentionEngine::default();
        let word = "我要打卡你好世界";
        let pinyin = "wo yao da ka ni hao shi jie";
        let xiaohe = crate::imp::xiaohe::pinyin_phrase_to_xiaohe(pinyin);
        r.register_typed_phrase(word, pinyin, &xiaohe, RetentionEngine::now_epoch_ms());
        assert_eq!(
            r.query_user_phrases("woyaodakanihaoshijie", false)[0].word,
            word
        );
        let mut other = RetentionEngine::default();
        other.merge_crdt_delta(&r.export_crdt_delta("test-device"));
        assert_eq!(other.query_user_phrases(&xiaohe, true)[0].word, word);
        r.register_typed_phrase(
            "我要打卡你好世界啊",
            pinyin,
            &xiaohe,
            RetentionEngine::now_epoch_ms(),
        );
        assert!(!r.user_phrases.contains_key("我要打卡你好世界啊"));
    }

    #[test]
    fn acronym_phrases_use_spaced_initials_and_legacy_xiaohe_fallback() {
        let mut r = RetentionEngine::default();
        r.harvest_clipboard_text("项目管理", |_| {
            ("xiang mu guan li".into(), "xlmugrli".into())
        });
        assert!(r.query_acronym_phrases("xmgl").is_empty());
        r.pin_word("项目管理");
        assert_eq!(r.query_acronym_phrases(" XMGL ")[0].acronym, "xmgl");
        assert_eq!(r.query_user_phrases("xiangmuguanli", false).len(), 1);
        let mut json = serde_json::to_value(&r).unwrap();
        json["user_phrases"]["项目管理"]
            .as_object_mut()
            .unwrap()
            .remove("acronym");
        let mut old: RetentionEngine = serde_json::from_value(json).unwrap();
        assert_eq!(old.query_acronym_phrases("xmgl")[0].word, "项目管理");
        assert!(old.query_acronym_phrases("x").is_empty());
        old.erase_word("项目管理");
        assert!(old.query_acronym_phrases("xmgl").is_empty());
    }

    #[test]
    fn prefix_continuations_filter_rank_decay_and_support_both_schemas() {
        let mut r = RetentionEngine::with_half_life(1.0);
        for (word, weight) in [
            ("打扰了", 2.0),
            ("对不起", 1.0),
            ("大", 0.1),
            ("ASCII", 8.0),
        ] {
            r.record_contextual_selection(word, "", Some("不好意思"), None, 1000, weight, true);
        }
        let annotate = |word: &str| match word {
            "打扰了" => ("da rao le".into(), "darc le".replace(' ', "")),
            "对不起" => ("dui bu qi".into(), "dvbuqi".into()),
            _ => (String::new(), String::new()),
        };
        for flypy in [false, true] {
            let found = r.query_context_continuations(" 不好意思 ", "D", flypy, 1000, annotate);
            assert_eq!(
                found.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>(),
                ["打扰了", "对不起"]
            );
            assert_eq!(
                r.query_context_continuations("不好意思", "dr", flypy, 1000, annotate)[0].0,
                "打扰了"
            );
        }
        assert!(r
            .query_context_continuations("不好意思", "", false, 1000, annotate)
            .is_empty());
        assert!(r
            .query_context_continuations("不好意思", "dara", false, 1000, annotate)
            .is_empty());
        let found = r.query_context_continuations("不好意思", "d", false, 2000, annotate);
        assert!((found[0].1 - 1.0).abs() < 0.01);
        r.erase_word("打扰了");
        assert_eq!(
            r.query_context_continuations("不好意思", "d", false, 1000, annotate)[0].0,
            "对不起"
        );
    }

    #[test]
    fn session_cache_decays_expires_isolates_apps_and_stays_transient() {
        let mut r = RetentionEngine::default();
        r.record_contextual_selection("项目", "", None, Some(" Chat "), 1000, 1.0, true);
        let mut baseline = r.clone();
        baseline.session_cache.clear();
        let resonance = |engine: &RetentionEngine, app: &str, now| {
            engine.contextual_score_at("项目", "", None, Some(app), now, true)
                - baseline.contextual_score_at("项目", "", None, Some(app), now, true)
        };
        assert!((resonance(&r, "CHAT", 1000) - 1.8).abs() < 1e-10);
        assert!((resonance(&r, "chat", 601_000) - 0.9).abs() < 1e-10);
        assert!(resonance(&r, "chat", 901_000) > 0.0);
        assert_eq!(resonance(&r, "chat", 901_001), 0.0);
        assert_eq!(resonance(&r, "editor", 1000), 0.0);
        let restored: RetentionEngine =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(restored.session_cache.is_empty());
        for _ in 0..40 {
            r.record_contextual_selection("项目", "", None, Some("chat"), 1000, 1.0, true);
        }
        assert_eq!(r.session_cache.len(), 32);
        let mut no_cache = r.clone();
        no_cache.session_cache.clear();
        assert!(
            (r.contextual_score_at("项目", "", None, Some("chat"), 1000, true)
                - no_cache.contextual_score_at("项目", "", None, Some("chat"), 1000, true)
                - 3.6)
                .abs()
                < 1e-10
        );
        r.erase_word("项目");
        assert!(r.session_cache.is_empty());
        r.record_contextual_selection("新词", "", None, None, 1000, 1.0, false);
        assert!(r.session_cache.is_empty());
        r.record_contextual_selection("新词", "", None, None, 1000, 1.0, true);
        r.clear_learned_habits();
        assert!(r.session_cache.is_empty());
        let mut fast = RetentionEngine::with_half_life(1.0);
        fast.record_contextual_selection("项目", "", None, None, 1000, 1.0, true);
        let mut baseline = fast.clone();
        baseline.session_cache.clear();
        let score = |engine: &RetentionEngine, now| {
            engine.contextual_score_at("项目", "", None, None, now, true)
        };
        assert!((score(&fast, 2000) - score(&baseline, 2000) - 0.9).abs() < 1e-10);
        assert_eq!(score(&fast, 2501), score(&baseline, 2501));
    }

    #[test]
    fn contextual_scores_are_opt_in_and_erasure_wins() {
        let mut r = RetentionEngine::default();
        assert_eq!(
            r.contextual_score_at("好", "hk", Some("你"), Some("chat"), 1000, true),
            0.0
        );
        r.record_contextual_selection("好", " HK ", Some(" 你 "), Some(" Chat "), 1000, 1.0, true);
        assert!(
            (r.contextual_score_at("好", "hk", Some("你"), Some("CHAT"), 1000, true) - 6.15).abs()
                < 1e-10
        );
        assert_eq!(
            r.contextual_score_at("好", "hk", Some("你"), Some("chat"), 1000, false),
            1.0
        );
        r.erase_word("好");
        assert_eq!(
            r.contextual_score_at("好", "hk", Some("你"), Some("chat"), 1000, true),
            0.0
        );
    }

    #[test]
    fn clipboard_harvest_filters_secrets_and_clear_preserves_explicit_choices() {
        let mut r = RetentionEngine::default();
        let annotate = |_: &str| ("nihao".into(), "nihk".into());
        for text in [
            "你好 https://example.org",
            "你好 sk-123456",
            "你好 gho_abc",
            "你好 Bearer xyz",
            "密码=你好",
            "你好 a8B2c9D3e7F1g6H4i5J0k2L9",
        ] {
            r.harvest_clipboard_text(text, annotate);
        }
        assert_eq!(r.stats().phrase_count, 0);
        r.harvest_clipboard_text("你好 Vitral OpenAI aBcDef", annotate);
        assert_eq!(r.query_user_phrases("nihk", true)[0].source, "clipboard");
        assert_eq!(r.query_user_phrases("nihao", false)[0].word, "你好");
        assert_eq!(r.query_user_english("openai"), Some("OpenAI"));
        assert_eq!(r.query_user_english("abcdef"), None);
        let score = r.frecency["你好"];
        r.harvest_clipboard_text("你好 Vitral", annotate);
        assert_eq!(r.frecency["你好"], score);
        r.pin_word("保留");
        r.erase_word("删除");
        r.clear_learned_habits();
        let stats = r.stats();
        assert_eq!(
            (
                stats.vocab_count,
                stats.phrase_count,
                stats.english_count,
                stats.bigram_count,
                stats.app_count
            ),
            (0, 0, 0, 0, 0)
        );
        assert_eq!((stats.pinned_count, stats.erased_count), (1, 1));
    }

    #[test]
    fn legacy_snapshots_and_crdt_keep_corrections_and_clear() {
        let old = r#"{"frecency":{},"pinned_words":[],"erased_words":[]}"#;
        let mut a: RetentionEngine = serde_json::from_str(old).unwrap();
        let now = RetentionEngine::now_epoch_ms();
        a.record_contextual_selection("错", "co", Some("前"), Some("app"), now, 1.0, true);
        a.observe_phrase_composition("你好", "nihk", "nihao", now);
        a.observe_phrase_composition("你好", "nihk", "nihao", now);
        a.record_english_casing("OpenAI");
        let stale = a.export_crdt_delta("a");
        let mut b = RetentionEngine::default();
        b.merge_crdt_delta(&stale);
        b.merge_crdt_delta(&stale);
        assert_eq!(b.code_preferences["co"]["错"], 1.0);
        assert_eq!(b.query_user_english("openai"), Some("OpenAI"));
        assert_eq!(b.query_user_phrases("nihk", true).len(), 1);
        a.record_correction("co", "错", "对", Some("前"), Some("app"), now);
        b.merge_crdt_delta(&a.export_crdt_delta("a"));
        assert!(!b.code_preferences["co"].contains_key("错"));
        assert_eq!(b.app_frecency["app"]["错"], 0.5);
        assert_eq!(b.frecency["错"], 0.5);
        b.merge_crdt_delta(&stale);
        assert!(!b.code_preferences["co"].contains_key("错"));
        b.clear_learned_habits();
        let clear = b.export_crdt_delta("b");
        a.merge_crdt_delta(&clear);
        a.merge_crdt_delta(&stale);
        assert_eq!(a.stats().vocab_count, 0);
        assert_eq!(a.stats().phrase_count, 0);
        assert_eq!(a.stats().english_count, 0);
        // Pending phrase observations never survive serialization.
        let restored: RetentionEngine =
            serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert!(restored.pending_phrase_pairs.is_empty());
        a.observe_phrase_composition("中国", "vsgo", "zhongguo", now);
        a.observe_phrase_composition("中国", "vsgo", "zhongguo", now);
        b.merge_crdt_delta(&a.export_crdt_delta("a"));
        assert_eq!(b.query_user_phrases("zhongguo", false).len(), 1);
    }

    #[test]
    fn learned_maps_are_bounded_and_prune_lowest_weights() {
        let mut r = RetentionEngine::default();
        for i in 0..10 {
            r.record_contextual_selection(
                &format!("词{i}"),
                "ci",
                Some("前"),
                Some("app"),
                1000,
                (i + 1) as f64,
                true,
            );
        }
        assert_eq!(r.context_bigrams["前"].len(), 8);
        assert_eq!(r.code_preferences["ci"].len(), 6);
        assert!(!r.code_preferences["ci"].contains_key("词0"));
        for i in 0..34 {
            r.record_contextual_selection(
                "词",
                &format!("c{i}"),
                Some(&format!("p{i}")),
                Some(&format!("a{i}")),
                1000,
                1.0,
                true,
            );
        }
        assert_eq!(r.app_frecency.len(), 32);
        // A malformed remote bucket must not bypass the same caps.
        let mut delta = r.export_crdt_delta("a");
        delta.context_bigrams = (0..1030)
            .map(|i| (format!("p{i}"), HashMap::from([("词".into(), 1.0)])))
            .collect();
        delta.code_preferences = (0..2050)
            .map(|i| (format!("c{i}"), HashMap::from([("词".into(), 1.0)])))
            .collect();
        r.merge_crdt_delta(&delta);
        assert_eq!(r.context_bigrams.len(), 1024);
        assert_eq!(r.code_preferences.len(), 2048);
    }
}

#[cfg(test)]
mod phase3_tests {
    use super::*;

    #[test]
    fn pairwise_learning_flips_rank_stays_bounded_and_is_opt_in() {
        let mut r = RetentionEngine::default();
        let skipped = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let chosen = [0.0, 0.0, 0.2, 0.2, 0.0, 0.2, 0.0, 0.0, 0.5];
        assert!(r.reranker.score(&chosen) < r.reranker.score(&skipped));
        for now in 1..=200 {
            r.update_reranker_pairwise(&chosen, &skipped, now);
        }
        assert!(r.reranker.score(&chosen) > r.reranker.score(&skipped));
        for i in [2, 3, 5] {
            assert!(r.reranker.weights[i] > DEFAULT_RERANK_WEIGHTS[i]);
        }
        for (i, &w) in r.reranker.weights.iter().enumerate() {
            assert!(if i == 8 {
                (0.0..=2.0).contains(&w)
            } else {
                (0.15..=4.0).contains(&w)
            });
        }
        assert_eq!(r.score_candidate_features(&chosen, false), 0.0);
        assert_eq!(r.score_candidate_features(&skipped, false), 1.0);
        let clock = r.reranker.updated_ms;
        assert!(!r.update_reranker_pairwise(&chosen, &chosen, 999));
        assert_eq!(r.reranker.updated_ms, clock);
        r.reranker.reset(1000);
        assert_eq!(r.reranker.weights, DEFAULT_RERANK_WEIGHTS);
        assert_eq!(r.reranker.updates, 0);
    }

    #[test]
    fn semantic_memory_matches_context_bounds_and_erasure() {
        let mut r = RetentionEngine::default();
        let context = project_context_fallback("今天准备讨论项目进度计划");
        let similar = project_context_fallback("今天准备讨论项目进度安排");
        let other = project_context_fallback("周末去海边游泳吃螃蟹");
        assert_eq!(project_context_fallback("ASCII only"), [0; 32]);
        assert_eq!(
            context,
            project_context_fallback("今天准备讨论项目进度计划")
        );
        r.record_semantic_memory(context, "会议", Some(" Editor "), 1000);
        let relevant = r.query_knn_semantic(&similar, "会议", Some("editor"), 1000);
        assert!(relevant > 0.3, "{relevant}");
        assert_eq!(
            r.query_knn_semantic(&other, "会议", Some("editor"), 1000),
            0.0
        );
        assert!(
            r.query_knn_semantic(&context, "会议", Some("editor"), 1000)
                > r.query_knn_semantic(&context, "会议", Some("other"), 1000)
        );
        r.record_semantic_memory(context, "会议", Some("editor"), 1001);
        assert_eq!(r.semantic_memories.len(), 1);
        assert_eq!(r.semantic_memories[0].count, 2);
        for i in 0..520 {
            r.record_semantic_memory(context, &format!("词{i}"), None, 2000 + i);
        }
        assert_eq!(r.semantic_memories.len(), 512);
        assert!(r.semantic_memories.iter().any(|e| e.word == "会议"));
        assert!(!r.semantic_memories.iter().any(|e| e.word == "词0"));
        r.erase_word("会议");
        r.record_semantic_memory(context, "会议", Some("editor"), 4000);
        assert_eq!(
            r.query_knn_semantic(&context, "会议", Some("editor"), 4000),
            0.0
        );
        assert!(!r.semantic_memories.iter().any(|e| e.word == "会议"));
        r.clear_learned_habits();
        assert!(r.semantic_memories.is_empty());
        assert_eq!(r.reranker.updated_ms, r.learned_reset_ms);
    }

    #[test]
    fn phase3_crdt_replay_clear_legacy_and_invalid_boundaries() {
        let mut a: RetentionEngine =
            serde_json::from_str(r#"{"frecency":{},"pinned_words":[],"erased_words":[]}"#).unwrap();
        let legacy: PersonalReranker = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.weights, DEFAULT_RERANK_WEIGHTS);
        let vec = project_context_fallback("项目开发讨论");
        let chosen = [0.0, 0.0, 0.2, 0.2, 0.0, 0.2, 0.0, 0.0, 0.0];
        let skipped = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        a.update_reranker_pairwise(&chosen, &skipped, 1000);
        a.record_semantic_memory(vec, "会议", None, 1000);
        let stale = a.export_crdt_delta("a");
        let mut json = serde_json::to_value(&stale).unwrap();
        json.as_object_mut().unwrap().remove("reranker");
        json.as_object_mut().unwrap().remove("semantic_memories");
        let old: RetentionCrdtDelta = serde_json::from_value(json).unwrap();
        assert!(old.reranker.is_none());
        let mut b = RetentionEngine::default();
        b.merge_crdt_delta(&stale);
        b.merge_crdt_delta(&stale);
        assert_eq!(a.reranker.weights, b.reranker.weights);
        assert_eq!(b.stats().reranker_updates, 1);
        assert_eq!(b.semantic_memories.len(), 1);
        b.erase_word("会议");
        b.merge_crdt_delta(&stale);
        assert!(b.semantic_memories.is_empty());
        b.clear_learned_habits();
        a.merge_crdt_delta(&b.export_crdt_delta("b"));
        a.merge_crdt_delta(&stale);
        assert_eq!(a.reranker.weights, DEFAULT_RERANK_WEIGHTS);
        assert_eq!(a.reranker.updated_ms, b.learned_reset_ms);
        assert!(a.semantic_memories.is_empty());
        let mut invalid = stale;
        invalid.reranker.as_mut().unwrap().weights[0] = f64::NAN;
        invalid.reranker.as_mut().unwrap().updated_ms = u64::MAX;
        invalid.semantic_memories[0].vec_i8.clear();
        a.merge_crdt_delta(&invalid);
        assert_eq!(a.reranker.weights, DEFAULT_RERANK_WEIGHTS);
        assert!(a.semantic_memories.is_empty());
    }
}
