use super::lattice::LatticeSlot;
use super::onnx_models::HwOnnxBundle;
use super::Mode2Slot;
use crate::eng::gram::{encode_char, EncodedChar, RimeGramDb};
use crate::error::VitralError;
use crate::imp::polyphone::RimeDictCorpus;
use crate::imp::ImportedLexiconIndex;
use crate::offline_guard::OfflineGuard;
use crate::ret::RetentionEngine;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

const BEAM_WIDTH: usize = 16;
const MAX_STACK_N: usize = 32;
const MAX_TIER2_SLOTS: usize = 3;
const W_VISUAL: f32 = 2.2;
const W_CORPUS: f32 = 0.85;
const W_GRAM: f32 = 1.0;
const W_IMPORTED: f32 = 0.35;
const W_SKIP: f32 = 1.5;
const W_MLM: f32 = 0.6;

const CACHE_SZ: usize = 32768;
const CACHE_MASK: usize = CACHE_SZ - 1;
const PREFIX_CACHE_SZ: usize = 8192;
const PREFIX_CACHE_MASK: usize = PREFIX_CACHE_SZ - 1;
const STEP_BATCH_SZ: usize = 512;
const STEP_BATCH_MASK: usize = STEP_BATCH_SZ - 1;

#[derive(Clone, Copy)]
struct CandExt {
    total_score: f32,
    dp_end: f32,
    vis_score: f32,
    skip_score: f32,
    hyp_idx: u16,
    cand_idx: u8,
    best_start: u8,
    ch: char,
}

#[derive(Clone, Copy)]
struct DynCandExt {
    total_score: f32,
    dp_end: f32,
    vis_score: f32,
    skip_score: f32,
    hyp_idx: usize,
    cand_idx: usize,
    best_start: usize,
    ch: char,
}

#[derive(Clone, Copy)]
struct SlotCands {
    chars: [char; 8],
    enc: [EncodedChar; 8],
    base_scores: [f32; 8],
    vis_lp: [f32; 8],
    cnt: usize,
    active_mask: u8,
    is_empty_slot: bool,
}

#[derive(Debug, Clone)]
pub struct Tier3ElasticSlot {
    pub slot_idx: usize,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Tier3SlotRequest {
    pub context_chars: Vec<String>,
    pub elastic_slots: Vec<Tier3ElasticSlot>,
}

/// Selects among the existing candidates of an ambiguous handwriting slot via BYOK.
pub trait Tier3SlotDisambiguator: Send + Sync {
    fn disambiguate_slots(
        &self,
        req: &Tier3SlotRequest,
    ) -> Result<Vec<(usize, usize)>, VitralError>;
}

pub struct HwDecodeContext<'a> {
    pub imported: &'a ImportedLexiconIndex,
    pub gram: Option<&'a RimeGramDb>,
    pub retention: &'a RetentionEngine,
    pub tier3: Option<&'a dyn Tier3SlotDisambiguator>,
    pub offline_guard: Option<&'a OfflineGuard>,
}

#[derive(Clone, Copy)]
struct StackBeamHyp {
    chosen_indices: [u8; MAX_STACK_N],
    chars: [char; MAX_STACK_N],
    dp: [f32; MAX_STACK_N + 1],
    prev_start: [u8; MAX_STACK_N + 1],
    prev_end: [u8; MAX_STACK_N + 1],
    len: usize,
    vis_score: f32,
    skip_score: f32,
    total_score: f32,
}

impl StackBeamHyp {
    fn new() -> Self {
        let mut dp = [f32::NEG_INFINITY; MAX_STACK_N + 1];
        dp[0] = 0.0;
        Self {
            chosen_indices: [0; MAX_STACK_N],
            chars: ['\0'; MAX_STACK_N],
            dp,
            prev_start: [0; MAX_STACK_N + 1],
            prev_end: [0; MAX_STACK_N + 1],
            len: 0,
            vis_score: 0.0,
            skip_score: 0.0,
            total_score: 0.0,
        }
    }
}

#[derive(Clone)]
struct DynBeamHyp {
    chosen_indices: Vec<usize>,
    chars: Vec<char>,
    dp: Vec<f32>,
    prev_start: Vec<usize>,
    prev_end: Vec<usize>,
    vis_score: f32,
    skip_score: f32,
    total_score: f32,
}

impl DynBeamHyp {
    fn with_capacity(n: usize) -> Self {
        let mut dp = Vec::with_capacity(n + 1);
        dp.push(0.0);
        let mut prev_start = Vec::with_capacity(n + 1);
        prev_start.push(0);
        let mut prev_end = Vec::with_capacity(n + 1);
        prev_end.push(0);
        Self {
            chosen_indices: Vec::with_capacity(n),
            chars: Vec::with_capacity(n),
            dp,
            prev_start,
            prev_end,
            vis_score: 0.0,
            skip_score: 0.0,
            total_score: 0.0,
        }
    }
}

#[derive(Clone, Copy)]
struct CacheSlot {
    key: u128,
    val: [f32; 2],
    epoch: u32,
    has_skip: bool,
}

#[derive(Clone, Copy)]
struct PrefixSlot {
    key: u128,
    val: Option<(usize, u32)>,
    epoch: u32,
}

#[derive(Clone, Copy)]
struct StepBatchSlot {
    key: u128,
    v0: [f32; 8],
    v1: [f32; 8],
    step_epoch: u32,
    skip_mask: u8,
}

struct FastCaches {
    main: Vec<CacheSlot>,
    prefix: Vec<PrefixSlot>,
    step_b8: Vec<StepBatchSlot>,
    epoch: u32,
    step_epoch: u32,
}

impl FastCaches {
    fn new() -> Self {
        Self {
            main: vec![
                CacheSlot {
                    key: 0,
                    val: [0.0; 2],
                    epoch: 0,
                    has_skip: false,
                };
                CACHE_SZ
            ],
            prefix: vec![
                PrefixSlot {
                    key: 0,
                    val: None,
                    epoch: 0,
                };
                PREFIX_CACHE_SZ
            ],
            step_b8: vec![
                StepBatchSlot {
                    key: 0,
                    v0: [0.0; 8],
                    v1: [0.0; 8],
                    step_epoch: 0,
                    skip_mask: 0,
                };
                STEP_BATCH_SZ
            ],
            epoch: 0,
            step_epoch: 0,
        }
    }
}

thread_local! {
    static TIER1_CACHE: RefCell<FastCaches> = RefCell::new(FastCaches::new());
}

#[inline]
fn pack_u128(kind: u8, meta: u8, chars: &[char]) -> u128 {
    let mut k: u128 = ((kind as u128) << 125) | ((meta as u128) << 116);
    for (idx, &ch) in chars.iter().enumerate() {
        k |= (ch as u128) << (idx * 21);
    }
    k
}

#[inline]
fn hash_u128_raw(k: u128) -> usize {
    let x = (k as u64) ^ ((k >> 64) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let y = x ^ (x >> 30);
    y.wrapping_mul(0xbf58_476d_1ce4_e5b9) as usize
}

#[inline]
fn chars_to_str<'a>(chars: &[char], buf: &'a mut [u8; 32]) -> &'a str {
    let mut pos = 0usize;
    for &c in chars {
        let s = c.encode_utf8(&mut buf[pos..]);
        pos += s.len();
    }
    // SAFETY: `encode_utf8` writes valid UTF-8 into `buf[..pos]`.
    unsafe { std::str::from_utf8_unchecked(&buf[..pos]) }
}

struct ScoreCtx<'a> {
    imported: &'a ImportedLexiconIndex,
    corpus: &'a Arc<RimeDictCorpus>,
    gram: Option<&'a RimeGramDb>,
    retention: &'a RetentionEngine,
    now_ms: u64,
    has_skipgrams: bool,
    has_retention: bool,
    caches: &'a mut FastCaches,
    epoch: u32,
    step_epoch: u32,
}

impl<'a> ScoreCtx<'a> {
    fn new(
        imported: &'a ImportedLexiconIndex,
        gram: Option<&'a RimeGramDb>,
        retention: &'a RetentionEngine,
        caches: &'a mut FastCaches,
    ) -> Self {
        caches.epoch = caches.epoch.wrapping_add(1);
        if caches.epoch == 0 {
            for s in &mut caches.main {
                s.epoch = 0;
            }
            for s in &mut caches.prefix {
                s.epoch = 0;
            }
            caches.epoch = 1;
        }
        caches.step_epoch = caches.step_epoch.wrapping_add(1);
        if caches.step_epoch == 0 {
            for s in &mut caches.step_b8 {
                s.step_epoch = 0;
            }
            caches.step_epoch = 1;
        }
        let epoch = caches.epoch;
        let step_epoch = caches.step_epoch;
        Self {
            imported,
            corpus: imported.corpus(),
            gram,
            retention,
            now_ms: RetentionEngine::now_epoch_ms(),
            has_skipgrams: imported.has_skipgrams(),
            has_retention: !retention.frecency.is_empty()
                || !retention.pinned_words.is_empty()
                || !retention.erased_words.is_empty(),
            caches,
            epoch,
            step_epoch,
        }
    }

    #[inline]
    fn next_step_epoch(&mut self) {
        self.caches.step_epoch = self.caches.step_epoch.wrapping_add(1);
        if self.caches.step_epoch == 0 {
            for s in &mut self.caches.step_b8 {
                s.step_epoch = 0;
            }
            self.caches.step_epoch = 1;
        }
        self.step_epoch = self.caches.step_epoch;
    }

    #[inline]
    fn retention_delta(&self, chars: &[char]) -> f32 {
        let mut wbuf = [0u8; 32];
        let w_str = chars_to_str(chars, &mut wbuf);
        let mut delta = 0.0f32;
        let frec = self.retention.decayed_score_at(w_str, self.now_ms);
        if frec > 0.0 {
            delta += 1.5 * (frec as f32);
        }
        if self.retention.pinned_words.contains(w_str) {
            delta += 3.0;
        }
        if self.retention.erased_words.contains(w_str) {
            delta -= 5.0;
        }
        delta
    }

    fn gram_prefix_state(&mut self, prefix: &[char]) -> Option<(usize, u32)> {
        let g = self.gram?;
        if prefix.is_empty() || prefix.len() > 5 {
            return None;
        }
        let key = pack_u128(6, prefix.len() as u8, prefix);
        let base = hash_u128_raw(key) & PREFIX_CACHE_MASK;
        for step in 0..4 {
            let idx = (base + step) & PREFIX_CACHE_MASK;
            let s = self.caches.prefix[idx];
            if s.epoch == self.epoch && s.key == key {
                return s.val;
            }
            if s.epoch != self.epoch {
                break;
            }
        }

        let st = if prefix.len() >= 2 {
            if let Some((np, u)) = self.gram_prefix_state(&prefix[..prefix.len() - 1]) {
                g.advance_state_chars(np, u, &prefix[prefix.len() - 1..])
            } else {
                None
            }
        } else {
            g.prefix_state_chars(prefix)
        };

        for step in 0..4 {
            let idx = (base + step) & PREFIX_CACHE_MASK;
            let s = &mut self.caches.prefix[idx];
            if s.epoch != self.epoch || s.key == key {
                s.key = key;
                s.val = st;
                s.epoch = self.epoch;
                return st;
            }
        }
        self.caches.prefix[base] = PrefixSlot {
            key,
            val: st,
            epoch: self.epoch,
        };
        st
    }

    #[inline]
    fn cache_get_slot(&self, key: u128) -> Option<([f32; 2], bool)> {
        let base = hash_u128_raw(key) & CACHE_MASK;
        for step in 0..4 {
            let idx = (base + step) & CACHE_MASK;
            let s = self.caches.main[idx];
            if s.epoch != self.epoch {
                return None;
            }
            if s.key == key {
                return Some((s.val, s.has_skip));
            }
        }
        None
    }

    #[inline]
    fn cache_put(&mut self, key: u128, val: [f32; 2], has_skip: bool) {
        let base = hash_u128_raw(key) & CACHE_MASK;
        for step in 0..4 {
            let idx = (base + step) & CACHE_MASK;
            let s = &mut self.caches.main[idx];
            if s.epoch != self.epoch || s.key == key {
                s.key = key;
                s.val = val;
                s.epoch = self.epoch;
                s.has_skip = has_skip;
                return;
            }
        }
        self.caches.main[base] = CacheSlot {
            key,
            val,
            epoch: self.epoch,
            has_skip,
        };
    }

    fn word_base_score(&mut self, word: &[char]) -> f32 {
        let len = word.len();
        debug_assert!((1..=4).contains(&len));
        let key = pack_u128(1, len as u8, word);
        if let Some((v, _)) = self.cache_get_slot(key) {
            return v[0];
        }

        let mut w_score = 0.0f32;
        if len == 1 {
            if let Some(list) = self.corpus.char_readings.get(&word[0]) {
                if let Some((_, w)) = list.first() {
                    w_score += W_CORPUS * 0.15 * ((*w as f32).max(1.0)).ln();
                }
            }
        } else {
            if let Some(w) = self.corpus.lookup_word_chars(word) {
                w_score += W_CORPUS * ((w as f32).max(1.0)).ln();
            }
            if let Some(imp_s) = self.imported.lookup_imported_score_chars(word) {
                if imp_s > 10.0 {
                    w_score += W_IMPORTED * (imp_s - 10.0);
                }
            }
            if let Some(g) = self.gram {
                let mut can_gram = true;
                if len >= 3 {
                    can_gram = self.gram_prefix_state(&word[..2]).is_some();
                }
                if can_gram {
                    if let Some((np, u)) = self.gram_prefix_state(&word[..len - 1]) {
                        if let Some(gs) = g.lookup_from_state_1char(np, u, word[len - 1]) {
                            w_score += W_GRAM * gs;
                        }
                    }
                }
            }
        }

        if self.has_retention {
            w_score += self.retention_delta(word);
        }

        self.cache_put(key, [w_score, 0.0], false);
        w_score
    }

    fn word_base_batch8(
        &mut self,
        prefix: &[char],
        sc: &SlotCands,
        active_mask: u8,
    ) -> [f32; 8] {
        let plen = prefix.len();
        debug_assert!((1..=3).contains(&plen));
        let step_key = pack_u128(4, ((1u8) << 4) | (plen as u8), prefix);
        let step_idx = hash_u128_raw(step_key) & STEP_BATCH_MASK;
        let slot = self.caches.step_b8[step_idx];
        if slot.step_epoch == self.step_epoch
            && slot.key == step_key
            && (slot.skip_mask & active_mask) == active_mask
        {
            return slot.v0;
        }

        let mut out = [0.0f32; 8];
        let mut miss_mask = 0u8;
        let mut wbuf = ['\0'; 4];
        wbuf[..plen].copy_from_slice(prefix);

        let compute_mask = sc.active_mask;
        for k in 0..sc.cnt {
            if (compute_mask & (1 << k)) == 0 {
                continue;
            }
            wbuf[plen] = sc.chars[k];
            let key = pack_u128(1, (plen + 1) as u8, &wbuf[..=plen]);
            if let Some((v, _)) = self.cache_get_slot(key) {
                out[k] = v[0];
            } else {
                miss_mask |= 1 << k;
            }
        }

        if miss_mask != 0 {
            let gram_vals = if let Some(g) = self.gram {
                let can_gram = if plen >= 2 {
                    self.gram_prefix_state(&prefix[..2]).is_some()
                } else {
                    true
                };
                if can_gram {
                    if let Some((np, u)) = self.gram_prefix_state(prefix) {
                        g.lookup_from_state_batch8(np, u, &sc.enc, sc.cnt, miss_mask)
                    } else {
                        [0.0; 8]
                    }
                } else {
                    [0.0; 8]
                }
            } else {
                [0.0; 8]
            };

            for k in 0..sc.cnt {
                if (miss_mask & (1 << k)) == 0 {
                    continue;
                }
                wbuf[plen] = sc.chars[k];
                let word = &wbuf[..=plen];
                let mut w_score = 0.0f32;
                if let Some(w) = self.corpus.lookup_word_chars(word) {
                    w_score += W_CORPUS * ((w as f32).max(1.0)).ln();
                }
                if let Some(imp_s) = self.imported.lookup_imported_score_chars(word) {
                    if imp_s > 10.0 {
                        w_score += W_IMPORTED * (imp_s - 10.0);
                    }
                }
                if gram_vals[k] != 0.0 {
                    w_score += W_GRAM * gram_vals[k];
                }
                if self.has_retention {
                    w_score += self.retention_delta(word);
                }
                out[k] = w_score;
                let key = pack_u128(1, (plen + 1) as u8, word);
                self.cache_put(key, [w_score, 0.0], false);
            }
        }

        self.caches.step_b8[step_idx] = StepBatchSlot {
            key: step_key,
            v0: out,
            v1: [0.0; 8],
            step_epoch: self.step_epoch,
            skip_mask: compute_mask,
        };
        out
    }

    fn gram_pair_score(&mut self, prev: &[char], word: &[char]) -> f32 {
        let Some(g) = self.gram else {
            return 0.0;
        };
        if prev.is_empty() || word.is_empty() {
            return 0.0;
        }
        let coeff = if word.len() == 1 { 0.45 } else { 0.55 };
        let plen = prev.len();
        let wlen = word.len();
        if plen + wlen <= 5 {
            let mut combined = ['\0'; 5];
            combined[..plen].copy_from_slice(prev);
            combined[plen..plen + wlen].copy_from_slice(word);
            let meta = ((plen as u8) << 4) | (wlen as u8);
            let key = pack_u128(2, meta, &combined[..plen + wlen]);
            if let Some((v, _)) = self.cache_get_slot(key) {
                return v[0];
            }
            let val = if let Some((np, u)) = self.gram_prefix_state(prev) {
                g.lookup_from_state_chars(np, u, word)
                    .map(|gs| W_GRAM * coeff * gs)
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            self.cache_put(key, [val, 0.0], false);
            val
        } else if let Some((np, u)) = self.gram_prefix_state(prev) {
            g.lookup_from_state_chars(np, u, word)
                .map(|gs| W_GRAM * coeff * gs)
                .unwrap_or(0.0)
        } else {
            0.0
        }
    }

    fn gram_pair_batch8(
        &mut self,
        prev: &[char],
        word_prefix: &[char],
        sc: &SlotCands,
        active_mask: u8,
    ) -> [f32; 8] {
        let Some(g) = self.gram else {
            return [0.0; 8];
        };
        if prev.is_empty() || active_mask == 0 {
            return [0.0; 8];
        }
        let plen = prev.len();
        let wplen = word_prefix.len();
        let wlen = wplen + 1;
        let coeff = if wlen == 1 { 0.45 } else { 0.55 };
        let total_len = plen + wlen;

        let mut pbuf = ['\0'; 8];
        pbuf[..plen].copy_from_slice(prev);
        pbuf[plen..plen + wplen].copy_from_slice(word_prefix);
        let full_prefix_len = plen + wplen;

        if total_len <= 5 {
            let step_key = pack_u128(
                4,
                (2u8 << 6) | ((plen as u8) << 3) | (wplen as u8),
                &pbuf[..full_prefix_len],
            );
            let step_idx = hash_u128_raw(step_key) & STEP_BATCH_MASK;
            let slot = self.caches.step_b8[step_idx];
            if slot.step_epoch == self.step_epoch
                && slot.key == step_key
                && (slot.skip_mask & active_mask) == active_mask
            {
                return slot.v0;
            }

            let mut out = [0.0f32; 8];
            let mut miss_mask = 0u8;
            let meta = ((plen as u8) << 4) | (wlen as u8);
            let compute_mask = sc.active_mask;
            for k in 0..sc.cnt {
                if (compute_mask & (1 << k)) == 0 {
                    continue;
                }
                pbuf[full_prefix_len] = sc.chars[k];
                let key = pack_u128(2, meta, &pbuf[..total_len]);
                if let Some((v, _)) = self.cache_get_slot(key) {
                    out[k] = v[0];
                } else {
                    miss_mask |= 1 << k;
                }
            }

            if miss_mask != 0 {
                if let Some((np, u)) = self.gram_prefix_state(&pbuf[..full_prefix_len]) {
                    let gs_arr = g.lookup_from_state_batch8(np, u, &sc.enc, sc.cnt, miss_mask);
                    for k in 0..sc.cnt {
                        if (miss_mask & (1 << k)) != 0 {
                            let val = if gs_arr[k] != 0.0 {
                                W_GRAM * coeff * gs_arr[k]
                            } else {
                                0.0
                            };
                            out[k] = val;
                            pbuf[full_prefix_len] = sc.chars[k];
                            let key = pack_u128(2, meta, &pbuf[..total_len]);
                            self.cache_put(key, [val, 0.0], false);
                        }
                    }
                } else {
                    for (k, &ch) in sc.chars.iter().enumerate().take(sc.cnt) {
                        if (miss_mask & (1 << k)) != 0 {
                            pbuf[full_prefix_len] = ch;
                            let key = pack_u128(2, meta, &pbuf[..total_len]);
                            self.cache_put(key, [0.0, 0.0], false);
                        }
                    }
                }
            }

            self.caches.step_b8[step_idx] = StepBatchSlot {
                key: step_key,
                v0: out,
                v1: [0.0; 8],
                step_epoch: self.step_epoch,
                skip_mask: compute_mask,
            };
            out
        } else {
            let mut out = [0.0f32; 8];
            let st = if full_prefix_len <= 5 {
                self.gram_prefix_state(&pbuf[..full_prefix_len])
            } else {
                self.gram_prefix_state(&pbuf[..5])
                    .and_then(|(np, u)| g.advance_state_chars(np, u, &pbuf[5..full_prefix_len]))
            };
            if let Some((np, u)) = st {
                let gs_arr = g.lookup_from_state_batch8(np, u, &sc.enc, sc.cnt, active_mask);
                for k in 0..sc.cnt {
                    if (active_mask & (1 << k)) != 0 && gs_arr[k] != 0.0 {
                        out[k] = W_GRAM * coeff * gs_arr[k];
                    }
                }
            }
            out
        }
    }

    fn skip_pair_score(&mut self, w1: &[char], w2: &[char], slot_gap: usize) -> f32 {
        let l1 = w1.len();
        let l2 = w2.len();
        debug_assert!((1..=2).contains(&l1) && (1..=2).contains(&l2) && (2..=6).contains(&slot_gap));
        let mut combined = ['\0'; 4];
        combined[..l1].copy_from_slice(w1);
        combined[l1..l1 + l2].copy_from_slice(w2);

        let meta_c = ((l1 as u8) << 4) | (l2 as u8);
        let key_cg = pack_u128(3, meta_c, &combined[..l1 + l2]);
        let ([corpus_d1, gram_base], has_skip) = if let Some(hit) = self.cache_get_slot(key_cg) {
            hit
        } else {
            let c_val = self
                .corpus
                .lookup_word_chars(&combined[..l1 + l2])
                .map(|w| (((w as f32).max(1.0)).ln() * 0.25).max(0.5))
                .unwrap_or(0.0);
            let g_val = if let Some(g) = self.gram {
                if let Some((np, u)) = self.gram_prefix_state(w1) {
                    g.lookup_from_state_chars(np, u, w2)
                        .map(|gs| (1.25 + 0.85) * gs)
                        .unwrap_or(0.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let hs = if self.has_skipgrams {
                self.imported.skipgram_pmis_2x2(w1, w2).is_some()
            } else {
                false
            };
            let pair = [c_val, g_val];
            self.cache_put(key_cg, pair, hs);
            (pair, hs)
        };

        let pmi_decay = 1.0 / (1.0 + 0.25 * ((slot_gap - 1) as f32));
        let gram_decay = 1.0 / (1.0 + 0.10 * ((slot_gap - 2) as f32));
        if !has_skip {
            return W_SKIP * (corpus_d1 * pmi_decay + gram_base * gram_decay);
        }

        let mut best_pmi = 0.0f32;
        if let Some(pmis) = self.imported.skipgram_pmis_2x2(w1, w2) {
            for d in 1..=slot_gap.min(6) {
                let mut pmi = pmis[d - 1];
                if pmi <= 0.0 && d == 1 {
                    pmi = corpus_d1;
                }
                if pmi > 0.0 {
                    let decay = 1.0 / (1.0 + 0.25 * ((slot_gap - d) as f32));
                    best_pmi = best_pmi.max(pmi * decay);
                }
            }
        } else {
            best_pmi = corpus_d1 * pmi_decay;
        }
        W_SKIP * (best_pmi + gram_base * gram_decay)
    }

    #[allow(clippy::too_many_arguments)]
    fn skip_pair_batch8(
        &mut self,
        w1: &[char],
        w2_first: Option<char>,
        sc: &SlotCands,
        active_mask: u8,
        slot_gap: usize,
        out_acc: &mut [f32; 8],
    ) {
        let l1 = w1.len();
        let l2 = if w2_first.is_some() { 2 } else { 1 };
        debug_assert!((1..=2).contains(&l1) && (1..=2).contains(&l2) && (2..=6).contains(&slot_gap));

        let mut pbuf = ['\0'; 4];
        pbuf[..l1].copy_from_slice(w1);
        let prefix_len = if let Some(c0) = w2_first {
            pbuf[l1] = c0;
            l1 + 1
        } else {
            l1
        };

        let meta_c = ((l1 as u8) << 4) | (l2 as u8);
        let step_key = pack_u128(
            4,
            (3u8 << 6) | ((l1 as u8) << 3) | (l2 as u8),
            &pbuf[..prefix_len],
        );
        let step_idx = hash_u128_raw(step_key) & STEP_BATCH_MASK;
        let slot = self.caches.step_b8[step_idx];
        // Note: step_b8 stores skip_mask for sc.active_mask; for skip_pair_batch8 we encode
        // validity via (step_epoch, key) because we always compute all bits of sc.active_mask on miss.
        let (c_arr, g_arr, skip_mask) =
            if slot.step_epoch == self.step_epoch && slot.key == step_key {
                (slot.v0, slot.v1, slot.skip_mask)
            } else {
                let mut c_arr = [0.0f32; 8];
                let mut g_arr = [0.0f32; 8];
                let mut skip_mask = 0u8;
                let mut miss_mask = 0u8;
                let compute_mask = sc.active_mask;

                for k in 0..sc.cnt {
                    if (compute_mask & (1 << k)) == 0 {
                        continue;
                    }
                    pbuf[prefix_len] = sc.chars[k];
                    let key_cg = pack_u128(3, meta_c, &pbuf[..l1 + l2]);
                    if let Some((pair, hs)) = self.cache_get_slot(key_cg) {
                        c_arr[k] = pair[0];
                        g_arr[k] = pair[1];
                        if hs {
                            skip_mask |= 1 << k;
                        }
                    } else {
                        miss_mask |= 1 << k;
                    }
                }

                if miss_mask != 0 {
                    let gs_arr = if let Some(g) = self.gram {
                        if let Some((np, u)) = self.gram_prefix_state(&pbuf[..prefix_len]) {
                            g.lookup_from_state_batch8(np, u, &sc.enc, sc.cnt, miss_mask)
                        } else {
                            [0.0; 8]
                        }
                    } else {
                        [0.0; 8]
                    };

                    let mut w2_buf = [w2_first.unwrap_or('\0'), '\0'];
                    let w2_start = if w2_first.is_some() { 0 } else { 1 };
                    for k in 0..sc.cnt {
                        if (miss_mask & (1 << k)) == 0 {
                            continue;
                        }
                        pbuf[prefix_len] = sc.chars[k];
                        let c_val = self
                            .corpus
                            .lookup_word_chars(&pbuf[..l1 + l2])
                            .map(|w| (((w as f32).max(1.0)).ln() * 0.25).max(0.5))
                            .unwrap_or(0.0);
                        let g_val = if gs_arr[k] != 0.0 {
                            (1.25 + 0.85) * gs_arr[k]
                        } else {
                            0.0
                        };
                        let hs = if self.has_skipgrams {
                            w2_buf[1] = sc.chars[k];
                            self.imported
                                .skipgram_pmis_2x2(w1, &w2_buf[w2_start..2])
                                .is_some()
                        } else {
                            false
                        };
                        c_arr[k] = c_val;
                        g_arr[k] = g_val;
                        if hs {
                            skip_mask |= 1 << k;
                        }
                        let key_cg = pack_u128(3, meta_c, &pbuf[..l1 + l2]);
                        self.cache_put(key_cg, [c_val, g_val], hs);
                    }
                }

                self.caches.step_b8[step_idx] = StepBatchSlot {
                    key: step_key,
                    v0: c_arr,
                    v1: g_arr,
                    step_epoch: self.step_epoch,
                    skip_mask,
                };
                (c_arr, g_arr, skip_mask)
            };

        let pmi_decay = 1.0 / (1.0 + 0.25 * ((slot_gap - 1) as f32));
        let gram_decay = 1.0 / (1.0 + 0.10 * ((slot_gap - 2) as f32));
        if skip_mask == 0 {
            for k in 0..sc.cnt {
                if (active_mask & (1 << k)) != 0 {
                    out_acc[k] += W_SKIP * (c_arr[k] * pmi_decay + g_arr[k] * gram_decay);
                }
            }
        } else {
            let mut w2_buf = [w2_first.unwrap_or('\0'), '\0'];
            let w2_start = if w2_first.is_some() { 0 } else { 1 };
            for k in 0..sc.cnt {
                if (active_mask & (1 << k)) == 0 {
                    continue;
                }
                if (skip_mask & (1 << k)) == 0 {
                    out_acc[k] += W_SKIP * (c_arr[k] * pmi_decay + g_arr[k] * gram_decay);
                } else {
                    w2_buf[1] = sc.chars[k];
                    let w2_slice = &w2_buf[w2_start..2];
                    let mut best_pmi = 0.0f32;
                    if let Some(pmis) = self.imported.skipgram_pmis_2x2(w1, w2_slice) {
                        for d in 1..=slot_gap.min(6) {
                            let mut pmi = pmis[d - 1];
                            if pmi <= 0.0 && d == 1 {
                                pmi = c_arr[k];
                            }
                            if pmi > 0.0 {
                                let decay = 1.0 / (1.0 + 0.25 * ((slot_gap - d) as f32));
                                best_pmi = best_pmi.max(pmi * decay);
                            }
                        }
                    } else {
                        best_pmi = c_arr[k] * pmi_decay;
                    }
                    out_acc[k] += W_SKIP * (best_pmi + g_arr[k] * gram_decay);
                }
            }
        }
    }
}

#[inline]
fn skip_delta_batch8(
    ctx: &mut ScoreCtx<'_>,
    prefix_chars: &[char],
    sc: &SlotCands,
    active_mask: u8,
    i: usize,
) -> [f32; 8] {
    let mut delta = [0.0f32; 8];
    // l2 == 1 -> b = i, l1 MUST be 2 (l1 + l2 >= 3)
    for slot_gap in 2..=6 {
        let Some(end1) = (i + 1).checked_sub(slot_gap) else {
            break;
        };
        if end1 >= 2 {
            ctx.skip_pair_batch8(
                &prefix_chars[end1 - 2..end1],
                None,
                sc,
                active_mask,
                slot_gap,
                &mut delta,
            );
        }
    }
    // l2 == 2 -> b = i - 1
    if i >= 1 {
        let prev_ch = prefix_chars[i - 1];
        for slot_gap in 2..=6 {
            let Some(end1) = i.checked_sub(slot_gap) else {
                break;
            };
            for l1 in 1..=2 {
                if end1 >= l1 {
                    ctx.skip_pair_batch8(
                        &prefix_chars[end1 - l1..end1],
                        Some(prev_ch),
                        sc,
                        active_mask,
                        slot_gap,
                        &mut delta,
                    );
                }
            }
        }
    }
    delta
}

/// Skip-gram terms touching slot `i`, excluding pairs where `w2` ends at `i` (handled in batch8).
fn skip_touching_excluding_w2_end_at_i(ctx: &mut ScoreCtx<'_>, chars: &[char], i: usize) -> f32 {
    let n = chars.len();
    let mut score = 0.0f32;

    // Pairs where `i` lies inside w1 = chars[a..a+l1]
    for l1 in 1..=2 {
        if n < l1 {
            continue;
        }
        let a_start = i.saturating_sub(l1 - 1);
        let a_end = i.min(n - l1);
        for a in a_start..=a_end {
            let end1 = a + l1;
            for slot_gap in 2..=6 {
                let b = end1 + slot_gap - 1;
                if b >= n {
                    break;
                }
                for l2 in 1..=2.min(n - b) {
                    if l1 + l2 < 3 {
                        continue;
                    }
                    score += ctx.skip_pair_score(&chars[a..end1], &chars[b..b + l2], slot_gap);
                }
            }
        }
    }

    // Pairs where `i` lies inside w2 = chars[b..b+l2], ONLY when b + l2 - 1 > i (i.e. l2 == 2, b == i)
    if i + 1 < n {
        let l2 = 2usize;
        let b = i;
        for slot_gap in 2..=6 {
            if b + 1 < slot_gap {
                break;
            }
            let end1 = b + 1 - slot_gap;
            for l1 in 1..=2.min(end1) {
                let a = end1 - l1;
                score += ctx.skip_pair_score(&chars[a..end1], &chars[b..b + l2], slot_gap);
            }
        }
    }

    score
}

fn build_slot_cands(
    lattice: &[LatticeSlot],
    slot_allowed: &[Vec<usize>],
    vis_lp: &[Vec<f32>],
    sctx: &mut ScoreCtx<'_>,
) -> Vec<SlotCands> {
    let mut out = Vec::with_capacity(lattice.len());
    for (i, slot) in lattice.iter().enumerate() {
        let mut sc = SlotCands {
            chars: ['\0'; 8],
            enc: [EncodedChar {
                bytes: [0; 5],
                len: 0,
            }; 8],
            base_scores: [0.0; 8],
            vis_lp: [0.0; 8],
            cnt: slot.top8.len().min(8),
            active_mask: 0,
            is_empty_slot: slot.top8.is_empty(),
        };
        if slot.top8.is_empty() {
            sc.cnt = 1;
            sc.chars[0] = '一';
            sc.enc[0] = encode_char('一');
            sc.vis_lp[0] = W_VISUAL * 1e-4f32.ln();
            sc.active_mask = 1;
            sc.base_scores[0] = sctx.word_base_score(&['一']);
        } else {
            for &cand_idx in &slot_allowed[i] {
                if cand_idx < sc.cnt {
                    if let Some(ch) = slot.top8[cand_idx].ch.chars().next() {
                        sc.active_mask |= 1 << cand_idx;
                        sc.chars[cand_idx] = ch;
                        sc.enc[cand_idx] = encode_char(ch);
                        sc.vis_lp[cand_idx] = vis_lp[i]
                            .get(cand_idx)
                            .copied()
                            .unwrap_or(W_VISUAL * 1e-4f32.ln());
                        sc.base_scores[cand_idx] = sctx.word_base_score(&[ch]);
                    }
                }
            }
            if sc.active_mask == 0 {
                // Fallback if every allowed candidate had empty `ch`
                let ch = slot.top8[0].ch.chars().next().unwrap_or('一');
                sc.chars[0] = ch;
                sc.enc[0] = encode_char(ch);
                sc.vis_lp[0] = vis_lp[i].first().copied().unwrap_or(W_VISUAL * 1e-4f32.ln());
                sc.active_mask = 1;
                sc.base_scores[0] = sctx.word_base_score(&[ch]);
            }
        }
        out.push(sc);
    }
    out
}

/// Execute 3-tier backtracking (Tier 1 Viterbi + PMI, Tier 2 rbt3-ONNX, Tier 3 BYOK hook)
/// while strictly preserving `locked_by_entropy` and `manually_locked` slots.
pub fn decode_lattice_3tier(
    lattice: &[LatticeSlot],
    prior_locked_chars: &HashMap<(usize, usize), String>,
    initial_baseline_chars: &[String],
    bundle: &HwOnnxBundle,
    ctx: &HwDecodeContext<'_>,
) -> Vec<Mode2Slot> {
    let HwDecodeContext {
        imported,
        gram,
        retention,
        tier3,
        offline_guard,
    } = *ctx;
    if lattice.is_empty() {
        return Vec::new();
    }

    // Resolve locks per slot (entropy lock OR prior manual lock)
    let mut slot_allowed: Vec<Vec<usize>> = Vec::with_capacity(lattice.len());
    let mut manual_flags = vec![false; lattice.len()];
    let mut locked_chars: Vec<Option<String>> = vec![None; lattice.len()];
    let mut vis_lp: Vec<Vec<f32>> = Vec::with_capacity(lattice.len());

    for (i, slot) in lattice.iter().enumerate() {
        vis_lp.push(
            slot.top8
                .iter()
                .map(|c| W_VISUAL * c.visual_prob.max(1e-6).ln())
                .collect(),
        );
        if let Some(prev_ch) = prior_locked_chars.get(&(slot.stroke_start, slot.stroke_end)) {
            if let Some(idx) = slot.top8.iter().position(|c| &c.ch == prev_ch) {
                manual_flags[i] = true;
                locked_chars[i] = Some(prev_ch.clone());
                slot_allowed.push(vec![idx]);
                continue;
            }
        }
        if slot.locked_by_entropy {
            slot_allowed.push(vec![0]);
            continue;
        }
        let idxs: Vec<usize> = (0..slot.top8.len()).collect();
        slot_allowed.push(if idxs.is_empty() { vec![0] } else { idxs });
    }

    // Tier 1 (< 2ms): Incremental Viterbi beam search + delta skip-gram + cached batch8 lookups
    let mut updated_top8: Vec<Vec<super::Top8Candidate>> =
        lattice.iter().map(|s| s.top8.clone()).collect();

    let (mut best_indices, mut best_chars) = TIER1_CACHE.with_borrow_mut(|caches| {
        let mut sctx = ScoreCtx::new(imported, gram, retention, caches);
        let slot_cands = build_slot_cands(lattice, &slot_allowed, &vis_lp, &mut sctx);
        if lattice.len() <= MAX_STACK_N {
            run_tier1_stack(
                lattice,
                &slot_allowed,
                &slot_cands,
                &mut sctx,
                &mut updated_top8,
            )
        } else {
            run_tier1_dyn(
                lattice,
                &slot_allowed,
                &slot_cands,
                &mut sctx,
                &mut updated_top8,
            )
        }
    });

    // Tier 2 (rbt3-ONNX MaskedLM): windowed +/-4 chars for N > 10, capped at MAX_TIER2_SLOTS,
    // with per-slot min_lp anchoring so valid in-vocab tokens receive >= 0.0 and never demote below [UNK].
    let elastic_indices: Vec<usize> = (0..lattice.len())
        .filter(|&i| !lattice[i].locked_by_entropy && !manual_flags[i])
        .collect();

    if !elastic_indices.is_empty() && bundle.has_reranker_onnx() {
        if let Some(ref tok) = bundle.tokenizer {
            let mut active_tier2_indices = elastic_indices.clone();
            if active_tier2_indices.len() > MAX_TIER2_SLOTS {
                active_tier2_indices.sort_by(|&a, &b| {
                    lattice[a]
                        .margin_delta_p
                        .total_cmp(&lattice[b].margin_delta_p)
                        .then_with(|| b.cmp(&a))
                });
                active_tier2_indices.truncate(MAX_TIER2_SLOTS);
            }

            let mut sequences = Vec::with_capacity(active_tier2_indices.len());
            let mut mask_positions = Vec::with_capacity(active_tier2_indices.len());
            let mut cands_per_item = Vec::with_capacity(active_tier2_indices.len());

            for &slot_i in &active_tier2_indices {
                let (seq, mpos) = if best_chars.len() > 10 {
                    let win_start = slot_i.saturating_sub(4);
                    let win_end = (slot_i + 5).min(best_chars.len());
                    tok.encode_char_slots(&best_chars[win_start..win_end], Some(slot_i - win_start))
                } else {
                    tok.encode_char_slots(&best_chars, Some(slot_i))
                };
                let cands: Vec<char> = lattice[slot_i]
                    .top8
                    .iter()
                    .map(|c| c.ch.chars().next().unwrap_or('\0'))
                    .collect();
                sequences.push(seq);
                mask_positions.push(mpos);
                cands_per_item.push(cands);
            }

            if let Some(mlm_scores) =
                bundle.score_rbt3_masked_batch(&sequences, &mask_positions, &cands_per_item)
            {
                for (item_idx, &slot_i) in active_tier2_indices.iter().enumerate() {
                    let row = &mlm_scores[item_idx];
                    let mut min_lp = f32::INFINITY;
                    let mut cnt_lp = 0usize;
                    for lp in row.iter().flatten() {
                        if lp.is_finite() {
                            min_lp = min_lp.min(*lp);
                            cnt_lp += 1;
                        }
                    }
                    if cnt_lp == 0 || !min_lp.is_finite() {
                        continue;
                    }

                    let mut best_k = best_indices[slot_i];
                    let mut best_joint = f32::NEG_INFINITY;
                    for (k, cand) in updated_top8[slot_i].iter_mut().enumerate() {
                        if let Some(Some(lp)) = row.get(k) {
                            if lp.is_finite() {
                                cand.joint_score += W_MLM * (*lp - min_lp);
                            }
                        }
                        if cand.joint_score > best_joint {
                            best_joint = cand.joint_score;
                            best_k = k;
                        }
                    }
                    if let Some(ch) = updated_top8[slot_i]
                        .get(best_k)
                        .and_then(|c| c.ch.chars().next())
                    {
                        best_indices[slot_i] = best_k;
                        best_chars[slot_i] = ch;
                    }
                }
            }
        }
    }

    // Tier 3 (Optional BYOK constrained slot disambiguation hook — receives all elastic slots)
    if let Some(hook) = tier3 {
        let network_allowed = offline_guard.map(|g| !g.is_pure_offline()).unwrap_or(true);
        if network_allowed && !elastic_indices.is_empty() {
            let req = Tier3SlotRequest {
                context_chars: best_chars.iter().map(|c| c.to_string()).collect(),
                elastic_slots: elastic_indices
                    .iter()
                    .map(|&idx| Tier3ElasticSlot {
                        slot_idx: idx,
                        candidates: lattice[idx].top8.iter().map(|c| c.ch.clone()).collect(),
                    })
                    .collect(),
            };
            if let Ok(picks) = hook.disambiguate_slots(&req) {
                for (slot_i, cand_idx) in picks {
                    if elastic_indices.contains(&slot_i) && cand_idx < lattice[slot_i].top8.len() {
                        if let Some(ch) = lattice[slot_i].top8[cand_idx].ch.chars().next() {
                            best_indices[slot_i] = cand_idx;
                            best_chars[slot_i] = ch;
                        }
                    }
                }
            }
        }
    }

    // Materialize Mode2Slots preserving lock & retroactive flags
    lattice
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let chosen_idx = best_indices.get(i).copied().unwrap_or(0);
            let chosen_ch = locked_chars[i].clone().unwrap_or_else(|| {
                slot.top8
                    .get(chosen_idx)
                    .map(|c| c.ch.clone())
                    .unwrap_or_default()
            });
            let is_manual = manual_flags[i];
            let changed_retroactively = if is_manual || slot.locked_by_entropy {
                false
            } else if let Some(base_ch) = initial_baseline_chars.get(i) {
                base_ch != &chosen_ch
            } else {
                slot.top8.first().is_some_and(|c| c.ch != chosen_ch)
            };

            Mode2Slot {
                slot_idx: i,
                ch: chosen_ch,
                locked_by_entropy: slot.locked_by_entropy,
                manually_locked: is_manual,
                changed_retroactively,
                margin_delta_p: slot.margin_delta_p,
                top8: updated_top8[i].clone(),
            }
        })
        .collect()
}

fn run_tier1_stack(
    lattice: &[LatticeSlot],
    slot_allowed: &[Vec<usize>],
    slot_cands: &[SlotCands],
    sctx: &mut ScoreCtx<'_>,
    updated_top8: &mut [Vec<super::Top8Candidate>],
) -> (Vec<usize>, Vec<char>) {
    let n = lattice.len();
    let mut beam: Vec<StackBeamHyp> = Vec::with_capacity(BEAM_WIDTH);
    beam.push(StackBeamHyp::new());
    let mut next_beam: Vec<StackBeamHyp> = Vec::with_capacity(BEAM_WIDTH);
    let mut exts: Vec<CandExt> = Vec::with_capacity(BEAM_WIDTH * 8);
    let mut scratch_chars = ['\0'; MAX_STACK_N];

    for (i, sc) in slot_cands.iter().enumerate() {
        sctx.next_step_epoch();
        exts.clear();
        let end = i + 1;
        let max_len = 4.min(end);
        let active_mask = sc.active_mask;

        for (h_idx, hyp) in beam.iter().enumerate() {
            scratch_chars[..i].copy_from_slice(&hyp.chars[..i]);
            let mut best_dp = [f32::NEG_INFINITY; 8];
            let mut best_start = [i; 8];

            for len in (1..=max_len).rev() {
                let start = end - len;
                let base = hyp.dp[start];
                if !base.is_finite() {
                    continue;
                }
                let wb = if len == 1 {
                    sc.base_scores
                } else {
                    sctx.word_base_batch8(&scratch_chars[start..i], sc, active_mask)
                };
                let ps = hyp.prev_start[start] as usize;
                let pe = hyp.prev_end[start] as usize;
                let gp = if pe > ps {
                    sctx.gram_pair_batch8(&scratch_chars[ps..pe], &scratch_chars[start..i], sc, active_mask)
                } else {
                    [0.0; 8]
                };
                for k in 0..sc.cnt {
                    if (active_mask & (1 << k)) != 0 {
                        let cand_total = base + wb[k] + gp[k];
                        if cand_total > best_dp[k] {
                            best_dp[k] = cand_total;
                            best_start[k] = start;
                        }
                    }
                }
            }

            let skip_d = skip_delta_batch8(sctx, &scratch_chars[..i], sc, active_mask, i);
            for k in 0..sc.cnt {
                if (active_mask & (1 << k)) == 0 {
                    continue;
                }
                let vis_score = hyp.vis_score + sc.vis_lp[k];
                let skip_score = hyp.skip_score + skip_d[k];
                let total_score = vis_score + best_dp[k] + skip_score;
                exts.push(CandExt {
                    total_score,
                    dp_end: best_dp[k],
                    vis_score,
                    skip_score,
                    hyp_idx: h_idx as u16,
                    cand_idx: k as u8,
                    best_start: best_start[k] as u8,
                    ch: sc.chars[k],
                });
            }
        }
        if exts.len() > BEAM_WIDTH {
            exts.select_nth_unstable_by(BEAM_WIDTH - 1, |a, b| {
                b.total_score.total_cmp(&a.total_score)
            });
            exts.truncate(BEAM_WIDTH);
        }
        exts.sort_by(|a, b| b.total_score.total_cmp(&a.total_score));
        next_beam.clear();
        for ext in &exts {
            let mut nh = beam[ext.hyp_idx as usize];
            nh.chosen_indices[i] = ext.cand_idx;
            nh.chars[i] = ext.ch;
            nh.len = end;
            nh.dp[end] = ext.dp_end;
            nh.prev_start[end] = ext.best_start;
            nh.prev_end[end] = end as u8;
            nh.vis_score = ext.vis_score;
            nh.skip_score = ext.skip_score;
            nh.total_score = ext.total_score;
            next_beam.push(nh);
        }
        std::mem::swap(&mut beam, &mut next_beam);
    }

    let best_hyp = beam.first().copied().unwrap_or_else(|| {
        let mut fallback = StackBeamHyp::new();
        for (i, s) in lattice.iter().enumerate() {
            fallback.chars[i] = s
                .top8
                .first()
                .and_then(|c| c.ch.chars().next())
                .unwrap_or('一');
        }
        fallback.len = n;
        fallback
    });

    // Marginalize updated_top8 reusing best_hyp prefix DP[..=i], batch8 pos=i+1, and exact early convergence
    let mut alt_chars = best_hyp.chars;
    let mut alt_dp = best_hyp.dp;
    let mut alt_ps = best_hyp.prev_start;
    let mut alt_pe = best_hyp.prev_end;

    for (i, sc) in slot_cands.iter().enumerate() {
        if sc.is_empty_slot {
            continue;
        }
        let chosen_k = best_hyp.chosen_indices[i] as usize;
        if chosen_k < updated_top8[i].len() && slot_allowed[i].contains(&chosen_k) {
            updated_top8[i][chosen_k].joint_score = best_hyp.total_score;
        }

        let alt_mask = sc.active_mask & !(1 << chosen_k);
        if alt_mask == 0 {
            continue;
        }

        sctx.next_step_epoch();
        let base_vis = best_hyp.vis_score - sc.vis_lp[chosen_k];
        let base_skip_minus_w2 = best_hyp.skip_score
            - skip_delta_batch8(sctx, &best_hyp.chars[..i], sc, 1 << chosen_k, i)[chosen_k]
            - skip_touching_excluding_w2_end_at_i(sctx, &best_hyp.chars[..n], i);
        let skip_w2_alt = skip_delta_batch8(sctx, &best_hyp.chars[..i], sc, alt_mask, i);

        // Batched pos = i + 1 across all active alt_mask candidates
        let pos1 = i + 1;
        let max_len1 = 4.min(pos1);
        let mut pos1_dp = [f32::NEG_INFINITY; 8];
        let mut pos1_start = [i; 8];
        for len in (1..=max_len1).rev() {
            let start = pos1 - len;
            let base = alt_dp[start];
            if !base.is_finite() {
                continue;
            }
            let wb = if len == 1 {
                sc.base_scores
            } else {
                sctx.word_base_batch8(&alt_chars[start..i], sc, alt_mask)
            };
            let ps = alt_ps[start] as usize;
            let pe = alt_pe[start] as usize;
            let gp = if pe > ps {
                sctx.gram_pair_batch8(&alt_chars[ps..pe], &alt_chars[start..i], sc, alt_mask)
            } else {
                [0.0; 8]
            };
            for k in 0..sc.cnt {
                if (alt_mask & (1 << k)) != 0 {
                    let cand_total = base + wb[k] + gp[k];
                    if cand_total > pos1_dp[k] {
                        pos1_dp[k] = cand_total;
                        pos1_start[k] = start;
                    }
                }
            }
        }

        for k in 0..sc.cnt {
            if (alt_mask & (1 << k)) == 0 {
                continue;
            }
            alt_chars[i] = sc.chars[k];
            alt_dp[pos1] = pos1_dp[k];
            alt_ps[pos1] = pos1_start[k] as u8;
            alt_pe[pos1] = pos1 as u8;

            let mut final_dp_n = pos1_dp[k];
            let mut last_pos = pos1;
            if pos1 < n {
                let mut prev_delta = pos1_dp[k] - best_hyp.dp[pos1];
                let mut streak = if pos1_start[k] == (best_hyp.prev_start[pos1] as usize)
                    && pos1 == (best_hyp.prev_end[pos1] as usize)
                    && pos1_start[k] > i
                {
                    1usize
                } else {
                    0usize
                };

                for pos in (pos1 + 1)..=n {
                    let max_len = 4.min(pos);
                    let mut best_dp = f32::NEG_INFINITY;
                    let mut best_start = pos - 1;
                    for len in (1..=max_len).rev() {
                        let start = pos - len;
                        let base = alt_dp[start];
                        if !base.is_finite() {
                            continue;
                        }
                        let word = &alt_chars[start..pos];
                        let mut w_score = sctx.word_base_score(word);
                        let ps = alt_ps[start] as usize;
                        let pe = alt_pe[start] as usize;
                        if pe > ps {
                            w_score += sctx.gram_pair_score(&alt_chars[ps..pe], word);
                        }
                        let cand_total = base + w_score;
                        if cand_total > best_dp {
                            best_dp = cand_total;
                            best_start = start;
                        }
                    }
                    alt_dp[pos] = best_dp;
                    alt_ps[pos] = best_start as u8;
                    alt_pe[pos] = pos as u8;
                    last_pos = pos;

                    let cur_delta = best_dp - best_hyp.dp[pos];
                    if best_start == (best_hyp.prev_start[pos] as usize)
                        && pos == (best_hyp.prev_end[pos] as usize)
                        && best_start > i
                        && cur_delta.is_finite()
                    {
                        if streak > 0 && (cur_delta - prev_delta).abs() <= 1e-6 {
                            streak += 1;
                        } else {
                            streak = 1;
                            prev_delta = cur_delta;
                        }
                        if streak >= 4 && pos >= i + 4 {
                            final_dp_n = best_hyp.dp[n] + cur_delta;
                            break;
                        }
                    } else {
                        streak = 0;
                        prev_delta = cur_delta;
                    }
                    if pos == n {
                        final_dp_n = best_dp;
                    }
                }
            }

            let vis = base_vis + sc.vis_lp[k];
            let skip = base_skip_minus_w2
                + skip_w2_alt[k]
                + skip_touching_excluding_w2_end_at_i(sctx, &alt_chars[..n], i);
            updated_top8[i][k].joint_score = vis + final_dp_n + skip;

            // Restore only modified suffix positions for next k
            alt_dp[pos1..=last_pos].copy_from_slice(&best_hyp.dp[pos1..=last_pos]);
            alt_ps[pos1..=last_pos].copy_from_slice(&best_hyp.prev_start[pos1..=last_pos]);
            alt_pe[pos1..=last_pos].copy_from_slice(&best_hyp.prev_end[pos1..=last_pos]);
        }

        alt_chars[i] = best_hyp.chars[i];
    }

    let indices: Vec<usize> = (0..n).map(|i| best_hyp.chosen_indices[i] as usize).collect();
    let chars = best_hyp.chars[..n].to_vec();
    (indices, chars)
}

fn run_tier1_dyn(
    lattice: &[LatticeSlot],
    slot_allowed: &[Vec<usize>],
    slot_cands: &[SlotCands],
    sctx: &mut ScoreCtx<'_>,
    updated_top8: &mut [Vec<super::Top8Candidate>],
) -> (Vec<usize>, Vec<char>) {
    let n = lattice.len();
    let mut beam: Vec<DynBeamHyp> = vec![DynBeamHyp::with_capacity(n)];
    let mut next_beam: Vec<DynBeamHyp> = Vec::with_capacity(BEAM_WIDTH);
    let mut exts: Vec<DynCandExt> = Vec::with_capacity(BEAM_WIDTH * 8);
    let mut scratch_chars = vec!['\0'; n];

    for (i, sc) in slot_cands.iter().enumerate() {
        sctx.next_step_epoch();
        exts.clear();
        let end = i + 1;
        let max_len = 4.min(end);
        let active_mask = sc.active_mask;

        for (h_idx, hyp) in beam.iter().enumerate() {
            scratch_chars[..i].copy_from_slice(&hyp.chars[..i]);
            let mut best_dp = [f32::NEG_INFINITY; 8];
            let mut best_start = [i; 8];

            for len in (1..=max_len).rev() {
                let start = end - len;
                let base = hyp.dp[start];
                if !base.is_finite() {
                    continue;
                }
                let wb = if len == 1 {
                    sc.base_scores
                } else {
                    sctx.word_base_batch8(&scratch_chars[start..i], sc, active_mask)
                };
                let ps = hyp.prev_start[start];
                let pe = hyp.prev_end[start];
                let gp = if pe > ps {
                    sctx.gram_pair_batch8(&scratch_chars[ps..pe], &scratch_chars[start..i], sc, active_mask)
                } else {
                    [0.0; 8]
                };
                for k in 0..sc.cnt {
                    if (active_mask & (1 << k)) != 0 {
                        let cand_total = base + wb[k] + gp[k];
                        if cand_total > best_dp[k] {
                            best_dp[k] = cand_total;
                            best_start[k] = start;
                        }
                    }
                }
            }

            let skip_d = skip_delta_batch8(sctx, &scratch_chars[..i], sc, active_mask, i);
            for k in 0..sc.cnt {
                if (active_mask & (1 << k)) == 0 {
                    continue;
                }
                let vis_score = hyp.vis_score + sc.vis_lp[k];
                let skip_score = hyp.skip_score + skip_d[k];
                let total_score = vis_score + best_dp[k] + skip_score;
                exts.push(DynCandExt {
                    total_score,
                    dp_end: best_dp[k],
                    vis_score,
                    skip_score,
                    hyp_idx: h_idx,
                    cand_idx: k,
                    best_start: best_start[k],
                    ch: sc.chars[k],
                });
            }
        }
        if exts.len() > BEAM_WIDTH {
            exts.select_nth_unstable_by(BEAM_WIDTH - 1, |a, b| {
                b.total_score.total_cmp(&a.total_score)
            });
            exts.truncate(BEAM_WIDTH);
        }
        exts.sort_by(|a, b| b.total_score.total_cmp(&a.total_score));

        next_beam.clear();
        for ext in &exts {
            let mut nh = beam[ext.hyp_idx].clone();
            nh.chosen_indices.push(ext.cand_idx);
            nh.chars.push(ext.ch);
            nh.dp.push(ext.dp_end);
            nh.prev_start.push(ext.best_start);
            nh.prev_end.push(end);
            nh.vis_score = ext.vis_score;
            nh.skip_score = ext.skip_score;
            nh.total_score = ext.total_score;
            next_beam.push(nh);
        }
        std::mem::swap(&mut beam, &mut next_beam);
    }

    let best_hyp = beam.into_iter().next().unwrap_or_else(|| {
        let mut dp = vec![f32::NEG_INFINITY; n + 1];
        dp[0] = 0.0;
        DynBeamHyp {
            chosen_indices: vec![0; n],
            chars: lattice
                .iter()
                .map(|s| {
                    s.top8
                        .first()
                        .and_then(|c| c.ch.chars().next())
                        .unwrap_or('一')
                })
                .collect(),
            dp,
            prev_start: vec![0; n + 1],
            prev_end: vec![0; n + 1],
            vis_score: 0.0,
            skip_score: 0.0,
            total_score: 0.0,
        }
    });

    let mut alt_chars = best_hyp.chars.clone();
    let mut alt_dp = best_hyp.dp.clone();
    let mut alt_ps = best_hyp.prev_start.clone();
    let mut alt_pe = best_hyp.prev_end.clone();

    for (i, sc) in slot_cands.iter().enumerate() {
        if sc.is_empty_slot {
            continue;
        }
        let chosen_k = best_hyp.chosen_indices[i];
        if chosen_k < updated_top8[i].len() && slot_allowed[i].contains(&chosen_k) {
            updated_top8[i][chosen_k].joint_score = best_hyp.total_score;
        }

        let alt_mask = sc.active_mask & !(1 << chosen_k);
        if alt_mask == 0 {
            continue;
        }

        sctx.next_step_epoch();
        let base_vis = best_hyp.vis_score - sc.vis_lp[chosen_k];
        let base_skip_minus_w2 = best_hyp.skip_score
            - skip_delta_batch8(sctx, &best_hyp.chars[..i], sc, 1 << chosen_k, i)[chosen_k]
            - skip_touching_excluding_w2_end_at_i(sctx, &best_hyp.chars[..n], i);
        let skip_w2_alt = skip_delta_batch8(sctx, &best_hyp.chars[..i], sc, alt_mask, i);

        let pos1 = i + 1;
        let max_len1 = 4.min(pos1);
        let mut pos1_dp = [f32::NEG_INFINITY; 8];
        let mut pos1_start = [i; 8];
        for len in (1..=max_len1).rev() {
            let start = pos1 - len;
            let base = alt_dp[start];
            if !base.is_finite() {
                continue;
            }
            let wb = if len == 1 {
                sc.base_scores
            } else {
                sctx.word_base_batch8(&alt_chars[start..i], sc, alt_mask)
            };
            let ps = alt_ps[start];
            let pe = alt_pe[start];
            let gp = if pe > ps {
                sctx.gram_pair_batch8(&alt_chars[ps..pe], &alt_chars[start..i], sc, alt_mask)
            } else {
                [0.0; 8]
            };
            for k in 0..sc.cnt {
                if (alt_mask & (1 << k)) != 0 {
                    let cand_total = base + wb[k] + gp[k];
                    if cand_total > pos1_dp[k] {
                        pos1_dp[k] = cand_total;
                        pos1_start[k] = start;
                    }
                }
            }
        }

        for k in 0..sc.cnt {
            if (alt_mask & (1 << k)) == 0 {
                continue;
            }
            alt_chars[i] = sc.chars[k];
            alt_dp[pos1] = pos1_dp[k];
            alt_ps[pos1] = pos1_start[k];
            alt_pe[pos1] = pos1;

            let mut final_dp_n = pos1_dp[k];
            let mut last_pos = pos1;
            if pos1 < n {
                let mut prev_delta = pos1_dp[k] - best_hyp.dp[pos1];
                let mut streak = if pos1_start[k] == best_hyp.prev_start[pos1]
                    && pos1 == best_hyp.prev_end[pos1]
                    && pos1_start[k] > i
                {
                    1usize
                } else {
                    0usize
                };

                for pos in (pos1 + 1)..=n {
                    let max_len = 4.min(pos);
                    let mut best_dp = f32::NEG_INFINITY;
                    let mut best_start = pos - 1;
                    for len in (1..=max_len).rev() {
                        let start = pos - len;
                        let base = alt_dp[start];
                        if !base.is_finite() {
                            continue;
                        }
                        let word = &alt_chars[start..pos];
                        let mut w_score = sctx.word_base_score(word);
                        let ps = alt_ps[start];
                        let pe = alt_pe[start];
                        if pe > ps {
                            w_score += sctx.gram_pair_score(&alt_chars[ps..pe], word);
                        }
                        let cand_total = base + w_score;
                        if cand_total > best_dp {
                            best_dp = cand_total;
                            best_start = start;
                        }
                    }
                    alt_dp[pos] = best_dp;
                    alt_ps[pos] = best_start;
                    alt_pe[pos] = pos;
                    last_pos = pos;

                    let cur_delta = best_dp - best_hyp.dp[pos];
                    if best_start == best_hyp.prev_start[pos]
                        && pos == best_hyp.prev_end[pos]
                        && best_start > i
                        && cur_delta.is_finite()
                    {
                        if streak > 0 && (cur_delta - prev_delta).abs() <= 1e-6 {
                            streak += 1;
                        } else {
                            streak = 1;
                            prev_delta = cur_delta;
                        }
                        if streak >= 4 && pos >= i + 4 {
                            final_dp_n = best_hyp.dp[n] + cur_delta;
                            break;
                        }
                    } else {
                        streak = 0;
                        prev_delta = cur_delta;
                    }
                    if pos == n {
                        final_dp_n = best_dp;
                    }
                }
            }

            let vis = base_vis + sc.vis_lp[k];
            let skip = base_skip_minus_w2
                + skip_w2_alt[k]
                + skip_touching_excluding_w2_end_at_i(sctx, &alt_chars[..n], i);
            updated_top8[i][k].joint_score = vis + final_dp_n + skip;

            alt_dp[pos1..=last_pos].copy_from_slice(&best_hyp.dp[pos1..=last_pos]);
            alt_ps[pos1..=last_pos].copy_from_slice(&best_hyp.prev_start[pos1..=last_pos]);
            alt_pe[pos1..=last_pos].copy_from_slice(&best_hyp.prev_end[pos1..=last_pos]);
        }

        alt_chars[i] = best_hyp.chars[i];
    }

    (best_hyp.chosen_indices, best_hyp.chars)
}
