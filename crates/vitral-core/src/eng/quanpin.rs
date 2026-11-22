use crate::eng::gram::RimeGramDb;
use crate::imp::polyphone::RimeDictCorpus;
use crate::imp::xiaohe::{canonical_syllable_set, xiaohe_reverse_map};
use crate::imp::ImportedLexiconIndex;
use crate::ret::RetentionEngine;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct QuanpinCandidate {
    pub text: String,
    pub score: f64,
    pub consumed_len: usize,
    pub syllables: Vec<String>,
    pub segments: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WordEdge {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub syllables: Vec<String>,
    pub lex_score: f64,
    pub split_penalty: f64,
    zero_split_penalty: f64,
    pub is_imported: bool,
}

#[derive(Debug, Clone)]
struct BeamState {
    score: f64,
    text: String,
    syllables: Vec<String>,
    segments: Vec<String>,
    prev_word: String,
    prev_two: String,
    prev_syl: String,
}

impl BeamState {
    fn tail_key(&self) -> String {
        if self.prev_two.is_empty() {
            self.prev_word.clone()
        } else {
            self.prev_two.clone()
        }
    }
}

type SyllableEdge = (usize, String, f64);
type SyllableDag = (usize, Vec<Vec<SyllableEdge>>, Vec<bool>);

const BETA_ZERO_SPLIT: f64 = 16.0;

fn is_zero_initial_merge(prev_syl: &str, next_syl: &str) -> bool {
    let Some(first) = next_syl.chars().next() else {
        return false;
    };
    if !matches!(first, 'a' | 'e' | 'o') {
        return false;
    }
    let merged = format!("{prev_syl}{next_syl}");
    canonical_syllable_set().contains(merged.as_str())
}

fn is_zero_initial_3split(s1: &str, s2: &str, s3: &str) -> bool {
    let Some(f2) = s2.chars().next() else {
        return false;
    };
    let Some(f3) = s3.chars().next() else {
        return false;
    };
    if !matches!(f2, 'a' | 'e' | 'o') || !matches!(f3, 'a' | 'e' | 'o') {
        return false;
    }
    let merged = format!("{s1}{s2}{s3}");
    canonical_syllable_set().contains(merged.as_str())
}

pub(super) fn can_zero_initial_split(syl: &str) -> bool {
    if !syl.is_ascii() {
        return false;
    }
    let canon = canonical_syllable_set();
    (1..syl.len()).any(|i| {
        let (left, right) = syl.split_at(i);
        // A standalone interjection `o` is not a word boundary in ordinary ao/uo finals.
        canon.contains(left)
            && canon.contains(right)
            && right != "o"
            && is_zero_initial_merge(left, right)
            || (canon.contains(left)
                && (i + 1..syl.len()).any(|j| {
                    let middle = &syl[i..j];
                    let tail = &syl[j..];
                    canon.contains(middle)
                        && canon.contains(tail)
                        && is_zero_initial_3split(left, middle, tail)
                }))
    })
}

/// Build the Character-Offset Syllable Segmentation DAG $G = (V, E)$ respecting `'` barriers.
fn build_quanpin_syllable_paths(raw_input: &str, syllable_split: bool) -> SyllableDag {
    let mut letters = String::with_capacity(raw_input.len());
    let mut barrier_at: Vec<bool> = vec![false];

    for ch in raw_input.chars() {
        if ch == '\'' || ch == ' ' {
            if let Some(last) = barrier_at.last_mut() {
                *last = true;
            }
        } else if ch.is_ascii_alphabetic() {
            let mut letter = ch.to_ascii_lowercase();
            if letter == 'v'
                && !barrier_at.last().copied().unwrap_or(false)
                && letters.ends_with(['j', 'q', 'x', 'y'])
            {
                letter = 'u';
            }
            letters.push(letter);
            barrier_at.push(false);
        }
    }

    let n = letters.len();
    let mut syl_edges: Vec<Vec<(usize, String, f64)>> = vec![Vec::new(); n];
    if n == 0 {
        return (0, syl_edges, barrier_at);
    }

    let canon = canonical_syllable_set();
    let bytes = letters.as_bytes();

    for i in 0..n {
        let max_j = (i + 6).min(n);
        let mut found_full = false;
        for j in (i + 1)..=max_j {
            // Respect explicit apostrophe/space barrier strictly inside (i, j)
            if (i + 1..j).any(|k| barrier_at[k]) {
                break;
            }
            let slice = std::str::from_utf8(&bytes[i..j]).unwrap_or("");
            if canon.contains(slice) {
                if !syllable_split || !can_zero_initial_split(slice) {
                    syl_edges[i].push((j, slice.to_string(), 0.0));
                }
                found_full = true;
            }
        }

        // If no canonical syllable covers offset `i`, allow 1-letter or 2-letter initial (`zh/ch/sh`) fallback
        // with a split penalty so partial typing and abbreviations still decode cleanly.
        if !found_full {
            if i + 2 <= n && !barrier_at[i + 1] {
                let s2 = std::str::from_utf8(&bytes[i..i + 2]).unwrap_or("");
                if matches!(s2, "zh" | "ch" | "sh") {
                    syl_edges[i].push((i + 2, s2.to_string(), 8.0));
                }
            }
            let s1 = std::str::from_utf8(&bytes[i..i + 1]).unwrap_or("");
            syl_edges[i].push((i + 1, s1.to_string(), 10.0));
        }
    }

    // Completing a tail must never reinterpret a finished exact syllable (jiang -> jian g*).
    if exact_reading_end(&syl_edges) != n {
        for i in 0..n {
            if syl_edges[i].iter().any(|(_, _, penalty)| *penalty == 0.0)
                || barrier_at[i + 1..n].iter().any(|b| *b)
            {
                continue;
            }
            let tail = &letters[i..];
            let mut completions: Vec<_> = canon.iter().filter(|s| s.starts_with(tail)).collect();
            completions.sort_unstable();
            if !completions.is_empty() {
                syl_edges[i] = completions
                    .into_iter()
                    .map(|s| (n, s.to_string(), 2.0))
                    .collect();
            }
        }
    }
    (n, syl_edges, barrier_at)
}

fn exact_reading_end(edges: &[Vec<SyllableEdge>]) -> usize {
    let mut reachable = vec![false; edges.len() + 1];
    reachable[0] = true;
    for i in 0..edges.len() {
        if reachable[i] {
            for (end, _, penalty) in &edges[i] {
                if *penalty == 0.0 {
                    reachable[*end] = true;
                }
            }
        }
    }
    reachable.iter().rposition(|found| *found).unwrap_or(0)
}

/// Build syllable edges from Xiaohe Shuangpin (`double_pinyin_flypy`) input.
fn build_xiaohe_syllable_paths(raw_input: &str) -> SyllableDag {
    let letters: String = raw_input
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let n = letters.len();
    let mut syl_edges: Vec<Vec<(usize, String, f64)>> = vec![Vec::new(); n];
    if n == 0 {
        return (0, syl_edges, Vec::new());
    }

    let rev = xiaohe_reverse_map();
    let mut i = 0usize;
    while i < n {
        if i + 2 <= n {
            let code = &letters[i..i + 2];
            if let Some(syls) = rev.get(code) {
                for (rank, &syl) in syls.iter().enumerate() {
                    let pen = if rank == 0 { 0.0 } else { 0.5 };
                    syl_edges[i].push((i + 2, syl.to_string(), pen));
                }
            }
            if syl_edges[i].is_empty() {
                syl_edges[i].push((i + 2, code.to_string(), 10.0));
            }
            i += 2;
        } else {
            // Trailing single keystroke (incomplete 2nd key of Xiaohe pair)
            let prefix = &letters[i..i + 1];
            let mut matched = 0usize;
            for (code, syls) in rev {
                if code.starts_with(prefix) {
                    if let Some(&first_syl) = syls.first() {
                        syl_edges[i].push((i + 1, first_syl.to_string(), 4.0));
                        matched += 1;
                        if matched >= 6 {
                            break;
                        }
                    }
                }
            }
            if syl_edges[i].is_empty() {
                syl_edges[i].push((i + 1, prefix.to_string(), 10.0));
            }
            i += 1;
        }
    }

    (n, syl_edges, Vec::new())
}

// Expand only legal syllables, preserving the input offsets and exact edges.
fn add_fuzzy_syllables(edges: &mut [Vec<SyllableEdge>]) {
    let canon = canonical_syllable_set();
    for outgoing in edges {
        let originals = outgoing.clone();
        for (end, syllable, penalty) in originals {
            let mut variants = vec![(syllable.clone(), 0.0)];
            for (a, b) in [
                ("zh", "z"),
                ("ch", "c"),
                ("sh", "s"),
                ("l", "n"),
                ("f", "h"),
            ] {
                let replacement = syllable
                    .strip_prefix(a)
                    .map(|tail| format!("{b}{tail}"))
                    .or_else(|| syllable.strip_prefix(b).map(|tail| format!("{a}{tail}")));
                if let Some(other) = replacement {
                    variants.push((other, 1.5));
                }
            }
            for (base, cost) in variants.clone() {
                for (a, b) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
                    let replacement = base
                        .strip_suffix(a)
                        .map(|head| format!("{head}{b}"))
                        .or_else(|| base.strip_suffix(b).map(|head| format!("{head}{a}")));
                    if let Some(other) = replacement {
                        variants.push((other, cost + 1.5));
                    }
                }
            }
            for (other, cost) in variants.into_iter().skip(1) {
                if canon.contains(other.as_str())
                    && !outgoing
                        .iter()
                        .any(|(e, s, p)| *e == end && *s == other && *p <= penalty + cost)
                {
                    outgoing.push((end, other, penalty + cost));
                }
            }
        }
    }
}

/// Long-Context Quanpin Decoder (`F06`):
/// Character-Offset Syllable DAG + Joint Lattice Score ($\log P_{uni} + GramScore + Frecency - \gamma_{seg} - \beta_{split}$)
/// + Pruned Viterbi Forward Beam + Top-K N-Best Extraction.
pub fn decode_quanpin_long_context(
    raw_input: &str,
    corpus: &RimeDictCorpus,
    gram: Option<&RimeGramDb>,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    top_k: usize,
) -> Vec<QuanpinCandidate> {
    decode_quanpin_with_fuzzy(
        raw_input, corpus, gram, imported, retention, top_k, false, false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_quanpin_with_fuzzy(
    raw_input: &str,
    corpus: &RimeDictCorpus,
    gram: Option<&RimeGramDb>,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    top_k: usize,
    fuzzy: bool,
    syllable_split: bool,
) -> Vec<QuanpinCandidate> {
    let mut dag = build_quanpin_syllable_paths(raw_input, syllable_split);
    if fuzzy {
        add_fuzzy_syllables(&mut dag.1);
    }
    if dag.0 == 0 {
        return Vec::new();
    }
    let mut candidates = decode_from_syllable_dag(
        &dag, true, syllable_split, corpus, gram, imported, retention, top_k.max(1),
    );
    let compact: String = raw_input
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if let Some(words) = corpus.abbreviations.get(&compact) {
        for (word, weight) in words {
            if retention.is_some_and(|r| r.erased_words.contains(word)) {
                continue;
            }
            let (syllables, _) = &corpus.word_readings[word];
            candidates.push(QuanpinCandidate {
                text: word.clone(),
                score: (*weight as f64 + 1.0).ln() - 12.0,
                consumed_len: dag.0,
                syllables: syllables.clone(),
                segments: vec![word.clone()],
            });
        }
    }
    let tail = &compact[exact_reading_end(&dag.1)..];
    let unfinished_vowel_tail = tail.len() >= 2 && tail.bytes().any(|b| b"aeiouv".contains(&b));
    if unfinished_vowel_tail
        || !candidates
            .iter()
            .any(|c| c.consumed_len == dag.0 && c.text.chars().all(crate::ret::is_cjk))
    {
        for corrected in correction_codes(&compact) {
            let corrected_dag = build_quanpin_syllable_paths(&corrected, syllable_split);
            let mut recovered = decode_from_syllable_dag(
                &corrected_dag,
                true,
                syllable_split,
                corpus,
                gram,
                imported,
                retention,
                3,
            );
            recovered.retain(|c| {
                c.consumed_len == corrected.len() && c.text.chars().all(crate::ret::is_cjk)
            });
            for candidate in &mut recovered {
                candidate.consumed_len = dag.0;
                candidate.score -= 6.0;
            }
            candidates.extend(recovered);
        }
    }
    if syllable_split {
        let forced_full_split = canonical_syllable_set().contains(compact.as_str())
            && can_zero_initial_split(&compact)
            && !raw_input.contains(['\'', ' ']);
        candidates.retain(|c| {
            !c.syllables.iter().any(|s| can_zero_initial_split(s))
                && (!forced_full_split
                    || (c.consumed_len == dag.0
                        && c.syllables.concat() == compact
                        && c.syllables.iter().all(|s| canonical_syllable_set().contains(s.as_str()))
                        && c.text.chars().all(crate::ret::is_cjk)))
        });
    }
    for candidate in &mut candidates {
        candidate.consumed_len = raw_offset(raw_input, candidate.consumed_len);
    }
    candidates
}

/// Decode Xiaohe Shuangpin (`double_pinyin_flypy`) using the exact same lattice + Wanxiang grammar.
pub fn decode_xiaohe_long_context(
    raw_input: &str,
    corpus: &RimeDictCorpus,
    gram: Option<&RimeGramDb>,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    top_k: usize,
) -> Vec<QuanpinCandidate> {
    let dag = build_xiaohe_syllable_paths(raw_input);
    if dag.0 == 0 {
        return Vec::new();
    }
    let mut candidates =
        decode_from_syllable_dag(&dag, false, false, corpus, gram, imported, retention, top_k.max(1));
    for candidate in &mut candidates {
        candidate.consumed_len = raw_offset(raw_input, candidate.consumed_len);
    }
    candidates
}

#[derive(Debug, Clone)]
struct AStarNode {
    score: f64,
    pos: usize,
    state_idx: usize,
}

impl PartialEq for AStarNode {
    fn eq(&self, other: &Self) -> bool {
        self.score.to_bits() == other.score.to_bits() && self.pos == other.pos
    }
}
impl Eq for AStarNode {}
impl PartialOrd for AStarNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for AStarNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_from_syllable_dag(
    dag: &SyllableDag,
    use_barriers: bool,
    syllable_split: bool,
    corpus: &RimeDictCorpus,
    gram: Option<&RimeGramDb>,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    top_k: usize,
) -> Vec<QuanpinCandidate> {
    let zero_split_penalty = if syllable_split { 0.0 } else { BETA_ZERO_SPLIT };
    let (n, syl_edges, barrier_vec) = dag;
    let n = *n;
    let barrier_at = if use_barriers {
        Some(barrier_vec.as_slice())
    } else {
        None
    };
    // 1. Expand syllable paths of length 1..=max_word_syls into WordEdges per start offset
    let max_word_syls = imported
        .map(|i| i.max_hotword_chars())
        .unwrap_or(0)
        .clamp(6, 10);
    let mut word_edges: Vec<Vec<WordEdge>> = vec![Vec::new(); n];
    let mut full_span_readings = Vec::new();
    let mut seen_readings = HashSet::new();

    for (start, edges_from_start) in word_edges.iter_mut().enumerate().take(n) {
        // BFS/DFS up to depth max_word_syls over syl_edges from `start`
        let mut stack: Vec<(usize, Vec<String>, f64, f64)> = vec![(start, Vec::new(), 0.0, 0.0)];
        while let Some((cur_pos, cur_syls, cur_pen, cur_zero_pen)) = stack.pop() {
            if !cur_syls.is_empty() {
                let mut added_for_span = 0usize;

                // Imported hotwords first (highest priority boost + full multi-char length bonus)
                if let Some(imp) = imported {
                    let compact = cur_syls.concat();
                    for e in imp.query_quanpin(&compact) {
                        if syllable_split && e.pinyin.split_whitespace().any(can_zero_initial_split) {
                            continue;
                        }
                        let is_erased = retention
                            .is_some_and(|r| r.erased_words.contains(&e.text));
                        if is_erased {
                            continue;
                        }
                        let w_chars = e.text.chars().count();
                        let lex_score = ((e.weight as f64).max(1.0)).ln() * 1.4
                            + 10.0
                            + ((w_chars.saturating_sub(1)) as f64) * 7.5;
                        edges_from_start.push(WordEdge {
                            start,
                            end: cur_pos,
                            text: e.text,
                            syllables: cur_syls.clone(),
                            lex_score,
                            split_penalty: cur_pen,
                            zero_split_penalty: 0.0,
                            is_imported: true,
                        });
                        added_for_span += 1;
                    }
                }

                // Corpus entries for exact spaced syllable sequence (base corpus capped at 6 syllables)
                if cur_syls.len() <= 6 {
                    let spaced = cur_syls.join(" ");
                    if let Some(cands) = corpus.spaced_pinyin_to_words.get(&spaced) {
                        if start == 0
                            && cur_pos == n
                            && (cur_pen == 0.0 || (!use_barriers && cur_syls.len() * 2 == n))
                            && seen_readings.insert(spaced)
                        {
                            full_span_readings.push((cur_syls.clone(), cur_zero_pen > 0.0));
                        }
                        let limit = if cur_syls.len() == 1 { 12 } else { 8 };
                        for (word, weight) in cands.iter().take(limit) {
                            let is_erased =
                                retention.is_some_and(|r| r.erased_words.contains(word));
                            if is_erased {
                                continue;
                            }
                            let w_chars = word.chars().count();
                            let raw_ln = ((*weight as f64) + 1.0).ln();
                            let lex_score = if w_chars <= 1 {
                                raw_ln * 0.72
                            } else {
                                raw_ln + ((w_chars - 1) as f64) * 6.0
                            };
                            edges_from_start.push(WordEdge {
                                start,
                                end: cur_pos,
                                text: word.clone(),
                                syllables: cur_syls.clone(),
                                lex_score,
                                split_penalty: cur_pen + cur_zero_pen,
                                zero_split_penalty: cur_zero_pen,
                                is_imported: false,
                            });
                            added_for_span += 1;
                        }
                    }
                }

                // Fallback for 1-syllable initial abbreviations or unknown spans
                if cur_syls.len() == 1 && added_for_span == 0 {
                    let prefix = &cur_syls[0];
                    if let Some(cands) = corpus.spaced_pinyin_to_words.get(prefix) {
                        if let Some((word, weight)) = cands
                            .iter()
                            .find(|(w, _)| !retention.is_some_and(|r| r.erased_words.contains(w)))
                        {
                            edges_from_start.push(WordEdge {
                                start,
                                end: cur_pos,
                                text: word.clone(),
                                syllables: cur_syls.clone(),
                                lex_score: ((*weight as f64) + 1.0).ln() - 4.0,
                                split_penalty: cur_pen + 4.0,
                                zero_split_penalty: 0.0,
                                is_imported: false,
                            });
                            added_for_span += 1;
                        }
                    }
                    if added_for_span == 0 {
                        edges_from_start.push(WordEdge {
                            start,
                            end: cur_pos,
                            text: cur_syls[0].clone(),
                            syllables: cur_syls.clone(),
                            lex_score: 0.5,
                            split_penalty: cur_pen + 12.0,
                            zero_split_penalty: 0.0,
                            is_imported: false,
                        });
                    }
                }
            }

            if cur_syls.len() < max_word_syls && cur_pos < n {
                for (next_pos, syl, pen) in &syl_edges[cur_pos] {
                    let mut next_zero_pen = cur_zero_pen;
                    if barrier_at.is_some_and(|b| !b[cur_pos]) {
                        if let Some(prev_syl) = cur_syls.last() {
                            if is_zero_initial_merge(prev_syl, syl) {
                                next_zero_pen += zero_split_penalty;
                            } else if cur_syls.len() >= 2 {
                                let prev2 = &cur_syls[cur_syls.len() - 2];
                                if is_zero_initial_3split(prev2, prev_syl, syl) {
                                    next_zero_pen += zero_split_penalty;
                                }
                            }
                        }
                    }
                    let mut next_syls = cur_syls.clone();
                    next_syls.push(syl.clone());
                    stack.push((*next_pos, next_syls, cur_pen + *pen, next_zero_pen));
                }
            }
        }
    }

    // 2. Pruned Viterbi Forward Beam Search (B = 32 per character offset)
    const BEAM_WIDTH: usize = 32;
    const GAMMA_SEG: f64 = 9.0;

    let mut beams: Vec<Vec<BeamState>> = vec![Vec::new(); n + 1];
    beams[0].push(BeamState {
        score: 0.0,
        text: String::new(),
        syllables: Vec::new(),
        segments: Vec::new(),
        prev_word: String::new(),
        prev_two: String::new(),
        prev_syl: String::new(),
    });

    let now_ms = RetentionEngine::now_epoch_ms();
    for pos in 0..n {
        if beams[pos].is_empty() || word_edges[pos].is_empty() {
            continue;
        }
        prune_beam(&mut beams[pos], BEAM_WIDTH);
        // Snapshot current beam at `pos`
        let current_beam = beams[pos].clone();
        for edge in &word_edges[pos] {
            let word_chars = edge.text.chars().count();
            let frecency_bonus = retention
                .map(|r| {
                    let f = r.decayed_score_at(&edge.text, now_ms);
                    let p = if r.pinned_words.contains(&edge.text) {
                        12.0
                    } else {
                        0.0
                    };
                    f * 2.0 + p
                })
                .unwrap_or(0.0);

            let unigram_gram = if word_chars >= 2 {
                gram.and_then(|g| g.lookup("", &edge.text)).unwrap_or(0.0)
            } else {
                0.0
            };

            let edge_first_syl = edge.syllables.first().map(String::as_str).unwrap_or("");
            let edge_last_syl = edge.syllables.last().cloned().unwrap_or_default();
            let boundary_unbarred = barrier_at.is_some_and(|b| !b[edge.start]);

            for st in &current_beam {
                let cross_zero_pen = if boundary_unbarred
                    && !st.prev_syl.is_empty()
                    && is_zero_initial_merge(&st.prev_syl, edge_first_syl)
                {
                    zero_split_penalty
                } else {
                    0.0
                };

                // Query Wanxiang GramDb over trailing 1..=4 char context suffixes (matching Octagram::Query)
                // Normalize raw .gram values by N-gram character length baseline: 10.6 + 3.0 * (k - 2)
                // so longer keys (e.g., 4-char 西安做完) do not gain an artificial +3.0 offset over 3-char keys (先做完).
                let ctx_gram: Option<(f64, usize, usize)> =
                    if let Some(g) = gram.filter(|_| !st.text.is_empty()) {
                        let tail_chars: Vec<char> = st.text.chars().rev().take(4).collect();
                        let mut best: Option<(f64, usize, f64, usize)> = None;
                        for len in (1..=tail_chars.len()).rev() {
                            let ctx: String = tail_chars[..len].iter().rev().collect();
                            if let Some(score) = g.lookup(&ctx, &edge.text) {
                                let total_chars = len + word_chars;
                                let baseline = 11.2 + 2.8 * (total_chars.saturating_sub(2) as f64);
                                let norm = score - baseline;
                                let rank_key = norm + (len as f64 - 1.0) * 0.15;
                                if best.is_none_or(|(b_rank, _, _, _)| rank_key > b_rank) {
                                    best = Some((rank_key, len, norm, total_chars));
                                }
                            }
                        }
                        best.map(|(_, len, norm, total_chars)| (norm, len, total_chars))
                    } else {
                        None
                    };

                let gram_score = if let Some((norm, ctx_len, total_chars)) = ctx_gram {
                    let ctx_bonus = if ctx_len >= 2 { 0.4 } else { 0.0 };
                    let base_hit = if total_chars <= 2 { -4.0 } else { 2.5 };
                    let collocation = base_hit + 2.8 * norm.max(-1.5) + ctx_bonus;
                    if edge.is_imported {
                        collocation.max(0.0)
                    } else {
                        collocation
                    }
                } else if edge.is_imported {
                    0.0
                } else if unigram_gram > 0.0 {
                    let u_base = 11.2 + 2.8 * (word_chars.saturating_sub(2) as f64);
                    let centered_u = (unigram_gram - u_base).max(0.0);
                    0.5 * centered_u - if st.prev_word.is_empty() { 0.0 } else { 7.5 }
                } else if !st.prev_word.is_empty() {
                    -7.5
                } else {
                    0.0
                };

                // A positive contextual collocation can justify an implicit word boundary
                // (去 + 西安); isolated merged syllables retain the full split penalty.
                let contextual_split_bonus = if ctx_gram.is_some() && gram_score > 0.0 {
                    (edge.zero_split_penalty + cross_zero_pen) * 0.5
                } else {
                    0.0
                };
                let step_score = edge.lex_score + gram_score + frecency_bonus + contextual_split_bonus
                    - GAMMA_SEG
                    - edge.split_penalty
                    - cross_zero_pen;

                let mut next_text = st.text.clone();
                next_text.push_str(&edge.text);
                let mut next_syls = st.syllables.clone();
                next_syls.extend(edge.syllables.iter().cloned());
                let mut next_segs = st.segments.clone();
                next_segs.push(edge.text.clone());

                let prev_two = if st.prev_word.is_empty() {
                    edge.text.clone()
                } else {
                    let combined = format!("{}{}", st.prev_word, edge.text);
                    if combined.chars().count() <= 4 {
                        combined
                    } else {
                        edge.text.clone()
                    }
                };

                beams[edge.end].push(BeamState {
                    score: st.score + step_score,
                    text: next_text,
                    syllables: next_syls,
                    segments: next_segs,
                    prev_word: edge.text.clone(),
                    prev_two,
                    prev_syl: edge_last_syl.clone(),
                });
            }
        }

        for edge in &word_edges[pos] {
            if beams[edge.end].len() > BEAM_WIDTH * 4 {
                prune_beam(&mut beams[edge.end], BEAM_WIDTH);
            }
        }
    }

    // 3. Extract Top-K unique full-sentence candidates + top-1 partial prefix alternatives if needed
    prune_beam(&mut beams[n], BEAM_WIDTH.max(top_k * 2));

    let mut pq = BinaryHeap::new();
    for (idx, st) in beams[n].iter().enumerate() {
        pq.push(AStarNode {
            score: st.score,
            pos: n,
            state_idx: idx,
        });
    }

    let mut out = Vec::new();
    let mut seen_texts = HashSet::new();
    while let Some(node) = pq.pop() {
        let st = &beams[node.pos][node.state_idx];
        if seen_texts.insert(st.text.clone()) {
            out.push(QuanpinCandidate {
                text: st.text.clone(),
                score: st.score,
                consumed_len: n,
                syllables: st.syllables.clone(),
                segments: st.segments.clone(),
            });
            if out.len() >= top_k {
                break;
            }
        }
    }

    // If single-word / prefix candidates help fill top_k (for short ambiguous queries like `xian`),
    // ensure top candidates from word_edges[0..n] are also included when out.len() < top_k.
    if out.len() < top_k {
        let mut full_span_cands: Vec<&WordEdge> = word_edges[0]
            .iter()
            .filter(|e| e.end == n)
            .collect();
        full_span_cands.sort_by(|a, b| {
            (b.lex_score - b.split_penalty)
                .partial_cmp(&(a.lex_score - a.split_penalty))
                .unwrap_or(Ordering::Equal)
        });
        for edge in full_span_cands {
            if seen_texts.insert(edge.text.clone()) {
                out.push(QuanpinCandidate {
                    text: edge.text.clone(),
                    score: edge.lex_score - GAMMA_SEG - edge.split_penalty,
                    consumed_len: n,
                    syllables: edge.syllables.clone(),
                    segments: vec![edge.text.clone()],
                });
                if out.len() >= top_k {
                    break;
                }
            }
        }
    }

    // Exact dictionary homophones bypass the sentence beam without expanding its word edges.
    let mut homophones = Vec::new();
    for (syllables, zero_split) in full_span_readings {
        let limit = if syllables.len() == 1 { 120 } else { 48 };
        for (word, weight) in corpus.spaced_pinyin_to_words[&syllables.join(" ")]
            .iter()
            .filter(|(word, _)| !retention.is_some_and(|r| r.erased_words.contains(word)))
            .take(limit)
        {
            homophones.push((word, (*weight as f64 + 1.0).ln(), syllables.clone(), zero_split));
        }
    }
    homophones.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| b.1.total_cmp(&a.1)));
    let floor = out.iter().map(|c| c.score).reduce(f64::min).unwrap_or(0.0) - 1.0;
    let max_raw_ln = homophones.iter().map(|c| c.1).reduce(f64::max).unwrap_or(0.0);
    let zero_floor = homophones.iter()
        .filter(|c| !c.3)
        .map(|c| floor + c.1 - max_raw_ln)
        .reduce(f64::min)
        .unwrap_or(floor) - 1.0;
    let mut single_count = 0;
    for (word, raw_ln, syllables, zero_split) in homophones {
        if syllables.len() == 1 {
            if single_count == 120 {
                continue;
            }
            single_count += 1;
        }
        if seen_texts.insert(word.clone()) {
            out.push(QuanpinCandidate {
                text: word.clone(),
                score: if zero_split {
                    (floor + raw_ln - max_raw_ln - BETA_ZERO_SPLIT).min(zero_floor)
                } else {
                    floor + raw_ln - max_raw_ln
                },
                consumed_len: n,
                syllables,
                segments: vec![word.clone()],
            });
        }
    }

    // Only a whole-syllable query forces all merged homophones ahead of splits.
    // Sentence paths keep contextual scores so 去西安旅游 remains natural.
    if use_barriers && !syllable_split
        && syl_edges[0].iter().any(|(end, _, penalty)| *end == n && *penalty == 0.0)
    {
        let zero_split = |c: &QuanpinCandidate| {
            let mut offset = 0;
            c.syllables.iter().enumerate().any(|(i, syl)| {
                let unbarred = barrier_vec.get(offset).is_some_and(|b| !b);
                offset += syl.len();
                unbarred && i > 0
                    && (is_zero_initial_merge(&c.syllables[i - 1], syl)
                        || (i > 1
                            && barrier_vec.get(offset - syl.len() - c.syllables[i - 1].len()).is_some_and(|b| !b)
                            && is_zero_initial_3split(&c.syllables[i - 2], &c.syllables[i - 1], syl)))
            })
        };
        if let Some(floor) = out.iter().filter(|c| !zero_split(c)).map(|c| c.score).reduce(f64::min) {
            for candidate in &mut out {
                if zero_split(candidate) {
                    candidate.score = candidate.score.min(floor - 1.0);
                }
            }
            out.sort_by(|a, b| zero_split(a).cmp(&zero_split(b))
                .then_with(|| b.score.total_cmp(&a.score)));
        }
    }

    // Keep full sentences first, then one best path per prefix and first-syllable homophones.
    // Prefixes are separate selectable spans and do not compete for the full-sentence beam.
    for end in (1..n).rev() {
        prune_beam(&mut beams[end], BEAM_WIDTH);
        if let Some(st) = beams[end]
            .iter()
            .find(|st| st.text.chars().all(crate::ret::is_cjk))
        {
            if seen_texts.insert(st.text.clone()) {
                out.push(QuanpinCandidate {
                    text: st.text.clone(),
                    score: st.score,
                    consumed_len: end,
                    syllables: st.syllables.clone(),
                    segments: st.segments.clone(),
                });
            }
        }
    }
    for edge in &word_edges[0] {
        if edge.end < n
            && edge.syllables.len() == 1
            && edge.text.chars().count() == 1
            && edge.text.chars().all(crate::ret::is_cjk)
            && seen_texts.insert(edge.text.clone())
        {
            out.push(QuanpinCandidate {
                text: edge.text.clone(),
                score: edge.lex_score - edge.split_penalty,
                consumed_len: edge.end,
                syllables: edge.syllables.clone(),
                segments: vec![edge.text.clone()],
            });
        }
    }
    // Extend first-syllable choices only after the existing selectable prefixes.
    let mut seen_prefix_readings = HashSet::new();
    let floor = out.iter().map(|c| c.score).reduce(f64::min).unwrap_or(0.0) - 1.0;
    for edge in &word_edges[0] {
        if edge.end >= n
            || edge.syllables.len() != 1
            || !seen_prefix_readings.insert((edge.end, &edge.syllables[0]))
        {
            continue;
        }
        if let Some(words) = corpus.spaced_pinyin_to_words.get(&edge.syllables[0]) {
            let max_raw_ln = words.first().map_or(0.0, |(_, w)| (*w as f64 + 1.0).ln());
            for (word, weight) in words
                .iter()
                .filter(|(word, _)| {
                    word.chars().count() == 1
                        && word.chars().all(crate::ret::is_cjk)
                        && !retention.is_some_and(|r| r.erased_words.contains(word))
                })
                .take(24)
            {
                if seen_texts.insert(word.clone()) {
                    out.push(QuanpinCandidate {
                        text: word.clone(),
                        score: floor + (*weight as f64 + 1.0).ln() - max_raw_ln,
                        consumed_len: edge.end,
                        syllables: edge.syllables.clone(),
                        segments: vec![word.clone()],
                    });
                }
            }
        }
    }

    out
}

fn raw_offset(raw_input: &str, letters: usize) -> usize {
    raw_input
        .char_indices()
        .filter(|(_, ch)| ch.is_ascii_alphabetic())
        .nth(letters.saturating_sub(1))
        .map_or(raw_input.len(), |(i, ch)| i + ch.len_utf8())
}

fn prune_beam(beam: &mut Vec<BeamState>, width: usize) {
    beam.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    let mut kept = Vec::with_capacity(width.min(beam.len()));
    let mut seen_tails: HashMap<String, usize> = HashMap::new();
    let mut seen_texts: HashSet<String> = HashSet::new();

    for st in beam.drain(..) {
        if !seen_texts.insert(st.text.clone()) {
            continue;
        }
        let tail = st.tail_key();
        let cnt = seen_tails.entry(tail).or_insert(0);
        if *cnt >= 3 && kept.len() >= width / 2 {
            continue;
        }
        *cnt += 1;
        kept.push(st);
        if kept.len() >= width {
            break;
        }
    }
    *beam = kept;
}

/// Try one edit only, with bounded input, variants and lattice evaluations.
fn correction_codes(code: &str) -> Vec<String> {
    if !(4..=48).contains(&code.len()) {
        return Vec::new();
    }
    let canonical = canonical_syllable_set();
    let legal = |text: &str| {
        let mut reachable = vec![false; text.len() + 1];
        reachable[0] = true;
        for i in 0..text.len() {
            if !reachable[i] {
                continue;
            }
            for end in i + 1..=(i + 6).min(text.len()) {
                if canonical.contains(&text[i..end]) {
                    reachable[end] = true;
                }
            }
        }
        reachable[text.len()]
    };
    let mut codes = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |bytes: Vec<u8>| {
        let text = String::from_utf8(bytes).expect("ASCII pinyin");
        if text != code && legal(&text) && seen.insert(text.clone()) && codes.len() < 24 {
            codes.push(text);
        }
    };
    let bytes = code.as_bytes();
    for i in (0..bytes.len()).rev() {
        // Neighbouring keys and vowel slips account for the common wrong-key endings.
        let neighbours = match bytes[i] {
            b'a' => "qwsz",
            b'b' => "vghn",
            b'c' => "xdfv",
            b'd' => "serfcx",
            b'e' => "wsdr",
            b'f' => "drtgvc",
            b'g' => "ftyhbv",
            b'h' => "gyujnb",
            b'i' => "ujko",
            b'j' => "huikmn",
            b'k' => "jiolm",
            b'l' => "kop",
            b'm' => "njk",
            b'n' => "bhjm",
            b'o' => "iklp",
            b'p' => "ol",
            b'q' => "wa",
            b'r' => "edft",
            b's' => "awedxz",
            b't' => "rfgy",
            b'u' => "yhji",
            b'v' => "cfgb",
            b'w' => "qase",
            b'x' => "zsdc",
            b'y' => "tghu",
            b'z' => "asx",
            _ => "",
        };
        for b in neighbours.bytes().chain(b"aeiouv".iter().copied()) {
            let mut edited = bytes.to_vec();
            edited[i] = b;
            add(edited);
        }
        if i + 1 < bytes.len() {
            let mut edited = bytes.to_vec();
            edited.swap(i, i + 1);
            add(edited);
        }
        let mut edited = bytes.to_vec();
        edited.remove(i);
        add(edited);
        for b in b'a'..=b'z' {
            let mut edited = bytes.to_vec();
            edited.insert(i, b);
            add(edited);
        }
    }
    for b in b'a'..=b'z' {
        let mut edited = bytes.to_vec();
        edited.push(b);
        add(edited);
    }
    codes
}
