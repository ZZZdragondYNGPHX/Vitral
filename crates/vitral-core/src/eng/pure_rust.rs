use super::english::{lookup_core_english, EnglishDictionary};
use crate::eng::gram::RimeGramDb;
#[cfg(test)]
use crate::eng::quanpin::{decode_quanpin_with_fuzzy, decode_xiaohe_long_context};
#[cfg(test)]
use crate::eng::quanpin::decode_quanpin_context;
use crate::eng::quanpin::{decode_quanpin_with_touch, decode_xiaohe_with_touch, FuzzyPairs, QuanpinCandidate};
use crate::eng::spatial::TouchKeyHint;
use crate::eng::{CandidateItem, OpenCcMode, PageInfo, SchemaKind, TypoCorrectionLevel};
use crate::imp::polyphone::RimeDictCorpus;
use crate::imp::ImportedLexiconIndex;
use crate::ret::RetentionEngine;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use vitral_tokens::TOKENS;

#[derive(Debug)]
pub(crate) struct DecodedCandidate {
    pub item: CandidateItem,
    pub canonical: String,
    pub consumed_len: usize,
    pub lm_score: f64,
    pub spatial_edits: u8,
    pub spatial_guarded: bool,
}

impl DecodedCandidate {
    pub(super) fn language(
        word: &str,
        text: String,
        comment: &str,
        consumed_len: usize,
        retention: Option<&RetentionEngine>,
    ) -> Option<Self> {
        if retention.is_some_and(|r| r.erased_words.contains(word) || r.erased_words.contains(&text)) {
            return None;
        }
        let pinned = retention.is_some_and(|r| {
            r.pinned_words.contains(word) || r.pinned_words.contains(&text)
        });
        Some(Self {
            item: CandidateItem {
                index: 0,
                text,
                comment: comment.into(),
                pinned,
                is_clipboard_shortcut: false,
                quick_paste_id: None,
                function_action: None,
                shortcut_code: None,
                frecency_score: 0.0,
            },
            canonical: word.into(),
            consumed_len,
            lm_score: 0.0,
            spatial_edits: 0,
            spatial_guarded: false,
        })
    }
}

pub(super) const PERSONAL_PRIORITY_THRESHOLD: f64 = 0.5;

pub(super) fn personal_boost(frecency_score: f64) -> f64 {
    // Express personal history in decoder score units, with diminishing gains
    // and a finite limit even for long-running histories.
    let log_score = frecency_score.max(0.0).ln_1p();
    24.0 * (1.0 - 1.5 / (1.5 + log_score * log_score))
}

#[derive(Debug)]
pub struct PureRustEngine {
    assets_dir: Option<PathBuf>,
    grammar_path: Option<PathBuf>,
    corpus_cache_dir: Option<PathBuf>,
    corpus: OnceLock<Arc<RimeDictCorpus>>,
    gram: OnceLock<Option<Arc<RimeGramDb>>>,
    english: OnceLock<Arc<EnglishDictionary>>,
    #[cfg(all(test, target_os = "linux", feature = "native-rime"))]
    pub(super) decode_calls: std::cell::Cell<usize>,
    pub(super) confirmed_prefix: String,
    pub(super) confirming_initials: bool,
    pub fuzzy_pinyin: bool,
    pub(crate) fuzzy_pairs: FuzzyPairs,
    pub(crate) typo_correction_level: TypoCorrectionLevel,
    pub syllable_split: bool,
    pub page_index: usize,
    pub last_candidates: Vec<CandidateItem>,
}

impl PureRustEngine {
    pub fn new(assets_dir: Option<&Path>) -> Self {
        Self::with_corpus_cache(assets_dir, None)
    }

    pub(crate) fn with_corpus_cache(assets_dir: Option<&Path>, cache_dir: Option<&Path>) -> Self {
        Self {
            assets_dir: assets_dir.map(PathBuf::from),
            corpus_cache_dir: cache_dir.map(PathBuf::from),
            grammar_path: crate::models::ModelPaths::bundled(assets_dir).grammar,
            corpus: OnceLock::new(),
            gram: OnceLock::new(),
            english: OnceLock::new(),
            #[cfg(all(test, target_os = "linux", feature = "native-rime"))]
            decode_calls: std::cell::Cell::new(0),
            confirmed_prefix: String::new(),
            confirming_initials: false,
            fuzzy_pinyin: false,
            fuzzy_pairs: FuzzyPairs::ALL,
            typo_correction_level: TypoCorrectionLevel::default(),
            syllable_split: false,
            page_index: 0,
            last_candidates: Vec::new(),
        }
    }

    pub fn reload_assets(&mut self, assets_dir: Option<&Path>) {
        self.assets_dir = assets_dir.map(PathBuf::from);
        self.grammar_path = crate::models::ModelPaths::bundled(assets_dir).grammar;
        self.corpus = OnceLock::new();
        self.gram = OnceLock::new();
        self.english = OnceLock::new();
        EnglishDictionary::invalidate(assets_dir);
    }

    pub fn set_model_paths(&mut self, paths: &crate::models::ModelPaths) {
        self.grammar_path = paths.grammar.clone();
        self.gram = OnceLock::new();
    }

    pub(crate) fn get_corpus(&self) -> &Arc<RimeDictCorpus> {
        self.corpus.get_or_init(|| {
            RimeDictCorpus::load_or_fallback_with_cache(
                self.assets_dir.as_deref(),
                self.corpus_cache_dir.as_deref(),
            )
        })
    }

    pub(super) fn english_dictionary(&self) -> &EnglishDictionary {
        self.english
            .get_or_init(|| EnglishDictionary::open_cached(self.assets_dir.as_deref()))
    }

    pub(crate) fn get_gram(&self) -> Option<&RimeGramDb> {
        self.gram
            .get_or_init(|| {
                self.grammar_path.clone()
                    .filter(|p| p.is_file())
                    .and_then(|p| RimeGramDb::open_cached(&p).ok())
            })
            .as_deref()
    }

    pub(crate) fn cached_gram(&self) -> Option<&RimeGramDb> {
        self.gram.get().and_then(|gram| gram.as_deref())
    }

    #[cfg(test)]
    pub(crate) fn lexicon_init_state(&self) -> (bool, bool) {
        (self.corpus.get().is_some(), self.gram.get().is_some())
    }

    pub fn query_candidates(
        &mut self,
        raw_input: &str,
        schema: SchemaKind,
        opencc_mode: OpenCcMode,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
    ) -> (Vec<CandidateItem>, PageInfo) {
        let page_size = TOKENS.ergonomics.desktop_candidate_page_size as usize;
        if raw_input.is_empty() {
            self.page_index = 0;
            self.last_candidates.clear();
            return (
                Vec::new(),
                PageInfo {
                    page_index: 0,
                    page_size,
                    has_prev: false,
                    has_next: false,
                },
            );
        }

        let pairs = self.query_all_candidates_with_canonical(
            raw_input,
            schema,
            opencc_mode,
            imported,
            retention,
        );
        let items: Vec<CandidateItem> = pairs.into_iter().map(|(c, _)| c).collect();

        let total_pages = items.len().div_ceil(page_size).max(1);
        if self.page_index >= total_pages {
            self.page_index = 0;
        }
        let start = self.page_index * page_size;
        let end = (start + page_size).min(items.len());
        let page_items: Vec<CandidateItem> = items[start..end]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut cloned = c.clone();
                cloned.index = i;
                cloned
            })
            .collect();

        let page_info = PageInfo {
            page_index: self.page_index,
            page_size,
            has_prev: self.page_index > 0,
            has_next: end < items.len(),
        };

        self.last_candidates = items;
        (page_items, page_info)
    }

    pub(crate) fn query_all_candidates_with_canonical(
        &self,
        raw_input: &str,
        schema: SchemaKind,
        opencc_mode: OpenCcMode,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
    ) -> Vec<(CandidateItem, String)> {
        self.query_all_candidates_contextual(
            raw_input,
            schema,
            opencc_mode,
            imported,
            retention,
            None,
            None,
            false,
        )
        .into_iter()
        .map(|c| (c.item, c.canonical))
        .collect()
    }

    /// Exact dictionary words for typo recovery, without the sentence beam or grammar LM.
    /// Bound both syllable paths and homophones so a malformed code stays inexpensive.
    pub(super) fn query_correction_candidates(
        &self,
        code: &str,
        schema: SchemaKind,
        imported: &ImportedLexiconIndex,
        retention: &RetentionEngine,
    ) -> Vec<(CandidateItem, String, u64)> {
        if !(2..=49).contains(&code.len()) {
            return Vec::new();
        }
        let now_ms = RetentionEngine::now_epoch_ms();
        let mut words = HashMap::new();
        super::quanpin::visit_exact_words(
            code, schema == SchemaKind::DoublePinyinFlypy, self.get_corpus(),
            Some(imported), Some(retention), |word, reading, weight, count| {
                let weight = if count > 0 { weight.max(1000 + u64::from(count)) } else { weight };
                let entry = words.entry(word.to_owned()).or_insert_with(|| (reading.to_owned(), weight));
                if weight > entry.1 {
                    *entry = (reading.to_owned(), weight);
                }
            },
        );
        let mut ranked: Vec<_> = words.into_iter().map(|(word, (reading, weight))| {
            let pinned = retention.pinned_words.contains(&word);
            let frecency_score = retention.decayed_score_at(&word, now_ms);
            (
                CandidateItem {
                    index: 0,
                    text: word.clone(),
                    comment: reading,
                    pinned,
                    is_clipboard_shortcut: false,
                    quick_paste_id: None,
                    function_action: None,
                    shortcut_code: None,
                    frecency_score,
                },
                word,
                weight,
            )
        }).collect();
        ranked.sort_by(|(a, a_word, a_weight), (b, b_word, b_weight)| {
            let score = |item: &CandidateItem, weight: u64| {
                item.frecency_score + (weight as f64).ln_1p()
            };
            b.pinned.cmp(&a.pinned)
                .then_with(|| score(b, *b_weight).total_cmp(&score(a, *a_weight)))
                .then_with(|| a_word.cmp(b_word))
        });
        ranked.truncate(3);
        ranked
    }

    /// At most three dictionary buckets (24/10/10 entries) and bounded personal buckets.
    pub(super) fn contextual_prefix_candidate(
        &self,
        raw_input: &str,
        schema: SchemaKind,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
        prev_context: &str,
        adaptive: bool,
    ) -> Option<(String, Vec<String>)> {
        if !(1..=6).contains(&raw_input.len())
            || !raw_input.bytes().all(|b| b.is_ascii_alphabetic())
        {
            return None;
        }
        let corpus = self.get_corpus();
        let raw = raw_input.to_ascii_lowercase();
        let flypy = schema == SchemaKind::DoublePinyinFlypy;
        let mut best: Option<(bool, usize, f64, String, Vec<String>)> = None;
        // The fourth suffix can only have a personal bucket: dictionary prefixes stop at three.
        for (prefix_len, (offset, _)) in prev_context.char_indices().rev().take(4).enumerate() {
            let prefix = &prev_context[offset..];
            if !prefix.chars().all(crate::ret::is_cjk) {
                break;
            }
            let dictionary = corpus
                .continuations(prefix)
                .map(|(word, weight)| (word, false, weight as f64));
            let personal = retention
                .filter(|_| adaptive)
                .and_then(|r| r.context_bigrams.get(prefix))
                .into_iter()
                .flatten()
                .filter(|(_, weight)| **weight > 0.25)
                .map(|(word, weight)| (word.as_str(), true, *weight));
            for (word, personal, weight) in dictionary.chain(personal) {
                let word_len = word.chars().count();
                if !(1..=6).contains(&word_len)
                    || !word.chars().all(crate::ret::is_cjk)
                    || retention.is_some_and(|r| r.erased_words.contains(word))
                {
                    continue;
                }
                let syllables = corpus
                    .word_reading(word)
                    .map(|(s, _)| s.map(str::to_owned).collect())
                    .or_else(|| {
                        (word_len == 1)
                            .then(|| {
                                corpus
                                    .char_readings(word.chars().next()?)
                                    .next()
                                    .map(|(syllable, _)| vec![syllable.to_owned()])
                            })
                            .flatten()
                    })
                    // A suffix need not itself be a dictionary word. Its source phrase
                    // still carries the correct, context-sensitive pronunciation.
                    .or_else(|| {
                        corpus
                            .word_reading(&format!("{prefix}{word}"))
                            .map(|(s, _)| s.skip(prefix_len + 1).map(str::to_owned).collect())
                    })
                    .or_else(|| {
                        word.chars()
                            .map(|ch| {
                                corpus
                                    .char_readings(ch)
                                    .next()
                                    .map(|(s, _)| s.to_owned())
                            })
                            .collect::<Option<Vec<_>>>()
                    });
                let Some(syllables) = syllables.filter(|s| {
                    s.len() == word_len
                        && !(self.syllable_split && !flypy
                            && s.iter().any(|s| crate::eng::quanpin::can_zero_initial_split(s)))
                }) else {
                    continue;
                };
                let compact = if flypy {
                    crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&syllables.join(" "))
                } else {
                    syllables.concat()
                };
                let initials: String = syllables.iter().filter_map(|s| s.chars().next()).collect();
                if !(compact.starts_with(&raw) || initials.starts_with(&raw))
                    || imported.is_some_and(|i| !i.matches_reading(word, &compact, flypy))
                {
                    continue;
                }
                let score = weight.ln_1p();
                let better = best
                    .as_ref()
                    .is_none_or(|(p, len, old_score, old_word, _)| {
                        personal
                            .cmp(p)
                            .then_with(|| (prefix_len + 1).cmp(len))
                            .then_with(|| score.total_cmp(old_score))
                            .then_with(|| old_word.as_str().cmp(word))
                            .is_gt()
                    });
                if better {
                    best = Some((personal, prefix_len + 1, score, word.to_owned(), syllables));
                }
            }
        }
        best.map(|(_, _, _, word, syllables)| (word, syllables))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn query_all_candidates_contextual(
        &self,
        raw_input: &str,
        schema: SchemaKind,
        opencc_mode: OpenCcMode,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        adaptive: bool,
    ) -> Vec<DecodedCandidate> {
        self.query_candidates_with_prediction(
            raw_input, schema, opencc_mode, imported, retention, prev_context, app_id, adaptive, None,
        )
        .0
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn query_candidates_with_prediction(
        &self,
        raw_input: &str,
        schema: SchemaKind,
        opencc_mode: OpenCcMode,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        adaptive: bool,
        touch_hints: Option<&[TouchKeyHint]>,
    ) -> (Vec<DecodedCandidate>, Option<(String, f64)>) {
        let decoded = self.decode_candidates(raw_input, schema, imported, retention, touch_hints);
        self.rank_candidates(&decoded, raw_input, schema, opencc_mode, imported, retention,
            prev_context, app_id, adaptive, Some(self.english_dictionary()))
    }

    pub(super) fn decode_candidates(
        &self,
        raw_input: &str,
        schema: SchemaKind,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
        touch_hints: Option<&[TouchKeyHint]>,
    ) -> Vec<QuanpinCandidate> {
        if raw_input.is_empty() {
            return Vec::new();
        }
        #[cfg(all(test, target_os = "linux", feature = "native-rime"))]
        self.decode_calls.set(self.decode_calls.get() + 1);

        let corpus = self.get_corpus();
        let gram = self.get_gram();
        let fuzzy = if self.fuzzy_pinyin { self.fuzzy_pairs } else { FuzzyPairs::NONE };
        let mut decoded = match schema {
            SchemaKind::RimeIce => decode_quanpin_with_touch(
                raw_input,
                corpus,
                gram,
                imported,
                retention,
                40,
                fuzzy,
                self.syllable_split,
                &self.confirmed_prefix,
                self.typo_correction_level,
                touch_hints,
            ),
            SchemaKind::DoublePinyinFlypy => decode_xiaohe_with_touch(
                raw_input,
                corpus,
                gram,
                imported,
                retention,
                40,
                fuzzy,
                &self.confirmed_prefix,
                self.typo_correction_level,
                touch_hints,
            ),
        };

        super::quanpin::append_initial_choices(
            &mut decoded,
            raw_input,
            corpus,
            retention,
            schema == SchemaKind::DoublePinyinFlypy,
            self.confirming_initials,
        );
        // Prefer the longest valid input span before deduplicating typo and prefix paths.
        decoded.sort_by(|a, b| {
            b.consumed_len
                .cmp(&a.consumed_len)
                .then_with(|| b.score.total_cmp(&a.score))
        });
        decoded
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn rank_candidates(
        &self,
        decoded: &[QuanpinCandidate],
        raw_input: &str,
        schema: SchemaKind,
        opencc_mode: OpenCcMode,
        imported: Option<&ImportedLexiconIndex>,
        retention: Option<&RetentionEngine>,
        prev_context: Option<&str>,
        app_id: Option<&str>,
        adaptive: bool,
        dictionary: Option<&EnglishDictionary>,
    ) -> (Vec<DecodedCandidate>, Option<(String, f64)>) {
        if raw_input.is_empty() {
            return (Vec::new(), None);
        }
        let corpus = self.get_corpus();
        let xiaohe_fuzzy = schema == SchemaKind::DoublePinyinFlypy && self.fuzzy_pinyin;
        let matches_reading = |syllables: &[String], code: &str| {
            syllables
                .iter()
                .all(|s| crate::imp::xiaohe::canonical_syllable_set().contains(s.as_str()))
                && super::quanpin::matches_exact_reading(
                    syllables, code, schema == SchemaKind::DoublePinyinFlypy,
                )
        };
        let exact_reading = |syllables: &[String]| matches_reading(syllables, raw_input);
        let mut has_exact_chinese = retention.is_some_and(|r| {
            !r.query_user_phrases(raw_input, schema == SchemaKind::DoublePinyinFlypy)
                .is_empty()
        });
        let mut has_complete_chinese = has_exact_chinese;
        let mut has_abbreviated_chinese = false;
        let short_quanpin = dictionary.is_some() && schema == SchemaKind::RimeIce
            && raw_input.len() <= 4 && raw_input.bytes().all(|b| b.is_ascii_lowercase());
        let leading_weight = retention.map_or(0, |_| {
            decoded.iter()
                .filter(|c| c.consumed_len == raw_input.len() && exact_reading(&c.syllables))
                .filter_map(|c| corpus.word_weight(&c.text))
                .max()
                .unwrap_or(0)
        });
        let now_ms = RetentionEngine::now_epoch_ms();
        let mut pinned_items = Vec::new();
        let mut ranked = Vec::new();
        let mut seen = HashSet::new();
        // Deduplicate complete exact Xiaohe readings before alternate readings.
        let exactly_matches = |cand: &&QuanpinCandidate| {
            cand.consumed_len == raw_input.len() && exact_reading(&cand.syllables)
        };
        let (exact, other): (Vec<_>, Vec<_>) = if xiaohe_fuzzy {
            decoded.iter().partition(exactly_matches)
        } else {
            (Vec::new(), decoded.iter().collect())
        };
        for cand in exact.into_iter().chain(other) {
            let flypy = schema == SchemaKind::DoublePinyinFlypy;
            let reading = if !flypy {
                cand.syllables.concat()
            } else if cand.spatial_edits > 0 {
                crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&cand.syllables.join(" "))
            } else {
                raw_input[..cand.consumed_len].to_string()
            };
            if retention.is_some_and(|r| r.erased_words.contains(&cand.text))
                || imported.is_some_and(|i| {
                    !i.matches_reading(&cand.text, &reading, flypy)
                        && !(xiaohe_fuzzy && i.matches_reading(
                            &cand.text,
                            &crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&cand.syllables.join(" ")),
                            true,
                        ))
                })
            {
                continue;
            }
            let exact = cand.consumed_len == raw_input.len() && exact_reading(&cand.syllables);
            if cand.consumed_len == raw_input.len() && cand.text.chars().all(crate::ret::is_cjk) {
                if exact {
                    has_complete_chinese = true;
                    has_exact_chinese |= cand.segments.len() == 1;
                } else if cand.spatial_edits > 0 {
                    has_complete_chinese = true;
                } else if short_quanpin && !has_abbreviated_chinese && cand.segments.len() == 1 {
                    // A fully covered abbreviation such as ru + g can have a strong
                    // Chinese match even though the input has no exact pinyin reading.
                    let ends_in_initial = cand.syllables.split_last().is_some_and(|(last, prefix)| {
                        !prefix.is_empty() && (1..=2).any(|len| {
                            let (full, initial) = raw_input.split_at(raw_input.len().saturating_sub(len));
                            super::quanpin::is_initial_token(initial)
                                && last.starts_with(initial)
                                && matches_reading(prefix, full)
                        })
                    });
                    has_abbreviated_chinese = ends_in_initial
                        || corpus.abbreviations(raw_input).any(|(word, _)| word == cand.text);
                }
            }
            let converted = pure_rust_opencc(opencc_mode, &cand.text);
            if !seen.insert(converted.clone()) {
                continue;
            }
            let pinned = retention.is_some_and(|r| r.pinned_words.contains(&cand.text));
            let frec = retention
                .map(|r| {
                    let score = r.contextual_score_at(
                        &cand.text,
                        &raw_input[..cand.consumed_len],
                        prev_context,
                        app_id,
                        now_ms,
                        adaptive,
                    );
                    if cand.consumed_len == raw_input.len() {
                        r.protect_dictionary_rank(
                            &cand.text, score, corpus.word_weight(&cand.text).unwrap_or(0), leading_weight,
                        )
                    } else {
                        score
                    }
                })
                .unwrap_or(0.0);
            let single_syllable = cand.syllables.len() == 1;
            let comment = cand.syllables.join(" ");
            let item = CandidateItem {
                index: 0,
                text: converted,
                comment,
                pinned,
                is_clipboard_shortcut: false,
                quick_paste_id: None,
                function_action: None,
                shortcut_code: None,
                frecency_score: frec,
            };
            let decoded = DecodedCandidate {
                item,
                canonical: cand.text.clone(),
                consumed_len: cand.consumed_len,
                lm_score: cand.score - cand.full_span_frecency_bonus,
                spatial_edits: cand.spatial_edits,
                spatial_guarded: cand.spatial_guarded,
            };
            if pinned && decoded.consumed_len == raw_input.len() && (!xiaohe_fuzzy || exact) {
                pinned_items.push(decoded);
            } else {
                let score = decoded.lm_score + personal_boost(decoded.item.frecency_score);
                ranked.push((decoded, score, exact, single_syllable));
            }
        }

        let single_query = match schema {
            SchemaKind::RimeIce => crate::imp::xiaohe::canonical_syllable_set()
                .contains(crate::imp::xiaohe::normalize_syllable(raw_input).as_str()),
            SchemaKind::DoublePinyinFlypy => raw_input.len() == 2,
        };
        ranked.sort_by(|(a, a_score, a_exact, a_single), (b, b_score, b_exact, b_single)| {
            (b.consumed_len == raw_input.len())
                .cmp(&(a.consumed_len == raw_input.len()))
                .then_with(|| (xiaohe_fuzzy && *b_exact).cmp(&(xiaohe_fuzzy && *a_exact)))
                .then_with(|| {
                    (single_query && *b_single && *b_exact)
                        .cmp(&(single_query && *a_single && *a_exact))
                })
                .then_with(|| b_score.total_cmp(a_score))
                .then_with(|| b.consumed_len.cmp(&a.consumed_len))
                .then_with(|| b_exact.cmp(a_exact))
        });
        let mut normal_items: Vec<_> = ranked
            .into_iter()
            .map(|(candidate, _, _, _)| candidate)
            .collect();
        let personal_count = normal_items
            .iter()
            .take_while(|c| {
                c.consumed_len == raw_input.len()
                    && c.item.frecency_score >= PERSONAL_PRIORITY_THRESHOLD
            })
            .count();
        pinned_items.extend(normal_items.drain(..personal_count));
        // Keep typo/completion ranking intact until there is an exact full reading.
        let has_sentence = pinned_items
            .iter()
            .chain(&normal_items)
            .any(|c| {
                c.consumed_len == raw_input.len()
                    && c.item.comment.split_whitespace().count() > 1
                    && (schema == SchemaKind::DoublePinyinFlypy
                        || c.item
                            .comment
                            .chars()
                            .filter(|ch| !ch.is_whitespace())
                            .eq(raw_input.chars().filter(|ch| ch.is_ascii_alphabetic())))
            });
        let has_partial = normal_items
            .iter()
            .any(|c| c.consumed_len < raw_input.len());
        if has_sentence && has_partial {
            let mut full = Vec::new();
            let mut prefixes = Vec::new();
            let mut singles = Vec::new();
            let mut remaining = Vec::new();
            let mut lengths = HashSet::new();
            for candidate in normal_items {
                if candidate.consumed_len == raw_input.len() {
                    full.push(candidate);
                } else if candidate.canonical.chars().count() > 1
                    && (schema == SchemaKind::DoublePinyinFlypy || {
                        let reading: String = candidate.item.comment.split_whitespace().collect();
                        let raw_prefix = raw_input[..candidate.consumed_len].replace(['\'', ' '], "");
                        let last = candidate
                            .item
                            .comment
                            .split_whitespace()
                            .last()
                            .unwrap_or("");
                        reading == raw_prefix
                            && !raw_input[candidate.consumed_len..]
                                .chars()
                                .next()
                                .is_some_and(|ch| {
                                    ch.is_ascii_alphabetic()
                                        && crate::imp::xiaohe::canonical_syllable_set()
                                            .contains(format!("{last}{ch}").as_str())
                                })
                    })
                    && lengths.insert(candidate.consumed_len)
                {
                    prefixes.push(candidate);
                } else if candidate.canonical.chars().count() == 1
                    && candidate.item.comment.split_whitespace().count() == 1
                {
                    let frequency = corpus
                        .spaced_pinyin_words(&candidate.item.comment)
                        .find(|(w, _)| *w == candidate.canonical)
                        .map_or(candidate.lm_score, |(_, weight)| (weight as f64 + 1.0).ln());
                    singles.push((candidate, frequency));
                } else {
                    remaining.push(candidate);
                }
            }
            // Preserve the best sentences, then expose selectable prefixes and homophones.
            singles.sort_by(|a, b| b.1.total_cmp(&a.1));
            let head_count = 2_usize.saturating_sub(pinned_items.len()).min(full.len());
            let rest_full = full.split_off(head_count);
            let rest_singles = singles.split_off(singles.len().min(2));
            full.extend(prefixes);
            full.extend(singles.into_iter().map(|(candidate, _)| candidate));
            full.extend(rest_full);
            full.extend(remaining);
            full.extend(rest_singles.into_iter().map(|(candidate, _)| candidate));
            normal_items = full;
        }
        pinned_items.extend(normal_items);
        let mut insert_language = |at: usize, word: &str, text: String, comment: &str| {
            let Some(candidate) = DecodedCandidate::language(word, text, comment, raw_input.len(), retention) else {
                return;
            };
            if seen.insert(candidate.item.text.clone()) {
                let pinned_end = pinned_items.iter().take_while(|c| c.item.pinned).count();
                let target = if candidate.item.pinned {
                    pinned_end
                } else {
                    pinned_end + at
                };
                pinned_items.insert(target.min(pinned_items.len()), candidate);
            }
        };
        let exact_english = match dictionary {
            Some(dictionary) => dictionary.lookup_english(raw_input, retention),
            None => lookup_core_english(raw_input, retention),
        };
        let exact_word_at = usize::from(has_exact_chinese);
        if let Some(word) = exact_english {
            let at = if dictionary.is_some() && raw_input.len() <= 4
                && (has_complete_chinese || has_abbreviated_chinese)
                && lookup_core_english(raw_input, retention) != Some(word)
            {
                3
            } else {
                exact_word_at
            };
            insert_language(at, word, word.to_owned(), "英文");
        } else if let Some(dictionary) = dictionary.filter(|_| !has_complete_chinese) {
            let at = if has_abbreviated_chinese { 3 } else { 0 };
            for (offset, word) in dictionary.complete_english(raw_input, retention, 2).into_iter().enumerate() {
                insert_language(at + offset, word, word.to_owned(), "英文");
            }
        }
        if let Some(dictionary) = dictionary {
            for (offset, word) in dictionary.lookup_cn_en(raw_input, schema, retention).enumerate() {
                insert_language(exact_word_at + offset, word, pure_rust_opencc(opencc_mode, word), "中英");
            }
        }
        let prediction = prev_context
            .filter(|_| !self.confirming_initials)
            .and_then(|context| {
                self.contextual_prefix_candidate(raw_input, schema, imported, retention, context, adaptive)
            })
            .filter(|(word, _)| {
                !retention.is_some_and(|r| {
                    r.is_provisional_rare_selection(
                        word, corpus.word_weight(word).unwrap_or(0), leading_weight,
                    )
                })
            });
        if let Some((word, syllables)) = &prediction {
            // An explicitly typed sentence wins over an unfinished contextual completion.
            let longer_sentence = !exact_reading(syllables)
                && pinned_items.iter().any(|c| {
                    c.canonical != *word
                        && c.canonical.chars().count() > 1
                        && c.canonical.chars().all(crate::ret::is_cjk)
                        && c.consumed_len == raw_input.len()
                        && exact_reading(
                            &c.item
                                .comment
                                .split_whitespace()
                                .map(str::to_string)
                                .collect::<Vec<_>>(),
                        )
                });
            if !longer_sentence {
                let frecency_score = retention.map_or(0.0, |r| {
                    r.contextual_score_at(word, raw_input, prev_context, app_id, now_ms, adaptive)
                });
                let text = pure_rust_opencc(opencc_mode, word);
                let pinned = retention.is_some_and(|r| r.pinned_words.contains(word));
                let (lm_score, spatial_edits, spatial_guarded) = pinned_items
                    .iter()
                    .position(|c| c.item.text == text)
                    .map_or((0.0, 0, false), |index| {
                        let candidate = pinned_items.remove(index);
                        (candidate.lm_score, candidate.spatial_edits, candidate.spatial_guarded)
                    });
                let at = pinned_items
                    .iter()
                    .rposition(|c| {
                        c.item.pinned
                            || (!pinned
                                && c.item.frecency_score >= PERSONAL_PRIORITY_THRESHOLD
                                && c.item.frecency_score > frecency_score.max(0.0))
                    })
                    .map_or(0, |index| index + 1);
                pinned_items.insert(
                    at,
                    DecodedCandidate {
                        item: CandidateItem {
                            index: at,
                            text,
                            comment: syllables.join(" "),
                            pinned,
                            is_clipboard_shortcut: false,
                            quick_paste_id: None,
                            function_action: None,
                            shortcut_code: None,
                            frecency_score,
                        },
                        canonical: word.clone(),
                        consumed_len: raw_input.len(),
                        lm_score,
                        spatial_edits,
                        spatial_guarded,
                    },
                );
            }
        }

        if pinned_items.is_empty() {
            pinned_items.push(DecodedCandidate {
                item: CandidateItem {
                    index: 0,
                    text: raw_input.to_string(),
                    comment: raw_input.to_string(),
                    pinned: false,
                    is_clipboard_shortcut: false,
                    quick_paste_id: None,
                    function_action: None,
                    shortcut_code: None,
                    frecency_score: 0.0,
                },
                canonical: raw_input.to_string(),
                consumed_len: raw_input.len(),
                lm_score: 0.0,
                spatial_edits: 0,
                spatial_guarded: false,
            });
        }
        for (i, candidate) in pinned_items.iter_mut().enumerate() {
            candidate.item.index = i;
        }
        let prediction = prediction.and_then(|(word, _)| {
            pinned_items
                .first()
                .filter(|candidate| candidate.canonical == word)
                .map(|candidate| (word, candidate.item.frecency_score))
        });
        (pinned_items, prediction)
    }
}

struct OpenCcTable {
    entries: HashMap<&'static str, [&'static str; 3]>,
    max_prefix_bytes: HashMap<char, usize>,
}

impl OpenCcTable {
    fn load() -> Self {
        let mut table = Self {
            entries: HashMap::new(),
            max_prefix_bytes: HashMap::new(),
        };
        for line in include_str!("opencc_delta.tsv")
            .lines()
            .filter(|line| !line.starts_with('#'))
        {
            let mut fields = line.split('\t');
            let input = fields.next().unwrap();
            let standard = fields.next().unwrap_or_default();
            let regional = |value| if value == "=" { standard } else { value };
            let taiwan = regional(fields.next().unwrap_or_default());
            let hong_kong = regional(fields.next().unwrap_or_default());
            table.entries.insert(input, [standard, taiwan, hong_kong]);
            table
                .max_prefix_bytes
                .entry(input.chars().next().unwrap())
                .and_modify(|max| *max = (*max).max(input.len()))
                .or_insert(input.len());
        }
        table
    }
}

/// Use the longest matching OpenCC phrase or character for the selected locale.
pub fn pure_rust_opencc(mode: OpenCcMode, mut text: &str) -> String {
    let mode = match mode {
        OpenCcMode::Simplified => return text.to_owned(),
        OpenCcMode::Traditional => 0,
        OpenCcMode::TaiwanStandard => 1,
        OpenCcMode::HongKong => 2,
    };
    if text.is_empty() {
        return String::new();
    }

    static TABLE: OnceLock<OpenCcTable> = OnceLock::new();
    let table = TABLE.get_or_init(OpenCcTable::load);
    let mut output = String::with_capacity(text.len());
    while let Some(first) = text.chars().next() {
        let mut matched = (first.len_utf8(), &text[..first.len_utf8()]);
        if let Some(&max_bytes) = table.max_prefix_bytes.get(&first) {
            for (offset, ch) in text.char_indices() {
                let end = offset + ch.len_utf8();
                if end > max_bytes {
                    break;
                }
                if let Some(converted) = table
                    .entries
                    .get(&text[..end])
                    .map(|values| values[mode])
                    .filter(|value| !value.is_empty())
                {
                    matched = (end, converted);
                }
            }
        }
        output.push_str(matched.1);
        text = &text[matched.0..];
    }
    output
}

#[cfg(test)]
mod device_tests {
    use super::*;

    #[test]
    fn bundled_mixed_language_candidates_follow_schema_and_opencc() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let mut engine = PureRustEngine::new(None);
        let query = |engine: &PureRustEngine, code, schema, retention| {
            engine.query_all_candidates_contextual(
                code, schema, OpenCcMode::Traditional, None, retention, None, None, true,
            )
        };
        assert!(!query(&engine, "xguang", SchemaKind::RimeIce, None)
            .iter().any(|c| c.item.comment == "中英"));
        engine.reload_assets(Some(&assets));
        for (schema, code, word) in [
            (SchemaKind::RimeIce, "xguang", "X光"),
            (SchemaKind::DoublePinyinFlypy, "xgl", "X光"),
            (SchemaKind::RimeIce, "txu", "T恤"),
            (SchemaKind::DoublePinyinFlypy, "txu", "T恤"),
            (SchemaKind::RimeIce, "adgainai", "AD鈣奶"),
            (SchemaKind::DoublePinyinFlypy, "adgdnd", "AD鈣奶"),
        ] {
            let candidates = query(&engine, code, schema, None);
            assert_eq!(candidates[0].item.text, word, "{code}");
            assert_eq!(candidates[0].item.comment, "中英", "{code}");
        }
        let candidates = query(&engine, "agu", SchemaKind::RimeIce, None);
        assert!(candidates[0].item.text.chars().all(crate::ret::is_cjk));
        assert_eq!(candidates[1].item.text, "A股");
        assert_eq!(candidates[1].item.comment, "中英");
        let candidates = query(&engine, "shang", SchemaKind::RimeIce, None);
        assert!(!candidates.iter().any(|c| c.item.comment == "英文"));
        let mut retention = RetentionEngine::default();
        retention.erased_words.insert("AD钙奶".to_owned());
        assert!(!query(&engine, "adgainai", SchemaKind::RimeIce, Some(&retention))
            .iter().any(|c| c.item.text == "AD鈣奶"));
        engine.reload_assets(None);
        assert!(!query(&engine, "xguang", SchemaKind::RimeIce, None)
            .iter().any(|c| c.item.comment == "中英"));
    }

    #[test]
    fn xiaohe_fuzzy_candidates_follow_exact_matches_even_with_personal_weight() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let mut engine = PureRustEngine::new(Some(&assets));
        engine.typo_correction_level = TypoCorrectionLevel::Off;
        engine.fuzzy_pairs = FuzzyPairs::from_names(&["sh_s".into()]).unwrap();
        let mut retention = RetentionEngine::default();
        retention.record_selection_at("是", RetentionEngine::now_epoch_ms(), 50.0);
        let query = |engine: &PureRustEngine| engine.query_all_candidates_contextual(
            "si", SchemaKind::DoublePinyinFlypy, OpenCcMode::Simplified,
            None, Some(&retention), None, None, true,
        );
        let exact = query(&engine);
        assert!(!exact.iter().any(|c| c.canonical == "是"));
        engine.fuzzy_pinyin = true;
        let candidates = query(&engine);
        assert!(candidates.iter().any(|c| c.canonical == "是"), "{candidates:?}");
        assert_eq!(candidates[0].canonical, exact[0].canonical);
        assert_eq!(candidates[0].item.comment, "si");
        assert!(!candidates.iter().any(|c| c.item.comment == "chi"));
    }

    #[test]
    fn xiaohe_fuzzy_keeps_imported_candidate_with_expanded_reading() {
        let mut engine = PureRustEngine::new(None);
        engine.typo_correction_level = TypoCorrectionLevel::Off;
        engine.fuzzy_pairs = FuzzyPairs::from_names(&["sh_s".into()]).unwrap();
        let mut imported = ImportedLexiconIndex::new(None);
        imported.inject_hotwords(&[crate::imp::ImportedLexiconEntry {
            text: "是".into(),
            pinyin: "shi".into(),
            toned_pinyin: "shì".into(),
            xiaohe: "ui".into(),
            weight: 500_000,
        }]);
        let query = |engine: &PureRustEngine| engine.query_all_candidates_contextual(
            "si", SchemaKind::DoublePinyinFlypy, OpenCcMode::Simplified,
            Some(&imported), None, None, None, false,
        );
        assert!(!query(&engine).iter().any(|c| c.canonical == "是"));
        engine.fuzzy_pinyin = true;
        let candidates = query(&engine);
        assert!(candidates.iter().any(|c| c.canonical == "是" && c.item.comment == "shi"), "{candidates:?}");
    }

    #[test]
    fn opencc_disambiguates_phrases_and_regional_variants() {
        let text = "头发发财，理发皇后后来；干活干净茶几几乎出丑。方便面/软件/内存/鼠标/信息，兑换说话卫生，𫔶Vitral😀";
        for (mode, expected) in [
            (OpenCcMode::Simplified, text),
            (
                OpenCcMode::Traditional,
                "頭髮發財，理髮皇后後來；幹活乾淨茶几幾乎出醜。方便麪/軟件/內存/鼠標/信息，兌換說話衛生，闑Vitral😀",
            ),
            (
                OpenCcMode::TaiwanStandard,
                "頭髮發財，理髮皇后後來；幹活乾淨茶几幾乎出醜。泡麵/軟體/記憶體/滑鼠/資訊，兌換說話衛生，闑Vitral😀",
            ),
            (
                OpenCcMode::HongKong,
                "頭髮發財，理髮皇后後來；幹活乾淨茶几幾乎出醜。方便麪/軟件/內存/鼠標/信息，兑換説話衞生，闑Vitral😀",
            ),
        ] {
            assert_eq!(pure_rust_opencc(mode, text), expected, "{mode:?}");
            assert_eq!(pure_rust_opencc(mode, ""), "");
        }
    }

    #[test]
    fn opencc_preserves_native_phrase_boundaries_before_suffixes() {
        for (mode, expected) in [
            (OpenCcMode::Traditional, "發明了，啓發式；明瞭、髮式／會發明瞭"),
            (OpenCcMode::TaiwanStandard, "發明了，啟發式；明瞭、髮式／會發明瞭"),
            (OpenCcMode::HongKong, "發明了，啓發式；明瞭、髮式／會發明瞭"),
        ] {
            assert_eq!(
                pure_rust_opencc(mode, "发明了，启发式；明了、发式／会发明了"),
                expected,
                "{mode:?}",
            );
        }
    }

    #[cfg(all(target_os = "linux", feature = "native-rime"))]
    #[test]
    fn opencc_phrases_match_the_locked_native_configs() {
        let opencc_dir = crate::imp::polyphone::default_assets_dir_candidates()
            .unwrap()
            .join("opencc");
        for mode in [
            OpenCcMode::Traditional,
            OpenCcMode::TaiwanStandard,
            OpenCcMode::HongKong,
        ] {
            for input in [
                "头发", "发财", "理发", "皇后", "后来", "干活", "干净", "茶几", "几乎", "出丑",
                "方便面", "软件", "内存", "鼠标", "信息", "兑换", "里面", "卫生", "写字台", "𫔶",
                "发明了", "启发式", "会发明了", "发明了一个启发式方法",
                "皇后来理发，顺便买了方便面。",
                "我把软件装到电脑上，查看内存和鼠标信息。",
            ] {
                let native = crate::eng::opencc_ffi::convert_text(&opencc_dir, mode, input).unwrap();
                assert_eq!(pure_rust_opencc(mode, input), native, "{mode:?}: {input}");
            }
        }
    }

    #[test]
    fn sentence_polish_real_corpus_regressions() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let mut engine = PureRustEngine::new(Some(&assets));
        for (code, expected) in [
            ("wmquchifan", "我们去吃饭"),
            ("jttianqi", "今天天气"),
            ("zenmban", "怎么办"),
            ("nuedai", "虐待"),
            ("luewei", "略微"),
            ("zhonggou", "中国"),
            ("zhongguo", "中国"),
        ] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                None,
                None,
                None,
                false,
            );
            assert!(
                candidates
                    .iter()
                    .take(5)
                    .any(|c| c.canonical == expected && c.consumed_len == code.len()),
                "{code}: {candidates:?}"
            );
        }
        let plain = decode_quanpin_context(
            "quchifan",
            engine.get_corpus(),
            engine.get_gram(),
            None,
            None,
            40,
            FuzzyPairs::NONE,
            false,
            "",
            TypoCorrectionLevel::Standard,
        );
        let prefixed = decode_quanpin_context(
            "quchifan",
            engine.get_corpus(),
            engine.get_gram(),
            None,
            None,
            40,
            FuzzyPairs::NONE,
            false,
            "我要",
            TypoCorrectionLevel::Standard,
        );
        let plain = plain.iter().find(|c| c.text == "去吃饭").unwrap();
        let prefixed = prefixed.iter().find(|c| c.text == "去吃饭").unwrap();
        assert_ne!(plain.score, prefixed.score);
        assert_eq!(prefixed.consumed_len, "quchifan".len());
        assert_eq!(prefixed.syllables, ["qu", "chi", "fan"]);
        engine.fuzzy_pinyin = true;
        let mut preference = RetentionEngine::default();
        let now = RetentionEngine::now_epoch_ms();
        for _ in 0..8 {
            preference.record_contextual_selection(
                "你是李老师",
                "nishilaoshi",
                None,
                None,
                now,
                1.0,
                true,
            );
        }
        // A learned sentence must cross exact scattered characters by total score.
        let candidates = engine.query_all_candidates_contextual(
            "nishinilaoshi",
            SchemaKind::RimeIce,
            OpenCcMode::Simplified,
            None,
            Some(&preference),
            None,
            None,
            true,
        );
        assert_eq!(candidates[0].canonical, "你是李老师", "{candidates:?}");
        engine.fuzzy_pinyin = false;
        let mut retention = RetentionEngine::default();
        retention.register_typed_phrase("云璃", "yun li", "yyli", RetentionEngine::now_epoch_ms());
        for code in ["yunlishurufa", "yun'li'shurufa"] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                Some(&retention),
                None,
                None,
                true,
            );
            assert!(
                candidates
                    .iter()
                    .take(5)
                    .any(|c| c.canonical == "云璃输入法" && c.consumed_len == code.len()),
                "{code}: {candidates:?}"
            );
        }
        retention.erase_word("云璃");
        let erased = decode_quanpin_context(
            "yunlishurufa",
            engine.get_corpus(),
            engine.get_gram(),
            None,
            Some(&retention),
            40,
            FuzzyPairs::NONE,
            false,
            "",
            TypoCorrectionLevel::Standard,
        );
        assert!(!erased
            .iter()
            .any(|c| c.segments.iter().any(|word| word == "云璃")));
    }

    #[test]
    fn long_quanpin_and_mixed_initials_preserve_full_candidates() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let mut engine = PureRustEngine::new(Some(&assets));
        for (code, fuzzy, first, reading) in [
            (
                "womenjintianquchifan",
                false,
                "我们今天去吃饭",
                "wo men jin tian qu chi fan",
            ),
            (
                "zhongguorenminyinhangkaa",
                true,
                "中国人民银行卡啊",
                "zhong guo ren min yin hang ka a",
            ),
            (
                "womenjintianquchifna",
                true,
                "我们今天去吃饭那",
                "wo men jin tian qu chi fan na",
            ),
            ("shij", false, "时间", "shi jian"),
        ] {
            engine.fuzzy_pinyin = fuzzy;
            let candidates = engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                None,
                None,
                None,
                false,
            );
            assert_eq!(candidates[0].canonical, first, "{code}");
            assert_eq!(candidates[0].consumed_len, code.len(), "{code}");
            assert_eq!(candidates[0].item.comment, reading, "{code}");
        }
        let code = "womenjintianquchifna";
        engine.fuzzy_pinyin = true;
        let decoded = engine.decode_candidates(code, SchemaKind::RimeIce, None, None, None);
        assert!(
            decoded.iter().any(|c| {
                c.text == "我们今天去吃饭"
                    && c.consumed_len == code.len()
                    && c.syllables == ["wo", "men", "jin", "tian", "qu", "chi", "fan"]
            }),
            "{code}"
        );
    }

    #[test]
    fn merged_syllables_win_by_default_and_split_mode_forces_zero_initial_boundaries() {
        let assets =
            crate::imp::polyphone::default_assets_dir_candidates().expect("workspace assets");
        let mut engine = PureRustEngine::new(Some(&assets));
        let corpus = Arc::clone(engine.get_corpus());
        let normal = decode_quanpin_with_fuzzy(
            "dian",
            &corpus,
            engine.get_gram(),
            None,
            None,
            40,
            false,
            false,
        );
        assert!(
            matches!(normal[0].text.as_str(), "点" | "电" | "店"),
            "{normal:?}"
        );
        let split_at = normal
            .iter()
            .position(|c| c.syllables == ["di", "an"])
            .expect("split candidates retained");
        let merged_last = normal
            .iter()
            .rposition(|c| c.syllables == ["dian"])
            .unwrap();
        assert!(merged_last < split_at);
        assert!(normal[merged_last].score > normal[split_at].score);
        engine.syllable_split = true;
        for fuzzy in [false, true] {
            engine.fuzzy_pinyin = fuzzy;
            let split = engine.query_all_candidates_contextual(
                "dian",
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                None,
                None,
                None,
                false,
            );
            assert!(!split.is_empty());
            assert!(split.iter().all(|c| c.item.comment == "di an"), "{split:?}");
            assert!(
                split
                    .iter()
                    .any(|c| matches!(c.canonical.as_str(), "堤岸" | "底案" | "地安")),
                "{split:?}"
            );
            assert!(!split
                .iter()
                .any(|c| matches!(c.canonical.as_str(), "点" | "电" | "店")));
        }
        for code in ["wo", "ni", "hao", "zhong", "guo"] {
            let result = decode_quanpin_with_fuzzy(code, &corpus, None, None, None, 5, false, true);
            assert!(
                result.iter().any(|c| c.syllables == [code]),
                "{code}: {result:?}"
            );
        }
        for code in [
            "dian", "xian", "jian", "tian", "bian", "qian", "guan", "huan", "yuan",
        ] {
            assert!(crate::eng::quanpin::can_zero_initial_split(code), "{code}");
            let result = decode_quanpin_with_fuzzy(code, &corpus, None, None, None, 5, false, true);
            assert!(!result.is_empty(), "{code}");
            assert!(
                result
                    .iter()
                    .all(|c| c.syllables.concat() == code && c.syllables.len() > 1),
                "{code}: {result:?}"
            );
        }
        // Explicit barriers already opt into splitting in the default mode.
        let explicit =
            decode_quanpin_with_fuzzy("di'an", &corpus, None, None, None, 40, false, false);
        let forced = decode_quanpin_with_fuzzy("dian", &corpus, None, None, None, 40, false, true);
        assert_eq!(forced[0].text, explicit[0].text);
        assert_eq!(forced[0].score, explicit[0].score);
    }

    #[test]
    fn exact_homophones_bypass_sentence_top_k_and_respect_erasure() {
        let assets =
            crate::imp::polyphone::default_assets_dir_candidates().expect("workspace assets");
        let engine = PureRustEngine::new(Some(&assets));
        let corpus = engine.get_corpus();
        for (code, xiaohe, reading, minimum, limit) in [
            ("shi", false, "shi", 40, 120),
            ("ui", true, "shi", 40, 120),
            ("yi", false, "yi", 40, 120),
            ("yi", true, "yi", 40, 120),
            ("yishi", false, "yi shi", 13, 48),
            ("yiui", true, "yi shi", 13, 48),
        ] {
            let mut retention = RetentionEngine::default();
            let erased = corpus.spaced_pinyin_words(reading).nth(15).unwrap().0.to_owned();
            retention.erased_words.insert(erased.clone());
            let candidates = if xiaohe {
                decode_xiaohe_long_context(code, corpus, None, None, Some(&retention), 1)
            } else {
                decode_quanpin_with_fuzzy(code, corpus, None, None, Some(&retention), 1, false, false)
            };
            let exact: Vec<_> = candidates
                .iter()
                .filter(|c| c.consumed_len == code.len() && c.syllables.join(" ") == reading)
                .collect();
            assert!(exact.len() >= minimum, "{code}: {}", exact.len());
            assert!(exact.len() <= limit, "{code}: {}", exact.len());
            assert_eq!(
                candidates
                    .iter()
                    .map(|c| &c.text)
                    .collect::<HashSet<_>>()
                    .len(),
                candidates.len(),
                "{code}"
            );
            assert!(!candidates.iter().any(|c| c.text == erased), "{code}");
            assert!(exact.windows(2).all(|pair| pair[0].score >= pair[1].score));
            let tail: Vec<_> = exact.iter().skip(1).collect();
            let weights: HashMap<_, _> = corpus.spaced_pinyin_words(reading).collect();
            assert!(tail
                .windows(2)
                .all(|pair| { weights[pair[0].text.as_str()] >= weights[pair[1].text.as_str()] }));
        }
        let partial = decode_quanpin_with_fuzzy("shiwoyao", corpus, None, None, None, 1, false, false);
        assert!(
            partial
                .iter()
                .filter(|c| c.consumed_len == 3
                    && c.syllables == ["shi"]
                    && c.text.chars().count() == 1)
                .count()
                >= 24
        );
    }

    #[test]
    fn contextual_prefix_respects_readings_erasure_and_personal_priority() {
        let engine = PureRustEngine::new(None);
        let mut imported = ImportedLexiconIndex::new(None);
        let mut retention = RetentionEngine::default();
        let query =
            |code: &str, schema, imported: &ImportedLexiconIndex, retention: &RetentionEngine| {
                engine.query_all_candidates_contextual(
                    code,
                    schema,
                    OpenCcMode::Simplified,
                    Some(imported),
                    Some(retention),
                    Some("正"),
                    None,
                    true,
                )
            };
        for (schema, code) in [
            (SchemaKind::RimeIce, "q"),
            (SchemaKind::DoublePinyinFlypy, "q"),
        ] {
            assert_eq!(
                query(code, schema, &imported, &retention)[0].canonical,
                "确"
            );
        }
        retention.pinned_words.insert("去".into());
        assert_eq!(
            query("q", SchemaKind::RimeIce, &imported, &retention)[0].canonical,
            "去"
        );
        retention.pinned_words.clear();
        retention.record_selection_at("去", RetentionEngine::now_epoch_ms(), 10.0);
        assert_eq!(
            query("q", SchemaKind::RimeIce, &imported, &retention)[0].canonical,
            "去"
        );
        retention = RetentionEngine::default();
        retention
            .context_bigrams
            .insert("正".into(), HashMap::from([("情绪".into(), 4.0)]));
        assert_eq!(
            query("q", SchemaKind::RimeIce, &imported, &retention)[0].canonical,
            "情绪"
        );
        retention.erased_words.insert("情绪".into());
        assert_eq!(
            query("q", SchemaKind::RimeIce, &imported, &retention)[0].canonical,
            "确"
        );
        retention.erased_words.insert("确".into());
        assert!(!query("q", SchemaKind::RimeIce, &imported, &retention)
            .iter()
            .any(|c| c.canonical == "确"));
        retention = RetentionEngine::default();
        retention
            .context_bigrams
            .insert("不好意思".into(), HashMap::from([("你好".into(), 4.0)]));
        for (schema, code) in [
            (SchemaKind::RimeIce, "niha"),
            (SchemaKind::DoublePinyinFlypy, "nih"),
        ] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                schema,
                OpenCcMode::Simplified,
                Some(&imported),
                Some(&retention),
                Some("不好意思"),
                None,
                true,
            );
            assert_eq!(candidates[0].canonical, "你好");
        }
        imported.inject_hotwords(&[crate::imp::ImportedLexiconEntry {
            text: "确".into(),
            pinyin: "hu".into(),
            toned_pinyin: "hú".into(),
            xiaohe: "hu".into(),
            weight: 1000,
        }]);
        for schema in [SchemaKind::RimeIce, SchemaKind::DoublePinyinFlypy] {
            assert!(!query("q", schema, &imported, &retention)
                .iter()
                .any(|c| c.canonical == "确"));
        }
    }

    #[test]
    fn contextual_prefix_does_not_replace_explicit_sentences_or_unrelated_codes() {
        let engine = PureRustEngine::new(None);
        let mut retention = RetentionEngine::default();
        retention
            .context_bigrams
            .insert("前".into(), HashMap::from([("你好世界".into(), 4.0)]));
        for (schema, code) in [
            (SchemaKind::RimeIce, "nihao"),
            (SchemaKind::DoublePinyinFlypy, "nihc"),
        ] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                schema,
                OpenCcMode::Simplified,
                None,
                Some(&retention),
                Some("前"),
                None,
                true,
            );
            assert_eq!(candidates[0].canonical, "你好");
        }
        for code in ["x", "a", "n'", "123", "nǐ", "nihaosh"] {
            assert!(
                engine
                    .contextual_prefix_candidate(
                        code,
                        SchemaKind::RimeIce,
                        None,
                        Some(&retention),
                        "前",
                        true,
                    )
                    .is_none(),
                "{code}"
            );
        }
    }

    #[test]
    fn v_spelling_decodes_umlaut_and_jqx_aliases() {
        let engine = PureRustEngine::new(None);
        for (code, expected) in [
            ("nv", "女"),
            ("lv", "绿"),
            ("nve", "虐"),
            ("lve", "略"),
            ("jv", "局"),
            ("qv", "去"),
            ("xv", "需"),
            ("lvyou", "旅游"),
        ] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                None,
                None,
                None,
                false,
            );
            assert_eq!(candidates[0].canonical, expected, "{code}: {candidates:?}");
            assert_eq!(candidates[0].consumed_len, code.len());
        }
    }

    #[test]
    fn sentence_prefix_completion_abbreviation_and_typo() {
        let engine = PureRustEngine::new(None);
        let query = |code| {
            engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                None,
                None,
                None,
                false,
            )
        };
        let sentence = query("woyaoquka");
        for (word, consumed) in [
            ("我要去卡", 9),
            ("我要去", 7),
            ("我要", 5),
            ("我", 2),
            ("握", 2),
        ] {
            assert!(
                sentence
                    .iter()
                    .any(|c| c.canonical == word && c.consumed_len == consumed),
                "{word}: {sentence:?}"
            );
        }
        assert_eq!(sentence[0].consumed_len, 9);
        for word in ["我要去", "我要", "我", "握"] {
            assert!(
                sentence.iter().take(6).any(|c| c.canonical == word),
                "{sentence:?}"
            );
        }
        assert!(
            sentence.iter().position(|c| c.canonical == "我")
                < sentence.iter().position(|c| c.canonical == "握")
        );
        assert_eq!(query("w")[0].canonical, "我");
        assert_eq!(query("wod")[0].canonical, "我的");
        assert!(query("zh").iter().any(|c| c.canonical == "重"));
        for (code, word) in [
            ("srf", "输入法"),
            ("wsm", "为什么"),
            ("nh", "你好"),
            ("bzd", "不知道"),
            ("zmy", "怎么样"),
        ] {
            assert!(query(code).iter().any(|c| c.canonical == word), "{code}");
        }
        assert_eq!(query("nihao")[0].canonical, "你好");
        assert!(query("woyaodako").iter().any(|c| c.canonical == "我要打卡"));
        let separated = query("wo'yaoquka");
        assert!(separated
            .iter()
            .any(|c| c.canonical == "我要" && c.consumed_len == 6));
    }
    #[test]
    fn bundled_corpus_device_inputs() {
        let assets =
            crate::imp::polyphone::default_assets_dir_candidates().expect("workspace assets");
        let engine = PureRustEngine::new(Some(&assets));
        let corpus = engine.get_corpus();
        for prefix in ["我", "你好", "输入法"] {
            let mut expected: Vec<_> = corpus
                .word_weights()
                .filter(|(word, _)| (2..=6).contains(&word.chars().count()))
                .filter_map(|(word, weight)| {
                    word.strip_prefix(prefix)
                        .filter(|suffix| !suffix.is_empty())
                        .map(|suffix| (suffix, weight))
                })
                .collect();
            expected.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            expected.truncate(if prefix.chars().count() == 1 { 24 } else { 10 });
            assert_eq!(
                corpus.continuations(prefix).collect::<Vec<_>>(),
                expected
            );
        }
        for (code, expected) in [
            ("w", "我"),
            ("wod", "我的"),
            ("srf", "输入法"),
            ("nihao", "你好"),
            ("jintiantianqizhenhao", "今天天气真好"),
            ("woyaoquka", "我"),
            ("woyaodako", "我要打卡"),
        ] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                None,
                None,
                None,
                None,
                false,
            );
            assert!(
                candidates.iter().any(|c| c.canonical == expected),
                "{code}: {candidates:?}"
            );
            if matches!(code, "nihao" | "jintiantianqizhenhao") {
                assert_eq!(candidates[0].canonical, expected);
            }
            if code == "woyaodako" {
                assert!(
                    candidates.iter().take(5).any(|c| c.canonical == expected),
                    "{candidates:?}"
                );
            }
            if code == "woyaoquka" {
                for word in ["我要去", "我要", "我"] {
                    assert!(
                        candidates.iter().take(6).any(|c| c.canonical == word),
                        "{candidates:?}"
                    );
                }
                let prefixes: Vec<_> = candidates
                    .iter()
                    .take(6)
                    .map(|c| c.canonical.as_str())
                    .filter(|word| ["我要去", "我要", "我"].contains(word))
                    .collect();
                assert_eq!(prefixes, ["我要去", "我要", "我"]);
                assert_eq!(candidates[0].consumed_len, code.len());
            }
        }
    }
    #[test]
    fn imported_readings_keep_prefix_abbreviation_and_correction_candidates() {
        let engine = PureRustEngine::new(None);
        let mut imported = ImportedLexiconIndex::new(None);
        let entries: Vec<_> = [
            ("我", "wo"),
            ("我的", "wo de"),
            ("输入法", "shu ru fa"),
            ("我要打卡", "wo yao da ka"),
        ]
        .into_iter()
        .map(|(text, pinyin)| crate::imp::ImportedLexiconEntry {
            text: text.into(),
            pinyin: pinyin.into(),
            toned_pinyin: pinyin.into(),
            xiaohe: crate::imp::xiaohe::pinyin_phrase_to_xiaohe(pinyin),
            weight: 500_000,
        })
        .collect();
        imported.inject_hotwords(&entries);
        for (code, expected) in [
            ("woyaoquka", "我"),
            ("wod", "我的"),
            ("srf", "输入法"),
            ("woyaodako", "我要打卡"),
        ] {
            let candidates = engine.query_all_candidates_contextual(
                code,
                SchemaKind::RimeIce,
                OpenCcMode::Simplified,
                Some(&imported),
                None,
                None,
                None,
                false,
            );
            assert!(
                candidates.iter().any(|c| c.canonical == expected),
                "{code}: {candidates:?}"
            );
        }
    }
    #[test]
    fn personal_partial_candidates_follow_full_sentences() {
        let engine = PureRustEngine::new(None);
        let mut retention = RetentionEngine::default();
        retention.pin_word("握");
        retention.record_contextual_selection(
            "我要",
            "woyao",
            None,
            None,
            RetentionEngine::now_epoch_ms(),
            2.0,
            true,
        );
        let candidates = engine.query_all_candidates_contextual(
            "woyaoquka",
            SchemaKind::RimeIce,
            OpenCcMode::Simplified,
            None,
            Some(&retention),
            None,
            None,
            true,
        );
        assert!(candidates
            .iter()
            .take(2)
            .all(|c| c.consumed_len == "woyaoquka".len()));
        for word in ["握", "我要"] {
            assert!(candidates.iter().position(|c| c.canonical == word).unwrap() >= 2);
        }
        assert!(
            candidates
                .iter()
                .find(|c| c.canonical == "握")
                .unwrap()
                .item
                .pinned
        );
    }

    #[test]
    fn full_span_word_history_is_counted_once_in_final_ranking() {
        let mut engine = PureRustEngine::new(None);
        engine.typo_correction_level = TypoCorrectionLevel::Off;
        let mut imported = ImportedLexiconIndex::new(None);
        let preferred = "你号";
        let learned = "妮好";
        imported.inject_hotwords(&[(preferred, 2_000), (learned, 1_000)].map(|(word, weight)| {
            crate::imp::ImportedLexiconEntry {
                text: word.into(),
                pinyin: "ni hao".into(),
                toned_pinyin: "nǐ hǎo".into(),
                xiaohe: "nihc".into(),
                weight,
            }
        }));
        let mut retention = RetentionEngine::default();
        let now = RetentionEngine::now_epoch_ms();
        let three_half_lives_ms = (retention.half_life_sec * 3_000.0) as u64;
        retention.record_selection_at(learned, now - three_half_lives_ms, 2.0);
        let code = "nihao";
        let schema = SchemaKind::RimeIce;
        let decoded = engine.decode_candidates(code, schema, Some(&imported), Some(&retention), None);
        let from_beam = decoded.iter().find(|c| c.text == learned).unwrap();
        assert_eq!(from_beam.segments, [learned]);
        assert!(from_beam.full_span_frecency_bonus > 0.0);

        let (ranked, _) = engine.rank_candidates(
            &decoded, code, schema, OpenCcMode::Simplified, Some(&imported),
            Some(&retention), None, None, true, None,
        );
        let top = &ranked[0];
        let learned = ranked.iter().find(|c| c.canonical == learned).unwrap();
        let boost = personal_boost(learned.item.frecency_score);
        assert!(from_beam.score + boost > top.lm_score);
        assert!(learned.lm_score + boost < top.lm_score);
        assert_eq!(top.canonical, preferred);
    }

    #[test]
    fn decayed_history_keeps_common_words_and_sentences_first() {
        let assets = crate::imp::polyphone::default_assets_dir_candidates().unwrap();
        let engine = PureRustEngine::new(Some(&assets));
        let now = RetentionEngine::now_epoch_ms();
        for (code, expected) in [("ni", "你"), ("jintiantianqizhenhao", "今天天气真好")] {
            let query = |retention| {
                engine.query_all_candidates_contextual(
                    code,
                    SchemaKind::RimeIce,
                    OpenCcMode::Simplified,
                    None,
                    retention,
                    None,
                    None,
                    true,
                )
            };
            let baseline = query(None);
            assert_eq!(baseline[0].canonical, expected);
            let old = baseline.iter()
                .find(|c| c.consumed_len == code.len() && baseline[0].lm_score - c.lm_score > 1.0)
                .unwrap_or_else(|| panic!("{code}: missing a full-span alternative"));
            let mut retention = RetentionEngine::default();
            let six_half_lives_ms = (retention.half_life_sec * 6_000.0) as u64;
            retention.record_selection_at(&old.canonical, now - six_half_lives_ms, 3.2);
            assert!((retention.decayed_score_at(&old.canonical, now) - 0.05).abs() < 1e-9);
            let ranked = query(Some(&retention));
            assert_eq!(ranked[0].canonical, expected, "{code}: {:?}", &ranked[..2]);
            assert!(ranked.iter().any(|c| c.canonical == old.canonical));
        }
    }
}
