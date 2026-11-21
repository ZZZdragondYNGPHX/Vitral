//! Score complete native hypotheses against the same weighted words used by the
//! portable decoder. Partial Rime selections must keep their native page index.
use crate::imp::polyphone::{PolyphoneAnnotator, RimeDictCorpus};
use std::sync::Arc;

/// Viterbi unigram likelihood over alternative word boundaries.
fn word_score(chars: &[char], corpus: &RimeDictCorpus) -> f64 {
    let mut best = vec![f64::NEG_INFINITY; chars.len() + 1];
    best[0] = 0.0;
    for i in 0..chars.len() {
        best[i + 1] = best[i + 1].max(best[i] - 21.0);
        for j in i + 1..=(i + corpus.max_word_chars).min(chars.len()) {
            if let Some(weight) = corpus.lookup_word_chars(&chars[i..j]) {
                let log_prob = (weight as f64 + 1.0).ln() - 21.0;
                // Cardinal expressions form a productive class before a measure word:
                // for example 十二公里 shares its grammar with 三十米.
                let measure = j < chars.len()
                    && "年月日天周岁时分秒个本张组项人名次件米公里斤吨元角层页台部场届倍成度"
                        .contains(chars[j]);
                let cardinal = j - i >= 2
                    && chars[i..j]
                        .iter()
                        .all(|ch| "零一二三四五六七八九十百千万亿两".contains(*ch));
                let grammar = if measure && cardinal { 6.0 } else { 0.0 };
                best[j] = best[j].max(best[i] + log_prob + grammar + 0.45 * (j - i - 1) as f64);
            }
        }
    }
    best[chars.len()]
}

/// Compare collocations that cross the point at which two complete readings diverge.
/// The grammar encodes a length-dependent offset, as in the portable sentence decoder.
pub(super) fn context_favors_native(
    native: &str,
    picked: &str,
    gram: Option<&super::gram::RimeGramDb>,
) -> bool {
    let Some((gram, (root, unit))) = gram.and_then(|db| db.root_state().map(|root| (db, root)))
    else {
        return false;
    };
    let native: Vec<_> = native.chars().collect();
    let picked: Vec<_> = picked.chars().collect();
    let common = native
        .iter()
        .zip(&picked)
        .take_while(|(a, b)| a == b)
        .count();
    let strength = |chars: &[char]| {
        let mut best = f64::NEG_INFINITY;
        for start in common.saturating_sub(4)..common {
            for end in (common + 1).max(start + 3)..=(start + 5).min(chars.len()) {
                if let Some(raw) = gram.lookup_from_state_chars(root, unit, &chars[start..end]) {
                    let baseline = 11.2 + 2.8 * (end - start - 2) as f64;
                    best = best.max(f64::from(raw) - baseline);
                }
            }
        }
        best
    };
    strength(&native) > strength(&picked).max(0.0)
}

pub(super) fn evaluate(
    text: &str,
    input: &str,
    corpus: &Arc<RimeDictCorpus>,
    expand: bool,
) -> Option<(f64, String, Option<String>)> {
    if text.is_ascii() || text.chars().count() < 2 {
        return None;
    }
    let pronunciation = PolyphoneAnnotator::new(Arc::clone(corpus)).annotate_text(text);
    let syllables: Vec<_> = pronunciation.untoned.split_whitespace().collect();
    if syllables.concat() != input.replace('\'', "") {
        return None;
    }
    let chars: Vec<_> = text.chars().collect();
    let score = word_score(&chars, corpus);
    if !expand || syllables.len() != chars.len() {
        return Some((score, pronunciation.untoned, None));
    }
    // Whole-word homophones can be lost when the native sentence beam commits
    // to a sequence of smaller words. Recover a spanning dictionary word only
    // when it improves the likelihood of the entire sentence.
    let mut best = score + 1.5;
    let mut alternative = None;
    for start in 0..chars.len() {
        for end in start + 3..=(start + corpus.max_word_chars).min(chars.len()) {
            let key = syllables[start..end].join(" ");
            let Some(words) = corpus.spaced_pinyin_to_words.get(&key) else {
                continue;
            };
            let Some((word, _)) = words.first() else {
                continue;
            };
            if chars[start..end].iter().copied().eq(word.chars()) {
                continue;
            }
            let repaired: Vec<_> = chars[..start]
                .iter()
                .copied()
                .chain(word.chars())
                .chain(chars[end..].iter().copied())
                .collect();
            let value = word_score(&repaired, corpus);
            if value > best {
                best = value;
                alternative = Some(repaired.into_iter().collect());
            }
        }
    }
    Some((score, pronunciation.untoned, alternative))
}
