use crate::eng::gram::RimeGramDb;
use crate::eng::quanpin::{decode_quanpin_with_fuzzy, decode_xiaohe_long_context};
use crate::eng::{CandidateItem, OpenCcMode, PageInfo, SchemaKind};
use crate::imp::polyphone::RimeDictCorpus;
use crate::imp::ImportedLexiconIndex;
use crate::ret::RetentionEngine;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use vitral_tokens::TOKENS;

// Canonical casing is shared by the native and pure-Rust candidate paths.
pub(super) fn mixed_english<'a>(
    raw: &str,
    retention: Option<&'a RetentionEngine>,
) -> Option<&'a str> {
    const WORDS: &[&str] = &[
        "OK",
        "App",
        "iPhone",
        "iPad",
        "Mac",
        "GitHub",
        "VSCode",
        "Python",
        "Rust",
        "Java",
        "Linux",
        "Android",
        "Windows",
        "Wi-Fi",
        "PDF",
        "API",
        "URL",
        "HTTP",
        "JSON",
        "SQL",
        "AI",
        "LLM",
        "CPU",
        "GPU",
        "RAM",
        "USB",
        "VPN",
        "UI",
        "UX",
        "Git",
        "Docker",
        "Chrome",
        "WeChat",
        "Google",
        "Apple",
        "Email",
        "Gmail",
        "Office",
        "Excel",
        "Word",
        "PPT",
        "Hello",
        "Thanks",
        "Sorry",
        "Please",
        "Test",
        "Demo",
        "Beta",
        "Pro",
        "Max",
        "Mini",
        "Plus",
        "VIP",
        "TODO",
        "BUG",
        "Fix",
        "WebDAV",
        "Koofr",
        "Vitral",
        "OpenAI",
        "ChatGPT",
        "HTML",
        "CSS",
        "JavaScript",
        "TypeScript",
        "React",
        "Node",
        "NPM",
        "SDK",
        "IDE",
        "HTTPS",
        "TCP",
        "IP",
        "DNS",
        "SSH",
        "FTP",
        "SSD",
        "HDD",
        "WiFi",
        "Bluetooth",
        "Zoom",
    ];
    if raw.len() < 2 || !raw.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    retention
        .and_then(|r| r.query_user_english(raw))
        .or_else(|| {
            WORDS
                .iter()
                .copied()
                .find(|word| word.eq_ignore_ascii_case(raw))
        })
        .filter(|word| !retention.is_some_and(|r| r.erased_words.contains(*word)))
}

#[derive(Debug)]
pub(crate) struct DecodedCandidate {
    pub item: CandidateItem,
    pub canonical: String,
    pub consumed_len: usize,
}

#[derive(Debug)]
pub struct PureRustEngine {
    assets_dir: Option<PathBuf>,
    grammar_path: Option<PathBuf>,
    corpus: OnceLock<Arc<RimeDictCorpus>>,
    gram: OnceLock<Option<Arc<RimeGramDb>>>,
    pub fuzzy_pinyin: bool,
    pub syllable_split: bool,
    pub page_index: usize,
    pub last_candidates: Vec<CandidateItem>,
}

impl PureRustEngine {
    pub fn new(assets_dir: Option<&Path>) -> Self {
        Self {
            assets_dir: assets_dir.map(PathBuf::from),
            grammar_path: crate::models::ModelPaths::bundled(assets_dir).grammar,
            corpus: OnceLock::new(),
            gram: OnceLock::new(),
            fuzzy_pinyin: false,
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
    }

    pub fn set_model_paths(&mut self, paths: &crate::models::ModelPaths) {
        self.grammar_path = paths.grammar.clone();
        self.gram = OnceLock::new();
    }

    pub(crate) fn get_corpus(&self) -> &Arc<RimeDictCorpus> {
        self.corpus
            .get_or_init(|| RimeDictCorpus::load_or_fallback(self.assets_dir.as_deref()))
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
    ) -> Vec<(CandidateItem, String, f64)> {
        use crate::imp::xiaohe::{canonical_syllable_set, xiaohe_reverse_map};

        if !(4..=48).contains(&code.len()) || !code.bytes().all(|b| b.is_ascii_lowercase()) {
            return Vec::new();
        }
        let flypy = schema == SchemaKind::DoublePinyinFlypy;
        if flypy && code.len() % 2 != 0 {
            return Vec::new();
        }
        let corpus = self.get_corpus();
        let now_ms = RetentionEngine::now_epoch_ms();
        let mut seen = HashSet::new();
        let mut ranked = Vec::new();
        let mut add = |word: &str, reading: &str, weight: u64| {
            if word.chars().count() < 2
                || !word.chars().all(crate::ret::is_cjk)
                || retention.erased_words.contains(word)
                || !imported.matches_reading(word, code, flypy)
                || !seen.insert(word.to_string())
            {
                return;
            }
            let frecency_score = retention.decayed_score_at(word, now_ms);
            ranked.push((
                CandidateItem {
                    index: 0,
                    text: word.to_string(),
                    comment: reading.to_string(),
                    pinned: retention.pinned_words.contains(word),
                    is_clipboard_shortcut: false,
                    quick_paste_id: None,
                    function_action: None,
                    shortcut_code: None,
                    frecency_score,
                },
                word.to_string(),
                frecency_score + (weight as f64 + 1.0).ln(),
            ));
        };
        let imported_words = if flypy {
            imported.query_xiaohe(code)
        } else {
            imported.query_quanpin(code)
        };
        for word in imported_words.iter().take(8) {
            add(&word.text, &word.pinyin, u64::from(word.weight));
        }
        let mut paths = vec![(0usize, String::new(), 0usize)];
        let mut visited = 0;
        while let Some((offset, reading, syllable_count)) = paths.pop() {
            if visited == 64 {
                break;
            }
            visited += 1;
            if offset == code.len() {
                if syllable_count >= 2 {
                    if let Some(words) = corpus.spaced_pinyin_to_words.get(&reading) {
                        for (word, weight) in words.iter().take(8) {
                            add(word, &reading, *weight);
                        }
                    }
                }
                continue;
            }
            if syllable_count == 8 {
                continue;
            }
            let mut push = |end: usize, syllable: &str| {
                let mut next = reading.clone();
                if !next.is_empty() {
                    next.push(' ');
                }
                next.push_str(syllable);
                paths.push((end, next, syllable_count + 1));
            };
            if flypy {
                if let Some(syllables) = xiaohe_reverse_map().get(&code[offset..offset + 2]) {
                    for syllable in syllables.iter().rev() {
                        push(offset + 2, syllable);
                    }
                }
            } else {
                for end in offset + 1..=(offset + 6).min(code.len()) {
                    let syllable = &code[offset..end];
                    if canonical_syllable_set().contains(syllable) {
                        push(end, syllable);
                    }
                }
            }
        }
        ranked.sort_by(|a, b| {
            b.0.pinned
                .cmp(&a.0.pinned)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| a.1.cmp(&b.1))
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
                .continuations
                .get(prefix)
                .into_iter()
                .flatten()
                .map(|(word, weight)| (word, false, *weight as f64));
            let personal = retention
                .filter(|_| adaptive)
                .and_then(|r| r.context_bigrams.get(prefix))
                .into_iter()
                .flatten()
                .filter(|(_, weight)| **weight > 0.25)
                .map(|(word, weight)| (word, true, *weight));
            for (word, personal, weight) in dictionary.chain(personal) {
                let word_len = word.chars().count();
                if !(1..=6).contains(&word_len)
                    || !word.chars().all(crate::ret::is_cjk)
                    || retention.is_some_and(|r| r.erased_words.contains(word))
                {
                    continue;
                }
                let syllables = corpus
                    .word_readings
                    .get(word)
                    .map(|(s, _)| s.clone())
                    .or_else(|| {
                        (word_len == 1)
                            .then(|| {
                                corpus
                                    .char_readings
                                    .get(&word.chars().next()?)?
                                    .first()
                                    .map(|(syllable, _)| vec![syllable.clone()])
                            })
                            .flatten()
                    })
                    // A suffix need not itself be a dictionary word. Its source phrase
                    // still carries the correct, context-sensitive pronunciation.
                    .or_else(|| {
                        corpus
                            .word_readings
                            .get(&format!("{prefix}{word}"))
                            .map(|(s, _)| s.iter().skip(prefix_len + 1).cloned().collect())
                    })
                    .or_else(|| {
                        word.chars()
                            .map(|ch| {
                                corpus
                                    .char_readings
                                    .get(&ch)?
                                    .first()
                                    .map(|(s, _)| s.clone())
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
                            .then_with(|| old_word.cmp(word))
                            .is_gt()
                    });
                if better {
                    best = Some((personal, prefix_len + 1, score, word.clone(), syllables));
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
        if raw_input.is_empty() {
            return Vec::new();
        }

        let corpus = Arc::clone(self.get_corpus());
        let gram = self.get_gram();
        let decoded = match schema {
            SchemaKind::RimeIce => decode_quanpin_with_fuzzy(
                raw_input,
                &corpus,
                gram,
                imported,
                retention,
                40,
                self.fuzzy_pinyin,
                self.syllable_split,
            ),
            SchemaKind::DoublePinyinFlypy => {
                decode_xiaohe_long_context(raw_input, &corpus, gram, imported, retention, 40)
            }
        };

        let exact_reading = |syllables: &[String]| {
            syllables
                .iter()
                .all(|s| crate::imp::xiaohe::canonical_syllable_set().contains(s.as_str()))
                && if schema == SchemaKind::RimeIce {
                    syllables.concat() == raw_input.replace(['\'', ' '], "")
                } else {
                    crate::imp::xiaohe::pinyin_phrase_to_xiaohe(&syllables.join(" ")) == raw_input
                }
        };
        let has_exact_chinese = decoded.iter().any(|c| {
            c.consumed_len == raw_input.len()
                && c.segments.len() == 1
                && c.text.chars().all(crate::ret::is_cjk)
                && exact_reading(&c.syllables)
                && !retention.is_some_and(|r| r.erased_words.contains(&c.text))
        }) || retention.is_some_and(|r| {
            !r.query_user_phrases(raw_input, schema == SchemaKind::DoublePinyinFlypy)
                .is_empty()
        });
        let now_ms = RetentionEngine::now_epoch_ms();
        let mut pinned_items = Vec::new();
        let mut normal_items: Vec<(DecodedCandidate, f64)> = Vec::new();
        let mut seen = HashSet::new();
        for cand in decoded {
            let reading = if schema == SchemaKind::RimeIce {
                cand.syllables.concat()
            } else {
                raw_input[..cand.consumed_len].to_string()
            };
            if retention.is_some_and(|r| r.erased_words.contains(&cand.text))
                || imported.is_some_and(|i| {
                    !i.matches_reading(
                        &cand.text,
                        &reading,
                        schema == SchemaKind::DoublePinyinFlypy,
                    )
                })
            {
                continue;
            }
            let converted = pure_rust_opencc(opencc_mode, &cand.text);
            if !seen.insert(converted.clone()) {
                continue;
            }
            let pinned = retention.is_some_and(|r| r.pinned_words.contains(&cand.text));
            let frec = retention
                .map(|r| {
                    r.contextual_score_at(
                        &cand.text,
                        &raw_input[..cand.consumed_len],
                        prev_context,
                        app_id,
                        now_ms,
                        adaptive,
                    )
                })
                .unwrap_or(0.0);
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
                canonical: cand.text,
                consumed_len: cand.consumed_len,
            };
            if pinned {
                pinned_items.push(decoded);
            } else {
                normal_items.push((decoded, cand.score));
            }
        }

        normal_items.sort_by(|a, b| {
            let exact = |c: &DecodedCandidate| {
                c.consumed_len == raw_input.len()
                    && exact_reading(
                        &c.item
                            .comment
                            .split_whitespace()
                            .map(str::to_string)
                            .collect::<Vec<_>>(),
                    )
            };
            let fuzzy = self.fuzzy_pinyin && schema == SchemaKind::RimeIce;
            (fuzzy && exact(&b.0))
                .cmp(&(fuzzy && exact(&a.0)))
                .then_with(|| {
                    b.0.item
                        .frecency_score
                        .max(0.0)
                        .total_cmp(&a.0.item.frecency_score.max(0.0))
                        .then_with(|| b.0.consumed_len.cmp(&a.0.consumed_len))
                        .then_with(|| b.1.total_cmp(&a.1))
                })
        });
        let personal_count = normal_items
            .iter()
            .take_while(|(c, _)| c.item.frecency_score > 0.0)
            .count();
        pinned_items.extend(normal_items.drain(..personal_count).map(|(c, _)| c));
        // Keep typo/completion ranking intact until there is an exact full reading.
        let has_sentence = normal_items.iter().any(|(c, _)| {
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
            .any(|(c, _)| c.consumed_len < raw_input.len());
        if has_sentence && has_partial {
            let mut full = Vec::new();
            let mut prefixes = Vec::new();
            let mut singles = Vec::new();
            let mut remaining = Vec::new();
            let mut lengths = HashSet::new();
            for (candidate, score) in normal_items {
                if candidate.consumed_len == raw_input.len() {
                    full.push((candidate, score));
                } else if candidate.canonical.chars().count() > 1
                    && lengths.insert(candidate.consumed_len)
                {
                    prefixes.push((candidate, score));
                } else if candidate.canonical.chars().count() == 1
                    && candidate.item.comment.split_whitespace().count() == 1
                {
                    let frequency = corpus
                        .spaced_pinyin_to_words
                        .get(&candidate.item.comment)
                        .and_then(|words| words.iter().find(|(w, _)| *w == candidate.canonical))
                        .map_or(score, |(_, weight)| (*weight as f64 + 1.0).ln());
                    singles.push((candidate, frequency));
                } else {
                    remaining.push((candidate, score));
                }
            }
            // Preserve the best sentences, then expose selectable prefixes and homophones.
            singles.sort_by(|a, b| b.1.total_cmp(&a.1));
            let rest_full = full.split_off(full.len().min(2));
            let rest_singles = singles.split_off(singles.len().min(2));
            full.extend(prefixes);
            full.extend(singles);
            full.extend(rest_full);
            full.extend(remaining);
            full.extend(rest_singles);
            normal_items = full;
        }
        pinned_items.extend(normal_items.into_iter().map(|(candidate, _)| candidate));
        if let Some(word) = mixed_english(raw_input, retention) {
            let at = usize::from(has_exact_chinese).min(pinned_items.len());
            pinned_items.insert(
                at,
                DecodedCandidate {
                    item: CandidateItem {
                        index: at,
                        text: word.into(),
                        comment: "英文".into(),
                        pinned: false,
                        is_clipboard_shortcut: false,
                        quick_paste_id: None,
                        function_action: None,
                        shortcut_code: None,
                        frecency_score: 0.0,
                    },
                    canonical: word.into(),
                    consumed_len: raw_input.len(),
                },
            );
        }
        if let Some((word, syllables)) = prev_context.and_then(|context| {
            self.contextual_prefix_candidate(raw_input, schema, imported, retention, context, adaptive)
        }) {
            // An explicitly typed sentence wins over an unfinished contextual completion.
            let longer_sentence = !exact_reading(&syllables)
                && pinned_items.iter().any(|c| {
                    c.canonical != word
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
                    r.contextual_score_at(&word, raw_input, prev_context, app_id, now_ms, adaptive)
                });
                let text = pure_rust_opencc(opencc_mode, &word);
                let pinned = retention.is_some_and(|r| r.pinned_words.contains(&word));
                pinned_items.retain(|c| c.item.text != text);
                let at = pinned_items
                    .iter()
                    .rposition(|c| {
                        c.item.pinned
                            || (!pinned && c.item.frecency_score.max(0.0) > frecency_score.max(0.0))
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
                        canonical: word,
                        consumed_len: raw_input.len(),
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
            });
        }
        for (i, candidate) in pinned_items.iter_mut().enumerate() {
            candidate.item.index = i;
        }
        pinned_items
    }
}

/// Cross-platform pure-Rust Simplified -> Traditional conversion fallback (used on Android/Windows
/// and `--no-default-features`; Linux `native-rime` uses `libopencc.so` directly).
pub fn pure_rust_opencc(mode: OpenCcMode, text: &str) -> String {
    if mode == OpenCcMode::Simplified || text.is_empty() {
        return text.to_string();
    }
    let mut s = if mode == OpenCcMode::TaiwanStandard {
        text.replace("软件", "軟體")
            .replace("硬件", "硬體")
            .replace("内存", "記憶體")
            .replace("网络", "網路")
            .replace("信息", "資訊")
            .replace("屏幕", "螢幕")
            .replace("鼠标", "滑鼠")
            .replace("打印", "列印")
            .replace("程序", "程式")
            .replace("默认", "預設")
    } else {
        text.to_string()
    };

    let map = s2t_char_map();
    s = s
        .chars()
        .map(|c| map.get(&c).copied().unwrap_or(c))
        .collect();
    s
}

fn s2t_char_map() -> &'static HashMap<char, char> {
    static MAP: OnceLock<HashMap<char, char>> = OnceLock::new();
    MAP.get_or_init(|| {
        const PAIRS: &[(char, char)] = &[
            ('万', '萬'), ('与', '與'), ('专', '專'), ('业', '業'), ('东', '東'), ('丝', '絲'),
            ('两', '兩'), ('严', '嚴'), ('丧', '喪'), ('个', '個'), ('丰', '豐'), ('临', '臨'),
            ('为', '為'), ('丽', '麗'), ('举', '舉'), ('么', '麼'), ('义', '義'), ('乌', '烏'),
            ('乐', '樂'), ('乔', '喬'), ('习', '習'), ('乡', '鄉'), ('书', '書'), ('买', '買'),
            ('乱', '亂'), ('争', '爭'), ('亏', '虧'), ('云', '雲'), ('亚', '亞'), ('产', '產'),
            ('亲', '親'), ('亿', '億'), ('仅', '僅'), ('从', '從'), ('仓', '倉'), ('仪', '儀'),
            ('们', '們'), ('价', '價'), ('众', '眾'), ('优', '優'), ('会', '會'), ('伟', '偉'),
            ('传', '傳'), ('伤', '傷'), ('伦', '倫'), ('伪', '偽'), ('体', '體'), ('余', '餘'),
            ('佣', '傭'), ('侠', '俠'), ('侣', '侶'), ('侦', '偵'), ('侧', '側'), ('侨', '僑'),
            ('俩', '倆'), ('债', '債'), ('倾', '傾'), ('假', '假'), ('偿', '償'), ('储', '儲'),
            ('儿', '兒'), ('党', '黨'), ('兰', '蘭'), ('关', '關'), ('兴', '興'), ('养', '養'),
            ('内', '內'), ('冈', '岡'), ('册', '冊'), ('写', '寫'), ('军', '軍'), ('农', '農'),
            ('冲', '衝'), ('决', '決'), ('况', '況'), ('冻', '凍'), ('净', '淨'), ('准', '準'),
            ('凉', '涼'), ('凌', '凌'), ('减', '減'), ('几', '幾'), ('凤', '鳳'), ('凭', '憑'),
            ('凯', '凱'), ('击', '擊'), ('凿', '鑿'), ('划', '劃'), ('刘', '劉'), ('则', '則'),
            ('刚', '剛'), ('创', '創'), ('删', '刪'), ('别', '別'), ('利', '利'), ('刮', '颳'),
            ('制', '制'), ('刷', '刷'), ('刺', '刺'), ('刻', '刻'), ('剂', '劑'), ('剑', '劍'),
            ('剧', '劇'), ('劝', '勸'), ('办', '辦'), ('务', '務'), ('动', '動'), ('助', '助'),
            ('劳', '勞'), ('势', '勢'), ('勤', '勤'), ('区', '區'), ('医', '醫'), ('华', '華'),
            ('协', '協'), ('单', '單'), ('卖', '賣'), ('卢', '盧'), ('卫', '衛'), ('却', '卻'),
            ('厂', '廠'), ('厅', '廳'), ('历', '歷'), ('厉', '厲'), ('压', '壓'), ('厌', '厭'),
            ('厕', '廁'), ('县', '縣'), ('参', '參'), ('双', '雙'), ('发', '發'), ('变', '變'),
            ('叙', '敘'), ('台', '臺'), ('叶', '葉'), ('号', '號'), ('叹', '嘆'), ('后', '後'),
            ('吓', '嚇'), ('吗', '嗎'), ('吨', '噸'), ('听', '聽'), ('启', '啟'), ('吴', '吳'),
            ('员', '員'), ('呜', '嗚'), ('咏', '詠'), ('品', '品'), ('响', '響'), ('哑', '啞'),
            ('哗', '嘩'), ('咱', '咱'), ('哀', '哀'), ('唤', '喚'), ('喜', '喜'), ('喝', '喝'),
            ('喷', '噴'), ('器', '器'), ('园', '園'), ('困', '困'), ('围', '圍'), ('固', '固'),
            ('国', '國'), ('图', '圖'), ('团', '團'), ('圣', '聖'), ('场', '場'), ('坏', '壞'),
            ('块', '塊'), ('坚', '堅'), ('坛', '壇'), ('坝', '壩'), ('坟', '墳'), ('坠', '墜'),
            ('垄', '壟'), ('垒', '壘'), ('垦', '墾'), ('垫', '墊'), ('尘', '塵'), ('墙', '牆'),
            ('增', '增'), ('声', '聲'), ('处', '處'), ('备', '備'), ('复', '復'), ('够', '夠'),
            ('头', '頭'), ('夸', '誇'), ('夺', '奪'), ('奋', '奮'), ('奖', '獎'), ('奥', '奧'),
            ('妆', '妝'), ('妇', '婦'), ('妈', '媽'), ('妖', '妖'), ('妙', '妙'), ('妥', '妥'),
            ('妨', '妨'), ('妹', '妹'), ('妻', '妻'), ('始', '始'), ('姐', '姐'), ('姑', '姑'),
            ('姓', '姓'), ('委', '委'), ('威', '威'), ('娃', '娃'), ('娇', '嬌'), ('娘', '娘'),
            ('娱', '娛'), ('孙', '孫'), ('学', '學'), ('宁', '寧'), ('宝', '寶'), ('实', '實'),
            ('审', '審'), ('写', '寫'), ('宽', '寬'), ('宠', '寵'), ('将', '將'), ('尊', '尊'),
            ('寻', '尋'), ('对', '對'), ('导', '導'), ('小', '小'), ('尔', '爾'), ('尘', '塵'),
            ('尝', '嘗'), ('层', '層'), ('届', '屆'), ('属', '屬'), ('岗', '崗'), ('岛', '島'),
            ('峡', '峽'), ('川', '川'), ('州', '州'), ('巡', '巡'), ('帅', '帥'), ('师', '師'),
            ('帐', '帳'), ('带', '帶'), ('帮', '幫'), ('广', '廣'), ('庄', '莊'), ('庆', '慶'),
            ('库', '庫'), ('应', '應'), ('庙', '廟'), ('庞', '龐'), ('废', '廢'), ('开', '開'),
            ('异', '異'), ('弃', '棄'), ('张', '張'), ('弥', '彌'), ('弯', '彎'), ('强', '強'),
            ('弹', '彈'), ('归', '歸'), ('当', '當'), ('录', '錄'), ('形', '形'), ('彩', '彩'),
            ('影', '影'), ('彻', '徹'), ('征', '徵'), ('待', '待'), ('很', '很'), ('律', '律'),
            ('复', '復'), ('微', '微'), ('德', '德'), ('心', '心'), ('忆', '憶'), ('忧', '憂'),
            ('快', '快'), ('怀', '懷'), ('态', '態'), ('怂', '慫'), ('怜', '憐'), ('思', '思'),
            ('总', '總'), ('恋', '戀'), ('恳', '懇'), ('恶', '惡'), ('恼', '惱'), ('悬', '懸'),
            ('情', '情'), ('惊', '驚'), ('惠', '惠'), ('惨', '慘'), ('惯', '慣'), ('想', '想'),
            ('爱', '愛'), ('感', '感'), ('愿', '願'), ('慈', '慈'), ('慎', '慎'), ('慢', '慢'),
            ('戏', '戲'), ('战', '戰'), ('户', '戶'), ('房', '房'), ('所', '所'), ('才', '才'),
            ('打', '打'), ('扑', '撲'), ('托', '托'), ('执', '執'), ('扩', '擴'), ('扫', '掃'),
            ('扬', '揚'), ('扰', '擾'), ('扶', '扶'), ('找', '找'), ('承', '承'), ('技', '技'),
            ('把', '把'), ('报', '報'), ('拟', '擬'), ('拥', '擁'), ('择', '擇'), ('挂', '掛'),
            ('指', '指'), ('按', '按'), ('挑', '挑'), ('挖', '挖'), ('挥', '揮'), ('损', '損'),
            ('换', '換'), ('据', '據'), ('捷', '捷'), ('扫', '掃'), ('授', '授'), ('掉', '掉'),
            ('掌', '掌'), ('排', '排'), ('探', '探'), ('接', '接'), ('控', '控'), ('推', '推'),
            ('掩', '掩'), ('提', '提'), ('插', '插'), ('握', '握'), ('援', '援'), ('搜', '搜'),
            ('搞', '搞'), ('搬', '搬'), ('搭', '搭'), ('携', '攜'), ('摄', '攝'), ('摆', '擺'),
            ('摊', '攤'), ('支', '支'), ('收', '收'), ('改', '改'), ('攻', '攻'), ('放', '放'),
            ('政', '政'), ('故', '故'), ('效', '效'), ('敌', '敵'), ('敏', '敏'), ('救', '救'),
            ('教', '教'), ('敢', '敢'), ('散', '散'), ('敬', '敬'), ('数', '數'), ('整', '整'),
            ('文', '文'), ('斗', '鬥'), ('料', '料'), ('斜', '斜'), ('斩', '斬'), ('断', '斷'),
            ('新', '新'), ('方', '方'), ('施', '施'), ('旁', '旁'), ('旅', '旅'), ('旋', '旋'),
            ('无', '無'), ('既', '既'), ('时', '時'), ('明', '明'), ('易', '易'), ('星', '星'),
            ('春', '春'), ('显', '顯'), ('昼', '晝'), ('晚', '晚'), ('普', '普'), ('景', '景'),
            ('晴', '晴'), ('智', '智'), ('暂', '暫'), ('曲', '曲'), ('更', '更'), ('书', '書'),
            ('会', '會'), ('月', '月'), ('有', '有'), ('服', '服'), ('朗', '朗'), ('望', '望'),
            ('朝', '朝'), ('期', '期'), ('木', '木'), ('未', '未'), ('本', '本'), ('术', '術'),
            ('机', '機'), ('杀', '殺'), ('杂', '雜'), ('权', '權'), ('条', '條'), ('来', '來'),
            ('杨', '楊'), ('极', '極'), ('构', '構'), ('析', '析'), ('林', '林'), ('果', '果'),
            ('枪', '槍'), ('枯', '枯'), ('架', '架'), ('柏', '柏'), ('查', '查'), ('标', '標'),
            ('栈', '棧'), ('栋', '棟'), ('栏', '欄'), ('树', '樹'), ('校', '校'), ('样', '樣'),
            ('核', '核'), ('根', '根'), ('格', '格'), ('栽', '栽'), ('框', '框'), ('案', '案'),
            ('桌', '桌'), ('桥', '橋'), ('梦', '夢'), ('检', '檢'), ('棉', '棉'), ('棋', '棋'),
            ('棒', '棒'), ('棚', '棚'), ('森', '森'), ('植', '植'), ('椭', '橢'), ('楼', '樓'),
            ('概', '概'), ('榜', '榜'), ('模', '模'), ('横', '橫'), ('欢', '歡'), ('欧', '歐'),
            ('欲', '欲'), ('欺', '欺'), ('款', '款'), ('歌', '歌'), ('止', '止'), ('正', '正'),
            ('此', '此'), ('步', '步'), ('武', '武'), ('岁', '歲'), ('历', '歷'), ('归', '歸'),
            ('死', '死'), ('残', '殘'), ('段', '段'), ('杀', '殺'), ('母', '母'), ('每', '每'),
            ('毒', '毒'), ('比', '比'), ('毛', '毛'), ('毫', '毫'), ('民', '民'), ('气', '氣'),
            ('氢', '氫'), ('水', '水'), ('永', '永'), ('求', '求'), ('汉', '漢'), ('汗', '汗'),
            ('江', '江'), ('池', '池'), ('汤', '湯'), ('汪', '汪'), ('汽', '汽'), ('沈', '沈'),
            ('沉', '沉'), ('沙', '沙'), ('没', '沒'), ('河', '河'), ('油', '油'), ('治', '治'),
            ('沿', '沿'), ('泉', '泉'), ('法', '法'), ('波', '波'), ('注', '注'), ('泪', '淚'),
            ('泰', '泰'), ('泳', '泳'), ('泽', '澤'), ('洋', '洋'), ('洗', '洗'), ('洛', '洛'),
            ('洞', '洞'), ('津', '津'), ('洪', '洪'), ('洲', '洲'), ('活', '活'), ('派', '派'),
            ('流', '流'), ('浅', '淺'), ('测', '測'), ('济', '濟'), ('浑', '渾'), ('浓', '濃'),
            ('浙', '浙'), ('浦', '浦'), ('浩', '浩'), ('浪', '浪'), ('浮', '浮'), ('海', '海'),
            ('涂', '塗'), ('消', '消'), ('涉', '涉'), ('涌', '湧'), ('涛', '濤'), ('涨', '漲'),
            ('润', '潤'), ('清', '清'), ('渐', '漸'), ('渡', '渡'), ('港', '港'), ('游', '遊'),
            ('湖', '湖'), ('湾', '灣'), ('源', '源'), ('准', '準'), ('溜', '溜'), ('满', '滿'),
            ('滥', '濫'), ('滨', '濱'), ('滴', '滴'), ('演', '演'), ('漠', '漠'), ('漫', '漫'),
            ('潜', '潛'), ('潮', '潮'), ('激', '激'), ('火', '火'), ('灭', '滅'), ('灯', '燈'),
            ('灰', '灰'), ('灵', '靈'), ('灾', '災'), ('灿', '燦'), ('炉', '爐'), ('炎', '炎'),
            ('炮', '炮'), ('点', '點'), ('炼', '煉'), ('烂', '爛'), ('烈', '烈'), ('烤', '烤'),
            ('烧', '燒'), ('热', '熱'), ('焦', '焦'), ('然', '然'), ('照', '照'), ('煮', '煮'),
            ('熊', '熊'), ('熟', '熟'), ('燃', '燃'), ('爆', '爆'), ('爱', '愛'), ('父', '父'),
            ('爷', '爺'), ('片', '片'), ('版', '版'), ('牌', '牌'), ('牙', '牙'), ('牛', '牛'),
            ('物', '物'), ('特', '特'), ('牵', '牽'), ('牺', '犧'), ('犯', '犯'), ('状', '狀'),
            ('犹', '猶'), ('狂', '狂'), ('独', '獨'), ('狭', '狹'), ('狮', '獅'), ('狱', '獄'),
            ('猎', '獵'), ('猪', '豬'), ('猫', '貓'), ('献', '獻'), ('猴', '猴'), ('玄', '玄'),
            ('率', '率'), ('玉', '玉'), ('王', '王'), ('玩', '玩'), ('环', '環'), ('现', '現'),
            ('球', '球'), ('理', '理'), ('琴', '琴'), ('瑞', '瑞'), ('瓜', '瓜'), ('瓦', '瓦'),
            ('瓶', '瓶'), ('甘', '甘'), ('甜', '甜'), ('生', '生'), ('产', '產'), ('用', '用'),
            ('田', '田'), ('由', '由'), ('甲', '甲'), ('申', '申'), ('电', '電'), ('男', '男'),
            ('界', '界'), ('留', '留'), ('略', '略'), ('番', '番'), ('画', '畫'), ('异', '異'),
            ('当', '當'), ('疏', '疏'), ('疑', '疑'), ('疗', '療'), ('疯', '瘋'), ('疲', '疲'),
            ('疾', '疾'), ('病', '病'), ('痕', '痕'), ('痛', '痛'), ('痴', '痴'), ('瘦', '瘦'),
            ('登', '登'), ('白', '白'), ('百', '百'), ('的', '的'), ('皇', '皇'), ('皮', '皮'),
            ('益', '益'), ('监', '監'), ('盖', '蓋'), ('盘', '盤'), ('盛', '盛'), ('目', '目'),
            ('直', '直'), ('相', '相'), ('省', '省'), ('看', '看'), ('真', '真'), ('眠', '眠'),
            ('眼', '眼'), ('着', '著'), ('睁', '睜'), ('睡', '睡'), ('督', '督'), ('短', '短'),
            ('石', '石'), ('矿', '礦'), ('码', '碼'), ('破', '破'), ('研', '研'), ('硬', '硬'),
            ('确', '確'), ('磁', '磁'), ('磨', '磨'), ('示', '示'), ('礼', '禮'), ('社', '社'),
            ('祖', '祖'), ('祝', '祝'), ('神', '神'), ('票', '票'), ('禁', '禁'), ('福', '福'),
            ('离', '離'), ('秀', '秀'), ('私', '私'), ('秋', '秋'), ('种', '種'), ('科', '科'),
            ('秒', '秒'), ('秘', '秘'), ('租', '租'), ('称', '稱'), ('移', '移'), ('程', '程'),
            ('税', '稅'), ('稳', '穩'), ('究', '究'), ('穷', '窮'), ('空', '空'), ('穿', '穿'),
            ('突', '突'), ('窗', '窗'), ('立', '立'), ('站', '站'), ('竟', '竟'), ('章', '章'),
            ('童', '童'), ('端', '端'), ('竹', '竹'), ('笑', '笑'), ('笔', '筆'), ('符', '符'),
            ('第', '第'), ('等', '等'), ('简', '簡'), ('算', '算'), ('管', '管'), ('箱', '箱'),
            ('篇', '篇'), ('篮', '籃'), ('米', '米'), ('类', '類'), ('粉', '粉'), ('精', '精'),
            ('系', '系'), ('素', '素'), ('紧', '緊'), ('紫', '紫'), ('累', '累'), ('繁', '繁'),
            ('红', '紅'), ('约', '約'), ('级', '級'), ('纪', '紀'), ('纯', '純'), ('纲', '綱'),
            ('纳', '納'), ('纵', '縱'), ('纷', '紛'), ('纸', '紙'), ('纽', '紐'), ('线', '線'),
            ('练', '練'), ('组', '組'), ('细', '細'), ('织', '織'), ('终', '終'), ('经', '經'),
            ('结', '結'), ('绕', '繞'), ('绘', '繪'), ('给', '給'), ('绝', '絕'), ('统', '統'),
            ('继', '繼'), ('绩', '績'), ('绪', '緒'), ('续', '續'), ('维', '維'), ('综', '綜'),
            ('绿', '綠'), ('缓', '緩'), ('编', '編'), ('缘', '緣'), ('缝', '縫'), ('缩', '縮'),
            ('缺', '缺'), ('网', '網'), ('罗', '羅'), ('置', '置'), ('罪', '罪'), ('罚', '罰'),
            ('署', '署'), ('美', '美'), ('群', '群'), ('义', '義'), ('羽', '羽'), ('习', '習'),
            ('老', '老'), ('考', '考'), ('者', '者'), ('而', '而'), ('耐', '耐'), ('耳', '耳'),
            ('联', '聯'), ('聪', '聰'), ('声', '聲'), ('职', '職'), ('肉', '肉'), ('肩', '肩'),
            ('肯', '肯'), ('育', '育'), ('背', '背'), ('胎', '胎'), ('胜', '勝'), ('胞', '胞'),
            ('胡', '胡'), ('能', '能'), ('脉', '脈'), ('脱', '脫'), ('脑', '腦'), ('脸', '臉'),
            ('临', '臨'), ('自', '自'), ('至', '至'), ('致', '致'), ('与', '與'), ('兴', '興'),
            ('举', '舉'), ('旧', '舊'), ('舍', '舍'), ('舞', '舞'), ('航', '航'), ('舰', '艦'),
            ('良', '良'), ('色', '色'), ('节', '節'), ('花', '花'), ('芳', '芳'), ('英', '英'),
            ('茶', '茶'), ('草', '草'), ('药', '藥'), ('获', '獲'), ('菜', '菜'), ('营', '營'),
            ('落', '落'), ('叶', '葉'), ('著', '著'), ('蒸', '蒸'), ('蓝', '藍'), ('藏', '藏'),
            ('虎', '虎'), ('处', '處'), ('虚', '虛'), ('号', '號'), ('虫', '蟲'), ('蛋', '蛋'),
            ('蛙', '蛙'), ('融', '融'), ('血', '血'), ('行', '行'), ('街', '街'), ('冲', '衝'),
            ('衣', '衣'), ('表', '表'), ('补', '補'), ('装', '裝'), ('西', '西'), ('要', '要'),
            ('覆', '覆'), ('见', '見'), ('观', '觀'), ('规', '規'), ('视', '視'), ('觉', '覺'),
            ('角', '角'), ('解', '解'), ('触', '觸'), ('言', '言'), ('计', '計'), ('认', '認'),
            ('讨', '討'), ('让', '讓'), ('训', '訓'), ('议', '議'), ('记', '記'), ('讲', '講'),
            ('许', '許'), ('论', '論'), ('设', '設'), ('访', '訪'), ('证', '證'), ('评', '評'),
            ('识', '識'), ('诉', '訴'), ('词', '詞'), ('译', '譯'), ('试', '試'), ('诗', '詩'),
            ('话', '話'), ('该', '該'), ('详', '詳'), ('语', '語'), ('误', '誤'), ('说', '說'),
            ('请', '請'), ('诸', '諸'), ('读', '讀'), ('课', '課'), ('谁', '誰'), ('调', '調'),
            ('谈', '談'), ('谋', '謀'), ('谓', '謂'), ('谢', '謝'), ('谷', '谷'), ('豆', '豆'),
            ('象', '象'), ('豪', '豪'), ('贝', '貝'), ('负', '負'), ('贡', '貢'), ('财', '財'),
            ('责', '責'), ('贤', '賢'), ('败', '敗'), ('货', '貨'), ('质', '質'), ('购', '購'),
            ('贴', '貼'), ('贵', '貴'), ('贷', '貸'), ('费', '費'), ('贺', '賀'), ('资', '資'),
            ('赏', '賞'), ('赔', '賠'), ('赖', '賴'), ('赚', '賺'), ('赛', '賽'), ('赞', '贊'),
            ('走', '走'), ('赴', '赴'), ('起', '起'), ('超', '超'), ('越', '越'), ('趋', '趨'),
            ('足', '足'), ('跳', '跳'), ('路', '路'), ('身', '身'), ('车', '車'), ('轨', '軌'),
            ('军', '軍'), ('软', '軟'), ('较', '較'), ('载', '載'), ('轻', '輕'), ('辉', '輝'),
            ('辈', '輩'), ('输', '輸'), ('转', '轉'), ('轮', '輪'), ('辞', '辭'), ('辩', '辯'),
            ('农', '農'), ('边', '邊'), ('达', '達'), ('过', '過'), ('迈', '邁'), ('运', '運'),
            ('近', '近'), ('返', '返'), ('还', '還'), ('这', '這'), ('进', '進'), ('远', '遠'),
            ('违', '違'), ('连', '連'), ('迟', '遲'), ('追', '追'), ('退', '退'), ('送', '送'),
            ('适', '適'), ('逃', '逃'), ('选', '選'), ('透', '透'), ('逐', '逐'), ('递', '遞'),
            ('途', '途'), ('通', '通'), ('速', '速'), ('造', '造'), ('逢', '逢'), ('遇', '遇'),
            ('遍', '遍'), ('道', '道'), ('遗', '遺'), ('遥', '遙'), ('避', '避'), ('邦', '邦'),
            ('那', '那'), ('邮', '郵'), ('邻', '鄰'), ('部', '部'), ('都', '都'), ('乡', '鄉'),
            ('配', '配'), ('酒', '酒'), ('酸', '酸'), ('醒', '醒'), ('采', '采'), ('释', '釋'),
            ('里', '裡'), ('重', '重'), ('野', '野'), ('量', '量'), ('金', '金'), ('针', '針'),
            ('钟', '鐘'), ('钢', '鋼'), ('钱', '錢'), ('铁', '鐵'), ('银', '銀'), ('锁', '鎖'),
            ('错', '錯'), ('镜', '鏡'), ('长', '長'), ('门', '門'), ('闭', '閉'), ('问', '問'),
            ('闲', '閒'), ('间', '間'), ('闻', '聞'), ('阅', '閱'), ('防', '防'), ('阳', '陽'),
            ('阴', '陰'), ('阵', '陣'), ('阶', '階'), ('阻', '阻'), ('阿', '阿'), ('附', '附'),
            ('际', '際'), ('陆', '陸'), ('陈', '陳'), ('降', '降'), ('限', '限'), ('院', '院'),
            ('除', '除'), ('险', '險'), ('陪', '陪'), ('随', '隨'), ('隐', '隱'), ('隔', '隔'),
            ('难', '難'), ('雄', '雄'), ('集', '集'), ('雨', '雨'), ('雪', '雪'), ('零', '零'),
            ('雷', '雷'), ('雾', '霧'), ('需', '需'), ('震', '震'), ('青', '青'), ('静', '靜'),
            ('非', '非'), ('面', '面'), ('革', '革'), ('音', '音'), ('页', '頁'), ('顶', '頂'),
            ('项', '項'), ('顺', '順'), ('须', '須'), ('顾', '顧'), ('预', '預'), ('领', '領'),
            ('颇', '頗'), ('频', '頻'), ('颗', '顆'), ('题', '題'), ('颜', '顏'), ('愿', '願'),
            ('类', '類'), ('风', '風'), ('飞', '飛'), ('食', '食'), ('饭', '飯'), ('饮', '飲'),
            ('饰', '飾'), ('饱', '飽'), ('馆', '館'), ('首', '首'), ('香', '香'), ('马', '馬'),
            ('驾', '駕'), ('验', '驗'), ('骑', '騎'), ('骗', '騙'), ('高', '高'), ('鬼', '鬼'),
            ('魂', '魂'), ('鱼', '魚'), ('鲁', '魯'), ('鲜', '鮮'), ('鸟', '鳥'), ('鸡', '雞'),
            ('鸣', '鳴'), ('鸭', '鴨'), ('鸿', '鴻'), ('鹅', '鵝'), ('麦', '麥'), ('麻', '麻'),
            ('黄', '黃'), ('黑', '黑'), ('默', '默'), ('齐', '齊'), ('龙', '龍'), ('龟', '龜'),
        ];
        PAIRS.iter().copied().collect()
    })
}

#[cfg(test)]
mod device_tests {
    use super::*;

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
            let erased = corpus.spaced_pinyin_to_words[reading][15].0.clone();
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
            let weights: HashMap<_, _> = corpus.spaced_pinyin_to_words[reading]
                .iter()
                .map(|(word, weight)| (word.as_str(), *weight))
                .collect();
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
                .word_readings
                .iter()
                .filter(|(word, _)| (2..=6).contains(&word.chars().count()))
                .filter_map(|(word, (_, weight))| {
                    word.strip_prefix(prefix)
                        .filter(|suffix| !suffix.is_empty())
                        .map(|suffix| (suffix.to_string(), *weight))
                })
                .collect();
            expected.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            expected.truncate(if prefix.chars().count() == 1 { 24 } else { 10 });
            assert_eq!(
                corpus
                    .continuations
                    .get(prefix)
                    .cloned()
                    .unwrap_or_default(),
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
    fn personal_candidates_keep_priority_over_sentence_diversity() {
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
        assert_eq!(candidates[0].canonical, "握");
        assert_eq!(candidates[1].canonical, "我要");
    }
}
