use crate::eng::gram::RimeGramDb;
use crate::imp::polyphone::RimeDictCorpus;
use crate::imp::xiaohe::{canonical_syllable_set, xiaohe_reverse_map};
use crate::imp::ImportedLexiconIndex;
use crate::ret::{RetentionEngine, UserPhrase};
use std::borrow::Cow;
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
const INITIAL_PENALTY: f64 = 12.0;
const VALID_INPUT_INITIAL_PENALTY: f64 = 20.0;

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
        for j in (i + 1)..=max_j {
            // Respect explicit apostrophe/space barrier strictly inside (i, j)
            if (i + 1..j).any(|k| barrier_at[k]) {
                break;
            }
            let slice = std::str::from_utf8(&bytes[i..j]).unwrap_or("");
            let normalized = match slice {
                "nue" => "nve",
                "lue" => "lve",
                _ => slice,
            };
            if canon.contains(normalized) && (!syllable_split || !can_zero_initial_split(normalized)) {
                syl_edges[i].push((j, normalized.to_string(), 0.0));
            }
        }

        // Initials coexist with full syllables, including nasal m/n.
        if i + 2 <= n && !barrier_at[i + 1] {
            let initial = &letters[i..i + 2];
            if matches!(initial, "zh" | "ch" | "sh") {
                syl_edges[i].push((i + 2, initial.to_string(), INITIAL_PENALTY));
            }
        }
        let initial = &letters[i..i + 1];
        if b"bpmfdtnlgkhjqxrzcsyw".contains(&bytes[i])
            && !syl_edges[i].iter().any(|(end, _, _)| *end == i + 1)
        {
            syl_edges[i].push((i + 1, initial.to_string(), INITIAL_PENALTY));
        }
    }

    // Fully spelled input has a stronger prior than an abbreviation interpretation.
    let exact_end = exact_reading_end(&syl_edges);
    if exact_end == n {
        for edges in &mut syl_edges {
            for (_, syllable, penalty) in edges {
                if *penalty == INITIAL_PENALTY && is_initial(syllable) {
                    *penalty = VALID_INPUT_INITIAL_PENALTY;
                }
            }
        }
    }
    // Completing a tail must never reinterpret a finished exact syllable (jiang -> jian g*).
    if exact_end != n {
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
                syl_edges[i].extend(
                    completions
                        .into_iter()
                        .map(|s| (n, s.to_string(), 2.0))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    (n, syl_edges, barrier_at)
}

fn is_initial(s: &str) -> bool {
    matches!(s, "zh" | "ch" | "sh")
        || (s.len() == 1 && b"bpmfdtnlgkhjqxrzcsyw".contains(&s.as_bytes()[0]))
}

fn exact_reading_end(edges: &[Vec<SyllableEdge>]) -> usize {
    let mut reachable = vec![false; edges.len() + 1];
    reachable[0] = true;
    for i in 0..edges.len() {
        if reachable[i] {
            for (end, syllable, penalty) in &edges[i] {
                if *penalty == 0.0 && (edges.len() == 1 || !matches!(syllable.as_str(), "m" | "n"))
                {
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
            let mut matches: Vec<_> = rev
                .iter()
                .filter(|(code, _)| code.starts_with(prefix))
                .filter_map(|(code, syls)| syls.first().map(|&syl| (code, syl)))
                .collect();
            matches.sort_by(|a, b| a.0.cmp(b.0));
            for (_, syllable) in matches.into_iter().take(6) {
                syl_edges[i].push((i + 1, syllable.to_string(), 4.0));
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
    decode_quanpin_context(raw_input, corpus, gram, imported, retention, top_k, fuzzy, syllable_split, "")
}

fn phrase_readings(retention: Option<&RetentionEngine>) -> HashMap<Cow<'_, str>, Vec<&UserPhrase>> {
    let mut readings: HashMap<_, Vec<_>> = HashMap::new();
    if let Some(retention) = retention {
        for phrase in retention.user_phrases.values() {
            if !retention.erased_words.contains(&phrase.word) {
                if let Some(code) = phrase.spaced_quanpin() {
                    readings.entry(code).or_default().push(phrase);
                }
            }
        }
        for phrases in readings.values_mut() {
            phrases.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
        }
    }
    readings
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_quanpin_context(
    raw_input: &str,
    corpus: &RimeDictCorpus,
    gram: Option<&RimeGramDb>,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    top_k: usize,
    fuzzy: bool,
    syllable_split: bool,
    left_context: &str,
) -> Vec<QuanpinCandidate> {
    let mut dag = build_quanpin_syllable_paths(raw_input, syllable_split);
    if fuzzy {
        add_fuzzy_syllables(&mut dag.1);
    }
    if dag.0 == 0 {
        return Vec::new();
    }
    let phrases = phrase_readings(retention);
    let mut candidates = decode_from_syllable_dag(
        &dag,
        true,
        syllable_split,
        corpus,
        gram,
        imported,
        retention,
        &phrases,
        top_k.max(1),
        left_context,
    );
    let compact: String = raw_input
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if let Some(words) = corpus
        .abbreviations
        .get(&compact)
        .filter(|_| !canonical_syllable_set().contains(compact.as_str()))
    {
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
    let has_full_word = candidates.iter().any(|c| {
        c.consumed_len == dag.0
            && c.segments.len() == 1
            && c.text.chars().count() >= 2
            && c.text.chars().all(crate::ret::is_cjk)
    });
    let unfinished_vowel_tail =
        !has_full_word && tail.len() >= 2 && tail.bytes().any(|b| b"aeiouv".contains(&b));
    let reliable_exact_word = candidates.iter().any(|c| {
        c.consumed_len == dag.0
            && crate::imp::xiaohe::matches_quanpin_reading(&c.syllables, raw_input)
            && c.segments.iter().any(|word| {
                word.chars().count() >= 2
                    && (corpus
                        .word_readings
                        .get(word)
                        .is_some_and(|(_, weight)| *weight >= 1000)
                        || retention.is_some_and(|r| r.user_phrases.contains_key(word))
                        || imported.is_some_and(|i| i.contains_hotword(word)))
            })
    });
    let scattered = !canonical_syllable_set().contains(compact.as_str())
        && exact_reading_end(&dag.1) == dag.0
        && !reliable_exact_word;
    if scattered
        || unfinished_vowel_tail
        || !candidates
            .iter()
            .any(|c| c.consumed_len == dag.0 && c.text.chars().all(crate::ret::is_cjk))
    {
        let mut corrections = Vec::new();
        for corrected in correction_codes(raw_input, false) {
            let corrected_dag = build_quanpin_syllable_paths(&corrected, syllable_split);
            let mut recovered = decode_from_syllable_dag(
                &corrected_dag,
                true,
                syllable_split,
                corpus,
                gram,
                imported,
                retention,
                &phrases,
                3,
                left_context,
            );
            recovered.retain(|c| {
                c.consumed_len == corrected_dag.0
                    && c.text.chars().all(crate::ret::is_cjk)
                    && crate::imp::xiaohe::matches_quanpin_reading(&c.syllables, &corrected)
                    && (!scattered || (c.segments.len() == 1 && c.text.chars().count() >= 2))
            });
            for candidate in &mut recovered {
                candidate.consumed_len = dag.0;
                candidate.score -= 6.0;
            }
            corrections.extend(recovered);
        }
        if scattered {
            corrections.sort_by(|a, b| b.score.total_cmp(&a.score));
            corrections.truncate(3);
        }
        candidates.extend(corrections);
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
                        && c.syllables
                            .iter()
                            .all(|s| canonical_syllable_set().contains(s.as_str()))
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
    decode_xiaohe_context(raw_input, corpus, gram, imported, retention, top_k, "")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_xiaohe_context(
    raw_input: &str,
    corpus: &RimeDictCorpus,
    gram: Option<&RimeGramDb>,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    top_k: usize,
    left_context: &str,
) -> Vec<QuanpinCandidate> {
    let dag = build_xiaohe_syllable_paths(raw_input);
    if dag.0 == 0 {
        return Vec::new();
    }
    let phrases = phrase_readings(retention);
    let mut candidates = decode_from_syllable_dag(
        &dag,
        false,
        false,
        corpus,
        gram,
        imported,
        retention,
        &phrases,
        top_k.max(1),
        left_context,
    );
    if !candidates
        .iter()
        .any(|c| c.consumed_len == dag.0 && c.segments.iter().any(|s| s.chars().count() >= 2))
    {
        for code in correction_codes(raw_input, true) {
            let corrected = build_xiaohe_syllable_paths(&code);
            let mut recovered = decode_from_syllable_dag(
                &corrected,
                false,
                false,
                corpus,
                gram,
                imported,
                retention,
                &phrases,
                3,
                left_context,
            );
            recovered.retain(|c| {
                c.consumed_len == corrected.0
                    && c.segments.len() == 1
                    && c.text.chars().count() >= 2
                    && c.text.chars().all(crate::ret::is_cjk)
            });
            for c in &mut recovered {
                c.score -= 6.0;
                c.consumed_len = dag.0;
            }
            candidates.extend(recovered);
        }
    }
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
    phrases: &HashMap<Cow<'_, str>, Vec<&UserPhrase>>,
    top_k: usize,
    left_context: &str,
) -> Vec<QuanpinCandidate> {
    let zero_split_penalty = if syllable_split { 0.0 } else { BETA_ZERO_SPLIT };
    let (n, syl_edges, barrier_vec) = dag;
    let n = *n;
    let initial_penalty = if exact_reading_end(syl_edges) == n {
        VALID_INPUT_INITIAL_PENALTY
    } else {
        INITIAL_PENALTY
    };
    let barrier_at = if use_barriers {
        Some(barrier_vec.as_slice())
    } else {
        None
    };
    // 1. Expand syllable paths of length 1..=max_word_syls into WordEdges per start offset
    let max_word_syls = imported
        .map(|i| i.max_hotword_chars())
        .unwrap_or(0)
        .max(retention.map_or(0, |r| r.max_user_phrase_syllables()))
        .clamp(6, 10);
    let mut word_edges: Vec<Vec<WordEdge>> = vec![Vec::new(); n];
    let mut full_span_readings = Vec::new();
    let mut seen_readings = HashSet::new();

    for (start, edges_from_start) in word_edges.iter_mut().enumerate().take(n) {
        // BFS/DFS up to depth max_word_syls over syl_edges from `start`
        let mut stack: Vec<(usize, Vec<String>, f64, f64)> = vec![(start, Vec::new(), 0.0, 0.0)];
        let mut expanded = 0;
        while let Some((cur_pos, cur_syls, cur_pen, cur_zero_pen)) = stack.pop() {
            expanded += 1;
            if expanded > 2048 { break; }
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
                let spaced = cur_syls.join(" ");
                if cur_syls.len() <= 6 {
                    if let Some(cands) = corpus.spaced_pinyin_to_words.get(&spaced) {
                        if start == 0
                            && cur_pos == n
                            && (cur_pen == 0.0 || (!use_barriers && cur_syls.len() * 2 == n))
                            && seen_readings.insert(spaced.clone())
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

                if cur_syls.len() >= 2 && cur_syls.iter().any(|s| is_initial(s)) {
                    let key: String = cur_syls.iter().filter_map(|s| s.chars().next()).collect();
                    if let Some(readings) = corpus.initial_readings.get(&key) {
                        for reading in readings {
                            let full: Vec<String> = reading.split_whitespace().map(str::to_string).collect();
                            if !cur_syls
                                .iter()
                                .zip(&full)
                                .all(|(s, f)| s == f || (is_initial(s) && f.starts_with(s)))
                            {
                                continue;
                            }
                            // m/n already have zero-cost nasal edges; charge their initial interpretation here.
                            let extra = cur_syls
                                .iter()
                                .zip(&full)
                                .filter(|(s, f)| s != f && canonical_syllable_set().contains(s.as_str()))
                                .count() as f64
                                * initial_penalty;
                            for (word, weight) in corpus.spaced_pinyin_to_words[reading].iter().take(8) {
                                if retention.is_some_and(|r| r.erased_words.contains(word)) {
                                    continue;
                                }
                                edges_from_start.push(WordEdge {
                                    start,
                                    end: cur_pos,
                                    text: word.clone(),
                                    syllables: full.clone(),
                                    lex_score: (*weight as f64 + 1.0).ln() + (full.len() - 1) as f64 * 6.0,
                                    split_penalty: cur_pen + cur_zero_pen + extra,
                                    zero_split_penalty: cur_zero_pen,
                                    is_imported: false,
                                });
                                added_for_span += 1;
                            }
                        }
                    }
                }
                if let Some(matching_phrases) = phrases.get(spaced.as_str()) {
                    for phrase in matching_phrases {
                        if phrase.word.chars().count() != cur_syls.len() {
                            continue;
                        }
                        edges_from_start.push(WordEdge {
                            start,
                            end: cur_pos,
                            text: phrase.word.clone(),
                            syllables: cur_syls.clone(),
                            lex_score: (phrase.count as f64 + 1.0).ln()
                                + (cur_syls.len() - 1) as f64 * 6.0
                                + 16.0,
                            split_penalty: cur_pen + cur_zero_pen,
                            zero_split_penalty: cur_zero_pen,
                            is_imported: false,
                        });
                        added_for_span += 1;
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
                for (next_pos, syl, pen) in syl_edges[cur_pos].iter().rev() {
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
                    let f = r.reading_frecency_at(
                        &edge.text,
                        edge.syllables.first().map(String::as_str).unwrap_or(""),
                        !use_barriers,
                        now_ms,
                    );
                    if word_chars == 1 && f == 0.0 && r.decayed_score_at(&edge.text, now_ms) > 0.0 {
                        return 0.0;
                    }
                    // Keep personal frequency on the same logarithmic scale as lex_score.
                    // A common polyphonic character must not overwhelm a whole word.
                    let frequency = if word_chars >= 2 {
                        ((1.0 + f).ln() * 1.5).min(4.5)
                    } else {
                        ((1.0 + f).ln() * 0.6).min(1.8)
                    };
                    let p = if r.pinned_words.contains(&edge.text)
                        && (word_chars >= 2 || (edge.start == 0 && edge.end == n))
                    {
                        12.0
                    } else {
                        0.0
                    };
                    frequency + p
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
                    if let Some(g) = gram.filter(|_| !st.text.is_empty() || !left_context.is_empty()) {
                        let tail_chars: Vec<char> = st.text.chars().rev().chain(left_context.chars().rev()).take(4).collect();
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

/// Rank one-edit spellings before bounding decoder work; separators are immutable.
pub(super) fn correction_codes(code: &str, flypy: bool) -> Vec<String> {
    if !(4..=48).contains(&code.len())
        || !code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'\'' || b == b' ')
    {
        return Vec::new();
    }
    let legal = |text: &str| {
        text.split(['\'', ' ']).all(|part| {
            if part.is_empty() {
                return false;
            }
            if flypy {
                return part.len() % 2 == 0
                    && part.as_bytes().chunks(2).all(|pair| {
                        xiaohe_reverse_map()
                            .contains_key(std::str::from_utf8(pair).expect("ASCII code"))
                    });
            }
            exact_reading_end(&build_quanpin_syllable_paths(part, false).1) == part.len()
        })
    };
    let bytes = code.as_bytes();
    let mut ranked = Vec::new();
    let mut add = |bytes: Vec<u8>, quality: u8, position: usize| {
        let text = String::from_utf8(bytes).expect("ASCII pinyin");
        if text != code && legal(&text) {
            ranked.push((quality, position, text));
        }
    };
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_lowercase() {
            continue;
        }
        if i + 1 < bytes.len() && bytes[i + 1].is_ascii_lowercase() {
            let mut edited = bytes.to_vec();
            edited.swap(i, i + 1);
            add(edited, 0, i);
        }
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
        for b in neighbours.bytes() {
            let mut edited = bytes.to_vec();
            edited[i] = b;
            add(edited, 1, i);
        }
        if !flypy {
            for b in b"aeiouv" {
                let mut edited = bytes.to_vec();
                edited[i] = *b;
                add(edited, 2, i);
            }
            let mut edited = bytes.to_vec();
            edited.remove(i);
            add(edited, 3, i);
            for b in b'a'..=b'z' {
                let mut edited = bytes.to_vec();
                edited.insert(i, b);
                add(edited, 4, i);
            }
        }
    }
    if !flypy {
        for b in b'a'..=b'z' {
            let mut edited = bytes.to_vec();
            edited.push(b);
            add(edited, 4, bytes.len());
        }
    }
    ranked.sort();
    let mut seen = HashSet::new();
    ranked
        .into_iter()
        .filter(|(_, _, text)| seen.insert(text.clone()))
        .take(24)
        .map(|(_, _, text)| text)
        .collect()
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    fn aliases_initials_and_corrections_respect_explicit_boundaries() {
        let dag = build_quanpin_syllable_paths("nue'dai", false);
        assert!(dag.1[0]
            .iter()
            .any(|(end, s, p)| *end == 3 && s == "nve" && *p == 0.0));
        assert!(!build_quanpin_syllable_paths("nu'e", false).1[0]
            .iter()
            .any(|(_, s, _)| s == "nve"));
        let dag = build_quanpin_syllable_paths("mna", false);
        assert!(dag.1[0].iter().any(|(end, s, _)| *end == 1 && s == "m"));
        assert!(dag.1[1].iter().any(|(end, s, _)| *end == 2 && s == "n"));
        assert!(!build_quanpin_syllable_paths("z'hong", false).1[0]
            .iter()
            .any(|(end, _, _)| *end > 1));
        assert!(correction_codes("zhonggou", false)
            .iter()
            .take(5)
            .any(|c| c == "zhongguo"));
        for code in correction_codes("zhong'gou", false) {
            assert_eq!(code.matches('\'').count(), 1);
            assert!(code.split('\'').all(|part| !part.is_empty()));
        }
        assert!(correction_codes("nihk", true).iter().all(|code| code
            .as_bytes()
            .chunks(2)
            .all(|pair| xiaohe_reverse_map().contains_key(std::str::from_utf8(pair).unwrap()))));
    }

    #[test]
    fn xiaohe_odd_key_completions_follow_stable_code_order() {
        // ba, bb, bc, bd, bf, bg are the first six b-prefixed codes.
        let expected = vec![
            (3, "ba".to_string(), 4.0),
            (3, "bin".to_string(), 4.0),
            (3, "bao".to_string(), 4.0),
            (3, "bai".to_string(), 4.0),
            (3, "ben".to_string(), 4.0),
            (3, "beng".to_string(), 4.0),
        ];
        assert_eq!(build_xiaohe_syllable_paths("nib").1[2], expected);
        assert_eq!(
            build_xiaohe_syllable_paths("b").1[0],
            expected
                .iter()
                .map(|(_, syl, p)| (1, syl.clone(), *p))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn user_phrase_paths_keep_explicit_pinyin_boundaries_and_legacy_readings() {
        let corpus = RimeDictCorpus::load_or_fallback(None);
        let mut retention = RetentionEngine::default();
        let word = "芳庵";
        retention.register_typed_phrase(word, "fang an", "fhan", RetentionEngine::now_epoch_ms());
        for reading in ["fang an", "fangan"] {
            retention.user_phrases.get_mut(word).unwrap().quanpin = reading.into();
            assert_eq!(retention.query_user_phrases("fang'an", false).len(), 1);
            assert!(retention.query_user_phrases("fan'gan", false).is_empty());
            for (code, present) in [("fang'an", true), ("fan'gan", false)] {
                let phrases = phrase_readings(Some(&retention));
                let dag = build_quanpin_syllable_paths(code, false);
                let candidates = decode_from_syllable_dag(
                    &dag, true, false, &corpus, None, None, Some(&retention), &phrases, 5, "",
                );
                assert_eq!(candidates.iter().any(|c| c.text == word), present, "{reading}: {code}");
            }
        }
        retention.erased_words.insert(word.into());
        assert!(phrase_readings(Some(&retention)).is_empty());
    }

    #[test]
    fn mismatched_reading_does_not_receive_decoder_frequency_or_pin_bonus() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let corpus = RimeDictCorpus::load_or_fallback(Some(&assets));
        let now = RetentionEngine::now_epoch_ms();
        let mut r = RetentionEngine::default();
        r.record_contextual_selection("的", "de", None, None, now, 100.0, true);
        r.pin_word("的");
        for (code, word) in [("di", "的"), ("de", "的")] {
            let baseline = decode_quanpin_long_context(code, &corpus, None, None, None, 120);
            let personal = decode_quanpin_long_context(code, &corpus, None, None, Some(&r), 120);
            let score =
                |items: &[QuanpinCandidate]| items.iter().find(|c| c.text == word).unwrap().score;
            if code == "di" {
                assert_eq!(score(&personal), score(&baseline));
            } else {
                assert!(score(&personal) > score(&baseline));
            }
        }
    }
}
