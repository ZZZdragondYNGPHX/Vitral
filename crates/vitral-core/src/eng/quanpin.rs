use crate::eng::gram::RimeGramDb;
use crate::imp::polyphone::RimeDictCorpus;
use crate::imp::xiaohe::{canonical_syllable_set, xiaohe_reverse_map};
use crate::imp::ImportedLexiconIndex;
use crate::ret::RetentionEngine;
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct QuanpinCandidate {
    pub text: String,
    pub score: f64,
    pub consumed_len: usize,
    pub syllables: Vec<String>,
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct Reading {
    offset: usize,
    len: usize,
}

impl Reading {
    fn store<'a>(arena: &mut Vec<&'a str>, syllables: impl IntoIterator<Item = &'a str>) -> Self {
        let offset = arena.len();
        arena.extend(syllables);
        Self {
            offset,
            len: arena.len() - offset,
        }
    }

    fn syllables<'a>(self, arena: &'a [&'a str]) -> &'a [&'a str] {
        &arena[self.offset..self.offset + self.len]
    }

    fn owned(self, arena: &[&str]) -> Vec<String> {
        self.syllables(arena)
            .iter()
            .map(|&s| s.to_owned())
            .collect()
    }
}

#[derive(Debug, Clone)]
struct WordEdge<'a> {
    start: usize,
    end: usize,
    text: &'a str,
    reading: Reading,
    lex_score: f64,
    split_penalty: f64,
    zero_split_penalty: f64,
    is_imported: bool,
}

#[derive(Clone)]
struct BeamState {
    step: Option<BeamStep>,
    text_hash: u64,
    all_cjk: bool,
}

#[derive(Clone, Copy)]
struct BeamStep {
    score: f64,
    start: usize,
    state: u8,
    edge: u8,
}

#[derive(Clone, Copy)]
struct BeamPaths<'a> {
    beams: &'a [Vec<BeamState>],
    edges: &'a [Vec<WordEdge<'a>>],
}

impl<'a> BeamPaths<'a> {
    fn edge(self, step: BeamStep) -> &'a WordEdge<'a> {
        &self.edges[step.start][usize::from(step.edge)]
    }

    fn previous(self, step: BeamStep) -> &'a BeamState {
        &self.beams[step.start][usize::from(step.state)]
    }

    fn reverse_edges(self, step: Option<BeamStep>) -> impl Iterator<Item = &'a WordEdge<'a>> {
        std::iter::successors(step, move |step| self.previous(*step).step)
            .map(move |step| self.edge(step))
    }
}

// A rolling hash lets paths with different word boundaries share a lookup key.
// Equality compares the actual bytes when hashes agree.
struct BeamText<'a> {
    paths: BeamPaths<'a>,
    step: BeamStep,
    hash: u64,
}

impl BeamText<'_> {
    fn reverse_bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.paths
            .reverse_edges(Some(self.step))
            .flat_map(|edge| edge.text.bytes().rev())
    }
}

impl Hash for BeamText<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

impl PartialEq for BeamText<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.reverse_bytes().eq(other.reverse_bytes())
    }
}

impl Eq for BeamText<'_> {}

struct BeamTail<'a> {
    previous: &'a str,
    word: &'a str,
}

impl Hash for BeamTail<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if self.previous.is_empty() {
            self.word.as_bytes().hash(state);
        } else {
            // Joined tails are at most four Unicode characters.
            let mut buffer = [0; 16];
            let middle = self.previous.len();
            let end = middle + self.word.len();
            buffer[..middle].copy_from_slice(self.previous.as_bytes());
            buffer[middle..end].copy_from_slice(self.word.as_bytes());
            buffer[..end].hash(state);
        }
    }
}

impl PartialEq for BeamTail<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.previous
            .bytes()
            .chain(self.word.bytes())
            .eq(other.previous.bytes().chain(other.word.bytes()))
    }
}

impl Eq for BeamTail<'_> {}

type SyllableEdge = (usize, Cow<'static, str>, f64);
struct SyllableDag {
    len: usize,
    edges: Vec<Vec<SyllableEdge>>,
    barriers: Vec<bool>,
    exact_end: usize,
}

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
    is_canonical_merge(&[prev_syl, next_syl])
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
    is_canonical_merge(&[s1, s2, s3])
}

fn is_canonical_merge(parts: &[&str]) -> bool {
    let mut merged = [0; 6];
    let mut len = 0;
    for part in parts {
        let end = len + part.len();
        let Some(target) = merged.get_mut(len..end) else {
            return false;
        };
        target.copy_from_slice(part.as_bytes());
        len = end;
    }
    canonical_syllable_set().contains(std::str::from_utf8(&merged[..len]).expect("pinyin"))
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
    let mut syl_edges: Vec<Vec<SyllableEdge>> = vec![Vec::new(); n];
    if n == 0 {
        return SyllableDag {
            len: 0,
            edges: syl_edges,
            barriers: barrier_at,
            exact_end: 0,
        };
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
            if let Some(&syllable) = canon.get(normalized) {
                if !syllable_split || !can_zero_initial_split(syllable) {
                    syl_edges[i].push((j, Cow::Borrowed(syllable), 0.0));
                }
            }
        }

        // Initials coexist with full syllables, including nasal m/n.
        if i + 2 <= n && !barrier_at[i + 1] {
            if let Some(initial) = match &letters[i..i + 2] {
                "zh" => Some("zh"),
                "ch" => Some("ch"),
                "sh" => Some("sh"),
                _ => None,
            } {
                syl_edges[i].push((i + 2, Cow::Borrowed(initial), INITIAL_PENALTY));
            }
        }
        const INITIALS: [&str; 26] = [
            "", "b", "c", "d", "", "f", "g", "h", "", "j", "k", "l", "m", "n", "", "p", "q", "r",
            "s", "t", "", "", "w", "x", "y", "z",
        ];
        let initial = INITIALS[usize::from(bytes[i] - b'a')];
        if !initial.is_empty() && !syl_edges[i].iter().any(|(end, _, _)| *end == i + 1) {
            syl_edges[i].push((i + 1, Cow::Borrowed(initial), INITIAL_PENALTY));
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
            syl_edges[i].extend(completions.into_iter().map(|&s| (n, Cow::Borrowed(s), 2.0)));
        }
    }
    SyllableDag {
        len: n,
        edges: syl_edges,
        barriers: barrier_at,
        exact_end,
    }
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
                if *penalty == 0.0 && (edges.len() == 1 || !matches!(syllable.as_ref(), "m" | "n"))
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
    let mut syl_edges: Vec<Vec<SyllableEdge>> = vec![Vec::new(); n];
    if n == 0 {
        return SyllableDag {
            len: 0,
            edges: syl_edges,
            barriers: Vec::new(),
            exact_end: 0,
        };
    }

    let rev = xiaohe_reverse_map();
    let mut i = 0usize;
    while i < n {
        if i + 2 <= n {
            let code = &letters[i..i + 2];
            if let Some(syls) = rev.get(code) {
                for (rank, &syl) in syls.iter().enumerate() {
                    let pen = if rank == 0 { 0.0 } else { 0.5 };
                    syl_edges[i].push((i + 2, Cow::Borrowed(syl), pen));
                }
            }
            if syl_edges[i].is_empty() {
                syl_edges[i].push((i + 2, Cow::Owned(code.to_owned()), 10.0));
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
                syl_edges[i].push((i + 1, Cow::Borrowed(syllable), 4.0));
            }
            if syl_edges[i].is_empty() {
                syl_edges[i].push((i + 1, Cow::Owned(prefix.to_owned()), 10.0));
            }
            i += 1;
        }
    }

    let exact_end = exact_reading_end(&syl_edges);
    SyllableDag {
        len: n,
        edges: syl_edges,
        barriers: Vec::new(),
        exact_end,
    }
}

// Expand only legal syllables, preserving the input offsets and exact edges.
fn add_fuzzy_syllables(edges: &mut [Vec<SyllableEdge>]) {
    let canon = canonical_syllable_set();
    for outgoing in edges {
        for index in 0..outgoing.len() {
            let (end, ref syllable, penalty) = outgoing[index];
            let mut base = [0; 8];
            base[..syllable.len()].copy_from_slice(syllable.as_bytes());
            let base = std::str::from_utf8(&base[..syllable.len()]).expect("ASCII syllable");
            let mut replacement = [0; 8];
            let other_initial = [
                ("zh", "z"),
                ("ch", "c"),
                ("sh", "s"),
                ("l", "n"),
                ("f", "h"),
            ]
            .into_iter()
            .find_map(|(a, b)| {
                base.strip_prefix(a)
                    .map(|tail| (b, tail))
                    .or_else(|| base.strip_prefix(b).map(|tail| (a, tail)))
            })
            .map(|(prefix, tail)| join_ascii(&mut replacement, prefix, tail));
            let mut add = |other: &str, cost: f64| {
                if let Some(&canonical) = canon.get(other) {
                    if !outgoing.iter().any(|(e, s, p)| {
                        *e == end && s.as_ref() == canonical && *p <= penalty + cost
                    }) {
                        outgoing.push((end, Cow::Borrowed(canonical), penalty + cost));
                    }
                }
            };
            if let Some(other) = other_initial {
                add(other, 1.5);
            }
            for (base, cost) in [(base, 1.5)]
                .into_iter()
                .chain(other_initial.map(|s| (s, 3.0)))
            {
                for (a, b) in [("ang", "an"), ("eng", "en"), ("ing", "in")] {
                    if let Some((head, suffix)) = base
                        .strip_suffix(a)
                        .map(|head| (head, b))
                        .or_else(|| base.strip_suffix(b).map(|head| (head, a)))
                    {
                        add(join_ascii(&mut [0; 8], head, suffix), cost);
                    }
                }
            }
        }
    }
}

fn join_ascii<'a>(buffer: &'a mut [u8], left: &str, right: &str) -> &'a str {
    let mid = left.len();
    let end = mid + right.len();
    buffer[..mid].copy_from_slice(left.as_bytes());
    buffer[mid..end].copy_from_slice(right.as_bytes());
    std::str::from_utf8(&buffer[..end]).expect("ASCII syllable")
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
    decode_quanpin_context(
        raw_input,
        corpus,
        gram,
        imported,
        retention,
        top_k,
        fuzzy,
        syllable_split,
        "",
    )
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
        add_fuzzy_syllables(&mut dag.edges);
    }
    if dag.len == 0 {
        return Vec::new();
    }
    let mut incoming = Vec::new();
    let mut candidates = decode_from_syllable_dag(
        &dag,
        true,
        syllable_split,
        corpus,
        gram,
        imported,
        retention,
        DecodeTarget::WithPrefixes(top_k.max(1)),
        left_context,
        &mut incoming,
    );
    let compact: String = raw_input
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if !canonical_syllable_set().contains(compact.as_str()) {
        for (word, weight) in corpus.abbreviations(&compact) {
            if retention.is_some_and(|r| r.erased_words.contains(word)) {
                continue;
            }
            let Some((syllables, _)) = corpus.word_reading(word) else {
                continue;
            };
            candidates.push(QuanpinCandidate {
                text: word.to_owned(),
                score: (weight as f64 + 1.0).ln() - 12.0,
                consumed_len: dag.len,
                syllables: syllables.map(str::to_owned).collect(),
                segments: vec![word.to_owned()],
            });
        }
    }
    let exact_end = dag.exact_end;
    let tail = &compact[exact_end..];
    let has_full_word = candidates.iter().any(|c| {
        c.consumed_len == dag.len
            && c.segments.len() == 1
            && c.text.chars().count() >= 2
            && c.text.chars().all(crate::ret::is_cjk)
    });
    let unfinished_vowel_tail =
        !has_full_word && tail.len() >= 2 && tail.bytes().any(|b| b"aeiouv".contains(&b));
    let reliable_exact_word = candidates.iter().any(|c| {
        c.consumed_len == dag.len
            && crate::imp::xiaohe::matches_quanpin_reading(&c.syllables, raw_input)
            && c.segments.iter().any(|word| {
                word.chars().count() >= 2
                    && (corpus
                        .word_weight(word)
                        .is_some_and(|weight| weight >= 1000)
                        || retention.is_some_and(|r| r.user_phrases.contains_key(word))
                        || imported.is_some_and(|i| i.contains_hotword(word)))
            })
    });
    let scattered = !canonical_syllable_set().contains(compact.as_str())
        && exact_end == dag.len
        && !reliable_exact_word;
    let has_full_chinese = candidates
        .iter()
        .any(|c| c.consumed_len == dag.len && c.text.chars().all(crate::ret::is_cjk));
    if scattered || unfinished_vowel_tail || !has_full_chinese {
        let mut corrections = Vec::new();
        // A long, readable prefix is already usable. Only revisit its spelling when
        // a correction has support from a whole word.
        let can_complete_tail = dag.edges.get(exact_end).is_some_and(|edges| {
            edges
                .iter()
                .any(|(end, _, penalty)| *end == dag.len && *penalty == 2.0)
        });
        let word_only = scattered || (exact_end >= 8 && has_full_chinese && can_complete_tail);
        let plan = correction_plan(raw_input, false);
        let edits = correction_dags(&plan, syllable_split, corpus, imported, retention);
        for edit in edits
            .iter()
            .filter(|edit| !word_only || edit.word_weight > 0)
            .take(if word_only { 2 } else { 4 })
        {
            let mut recovered = decode_from_syllable_dag(
                &edit.dag,
                true,
                syllable_split,
                corpus,
                gram,
                imported,
                retention,
                DecodeTarget::Correction,
                left_context,
                &mut incoming,
            );
            recovered.retain(|c| {
                c.consumed_len == edit.dag.len
                    && c.text.chars().all(crate::ret::is_cjk)
                    && crate::imp::xiaohe::matches_quanpin_reading(&c.syllables, edit.code)
                    && (!scattered || (c.segments.len() == 1 && c.text.chars().count() >= 2))
            });
            for candidate in &mut recovered {
                candidate.consumed_len = dag.len;
                candidate.score -= 6.0;
            }
            let reliable = edit.quality <= 1 && edit.word_weight >= 1000 && !recovered.is_empty();
            corrections.extend(recovered);
            if reliable {
                break;
            }
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
                    || (c.consumed_len == dag.len
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
    if dag.len == 0 {
        return Vec::new();
    }
    let mut incoming = Vec::new();
    let mut candidates = decode_from_syllable_dag(
        &dag,
        false,
        false,
        corpus,
        gram,
        imported,
        retention,
        DecodeTarget::WithPrefixes(top_k.max(1)),
        left_context,
        &mut incoming,
    );
    if !candidates
        .iter()
        .any(|c| c.consumed_len == dag.len && c.segments.iter().any(|s| s.chars().count() >= 2))
    {
        let plan = correction_plan(raw_input, true);
        for edit in correction_dags(&plan, false, corpus, imported, retention)
            .iter()
            .filter(|edit| edit.word_weight > 0)
            .take(2)
        {
            let mut recovered = decode_from_syllable_dag(
                &edit.dag,
                false,
                false,
                corpus,
                gram,
                imported,
                retention,
                DecodeTarget::Correction,
                left_context,
                &mut incoming,
            );
            recovered.retain(|c| {
                c.consumed_len == edit.dag.len
                    && c.segments.len() == 1
                    && c.text.chars().count() >= 2
                    && c.text.chars().all(crate::ret::is_cjk)
            });
            for c in &mut recovered {
                c.score -= 6.0;
                c.consumed_len = dag.len;
            }
            let reliable = edit.quality <= 1 && edit.word_weight >= 1000 && !recovered.is_empty();
            candidates.extend(recovered);
            if reliable {
                break;
            }
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

enum DecodeTarget {
    WithPrefixes(usize),
    Correction,
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
    target: DecodeTarget,
    left_context: &str,
    incoming: &mut Vec<Vec<BeamStep>>,
) -> Vec<QuanpinCandidate> {
    let top_k = match target {
        DecodeTarget::WithPrefixes(top_k) => top_k,
        DecodeTarget::Correction => 3,
    };
    let zero_split_penalty = if syllable_split { 0.0 } else { BETA_ZERO_SPLIT };
    let n = dag.len;
    let syl_edges = &dag.edges;
    let barrier_vec = &dag.barriers;
    let initial_penalty = if dag.exact_end == n {
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
    const MAX_WORD_SYLLABLES: usize = 10;
    const MAX_EXACT_PATHS_PER_START: usize = 1024;
    const MAX_PATHS_PER_START: usize = 256;
    const MAX_INITIAL_PATHS_PER_START: usize = 64;
    const MAX_EDGES_PER_SPAN: usize = 24;
    const MAX_EDGES_PER_START: usize = 80;
    let imported = imported.filter(|index| !index.is_empty());
    let imported_chars = imported.map_or(0, ImportedLexiconIndex::max_hotword_chars);
    let max_word_syls = imported_chars
        .max(retention.map_or(0, |r| r.max_user_phrase_syllables()))
        .clamp(6, MAX_WORD_SYLLABLES);
    let mut word_edges: Vec<Vec<WordEdge>> = vec![Vec::new(); n];
    let mut full_span_readings = Vec::new();
    let mut readings = Vec::new();
    let mut edges_from_start = Vec::new();
    let mut seen_readings = HashSet::new();

    let mut stack = Vec::new();
    let mut spaced = String::new();
    let mut compact = String::new();
    let mut edge_counts = vec![0; n + 1];
    let mut seen_edges = HashSet::new();
    let canon = canonical_syllable_set();
    for (start, kept_edges) in word_edges.iter_mut().enumerate() {
        stack.clear();
        stack.push((start, [""; MAX_WORD_SYLLABLES], 0, 0.0, 0.0));
        let mut exact_paths = 0;
        let mut ordinary_paths = 0;
        let mut initial_paths = 0;
        while let Some((cur_pos, syllables, len, cur_pen, cur_zero_pen)) = stack.pop() {
            if cur_pen >= INITIAL_PENALTY && use_barriers {
                initial_paths += 1;
                if initial_paths > MAX_INITIAL_PATHS_PER_START {
                    continue;
                }
            } else if cur_pen == 0.0 && cur_zero_pen == 0.0 {
                exact_paths += 1;
                if exact_paths > MAX_EXACT_PATHS_PER_START {
                    continue;
                }
            } else {
                ordinary_paths += 1;
                if ordinary_paths > MAX_PATHS_PER_START {
                    continue;
                }
            }
            let cur_syls = &syllables[..len];
            if len > 0 {
                let mut path_reading = None;
                let mut path_reading = |readings: &mut Vec<_>| {
                    *path_reading
                        .get_or_insert_with(|| Reading::store(readings, cur_syls.iter().copied()))
                };
                let mut added_for_span = 0usize;

                // Imported hotwords first (highest priority boost + full multi-char length bonus)
                if let Some(imp) = imported {
                    compact.clear();
                    compact.extend(cur_syls.iter().copied());
                    for e in imp
                        .quanpin_entries(&compact)
                        .iter()
                        .take(MAX_EDGES_PER_SPAN)
                    {
                        if syllable_split && e.pinyin.split_whitespace().any(can_zero_initial_split)
                        {
                            continue;
                        }
                        let is_erased = retention.is_some_and(|r| r.erased_words.contains(&e.text));
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
                            text: &e.text,
                            reading: path_reading(&mut readings),
                            lex_score,
                            split_penalty: cur_pen,
                            zero_split_penalty: 0.0,
                            is_imported: true,
                        });
                        added_for_span += 1;
                    }
                }

                // Corpus entries for exact spaced syllable sequence (base corpus capped at 6 syllables)
                spaced.clear();
                spaced.push_str(cur_syls[0]);
                for syllable in &cur_syls[1..] {
                    spaced.push(' ');
                    spaced.push_str(syllable);
                }
                if len <= 6 {
                    let cands = corpus.spaced_pinyin_words(&spaced);
                    if cands.len() != 0 {
                        if start == 0
                            && cur_pos == n
                            && (cur_pen == 0.0 || (!use_barriers && cur_syls.len() * 2 == n))
                            && !seen_readings.contains(spaced.as_str())
                        {
                            seen_readings.insert(spaced.clone());
                            full_span_readings
                                .push((path_reading(&mut readings), cur_zero_pen > 0.0));
                        }
                        let limit = if cur_syls.len() == 1 { 12 } else { 8 };
                        for (word, weight) in cands.take(limit) {
                            let is_erased =
                                retention.is_some_and(|r| r.erased_words.contains(word));
                            if is_erased {
                                continue;
                            }
                            let w_chars = word.chars().count();
                            let raw_ln = ((weight as f64) + 1.0).ln();
                            let lex_score = if w_chars <= 1 {
                                raw_ln * 0.72
                            } else {
                                raw_ln + ((w_chars - 1) as f64) * 6.0
                            };
                            edges_from_start.push(WordEdge {
                                start,
                                end: cur_pos,
                                text: word,
                                reading: path_reading(&mut readings),
                                lex_score,
                                split_penalty: cur_pen + cur_zero_pen,
                                zero_split_penalty: cur_zero_pen,
                                is_imported: false,
                            });
                            added_for_span += 1;
                        }
                    }
                }

                if (2..=6).contains(&len) && cur_syls.iter().any(|s| is_initial(s)) {
                    let mut key = [0; 6];
                    for (index, syllable) in cur_syls.iter().enumerate() {
                        key[index] = syllable.as_bytes()[0];
                    }
                    let key = std::str::from_utf8(&key[..len]).expect("ASCII pinyin");
                    let initial_readings = corpus.initial_readings(key);
                    let limit = if cur_pen >= 2.0 * initial_penalty {
                        6
                    } else {
                        12
                    };
                    // Leave room for multiple readings while retaining the highest-weight
                    // homophones of each matching reading, e.g. shi + j -> shi jie / shi jian.
                    const MAX_WORDS_PER_INITIAL_READING: usize = 4;
                    let mut added = 0;
                    for reading in initial_readings {
                        let mut extra = 0.0;
                        let mut reading_syllables = reading.split_whitespace();
                        let matches = cur_syls.iter().all(|&typed| {
                            reading_syllables.next().is_some_and(|full| {
                                if typed == full {
                                    true
                                } else if is_initial(typed) && full.starts_with(typed) {
                                    // Nasal m/n carry no DAG penalty for their literal reading.
                                    if canon.contains(typed) {
                                        extra += initial_penalty;
                                    }
                                    true
                                } else {
                                    false
                                }
                            })
                        });
                        if !matches || reading_syllables.next().is_some() {
                            continue;
                        }
                        let mut full = None;
                        for (word, weight) in corpus
                            .spaced_pinyin_words(reading)
                            .take(MAX_WORDS_PER_INITIAL_READING)
                        {
                            if retention.is_some_and(|r| r.erased_words.contains(word)) {
                                continue;
                            }
                            let full = *full.get_or_insert_with(|| {
                                Reading::store(&mut readings, reading.split_whitespace())
                            });
                            edges_from_start.push(WordEdge {
                                start,
                                end: cur_pos,
                                text: word,
                                reading: full,
                                lex_score: (weight as f64 + 1.0).ln() + (len - 1) as f64 * 6.0,
                                split_penalty: cur_pen + cur_zero_pen + extra,
                                zero_split_penalty: cur_zero_pen,
                                is_imported: false,
                            });
                            added += 1;
                            added_for_span += 1;
                            if added == limit {
                                break;
                            }
                        }
                        if added == limit {
                            break;
                        }
                    }
                }
                if let Some(retention) = retention {
                    for phrase in retention
                        .user_phrases_by_reading(&spaced)
                        .take(MAX_EDGES_PER_SPAN)
                    {
                        if phrase.word.chars().count() != cur_syls.len() {
                            continue;
                        }
                        edges_from_start.push(WordEdge {
                            start,
                            end: cur_pos,
                            text: &phrase.word,
                            reading: path_reading(&mut readings),
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
                    let prefix = cur_syls[0];
                    if let Some((word, weight)) = corpus
                        .spaced_pinyin_words(prefix)
                        .find(|(w, _)| !retention.is_some_and(|r| r.erased_words.contains(*w)))
                    {
                        edges_from_start.push(WordEdge {
                            start,
                            end: cur_pos,
                            text: word,
                            reading: path_reading(&mut readings),
                            lex_score: ((weight as f64) + 1.0).ln() - 4.0,
                            split_penalty: cur_pen + 4.0,
                            zero_split_penalty: 0.0,
                            is_imported: false,
                        });
                        added_for_span += 1;
                    }
                    if added_for_span == 0 {
                        edges_from_start.push(WordEdge {
                            start,
                            end: cur_pos,
                            text: prefix,
                            reading: path_reading(&mut readings),
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
                    let next_pen = cur_pen + *pen;
                    if use_barriers && next_pen > 3.0 * initial_penalty {
                        continue;
                    }
                    let mut next_syls = syllables;
                    next_syls[len] = syl;
                    stack.push((*next_pos, next_syls, len + 1, next_pen, next_zero_pen));
                }
            }
        }
        // Keep the highest scoring reading for each word and span.
        edges_from_start.sort_by(|a, b| {
            (b.lex_score - b.split_penalty).total_cmp(&(a.lex_score - a.split_penalty))
        });
        seen_edges.clear();
        edge_counts.fill(0);
        let mut kept = 0;
        edges_from_start.retain(|edge| {
            if kept == MAX_EDGES_PER_START || edge_counts[edge.end] == MAX_EDGES_PER_SPAN {
                return false;
            }
            if !seen_edges.insert((edge.end, edge.text)) {
                return false;
            }
            edge_counts[edge.end] += 1;
            kept += 1;
            true
        });
        kept_edges.append(&mut edges_from_start);
    }

    // 2. Pruned Viterbi Forward Beam Search (B = 32 per character offset)
    const BEAM_WIDTH: usize = 32;
    const GAMMA_SEG: f64 = 9.0;

    let mut beams: Vec<Vec<BeamState>> = vec![Vec::new(); n + 1];
    beams[0].push(BeamState {
        step: None,
        text_hash: 0xcbf29ce484222325,
        all_cjk: true,
    });

    let now_ms = RetentionEngine::now_epoch_ms();
    if incoming.len() <= n {
        incoming.resize_with(n + 1, Vec::new);
    }
    let mut encoded_word = Vec::new();
    let mut gram_prefixes = Vec::new();
    for pos in 0..=n {
        if pos > 0 {
            let width = if pos == n {
                BEAM_WIDTH.max(top_k * 2)
            } else {
                BEAM_WIDTH
            };
            beams[pos] = collect_beam(&mut incoming[pos], &beams, &word_edges, width);
        }
        if pos == n || beams[pos].is_empty() {
            continue;
        }
        let current_beam = &beams[pos];
        let paths = BeamPaths {
            beams: &beams,
            edges: &word_edges,
        };
        gram_prefixes.clear();
        if let Some(gram) = gram {
            for state in current_beam {
                let mut tail = ['\0'; 4];
                let mut len = 0;
                for ch in paths
                    .reverse_edges(state.step)
                    .flat_map(|edge| edge.text.chars().rev())
                    .chain(left_context.chars().rev())
                    .take(4)
                {
                    len += 1;
                    tail[4 - len] = ch;
                }
                let mut prefixes = [None; 4];
                for (offset, prefix) in prefixes.iter_mut().enumerate().take(len) {
                    *prefix = gram.prefix_state_chars(&tail[3 - offset..]);
                }
                gram_prefixes.push(prefixes);
            }
        }
        for (edge_index, edge) in word_edges[pos].iter().enumerate() {
            let word_chars = edge.text.chars().count();
            let frecency_bonus = retention
                .map(|r| {
                    let f = r.reading_frecency_at(
                        edge.text,
                        edge.reading.syllables(&readings)[0],
                        !use_barriers,
                        now_ms,
                    );
                    if word_chars == 1 && f == 0.0 && r.decayed_score_at(edge.text, now_ms) > 0.0 {
                        return 0.0;
                    }
                    // Keep personal frequency on the same logarithmic scale as lex_score.
                    // A common polyphonic character must not overwhelm a whole word.
                    let frequency = if word_chars >= 2 {
                        ((1.0 + f).ln() * 1.5).min(4.5)
                    } else {
                        ((1.0 + f).ln() * 0.6).min(1.8)
                    };
                    let p = if r.pinned_words.contains(edge.text)
                        && (word_chars >= 2 || (edge.start == 0 && edge.end == n))
                    {
                        12.0
                    } else {
                        0.0
                    };
                    frequency + p
                })
                .unwrap_or(0.0);

            let unigram_gram = gram.map_or(0.0, |gram| {
                encoded_word.clear();
                RimeGramDb::encode_into(edge.text, &mut encoded_word);
                if word_chars >= 2 {
                    gram.lookup_encoded(&encoded_word).unwrap_or(0.0)
                } else {
                    0.0
                }
            });
            let edge_first_syl = edge.reading.syllables(&readings)[0];
            let boundary_unbarred =
                zero_split_penalty > 0.0 && barrier_at.is_some_and(|b| !b[edge.start]);

            for (state_index, st) in current_beam.iter().enumerate() {
                let cross_zero_pen = if boundary_unbarred
                    && st.step.is_some_and(|step| {
                        let previous = paths.edge(step).reading.syllables(&readings);
                        is_zero_initial_merge(previous[previous.len() - 1], edge_first_syl)
                    }) {
                    zero_split_penalty
                } else {
                    0.0
                };

                // Normalize each hit by key length before comparing context suffixes.
                let ctx_gram = gram.and_then(|g| {
                    let mut best = None;
                    for (index, prefix) in gram_prefixes[state_index].iter().enumerate().rev() {
                        let Some((node, unit)) = prefix else { continue };
                        if let Some(score) =
                            g.lookup_from_state_encoded(*node, *unit, &encoded_word)
                        {
                            let len = index + 1;
                            let total_chars = len + word_chars;
                            let baseline = 11.2 + 2.8 * total_chars.saturating_sub(2) as f64;
                            let norm = score - baseline;
                            let rank = norm + index as f64 * 0.15;
                            if best.is_none_or(|(old, _, _, _)| rank > old) {
                                best = Some((rank, len, norm, total_chars));
                            }
                        }
                    }
                    best.map(|(_, len, norm, total_chars)| (norm, len, total_chars))
                });

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
                    0.5 * centered_u - if st.step.is_none() { 0.0 } else { 7.5 }
                } else if st.step.is_some() {
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
                let step_score =
                    edge.lex_score + gram_score + frecency_bonus + contextual_split_bonus
                        - GAMMA_SEG
                        - edge.split_penalty
                        - cross_zero_pen;

                incoming[edge.end].push(BeamStep {
                    score: st.step.map_or(0.0, |step| step.score) + step_score,
                    start: pos,
                    // Only intermediate beams and retained word edges produce steps.
                    state: u8::try_from(state_index).expect("intermediate beam width"),
                    edge: u8::try_from(edge_index).expect("retained word edge limit"),
                });
            }
        }
    }

    // 3. Extract Top-K unique full-sentence candidates and selectable prefixes.
    let paths = BeamPaths {
        beams: &beams,
        edges: &word_edges,
    };
    let mut pq = BinaryHeap::new();
    for (idx, st) in beams[n].iter().enumerate() {
        pq.push(AStarNode {
            score: st.step.expect("nonempty beam path").score,
            pos: n,
            state_idx: idx,
        });
    }

    let mut out = Vec::new();
    let mut seen_texts = HashSet::new();
    let mut reverse_edges = Vec::new();
    let mut text = String::new();
    while let Some(node) = pq.pop() {
        let step = beams[node.pos][node.state_idx]
            .step
            .expect("nonempty beam path");
        if let Some(candidate) = beam_candidate(
            paths,
            step,
            n,
            &readings,
            &mut reverse_edges,
            &mut text,
            &mut seen_texts,
        ) {
            out.push(candidate);
            if out.len() >= top_k {
                break;
            }
        }
    }

    // If single-word / prefix candidates help fill top_k (for short ambiguous queries like `xian`),
    // ensure top candidates from word_edges[0..n] are also included when out.len() < top_k.
    if out.len() < top_k {
        let mut full_span_cands: Vec<&WordEdge> =
            word_edges[0].iter().filter(|e| e.end == n).collect();
        full_span_cands.sort_by(|a, b| {
            (b.lex_score - b.split_penalty)
                .partial_cmp(&(a.lex_score - a.split_penalty))
                .unwrap_or(Ordering::Equal)
        });
        for edge in full_span_cands {
            if !seen_texts.contains(edge.text) {
                seen_texts.insert(edge.text.to_owned());
                out.push(QuanpinCandidate {
                    text: edge.text.to_owned(),
                    score: edge.lex_score - GAMMA_SEG - edge.split_penalty,
                    consumed_len: n,
                    syllables: edge.reading.owned(&readings),
                    segments: vec![edge.text.to_owned()],
                });
                if out.len() >= top_k {
                    break;
                }
            }
        }
    }

    // Exact dictionary homophones bypass the sentence beam without expanding its word edges.
    let mut homophones = Vec::new();
    for (reading, zero_split) in full_span_readings {
        let limit = if reading.len == 1 { 120 } else { 48 };
        for (word, weight) in corpus
            .spaced_pinyin_words(&reading.syllables(&readings).join(" "))
            .filter(|(word, _)| !retention.is_some_and(|r| r.erased_words.contains(*word)))
            .take(limit)
        {
            homophones.push((word, (weight as f64 + 1.0).ln(), reading, zero_split));
        }
    }
    homophones.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| b.1.total_cmp(&a.1)));
    let floor = out.iter().map(|c| c.score).reduce(f64::min).unwrap_or(0.0) - 1.0;
    let max_raw_ln = homophones
        .iter()
        .map(|c| c.1)
        .reduce(f64::max)
        .unwrap_or(0.0);
    let zero_floor = homophones
        .iter()
        .filter(|c| !c.3)
        .map(|c| floor + c.1 - max_raw_ln)
        .reduce(f64::min)
        .unwrap_or(floor)
        - 1.0;
    let mut single_count = 0;
    for (word, raw_ln, reading, zero_split) in homophones {
        if reading.len == 1 {
            if single_count == 120 {
                continue;
            }
            single_count += 1;
        }
        if seen_texts.insert(word.to_owned()) {
            out.push(QuanpinCandidate {
                text: word.to_owned(),
                score: if zero_split {
                    (floor + raw_ln - max_raw_ln - BETA_ZERO_SPLIT).min(zero_floor)
                } else {
                    floor + raw_ln - max_raw_ln
                },
                consumed_len: n,
                syllables: reading.owned(&readings),
                segments: vec![word.to_owned()],
            });
        }
    }

    // Only a whole-syllable query forces all merged homophones ahead of splits.
    // Sentence paths keep contextual scores so 去西安旅游 remains natural.
    if use_barriers
        && !syllable_split
        && syl_edges[0]
            .iter()
            .any(|(end, _, penalty)| *end == n && *penalty == 0.0)
    {
        let zero_split = |c: &QuanpinCandidate| {
            let mut offset = 0;
            c.syllables.iter().enumerate().any(|(i, syl)| {
                let unbarred = barrier_vec.get(offset).is_some_and(|b| !b);
                offset += syl.len();
                unbarred
                    && i > 0
                    && (is_zero_initial_merge(&c.syllables[i - 1], syl)
                        || (i > 1
                            && barrier_vec
                                .get(offset - syl.len() - c.syllables[i - 1].len())
                                .is_some_and(|b| !b)
                            && is_zero_initial_3split(
                                &c.syllables[i - 2],
                                &c.syllables[i - 1],
                                syl,
                            )))
            })
        };
        if let Some(floor) = out
            .iter()
            .filter(|c| !zero_split(c))
            .map(|c| c.score)
            .reduce(f64::min)
        {
            for candidate in &mut out {
                if zero_split(candidate) {
                    candidate.score = candidate.score.min(floor - 1.0);
                }
            }
            out.sort_by(|a, b| {
                zero_split(a)
                    .cmp(&zero_split(b))
                    .then_with(|| b.score.total_cmp(&a.score))
            });
        }
    }

    if matches!(target, DecodeTarget::Correction) {
        return out;
    }

    // Keep full sentences first, then one best path per prefix and first-syllable homophones.
    // Prefixes are separate selectable spans and do not compete for the full-sentence beam.
    for end in (1..n).rev() {
        if let Some(step) = beams[end]
            .iter()
            .find(|st| st.all_cjk)
            .and_then(|st| st.step)
        {
            if let Some(candidate) = beam_candidate(
                paths,
                step,
                end,
                &readings,
                &mut reverse_edges,
                &mut text,
                &mut seen_texts,
            ) {
                out.push(candidate);
            }
        }
    }
    for edge in &word_edges[0] {
        if edge.end < n
            && edge.reading.len == 1
            && edge.text.chars().count() == 1
            && edge.text.chars().all(crate::ret::is_cjk)
            && !seen_texts.contains(edge.text)
        {
            seen_texts.insert(edge.text.to_owned());
            out.push(QuanpinCandidate {
                text: edge.text.to_owned(),
                score: edge.lex_score - edge.split_penalty,
                consumed_len: edge.end,
                syllables: edge.reading.owned(&readings),
                segments: vec![edge.text.to_owned()],
            });
        }
    }
    // Extend first-syllable choices only after the existing selectable prefixes.
    let mut seen_prefix_readings = HashSet::new();
    let floor = out.iter().map(|c| c.score).reduce(f64::min).unwrap_or(0.0) - 1.0;
    for edge in &word_edges[0] {
        if edge.end >= n
            || edge.reading.len != 1
            || !seen_prefix_readings.insert((edge.end, edge.reading.syllables(&readings)[0]))
        {
            continue;
        }
        let words = corpus.spaced_pinyin_words(edge.reading.syllables(&readings)[0]);
        let max_raw_ln = words
            .clone()
            .next()
            .map_or(0.0, |(_, w)| (w as f64 + 1.0).ln());
        for (word, weight) in words
            .filter(|(word, _)| {
                word.chars().count() == 1
                    && word.chars().all(crate::ret::is_cjk)
                    && !retention.is_some_and(|r| r.erased_words.contains(*word))
            })
            .take(24)
        {
            if seen_texts.insert(word.to_owned()) {
                out.push(QuanpinCandidate {
                    text: word.to_owned(),
                    score: floor + (weight as f64 + 1.0).ln() - max_raw_ln,
                    consumed_len: edge.end,
                    syllables: edge.reading.owned(&readings),
                    segments: vec![word.to_owned()],
                });
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

fn collect_beam(
    steps: &mut Vec<BeamStep>,
    beams: &[Vec<BeamState>],
    word_edges: &[Vec<WordEdge<'_>>],
    width: usize,
) -> Vec<BeamState> {
    let paths = BeamPaths {
        beams,
        edges: word_edges,
    };
    // The original push order is (start, edge, state), including among tied scores.
    let priority = |a: &BeamStep, b: &BeamStep| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| (a.start, a.edge, a.state).cmp(&(b.start, b.edge, b.state)))
    };
    let mut kept = Vec::with_capacity(width.min(steps.len()));
    let mut seen_tails = HashMap::new();
    let mut seen_texts = HashSet::new();

    // Most positions fill their beam from the leading batch. Partition again if
    // duplicate text or the tail cap rejects too many of its paths.
    let mut remaining = steps.as_mut_slice();
    while !remaining.is_empty() && kept.len() < width {
        let batch_size = remaining.len().min(width.saturating_mul(2));
        if batch_size < remaining.len() {
            remaining.select_nth_unstable_by(batch_size, priority);
        }
        let (batch, rest) = remaining.split_at_mut(batch_size);
        remaining = rest;
        batch.sort_unstable_by(priority);
        for &step in batch.iter() {
            let state = paths.previous(step);
            let edge = paths.edge(step);
            let text_hash = edge.text.bytes().fold(state.text_hash, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
            if !seen_texts.insert(BeamText {
                paths,
                step,
                hash: text_hash,
            }) {
                continue;
            }
            let previous = state
                .step
                .map(|step| paths.edge(step).text)
                .filter(|previous| previous.chars().count() + edge.text.chars().count() <= 4)
                .unwrap_or("");
            let count = seen_tails
                .entry(BeamTail {
                    previous,
                    word: edge.text,
                })
                .or_insert(0);
            if *count >= 3 && kept.len() >= width / 2 {
                continue;
            }
            *count += 1;
            kept.push(BeamState {
                step: Some(step),
                text_hash,
                all_cjk: state.all_cjk && edge.text.chars().all(crate::ret::is_cjk),
            });
            if kept.len() == width {
                break;
            }
        }
    }
    steps.clear();
    kept
}

fn beam_candidate<'a>(
    paths: BeamPaths<'a>,
    step: BeamStep,
    consumed_len: usize,
    readings: &[&str],
    reverse_edges: &mut Vec<&'a WordEdge<'a>>,
    text: &mut String,
    seen_texts: &mut HashSet<String>,
) -> Option<QuanpinCandidate> {
    reverse_edges.clear();
    reverse_edges.extend(paths.reverse_edges(Some(step)));
    text.clear();
    for edge in reverse_edges.iter().rev() {
        text.push_str(edge.text);
    }
    if seen_texts.contains(text.as_str()) {
        return None;
    }
    let text = std::mem::take(text);
    seen_texts.insert(text.clone());
    Some(QuanpinCandidate {
        text,
        score: step.score,
        consumed_len,
        syllables: reverse_edges
            .iter()
            .rev()
            .flat_map(|edge| {
                edge.reading
                    .syllables(readings)
                    .iter()
                    .map(|&s| s.to_owned())
            })
            .collect(),
        segments: reverse_edges
            .iter()
            .rev()
            .map(|edge| edge.text.to_owned())
            .collect(),
    })
}

struct Correction<'a> {
    quality: u8,
    word_weight: u64,
    tail_weight: u64,
    code: &'a str,
    dag: SyllableDag,
}

fn correction_dags<'a>(
    plan: &'a CorrectionPlan,
    syllable_split: bool,
    corpus: &RimeDictCorpus,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
) -> Vec<Correction<'a>> {
    let mut spaced = String::new();
    let flypy = plan.flypy;
    let mut edits: Vec<_> = plan
        .edits
        .iter()
        .map(|(quality, code)| {
            let dag = if flypy {
                build_xiaohe_syllable_paths(code)
            } else {
                build_quanpin_syllable_paths(code, syllable_split)
            };
            let (word_weight, tail_weight) = correction_weights(
                &plan.code,
                code,
                &dag,
                flypy,
                corpus,
                imported,
                retention,
                &mut spaced,
            );
            Correction {
                quality: *quality,
                word_weight,
                tail_weight,
                code,
                dag,
            }
        })
        .collect();
    // Give direct word matches priority, then favor changes that form a known suffix.
    edits.sort_by_key(|edit| {
        (
            edit.word_weight == 0,
            if edit.word_weight > 0 {
                edit.quality
            } else if edit.tail_weight > 0 {
                // A supported substitution is a closer fit than adding or dropping a key.
                u8::from(edit.quality >= 3)
            } else {
                2
            },
            std::cmp::Reverse(edit.word_weight.max(edit.tail_weight)),
            edit.quality,
        )
    });
    edits
}

#[allow(clippy::too_many_arguments)]
fn correction_weights(
    raw_input: &str,
    code: &str,
    dag: &SyllableDag,
    flypy: bool,
    corpus: &RimeDictCorpus,
    imported: Option<&ImportedLexiconIndex>,
    retention: Option<&RetentionEngine>,
    spaced: &mut String,
) -> (u64, u64) {
    let mut weight = 0;
    let mut tail_weight = 0;
    if let Some(imported) = imported {
        let entries = if flypy {
            imported.xiaohe_entries(code)
        } else {
            imported.quanpin_entries(code)
        };
        weight = entries
            .iter()
            .map(|entry| u64::from(entry.weight))
            .max()
            .unwrap_or(0);
    }
    let mut stack = vec![(0, [""; 12], 0)];
    for _ in 0..64 {
        let Some((pos, syllables, len)) = stack.pop() else {
            break;
        };
        if pos == dag.len && len >= 2 {
            spaced.clear();
            spaced.push_str(syllables[0]);
            for syllable in &syllables[1..len] {
                spaced.push(' ');
                spaced.push_str(syllable);
            }
            if let Some((_, w)) = corpus.spaced_pinyin_words(spaced).next() {
                weight = weight.max(w);
            }
            if let Some(phrase) = retention.and_then(|r| r.user_phrases_by_reading(spaced).next()) {
                weight = weight.max(1000 + u64::from(phrase.count));
            }
            if !flypy {
                for tail in 2..=3.min(len - 1) {
                    let syllables = &syllables[len - tail..len];
                    let mut letters = raw_input.bytes().rev().filter(u8::is_ascii_alphabetic);
                    if syllables
                        .iter()
                        .rev()
                        .flat_map(|s| s.bytes().rev())
                        .all(|letter| letters.next() == Some(letter))
                    {
                        continue;
                    }
                    let bytes = syllables.iter().map(|s| s.len()).sum::<usize>() + tail - 1;
                    let suffix = &spaced[spaced.len() - bytes..];
                    if let Some((_, w)) = corpus.spaced_pinyin_words(suffix).next() {
                        tail_weight = tail_weight.max(w);
                    }
                }
            }
        } else if pos < dag.len && len < syllables.len() {
            for (next, syllable, penalty) in &dag.edges[pos] {
                if (flypy && *penalty <= 0.5) || *penalty == 0.0 {
                    let mut reading = syllables;
                    reading[len] = syllable;
                    stack.push((*next, reading, len + 1));
                }
            }
        }
    }
    (weight, tail_weight)
}

/// Rank one-edit spellings before bounding decoder work; separators are immutable.
pub(super) fn correction_codes(code: &str, flypy: bool) -> Vec<String> {
    correction_plan(code, flypy)
        .edits
        .iter()
        .map(|(_, code)| code.clone())
        .collect()
}

#[derive(Default)]
struct CorrectionPlan {
    code: String,
    flypy: bool,
    edits: Vec<(u8, String)>,
}

thread_local! {
    // The sentence decoder and exact-word recovery may inspect the same keystroke.
    static LAST_CORRECTION_PLAN: RefCell<Option<Rc<CorrectionPlan>>> = const { RefCell::new(None) };
}

fn correction_plan(code: &str, flypy: bool) -> Rc<CorrectionPlan> {
    if !(4..=48).contains(&code.len())
        || !code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'\'' || b == b' ')
    {
        return Rc::default();
    }
    LAST_CORRECTION_PLAN.with(|last| {
        let mut last = last.borrow_mut();
        if let Some(plan) = last.as_ref().filter(|p| p.flypy == flypy && p.code == code) {
            return Rc::clone(plan);
        }
        let plan = Rc::new(CorrectionPlan {
            code: code.to_owned(),
            flypy,
            edits: ranked_correction_codes(code, flypy),
        });
        *last = Some(Rc::clone(&plan));
        plan
    })
}

fn ranked_correction_codes(code: &str, flypy: bool) -> Vec<(u8, String)> {
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
            legal_quanpin(part)
        })
    };
    // Generate in the original (quality, position, spelling) order. Once the
    // first 24 distinct legal edits are known, later edits cannot be selected.
    const NEIGHBOURS: [&str; 26] = [
        "qswz", "ghnv", "dfvx", "cefrsx", "drsw", "cdgrtv", "bfhtvy", "bgjnuy", "jkou", "hikmnu",
        "ijlmo", "kop", "jkn", "bhjm", "iklp", "lo", "aw", "deft", "adewxz", "fgry", "hijy",
        "bcfg", "aeqs", "cdsz", "ghtu", "asx",
    ];
    const MAX_CORRECTIONS: usize = 24;
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut ranked: Vec<(u8, String)> = Vec::new();
    let mut buffer = [0; 49];
    buffer[..len].copy_from_slice(bytes);
    for quality in 0..=if flypy { 1 } else { 4 } {
        for i in 0..=len {
            let key = bytes.get(i).copied();
            if key.is_some_and(|key| !key.is_ascii_lowercase()) {
                continue;
            }
            let mut add = |bytes: &[u8]| {
                let text = std::str::from_utf8(bytes).expect("ASCII pinyin");
                if ranked.len() < MAX_CORRECTIONS
                    && text != code
                    && legal(text)
                    && !ranked.iter().any(|(_, previous)| previous == text)
                {
                    ranked.push((quality, text.to_owned()));
                }
            };
            match quality {
                0 if i + 1 < len && bytes[i + 1].is_ascii_lowercase() => {
                    buffer.swap(i, i + 1);
                    add(&buffer[..len]);
                    buffer.swap(i, i + 1);
                }
                1 | 2 if i < len => {
                    let key = bytes[i];
                    let replacements = if quality == 1 {
                        NEIGHBOURS[usize::from(key - b'a')].as_bytes()
                    } else {
                        b"aeiouv"
                    };
                    for &letter in replacements {
                        buffer[i] = letter;
                        add(&buffer[..len]);
                    }
                    buffer[i] = key;
                }
                3 if key.is_some() => {
                    buffer[i..len - 1].copy_from_slice(&bytes[i + 1..]);
                    add(&buffer[..len - 1]);
                    buffer[i..len].copy_from_slice(&bytes[i..]);
                }
                4 => {
                    buffer[i + 1..=len].copy_from_slice(&bytes[i..]);
                    for letter in b'a'..=b'z' {
                        buffer[i] = letter;
                        add(&buffer[..=len]);
                    }
                    buffer[i..len].copy_from_slice(&bytes[i..]);
                }
                _ => {}
            }
            if ranked.len() == MAX_CORRECTIONS {
                return ranked;
            }
        }
    }
    ranked
}

fn legal_quanpin(text: &str) -> bool {
    let mut normalized = [0; 49];
    let mut code = text;
    if text.as_bytes().contains(&b'v') {
        let bytes = text.as_bytes();
        normalized[..bytes.len()].copy_from_slice(bytes);
        for index in 1..bytes.len() {
            if bytes[index] == b'v' && b"jqxy".contains(&bytes[index - 1]) {
                normalized[index] = b'u';
            }
        }
        code = std::str::from_utf8(&normalized[..bytes.len()]).expect("ASCII pinyin");
    }

    let canon = canonical_syllable_set();
    let mut reachable = 1_u64;
    for start in 0..code.len() {
        if reachable & (1 << start) == 0 {
            continue;
        }
        for end in start + 1..=(start + 6).min(code.len()) {
            let syllable = match &code[start..end] {
                "nue" => "nve",
                "lue" => "lve",
                "m" | "n" if code.len() > 1 => continue,
                syllable => syllable,
            };
            if canon.contains(syllable) {
                reachable |= 1 << end;
            }
        }
    }
    reachable & (1 << code.len()) != 0
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    fn aliases_initials_and_corrections_respect_explicit_boundaries() {
        let dag = build_quanpin_syllable_paths("nue'dai", false);
        assert!(dag.edges[0]
            .iter()
            .any(|(end, s, p)| *end == 3 && s == "nve" && *p == 0.0));
        assert!(!build_quanpin_syllable_paths("nu'e", false).edges[0]
            .iter()
            .any(|(_, s, _)| s == "nve"));
        let dag = build_quanpin_syllable_paths("mna", false);
        assert!(dag.edges[0].iter().any(|(end, s, _)| *end == 1 && s == "m"));
        assert!(dag.edges[1].iter().any(|(end, s, _)| *end == 2 && s == "n"));
        assert!(!build_quanpin_syllable_paths("z'hong", false).edges[0]
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
            (3, Cow::Borrowed("ba"), 4.0),
            (3, Cow::Borrowed("bin"), 4.0),
            (3, Cow::Borrowed("bao"), 4.0),
            (3, Cow::Borrowed("bai"), 4.0),
            (3, Cow::Borrowed("ben"), 4.0),
            (3, Cow::Borrowed("beng"), 4.0),
        ];
        assert_eq!(build_xiaohe_syllable_paths("nib").edges[2], expected);
        assert_eq!(
            build_xiaohe_syllable_paths("b").edges[0],
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
                let dag = build_quanpin_syllable_paths(code, false);
                let candidates = decode_from_syllable_dag(
                    &dag,
                    true,
                    false,
                    &corpus,
                    None,
                    None,
                    Some(&retention),
                    DecodeTarget::WithPrefixes(5),
                    "",
                    &mut Vec::new(),
                );
                assert_eq!(
                    candidates.iter().any(|c| c.text == word),
                    present,
                    "{reading}: {code}"
                );
            }
        }
        retention.erased_words.insert(word.into());
        assert!(retention
            .user_phrases_by_reading("fang an")
            .next()
            .is_none());
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

    #[test]
    fn long_exact_dictionary_words_survive_zero_split_pruning() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let corpus = RimeDictCorpus::load_or_fallback(Some(&assets));
        for (code, word) in [
            ("ruanjiananzhuangpeizhi", "软件安装配置"),
            ("shuanghuanglianjiaonang", "双黄连胶囊"),
        ] {
            let candidates = decode_quanpin_long_context(code, &corpus, None, None, None, 10);
            assert!(
                candidates.iter().any(|c| c.text == word && c.segments.len() == 1),
                "{code} should retain whole-word dictionary entry {word}: {candidates:?}"
            );
        }
    }
}
