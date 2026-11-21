use super::raster::{global_bbox, StrokeStats};
use super::StrokePoint;
use crate::imp::ImportedLexiconIndex;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdcOp {
    LeftRight, // ⿰ U+2FF0
    TopBottom, // ⿱ U+2FF1
    Other,     // ⿲..⿻
}

#[derive(Debug, Clone)]
pub struct BipartiteSplit {
    pub op: IdcOp,
    pub left_or_top: Vec<Vec<StrokePoint>>,
    pub right_or_bottom: Vec<Vec<StrokePoint>>,
    pub separation_score: f32,
}

#[derive(Debug, Default)]
pub struct IdsIndex {
    /// (IdcOp, canonical_a, canonical_b) -> Vec<char>
    by_pair: HashMap<(IdcOp, char, char), Vec<char>>,
    /// char -> (IdcOp, raw_a, raw_b)
    by_char: HashMap<char, (IdcOp, char, char)>,
    /// Fallback from radical_pinyin.dict.yaml: (first_syl, second_syl) -> Vec<char>
    by_radical_pinyin: HashMap<(String, String), Vec<char>>,
}

static IDS_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<IdsIndex>>>> = OnceLock::new();

pub fn canonical_radical(ch: char) -> char {
    match ch {
        '亻' => '人',
        '氵' => '水',
        '扌' => '手',
        '忄' => '心',
        '艹' | '艸' | '卝' => '艹',
        '訁' | '讠' => '言',
        '釒' | '钅' => '金',
        '飠' | '饣' => '食',
        '糹' | '纟' => '糸',
        '門' => '门',
        '頁' => '页',
        '龍' => '龙',
        '曰' => '日',
        '灬' => '火',
        '刂' => '刀',
        '⺮' => '竹',
        '玉' => '王',
        '礻' => '示',
        '衤' => '衣',
        '犭' => '犬',
        '牜' => '牛',
        '辶' => '辵',
        '阝' => '邑',
        '龵' => '手',
        '攵' => '攴',
        '广' | '疒' => ch,
        _ => ch,
    }
}

fn is_cjk_ideograph(ch: char) -> bool {
    let cp = ch as u32;
    (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x20000..=0x2EBFF).contains(&cp)
        || (0x2E80..=0x2EFF).contains(&cp)
        || (0x2F00..=0x2FDF).contains(&cp)
}

fn cjk_block_priority(ch: char) -> u8 {
    let cp = ch as u32;
    if (0x4E00..=0x9FFF).contains(&cp) {
        0 // CJK Unified Ideographs BMP (includes 頔 U+9814)
    } else if (0x3400..=0x4DBF).contains(&cp) {
        1 // CJK Extension A
    } else {
        2 // Ext-B+
    }
}

/// Parse a binary (or folded ternary ⿲/⿳) IDS expression into `(IdcOp, leaf_a, leaf_b)`.
fn parse_binary_ids(seq: &str) -> Option<(IdcOp, char, char)> {
    let chars: Vec<char> = seq.chars().collect();
    if chars.len() < 3 {
        return None;
    }
    match chars[0] {
        '\u{2FF0}' if chars.len() >= 3 && is_cjk_ideograph(chars[1]) && is_cjk_ideograph(chars[2]) => {
            Some((IdcOp::LeftRight, chars[1], chars[2]))
        }
        '\u{2FF1}' if chars.len() >= 3 && is_cjk_ideograph(chars[1]) && is_cjk_ideograph(chars[2]) => {
            Some((IdcOp::TopBottom, chars[1], chars[2]))
        }
        '\u{2FF2}' if chars.len() >= 4 && is_cjk_ideograph(chars[1]) && is_cjk_ideograph(chars[3]) => {
            // ⿲ A M B -> approximate outer LeftRight pair (A, B)
            Some((IdcOp::LeftRight, chars[1], chars[3]))
        }
        '\u{2FF3}' if chars.len() >= 4 && is_cjk_ideograph(chars[1]) && is_cjk_ideograph(chars[3]) => {
            // ⿳ A M B -> approximate outer TopBottom pair (A, B)
            Some((IdcOp::TopBottom, chars[1], chars[3]))
        }
        '\u{2FF4}'..='\u{2FFB}' if chars.len() >= 3 && is_cjk_ideograph(chars[1]) && is_cjk_ideograph(chars[2]) => {
            Some((IdcOp::Other, chars[1], chars[2]))
        }
        _ => None,
    }
}

impl IdsIndex {
    pub fn load_cached(assets_dir: Option<&Path>) -> Arc<Self> {
        Self::load_dir(assets_dir.map(|p| p.join("handwriting")).as_deref())
    }

    pub fn clear_cache() { if let Some(c) = IDS_CACHE.get() { c.lock().clear(); } }

    pub fn load_dir(assets_dir: Option<&Path>) -> Arc<Self> {
        let key = assets_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("__empty__"));
        let cache = IDS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock();
        if let Some(idx) = guard.get(&key) {
            return Arc::clone(idx);
        }
        let built = Arc::new(Self::build(assets_dir));
        guard.insert(key, Arc::clone(&built));
        built
    }

    fn build(assets_dir: Option<&Path>) -> Self {
        let mut idx = Self::default();
        let Some(dir) = assets_dir else {
            return idx;
        };

        let ids_path = dir.join("cjkvi_ids.txt");
        if let Ok(content) = fs::read_to_string(&ids_path) {
            for line in content.lines() {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split('\t');
                let _cp = parts.next();
                let Some(ch_str) = parts.next() else {
                    continue;
                };
                let mut ch_it = ch_str.chars();
                let Some(ch) = ch_it.next() else {
                    continue;
                };
                if ch_it.next().is_some() || !is_cjk_ideograph(ch) {
                    continue;
                }
                for raw_seq in parts {
                    let seq = raw_seq.split('[').next().unwrap_or("").trim();
                    if let Some((op, a, b)) = parse_binary_ids(seq) {
                        idx.by_char.entry(ch).or_insert((op, a, b));
                        let key_exact = (op, a, b);
                        idx.by_pair.entry(key_exact).or_default().push(ch);

                        let ca = canonical_radical(a);
                        let cb = canonical_radical(b);
                        if ca != a || cb != b {
                            idx.by_pair.entry((op, ca, cb)).or_default().push(ch);
                        }
                    }
                }
            }
            for list in idx.by_pair.values_mut() {
                list.sort_by_key(|&c| (cjk_block_priority(c), c as u32));
                list.dedup();
            }
        }

        let rad_path = dir.join("rime").join("radical_pinyin.dict.yaml");
        if let Ok(content) = fs::read_to_string(&rad_path) {
            let mut in_body = false;
            for line in content.lines() {
                let t = line.trim();
                if !in_body {
                    if t == "..." {
                        in_body = true;
                    }
                    continue;
                }
                if t.is_empty() || t.starts_with('#') {
                    continue;
                }
                let mut cols = line.split('\t');
                let Some(ch_str) = cols.next() else {
                    continue;
                };
                let Some(code_str) = cols.next() else {
                    continue;
                };
                let mut it = ch_str.trim().chars();
                let Some(ch) = it.next() else {
                    continue;
                };
                if it.next().is_some() || !is_cjk_ideograph(ch) {
                    continue;
                }
                let parts: Vec<&str> = code_str.trim().split('\'').collect();
                if parts.len() == 2 {
                    idx.by_radical_pinyin
                        .entry((parts[0].to_string(), parts[1].to_string()))
                        .or_default()
                        .push(ch);
                }
            }
            for list in idx.by_radical_pinyin.values_mut() {
                list.sort_by_key(|&c| (cjk_block_priority(c), c as u32));
                list.dedup();
            }
        }

        idx
    }

    pub fn is_empty(&self) -> bool {
        self.by_char.is_empty() && self.by_radical_pinyin.is_empty()
    }

    pub fn lookup_pair(&self, op: IdcOp, a: char, b: char) -> Vec<char> {
        let mut out = Vec::new();
        if let Some(list) = self.by_pair.get(&(op, a, b)) {
            out.extend_from_slice(list);
        }
        let ca = canonical_radical(a);
        let cb = canonical_radical(b);
        if (ca != a || cb != b) && !out.is_empty() {
            if let Some(list) = self.by_pair.get(&(op, ca, cb)) {
                for &c in list {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        } else if out.is_empty() {
            if let Some(list) = self.by_pair.get(&(op, ca, cb)) {
                out.extend_from_slice(list);
            }
        }
        out
    }

    pub fn lookup_by_radical_pinyin(
        &self,
        a: char,
        b: char,
        imported: &ImportedLexiconIndex,
    ) -> Vec<char> {
        let pa = imported.char_phonetics(a);
        let pb = imported.char_phonetics(b);
        if pa.is_empty() || pb.is_empty() {
            return Vec::new();
        }
        let sa = crate::imp::xiaohe::normalize_syllable(&pa[0].0);
        let sb = crate::imp::xiaohe::normalize_syllable(&pb[0].0);
        self.by_radical_pinyin
            .get(&(sa, sb))
            .cloned()
            .unwrap_or_default()
    }

    pub fn char_decomposition(&self, ch: char) -> Option<(IdcOp, char, char)> {
        self.by_char.get(&ch).copied()
    }

    /// True if `c1` and `c2` are structural siblings sharing the same major right/bottom component
    /// (e.g. `曝 = ⿰日暴` and `爆 = ⿰火暴`) while differing in the minor left/top radical.
    pub fn are_structural_siblings(&self, c1: char, c2: char) -> bool {
        if c1 == c2 {
            return false;
        }
        match (self.by_char.get(&c1), self.by_char.get(&c2)) {
            (Some(&(op1, a1, b1)), Some(&(op2, a2, b2))) => {
                op1 == op2
                    && (op1 == IdcOp::LeftRight || op1 == IdcOp::TopBottom)
                    && canonical_radical(b1) == canonical_radical(b2)
                    && canonical_radical(a1) != canonical_radical(a2)
            }
            _ => false,
        }
    }
}

/// Detect clean bipartite spatial & temporal component splits for compositional recognition.
pub fn find_bipartite_splits(strokes: &[Vec<StrokePoint>]) -> Vec<BipartiteSplit> {
    let valid: Vec<(usize, &Vec<StrokePoint>, StrokeStats)> = strokes
        .iter()
        .enumerate()
        .filter_map(|(i, s)| StrokeStats::from_stroke(s).map(|st| (i, s, st)))
        .collect();
    if valid.len() < 2 {
        return Vec::new();
    }
    let Some((g_min_x, g_max_x, g_min_y, g_max_y)) = global_bbox(strokes) else {
        return Vec::new();
    };
    let gw = (g_max_x - g_min_x).max(1e-5);
    let gh = (g_max_y - g_min_y).max(1e-5);

    let mut candidates: Vec<BipartiteSplit> = Vec::new();

    // 1. Spatial X-sorted split (LeftRight ⿰)
    let mut x_sorted = valid.clone();
    x_sorted.sort_by(|a, b| a.2.cx.total_cmp(&b.2.cx));
    if let Some(best_x) = best_axis_split(&x_sorted, gw, IdcOp::LeftRight) {
        candidates.push(best_x);
    }

    // 2. Spatial Y-sorted split (TopBottom ⿱)
    let mut y_sorted = valid.clone();
    y_sorted.sort_by(|a, b| a.2.cy.total_cmp(&b.2.cy));
    if let Some(best_y) = best_axis_split(&y_sorted, gh, IdcOp::TopBottom) {
        candidates.push(best_y);
    }

    // 3. Temporal prefix/suffix split (along dominant separation axis if not identical to spatial split)
    let temporal_op = if gw >= gh {
        IdcOp::LeftRight
    } else {
        IdcOp::TopBottom
    };
    let temporal_span = if temporal_op == IdcOp::LeftRight {
        gw
    } else {
        gh
    };
    if let Some(best_t) = best_axis_split(&valid, temporal_span, temporal_op) {
        if candidates
            .iter()
            .all(|c| c.left_or_top.len() != best_t.left_or_top.len() || c.op != best_t.op)
        {
            candidates.push(best_t);
        }
    }

    candidates.sort_by(|a, b| b.separation_score.total_cmp(&a.separation_score));
    candidates.truncate(3);
    candidates
}

fn best_axis_split(
    ordered: &[(usize, &Vec<StrokePoint>, StrokeStats)],
    global_span: f32,
    op: IdcOp,
) -> Option<BipartiteSplit> {
    let n = ordered.len();
    if n < 2 {
        return None;
    }
    let mut best_score = f32::NEG_INFINITY;
    let mut best_k = 0usize;

    for k in 1..n {
        let left_slice = &ordered[..k];
        let right_slice = &ordered[k..];

        let (l_min, l_max) = axis_bounds(left_slice, op);
        let (r_min, r_max) = axis_bounds(right_slice, op);

        let gap = (r_min - l_max) / global_span;
        let overlap_len = (l_max.min(r_max) - l_min.max(r_min)).max(0.0);
        let union_len = (l_max.max(r_max) - l_min.min(r_min)).max(1e-5);
        let iou = overlap_len / union_len;

        // Require at least minimal spatial separation (no heavily entangled split)
        let balance = (k.min(n - k) as f32) / (n as f32);
        let score = gap * 2.5 - iou * 2.0 + balance * 0.25;
        if iou < 0.28 && score > best_score {
            best_score = score;
            best_k = k;
        }
    }

    if best_k == 0 || best_score <= 0.05 {
        return None;
    }

    // Preserve original stroke relative ordering inside each part
    let mut left_idx: Vec<usize> = ordered[..best_k].iter().map(|x| x.0).collect();
    let mut right_idx: Vec<usize> = ordered[best_k..].iter().map(|x| x.0).collect();
    left_idx.sort_unstable();
    right_idx.sort_unstable();

    let left_or_top: Vec<Vec<StrokePoint>> = left_idx
        .iter()
        .filter_map(|&idx| ordered.iter().find(|x| x.0 == idx).map(|x| x.1.clone()))
        .collect();
    let right_or_bottom: Vec<Vec<StrokePoint>> = right_idx
        .iter()
        .filter_map(|&idx| ordered.iter().find(|x| x.0 == idx).map(|x| x.1.clone()))
        .collect();

    Some(BipartiteSplit {
        op,
        left_or_top,
        right_or_bottom,
        separation_score: best_score,
    })
}

fn axis_bounds(slice: &[(usize, &Vec<StrokePoint>, StrokeStats)], op: IdcOp) -> (f32, f32) {
    let mut min_v = f32::INFINITY;
    let mut max_v = f32::NEG_INFINITY;
    for (_, _, st) in slice {
        match op {
            IdcOp::TopBottom => {
                min_v = min_v.min(st.min_y);
                max_v = max_v.max(st.max_y);
            }
            _ => {
                min_v = min_v.min(st.min_x);
                max_v = max_v.max(st.max_x);
            }
        }
    }
    (min_v, max_v)
}

pub fn compose_rare_candidates(
    ids: &IdsIndex,
    imported: &ImportedLexiconIndex,
    hw_classes: Option<&HashSet<char>>,
    split: &BipartiteSplit,
    top_a: &[(char, f32)],
    top_b: &[(char, f32)],
    total_strokes: usize,
) -> Vec<(char, f32)> {
    let mut scores: HashMap<char, f32> = HashMap::new();
    for &(ca, pa) in top_a.iter().take(6) {
        if pa < 0.02 {
            continue;
        }
        for &(cb, pb) in top_b.iter().take(6) {
            if pb < 0.02 {
                continue;
            }
            let mut matches = ids.lookup_pair(split.op, ca, cb);
            if matches.is_empty() {
                matches = ids.lookup_by_radical_pinyin(ca, cb, imported);
            }
            for ch in matches {
                // Prioritize characters missing from hw_model.classes.json (e.g., 頔 & Ext-A)
                // and BMP/Ext-A over archaic Ext-B+.
                let in_cnn = hw_classes.is_some_and(|s| s.contains(&ch));
                let block_bonus = match cjk_block_priority(ch) {
                    0 => 1.35, // BMP (e.g., 頔)
                    1 => 1.20, // Ext-A (U+3400..=U+4DBF)
                    _ => 0.45,
                };
                let oov_bonus = if !in_cnn { 1.45 } else { 0.85 };
                let has_phonetic = if !imported.char_phonetics(ch).is_empty() {
                    1.15
                } else {
                    0.75
                };
                let stroke_bonus = if total_strokes >= 8 && cjk_block_priority(ch) <= 1 {
                    1.05
                } else {
                    1.0
                };
                let raw_score =
                    (pa * pb).sqrt() * block_bonus * oov_bonus * has_phonetic * stroke_bonus
                        + 0.08 * split.separation_score.min(1.5);
                let ent = scores.entry(ch).or_insert(0.0);
                if raw_score > *ent {
                    *ent = raw_score;
                }
            }
        }
    }
    let mut out: Vec<(char, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}
