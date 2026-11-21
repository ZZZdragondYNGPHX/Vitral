pub mod dict_yaml;
pub mod polyphone;
pub mod scel;
pub mod xiaohe;

use crate::dav::put_lexicon_import_artifact;
use crate::error::VitralError;
use crate::offline_guard::OfflineGuard;
use dict_yaml::parse_dict_yaml;
use polyphone::{default_assets_dir_candidates, PolyphoneAnnotator, RimeDictCorpus};
use scel::parse_scel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use xiaohe::{normalize_syllable, pinyin_phrase_to_xiaohe, syllable_to_xiaohe};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexiconFormat {
    Auto,
    Scel,
    DictYaml,
    Tsv,
    Txt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedLexiconEntry {
    pub text: String,
    pub pinyin: String,
    pub toned_pinyin: String,
    pub xiaohe: String,
    pub weight: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub imported_count: usize,
    pub skipped_count: usize,
    pub format: LexiconFormat,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub entries: Vec<ImportedLexiconEntry>,
    #[serde(default)]
    pub sync_artifact_path: Option<String>,
    #[serde(default)]
    pub sync_remote_path: Option<String>,
    #[serde(default)]
    pub sync_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexiconImportArtifact {
    pub schema_version: u32,
    pub source_filename: String,
    pub format: LexiconFormat,
    pub sha256: String,
    pub entry_count: usize,
    pub entries: Vec<ImportedLexiconEntry>,
}

pub fn detect_format(path: &Path, hint: LexiconFormat) -> Result<LexiconFormat, VitralError> {
    if hint != LexiconFormat::Auto {
        return Ok(hint);
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| VitralError::InvalidArgument("invalid lexicon path".into()))?;
    if name.ends_with(".scel") {
        Ok(LexiconFormat::Scel)
    } else if name.ends_with(".dict.yaml") || name.ends_with(".yaml") {
        Ok(LexiconFormat::DictYaml)
    } else if name.ends_with(".tsv") {
        Ok(LexiconFormat::Tsv)
    } else if name.ends_with(".txt") {
        Ok(LexiconFormat::Txt)
    } else {
        Err(VitralError::Parse(format!(
            "cannot auto-detect lexicon format for '{name}'"
        )))
    }
}

const SKIP_BLOOM_WORDS: usize = 2048; // 2048 * 8B = 16 KiB
const SKIP_BLOOM_BITS: u64 = (SKIP_BLOOM_WORDS as u64) * 64;

#[inline]
fn pack_chars_u64(chars: &[char]) -> Option<u64> {
    if chars.is_empty() || chars.len() > 3 {
        return None;
    }
    let mut k = 0u64;
    for (i, &ch) in chars.iter().enumerate() {
        k |= ((ch as u64) + 1) << (i * 21);
    }
    Some(k)
}

#[inline]
fn pack_skipgram_u128(w1: &[char], w2: &[char]) -> Option<u128> {
    let k1 = pack_chars_u64(w1)?;
    let k2 = pack_chars_u64(w2)?;
    Some((k1 as u128) | ((k2 as u128) << 63))
}

#[inline]
fn pack_word_u128(chars: &[char]) -> Option<u128> {
    if chars.is_empty() || chars.len() > 5 {
        return None;
    }
    let mut k = 0u128;
    for (i, &ch) in chars.iter().enumerate() {
        k |= ((ch as u128) + 1) << (i * 21);
    }
    Some(k)
}

#[inline]
fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[inline]
fn hash_u128_slot(k: u128) -> usize {
    let x = mix64((k as u64) ^ mix64(((k >> 64) as u64) ^ 0x9e37_79b9_7f4a_7c15));
    x as usize
}

#[inline]
fn skip_bloom_hashes(k: u128) -> (u64, u64) {
    let lo = mix64(k as u64);
    let hi = mix64(((k >> 64) as u64) ^ 0x9e37_79b9_7f4a_7c15);
    let h1 = mix64(lo ^ hi);
    let h2 = mix64(lo.wrapping_add(hi.rotate_left(17)));
    (h1, h2)
}

#[derive(Default)]
struct FlatSkipTable {
    keys: Vec<u128>,
    vals: Vec<[f32; 6]>,
    mask: usize,
}

impl FlatSkipTable {
    fn rebuild(&mut self, entries: &[(u128, [f32; 6])]) {
        if entries.is_empty() {
            self.keys.clear();
            self.vals.clear();
            self.mask = 0;
            return;
        }
        let cap = (entries.len() * 2).next_power_of_two().max(64);
        self.keys.clear();
        self.keys.resize(cap, 0);
        self.vals.clear();
        self.vals.resize(cap, [0.0; 6]);
        self.mask = cap - 1;
        for &(k, v) in entries {
            let mut idx = hash_u128_slot(k) & self.mask;
            while self.keys[idx] != 0 && self.keys[idx] != k {
                idx = (idx + 1) & self.mask;
            }
            self.keys[idx] = k;
            self.vals[idx] = v;
        }
    }

    #[inline]
    fn get(&self, key: u128) -> Option<[f32; 6]> {
        if self.keys.is_empty() {
            return None;
        }
        let mut idx = hash_u128_slot(key) & self.mask;
        loop {
            let k = self.keys[idx];
            if k == key {
                return Some(self.vals[idx]);
            }
            if k == 0 {
                return None;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Default)]
struct FlatWordTable {
    keys: Vec<u128>,
    vals: Vec<f32>,
    mask: usize,
}

impl FlatWordTable {
    fn rebuild(&mut self, entries: &[(u128, f32)]) {
        if entries.is_empty() {
            self.keys.clear();
            self.vals.clear();
            self.mask = 0;
            return;
        }
        let cap = (entries.len() * 2).next_power_of_two().max(64);
        self.keys.clear();
        self.keys.resize(cap, 0);
        self.vals.clear();
        self.vals.resize(cap, 0.0);
        self.mask = cap - 1;
        for &(k, v) in entries {
            let mut idx = hash_u128_slot(k) & self.mask;
            while self.keys[idx] != 0 && self.keys[idx] != k {
                idx = (idx + 1) & self.mask;
            }
            self.keys[idx] = k;
            self.vals[idx] = v;
        }
    }

    #[inline]
    fn get(&self, key: u128) -> Option<f32> {
        if self.keys.is_empty() {
            return None;
        }
        let mut idx = hash_u128_slot(key) & self.mask;
        loop {
            let k = self.keys[idx];
            if k == key {
                return Some(self.vals[idx]);
            }
            if k == 0 {
                return None;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

pub struct ImportedLexiconIndex {
    assets_dir: Option<PathBuf>,
    corpus: OnceLock<Arc<RimeDictCorpus>>,
    entries: Vec<ImportedLexiconEntry>,
    by_quanpin: HashMap<String, Vec<ImportedLexiconEntry>>,
    by_xiaohe: HashMap<String, Vec<ImportedLexiconEntry>>,
    word_weights: HashMap<String, u32>,
    unigram_counts: HashMap<u64, f32>,
    skipgram_counts: HashMap<u128, [f32; 6]>,
    total_skipgram_mass: f32,
    skipgram_bloom: Vec<u64>,
    skipgram_pmi_table: FlatSkipTable,
    word_weights_fast: FlatWordTable,
}

impl ImportedLexiconIndex {
    pub fn new(assets_dir: Option<&Path>) -> Self {
        let resolved = assets_dir
            .filter(|p| p.join("rime").is_dir())
            .map(PathBuf::from)
            .or_else(default_assets_dir_candidates);
        Self {
            assets_dir: resolved,
            corpus: OnceLock::new(),
            entries: Vec::new(),
            by_quanpin: HashMap::new(),
            by_xiaohe: HashMap::new(),
            word_weights: HashMap::new(),
            unigram_counts: HashMap::new(),
            skipgram_counts: HashMap::new(),
            total_skipgram_mass: 0.0,
            skipgram_bloom: Vec::new(),
            skipgram_pmi_table: FlatSkipTable::default(),
            word_weights_fast: FlatWordTable::default(),
        }
    }

    pub fn corpus(&self) -> &Arc<RimeDictCorpus> {
        self.corpus
            .get_or_init(|| RimeDictCorpus::load_or_fallback(self.assets_dir.as_deref()))
    }

    pub fn export_entries(&self) -> Vec<ImportedLexiconEntry> {
        self.entries.clone()
    }

    pub fn inject_hotwords(&mut self, new_entries: &[ImportedLexiconEntry]) {
        if new_entries.is_empty() {
            return;
        }
        for entry in new_entries {
            let mut is_new_entry = true;
            if let Some(existing) = self
                .entries
                .iter_mut()
                .find(|e| e.text == entry.text && e.pinyin == entry.pinyin)
            {
                is_new_entry = false;
                if entry.weight > existing.weight {
                    *existing = entry.clone();
                }
            } else {
                self.entries.push(entry.clone());
            }

            let compact_py: String = entry
                .pinyin
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("");
            if !compact_py.is_empty() {
                let list = self.by_quanpin.entry(compact_py).or_default();
                if let Some(existing) = list.iter_mut().find(|e| e.text == entry.text) {
                    if entry.weight > existing.weight {
                        *existing = entry.clone();
                    }
                } else {
                    list.push(entry.clone());
                }
                list.sort_by_key(|a| std::cmp::Reverse(a.weight));
            }

            if !entry.xiaohe.is_empty() {
                let list = self.by_xiaohe.entry(entry.xiaohe.clone()).or_default();
                if let Some(existing) = list.iter_mut().find(|e| e.text == entry.text) {
                    if entry.weight > existing.weight {
                        *existing = entry.clone();
                    }
                } else {
                    list.push(entry.clone());
                }
                list.sort_by_key(|a| std::cmp::Reverse(a.weight));
            }

            self.word_weights
                .entry(entry.text.clone())
                .and_modify(|w| *w = (*w).max(entry.weight))
                .or_insert(entry.weight);

            if !is_new_entry {
                continue;
            }

            // Populate Skip-gram PMI counts (distance 1..=6) across substrings of imported multi-char entries
            let chars: Vec<char> = entry.text.chars().collect();
            let n = chars.len();
            if n >= 2 {
                let mass = ((entry.weight as f32).max(1.0)).ln() + 1.0;
                for i in 0..n {
                    for l1 in 1..=3.min(n - i) {
                        let Some(k1) = pack_chars_u64(&chars[i..i + l1]) else {
                            continue;
                        };
                        *self.unigram_counts.entry(k1).or_insert(0.0) += mass;
                        let start2 = i + l1;
                        for k in start2..n {
                            let dist = (k - start2) + 1;
                            if dist > 6 {
                                break;
                            }
                            for l2 in 1..=3.min(n - k) {
                                if let Some(k12) =
                                    pack_skipgram_u128(&chars[i..i + l1], &chars[k..k + l2])
                                {
                                    let arr = self.skipgram_counts.entry(k12).or_insert([0.0; 6]);
                                    arr[dist - 1] += mass;
                                    self.total_skipgram_mass += mass;
                                }
                            }
                        }
                    }
                }
            }
        }

        self.rebuild_fast_tables();
    }

    fn rebuild_fast_tables(&mut self) {
        if self.skipgram_counts.is_empty() || self.total_skipgram_mass <= 0.0 {
            self.skipgram_bloom.clear();
            self.skipgram_pmi_table.rebuild(&[]);
        } else {
            if self.skipgram_bloom.len() != SKIP_BLOOM_WORDS {
                self.skipgram_bloom.resize(SKIP_BLOOM_WORDS, 0);
            } else {
                self.skipgram_bloom.fill(0);
            }
            let k1_mask = (1u128 << 63) - 1;
            let mut pmi_entries = Vec::with_capacity(self.skipgram_counts.len());
            for (&k12, joints) in &self.skipgram_counts {
                let k1 = (k12 & k1_mask) as u64;
                let k2 = (k12 >> 63) as u64;
                let u1 = self.unigram_counts.get(&k1).copied().unwrap_or(1.0).max(1.0);
                let u2 = self.unigram_counts.get(&k2).copied().unwrap_or(1.0).max(1.0);
                let mut pmis = [0.0f32; 6];
                let mut any = false;
                for (d_idx, &joint) in joints.iter().enumerate() {
                    if joint > 0.0 {
                        let ratio = (joint * self.total_skipgram_mass) / (u1 * u2);
                        let pmi = ratio.max(1.0).ln() + (joint / 10.0).min(2.0);
                        pmis[d_idx] = pmi / ((d_idx + 1) as f32);
                        any = true;
                    }
                }
                if any {
                    let (h1, h2) = skip_bloom_hashes(k12);
                    for probe in 0..3u64 {
                        let bit = h1.wrapping_add(probe.wrapping_mul(h2)) & (SKIP_BLOOM_BITS - 1);
                        self.skipgram_bloom[(bit >> 6) as usize] |= 1u64 << (bit & 63);
                    }
                    pmi_entries.push((k12, pmis));
                }
            }
            self.skipgram_pmi_table.rebuild(&pmi_entries);
        }

        let mut word_entries = Vec::with_capacity(self.word_weights.len());
        let mut tmp_chars = ['\0'; 5];
        for (w_str, &w) in &self.word_weights {
            let mut cnt = 0usize;
            let mut fit = true;
            for ch in w_str.chars() {
                if cnt >= 5 {
                    fit = false;
                    break;
                }
                tmp_chars[cnt] = ch;
                cnt += 1;
            }
            if fit && cnt >= 1 {
                if let Some(k) = pack_word_u128(&tmp_chars[..cnt]) {
                    word_entries.push((k, 10.0 + ((w as f32).max(1.0)).ln()));
                }
            }
        }
        self.word_weights_fast.rebuild(&word_entries);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn has_embedded_hotword_key(&self, compact_input: &str, is_xiaohe: bool) -> bool {
        if self.entries.is_empty() || compact_input.is_empty() {
            return false;
        }
        if is_xiaohe {
            self.by_xiaohe.iter().any(|(k, v)| {
                !k.is_empty()
                    && k.len() < compact_input.len()
                    && v.iter().any(|e| e.text.chars().count() >= 2)
                    && compact_input
                        .match_indices(k.as_str())
                        .any(|(idx, _)| idx % 2 == 0)
            })
        } else {
            self.by_quanpin.iter().any(|(k, v)| {
                k.len() >= 3
                    && k.len() < compact_input.len()
                    && v.iter().any(|e| e.text.chars().count() >= 2)
                    && compact_input.contains(k.as_str())
            })
        }
    }

    /// An imported pronunciation takes precedence over the base dictionary for this word.
    pub fn matches_reading(&self, word: &str, input: &str, xiaohe: bool) -> bool {
        if !self.word_weights.contains_key(word) { return true; }
        let compact = input.replace([' ', '\''], "");
        self.entries.iter().filter(|e| e.text == word).any(|e| {
            let key = if xiaohe { e.xiaohe.clone() } else { e.pinyin.replace(' ', "") };
            !key.is_empty() && (compact.starts_with(&key) || key.starts_with(&compact))
        })
    }

    pub fn contains_hotword(&self, candidate_text: &str) -> bool {
        if self.word_weights.is_empty() || candidate_text.is_empty() {
            return false;
        }
        self.word_weights.iter().any(|(w, _)| {
            w.chars().count() >= 2 && candidate_text.contains(w.as_str())
        })
    }

    pub fn max_hotword_chars(&self) -> usize {
        self.word_weights
            .keys()
            .map(|w| w.chars().count())
            .max()
            .unwrap_or(0)
    }

    pub fn export_hotwords(&self) -> Vec<ImportedLexiconEntry> {
        self.entries.clone()
    }

    pub fn query_quanpin(&self, compact_py: &str) -> Vec<ImportedLexiconEntry> {
        self.by_quanpin
            .get(compact_py)
            .cloned()
            .unwrap_or_default()
    }

    pub fn query_xiaohe(&self, xiaohe_code: &str) -> Vec<ImportedLexiconEntry> {
        self.by_xiaohe
            .get(xiaohe_code)
            .cloned()
            .unwrap_or_default()
    }

    /// Imported-hotword-only lookup (without falling back to base corpus).
    pub fn lookup_imported_score(&self, word: &str) -> Option<f32> {
        self.word_weights
            .get(word)
            .map(|&w| 10.0 + ((w as f32).max(1.0)).ln())
    }

    /// Zero-alloc imported-hotword-only lookup by `&[char]`.
    #[inline]
    pub fn lookup_imported_score_chars(&self, chars: &[char]) -> Option<f32> {
        if self.word_weights.is_empty() || chars.is_empty() {
            return None;
        }
        if let Some(k) = pack_word_u128(chars) {
            return self.word_weights_fast.get(k);
        }
        let s: String = chars.iter().collect();
        self.lookup_imported_score(&s)
    }

    /// True if multi-char imported hotwords populated skip-gram PMI counts.
    pub fn has_skipgrams(&self) -> bool {
        self.total_skipgram_mass > 0.0 && !self.skipgram_counts.is_empty()
    }

    /// Zero-alloc skip-gram PMI lookup across distances `1..=6` (`1 <= l1, l2 <= 3`).
    #[inline]
    pub fn skipgram_pmi_chars(&self, w1: &[char], w2: &[char]) -> Option<[f32; 6]> {
        if self.skipgram_bloom.is_empty() {
            return None;
        }
        let k12 = pack_skipgram_u128(w1, w2)?;
        let (h1, h2) = skip_bloom_hashes(k12);
        for probe in 0..3u64 {
            let bit = h1.wrapping_add(probe.wrapping_mul(h2)) & (SKIP_BLOOM_BITS - 1);
            if (self.skipgram_bloom[(bit >> 6) as usize] & (1u64 << (bit & 63))) == 0 {
                return None;
            }
        }
        self.skipgram_pmi_table.get(k12)
    }

    /// Fast `1..=2` x `1..=2` skip-gram PMI lookup for Mode-2 Tier-1 backtracking.
    #[inline]
    pub fn skipgram_pmis_2x2(&self, w1: &[char], w2: &[char]) -> Option<[f32; 6]> {
        if !(1..=2).contains(&w1.len()) || !(1..=2).contains(&w2.len()) {
            return None;
        }
        self.skipgram_pmi_chars(w1, w2)
    }

    /// Word score hook for continuous handwriting beam search (boosts imported hotwords over base corpus).
    pub fn lookup_word_score(&self, word: &str) -> Option<f32> {
        if let Some(&w) = self.word_weights.get(word) {
            return Some(10.0 + ((w as f32).max(1.0)).ln());
        }
        let corpus = self.corpus();
        if let Some((_, w)) = corpus.word_readings.get(word) {
            return Some(((*w as f32).max(1.0)).ln());
        }
        if word.chars().count() == 1 {
            let ch = word.chars().next()?;
            if let Some(list) = corpus.char_readings.get(&ch) {
                if let Some((_, w)) = list.first() {
                    return Some(((*w as f32).max(1.0)).ln() * 0.8);
                }
            }
        }
        None
    }

    /// Zero-alloc skip-gram PMI score (`1 <= distance <= 6`) for continuous handwriting beam search.
    pub fn bigram_pmi_score(&self, w1: &str, w2: &str, distance: usize) -> f32 {
        if !(1..=6).contains(&distance) {
            return 0.0;
        }
        let mut c1 = ['\0'; 3];
        let mut l1 = 0usize;
        for ch in w1.chars() {
            if l1 >= 3 {
                l1 = 4;
                break;
            }
            c1[l1] = ch;
            l1 += 1;
        }
        let mut c2 = ['\0'; 3];
        let mut l2 = 0usize;
        for ch in w2.chars() {
            if l2 >= 3 {
                l2 = 4;
                break;
            }
            c2[l2] = ch;
            l2 += 1;
        }
        if (1..=3).contains(&l1) && (1..=3).contains(&l2) {
            if let Some(pmis) = self.skipgram_pmi_chars(&c1[..l1], &c2[..l2]) {
                let pmi = pmis[distance - 1];
                if pmi > 0.0 {
                    return pmi;
                }
            }
        }
        if distance == 1 {
            if let Some(w) = self.corpus().lookup_joined_str(w1, w2) {
                return (((w as f32).max(1.0)).ln() * 0.25).max(0.5);
            }
        }
        0.0
    }

    /// Phonetic lookup hook for single-character phonetic lookup (`(toned_pinyin, xiaohe)`).
    pub fn char_phonetics(&self, ch: char) -> Vec<(String, String)> {
        let annotator = PolyphoneAnnotator::new(Arc::clone(self.corpus()));
        annotator.char_phonetics(ch)
    }
}

fn is_valid_lexicon_text(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let char_count = t.chars().count();
    if char_count > 64 {
        return false;
    }
    let mut has_cjk = false;
    let mut has_ascii_alpha = false;
    for ch in t.chars() {
        if ch.is_control() {
            return false;
        }
        let cp = ch as u32;
        if (0x1F000..=0x1FFFF).contains(&cp) || (0x2600..=0x27BF).contains(&cp) {
            return false; // reject emoji / pictographs
        }
        if is_cjk(ch) {
            has_cjk = true;
        } else if ch.is_ascii_alphabetic() {
            has_ascii_alpha = true;
        } else if ch.is_ascii_digit() || ch == '·' || ch == ' ' {
            // reject digit-prefixed tokens like "12345数字词"
            if ch.is_ascii_digit() {
                return false;
            }
        } else {
            return false;
        }
    }
    has_cjk || has_ascii_alpha
}

fn is_cjk(ch: char) -> bool {
    let cp = ch as u32;
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
}

pub(crate) fn is_valid_pinyin_code(code: &str) -> bool {
    let tokens: Vec<&str> = code.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    tokens.iter().all(|tok| {
        let mut has_alpha = false;
        let ok = tok.chars().all(|c| {
            if c.is_ascii_alphabetic()
                || matches!(
                    c,
                    'ü' | 'Ü'
                        | 'ā'
                        | 'á'
                        | 'ǎ'
                        | 'à'
                        | 'ē'
                        | 'é'
                        | 'ě'
                        | 'è'
                        | 'ī'
                        | 'í'
                        | 'ǐ'
                        | 'ì'
                        | 'ō'
                        | 'ó'
                        | 'ǒ'
                        | 'ò'
                        | 'ū'
                        | 'ú'
                        | 'ǔ'
                        | 'ù'
                        | 'ǖ'
                        | 'ǘ'
                        | 'ǚ'
                        | 'ǜ'
                )
            {
                has_alpha = true;
                true
            } else {
                matches!(c, '1'..='5' | '\'')
            }
        });
        ok && has_alpha
    })
}

#[derive(Clone)]
pub struct SyncContext<'a> {
    pub guard: &'a OfflineGuard,
    pub data_dir: &'a str,
    pub koofr_endpoint: Option<&'a str>,
    pub koofr_username: Option<&'a str>,
    pub koofr_password: Option<&'a str>,
    pub koofr_root: Option<&'a str>,
}

impl std::fmt::Debug for SyncContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncContext")
            .field("guard", &self.guard)
            .field("data_dir", &self.data_dir)
            .field("koofr_endpoint", &self.koofr_endpoint)
            .field("koofr_username", &self.koofr_username)
            .field(
                "koofr_password",
                &self.koofr_password.map(|_| "[REDACTED]"),
            )
            .field("koofr_root", &self.koofr_root)
            .finish()
    }
}

pub struct CompiledLexiconImport {
    pub format: LexiconFormat,
    pub skipped_count: usize,
    pub warnings: Vec<String>,
    pub entries: Vec<ImportedLexiconEntry>,
    pub sync_sha256: Option<String>,
    short_hex: String,
    filename: String,
    canonical_json: Vec<u8>,
    cbor_bytes: Vec<u8>,
}

pub fn compile_lexicon_file(
    corpus: Arc<RimeDictCorpus>,
    path: &Path,
    format_hint: LexiconFormat,
) -> Result<CompiledLexiconImport, VitralError> {
    let format = detect_format(path, format_hint)?;
    if !path.exists() {
        return Err(VitralError::NotFound(format!(
            "lexicon file not found: {}",
            path.display()
        )));
    }

    let raw_bytes = fs::read(path).map_err(|e| VitralError::Io(e.to_string()))?;
    let annotator = PolyphoneAnnotator::new(corpus);

    let mut entries: Vec<ImportedLexiconEntry> = Vec::new();
    let mut skipped_count = 0usize;
    let mut warnings = Vec::new();

    match format {
        LexiconFormat::Scel => {
            let raw = parse_scel(&raw_bytes)?;
            for item in raw {
                if !is_valid_lexicon_text(&item.text) || item.syllables.is_empty() {
                    skipped_count += 1;
                    warnings.push(format!("skipped invalid .scel entry '{}'", item.text));
                    continue;
                }
                let ann = annotator.annotate_with_untoned_hint(&item.text, &item.syllables);
                entries.push(ImportedLexiconEntry {
                    text: item.text,
                    pinyin: ann.untoned,
                    toned_pinyin: ann.toned,
                    xiaohe: ann.xiaohe,
                    weight: item.weight,
                });
            }
        }
        LexiconFormat::DictYaml => {
            let text_content = std::str::from_utf8(&raw_bytes)
                .map_err(|_| VitralError::Parse("non-UTF-8 content in .dict.yaml".into()))?;
            let parsed = parse_dict_yaml(text_content)?;
            skipped_count += parsed.skipped_count;
            warnings.extend(parsed.warnings);
            for item in parsed.entries {
                if !is_valid_lexicon_text(&item.text) {
                    skipped_count += 1;
                    warnings.push(format!("skipped invalid .dict.yaml entry '{}'", item.text));
                    continue;
                }
                let ann = match item.syllables {
                    Some(syls) if !syls.is_empty() => {
                        annotator.annotate_with_untoned_hint(&item.text, &syls)
                    }
                    _ => annotator.annotate_text(&item.text),
                };
                entries.push(ImportedLexiconEntry {
                    text: item.text,
                    pinyin: ann.untoned,
                    toned_pinyin: ann.toned,
                    xiaohe: ann.xiaohe,
                    weight: item.weight,
                });
            }
        }
        LexiconFormat::Tsv => {
            if raw_bytes.contains(&0) {
                return Err(VitralError::Parse("binary NUL byte in TSV lexicon".into()));
            }
            let text_content = std::str::from_utf8(&raw_bytes)
                .map_err(|_| VitralError::Parse("non-UTF-8 content in TSV lexicon".into()))?;
            parse_tsv_lines(
                text_content,
                &annotator,
                &mut entries,
                &mut skipped_count,
                &mut warnings,
            );
        }
        LexiconFormat::Txt => {
            if raw_bytes.contains(&0) {
                return Err(VitralError::Parse("binary NUL byte in TXT lexicon".into()));
            }
            let text_content = std::str::from_utf8(&raw_bytes)
                .map_err(|_| VitralError::Parse("non-UTF-8 content in TXT lexicon".into()))?;

            let has_tabs = text_content.lines().any(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with('#') && l.contains('\t')
            });

            if has_tabs {
                parse_tsv_lines(
                    text_content,
                    &annotator,
                    &mut entries,
                    &mut skipped_count,
                    &mut warnings,
                );
            } else {
                for (idx, raw_line) in text_content.lines().enumerate() {
                    let trimmed = raw_line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    if !is_valid_lexicon_text(trimmed) || !trimmed.chars().all(is_cjk) {
                        skipped_count += 1;
                        warnings.push(format!(
                            "txt line {}: skipped non-CJK or malformed line",
                            idx + 1
                        ));
                        continue;
                    }
                    let ann = annotator.annotate_text(trimmed);
                    entries.push(ImportedLexiconEntry {
                        text: trimmed.to_string(),
                        pinyin: ann.untoned,
                        toned_pinyin: ann.toned,
                        xiaohe: ann.xiaohe,
                        weight: 500,
                    });
                }
            }
        }
        LexiconFormat::Auto => unreachable!("detect_format resolves Auto"),
    }

    // Deduplicate identical (text, pinyin) keeping highest weight while preserving relative order
    let mut best_by_key: HashMap<(String, String), usize> = HashMap::new();
    let mut deduped: Vec<ImportedLexiconEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = (entry.text.clone(), entry.pinyin.clone());
        if let Some(&pos) = best_by_key.get(&key) {
            if entry.weight > deduped[pos].weight {
                deduped[pos] = entry;
            }
        } else {
            best_by_key.insert(key, deduped.len());
            deduped.push(entry);
        }
    }

    let mut sync_sha256 = None;
    let mut short_hex = String::new();
    let mut filename = String::new();
    let mut canonical_json = Vec::new();
    let mut cbor_bytes = Vec::new();

    if !deduped.is_empty() {
        let source_filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("lexicon")
            .to_string();

        // Deterministic hash over canonical JSON of entries
        canonical_json =
            serde_json::to_vec(&deduped).map_err(|e| VitralError::Internal(e.to_string()))?;
        let digest = Sha256::digest(&canonical_json);
        let sha_hex = hex_encode(&digest);
        short_hex = sha_hex[..16].to_string();

        let artifact = LexiconImportArtifact {
            schema_version: 1,
            source_filename,
            format,
            sha256: sha_hex.clone(),
            entry_count: deduped.len(),
            entries: deduped.clone(),
        };

        ciborium::into_writer(&artifact, &mut cbor_bytes)
            .map_err(|e| VitralError::Internal(format!("CBOR encode failed: {e}")))?;

        filename = format!("import_{short_hex}.cbor");
        sync_sha256 = Some(sha_hex);
    }

    let _ = HashSet::<String>::new();
    Ok(CompiledLexiconImport {
        format,
        skipped_count,
        warnings,
        entries: deduped,
        sync_sha256,
        short_hex,
        filename,
        canonical_json,
        cbor_bytes,
    })
}

pub fn finalize_compiled_lexicon_sync(
    mut compiled: CompiledLexiconImport,
    sync_ctx: Option<SyncContext<'_>>,
) -> ImportReport {
    let mut sync_artifact_path = None;
    let mut sync_remote_path = None;

    if !compiled.entries.is_empty() {
        if let Some(ctx) = sync_ctx {
            let base_dir = if !ctx.data_dir.trim().is_empty() {
                PathBuf::from(ctx.data_dir)
            } else {
                std::env::temp_dir().join("vitral-data")
            };
            let imports_dir = base_dir.join("lexicon").join("imports");
            if fs::create_dir_all(&imports_dir).is_ok() {
                let local_cbor = imports_dir.join(&compiled.filename);
                if fs::write(&local_cbor, &compiled.cbor_bytes).is_ok() {
                    sync_artifact_path = Some(local_cbor.to_string_lossy().into_owned());
                }
                let local_json = imports_dir.join(format!("import_{}.json", compiled.short_hex));
                let _ = fs::write(&local_json, &compiled.canonical_json);
            }

            if let Some(ep) = ctx.koofr_endpoint.filter(|s| !s.trim().is_empty()) {
                match put_lexicon_import_artifact(
                    ctx.guard,
                    ep,
                    ctx.koofr_username,
                    ctx.koofr_password,
                    ctx.koofr_root,
                    &compiled.filename,
                    &compiled.cbor_bytes,
                ) {
                    Ok(remote) => {
                        sync_remote_path = Some(remote);
                    }
                    Err(e @ VitralError::PureOfflineViolation(_)) => {
                        compiled
                            .warnings
                            .push(format!("remote import sync blocked: {e}"));
                    }
                    Err(e) => {
                        compiled
                            .warnings
                            .push(format!("remote import sync skipped: {e}"));
                    }
                }
            }
        }
    }

    ImportReport {
        imported_count: compiled.entries.len(),
        skipped_count: compiled.skipped_count,
        format: compiled.format,
        warnings: compiled.warnings,
        entries: compiled.entries,
        sync_artifact_path,
        sync_remote_path,
        sync_sha256: compiled.sync_sha256,
    }
}

pub fn compile_and_index(
    index: &mut ImportedLexiconIndex,
    path: &Path,
    format_hint: LexiconFormat,
    sync_ctx: Option<SyncContext<'_>>,
) -> Result<ImportReport, VitralError> {
    let corpus = Arc::clone(index.corpus());
    let compiled = compile_lexicon_file(corpus, path, format_hint)?;
    index.inject_hotwords(&compiled.entries);
    Ok(finalize_compiled_lexicon_sync(compiled, sync_ctx))
}

fn parse_tsv_lines(
    content: &str,
    annotator: &PolyphoneAnnotator,
    entries: &mut Vec<ImportedLexiconEntry>,
    skipped_count: &mut usize,
    warnings: &mut Vec<String>,
) {
    for (idx, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = raw_line.split('\t').collect();
        if cols.len() != 2 && cols.len() != 3 {
            *skipped_count += 1;
            warnings.push(format!(
                "tsv line {}: expected 2 or 3 tab-separated columns, got {}",
                idx + 1,
                cols.len()
            ));
            continue;
        }
        let text = cols[0].trim();
        let code = cols[1].trim();
        if !is_valid_lexicon_text(text) || !is_valid_pinyin_code(code) {
            *skipped_count += 1;
            warnings.push(format!(
                "tsv line {}: invalid text or pinyin '{}'",
                idx + 1,
                text
            ));
            continue;
        }
        let weight = if cols.len() == 3 {
            match cols[2].trim().parse::<u32>() {
                Ok(w) => w.max(1),
                Err(_) => {
                    *skipped_count += 1;
                    warnings.push(format!(
                        "tsv line {}: invalid non-negative integer weight '{}'",
                        idx + 1,
                        cols[2].trim()
                    ));
                    continue;
                }
            }
        } else {
            100
        };

        let syls: Vec<String> = code
            .split_whitespace()
            .map(normalize_syllable)
            .filter(|s| !s.is_empty())
            .collect();
        if syls.is_empty() {
            *skipped_count += 1;
            warnings.push(format!("tsv line {}: empty pinyin syllables", idx + 1));
            continue;
        }

        let ann = annotator.annotate_with_untoned_hint(text, &syls);
        let xiaohe = if ann.xiaohe.is_empty() {
            pinyin_phrase_to_xiaohe(&ann.untoned)
        } else {
            ann.xiaohe
        };
        let _ = syllable_to_xiaohe;
        entries.push(ImportedLexiconEntry {
            text: text.to_string(),
            pinyin: ann.untoned,
            toned_pinyin: ann.toned,
            xiaohe,
            weight,
        });
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
