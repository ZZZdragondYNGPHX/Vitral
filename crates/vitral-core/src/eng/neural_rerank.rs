use crate::hw::onnx_models::MaskedScores;
use crate::ret::is_cjk;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[cfg(feature = "native-onnx")]
use crate::{hw::onnx_models::HwOnnxBundle, models::ModelPaths};

const AMBIGUITY_MARGIN: f64 = 4.5;
const NEURAL_ALPHA: f64 = 0.85;
const MAX_POOL: usize = 4;
const MAX_CONTEXT: usize = 12;
const CACHE_CAPACITY: usize = 128;

pub(super) struct Candidate<'a> {
    pub text: &'a str,
    pub base_score: f64,
}

#[derive(Clone, PartialEq, Eq)]
struct MaskedSentence {
    context_tail: Vec<char>,
    sentence: Vec<Option<char>>,
    diff_positions: Vec<usize>,
}

struct CacheEntry {
    masked: MaskedSentence,
    chars: Vec<char>,
    score: f64,
}

#[derive(Default)]
pub(super) struct Cache {
    reranker_dir: Option<PathBuf>,
    generation: u64,
    entries: VecDeque<CacheEntry>,
}

pub(super) struct Pool {
    masked: MaskedSentence,
    chars: Vec<Vec<char>>,
    base_scores: Vec<f64>,
}

pub(super) struct MissingScores(Vec<Vec<char>>);

#[cfg(feature = "native-onnx")]
pub(crate) struct Pending {
    pub(super) model_paths: ModelPaths,
    pub(super) cache_generation: u64,
    pub(super) pool: Pool,
    pub(super) missing: MissingScores,
}

#[cfg(feature = "native-onnx")]
impl Pending {
    pub(crate) fn score(&self) -> Option<Vec<f64>> {
        let bundle = HwOnnxBundle::try_cached_paths(&self.model_paths)?;
        self.missing
            .score(&self.pool.masked, |input, positions, chars| {
                score_with_bundle(&bundle, input, positions, chars)
            })
    }
}

impl Pool {
    /// The first ineligible candidate ends the pool. Positions are display indices.
    pub(super) fn new<'a>(
        candidates: impl IntoIterator<Item = Option<Candidate<'a>>>,
        left_context: &[char],
    ) -> Option<Self> {
        let mut candidates = candidates.into_iter().take(MAX_POOL);
        let top = candidates.next()??;
        let top_chars: Vec<_> = top.text.chars().collect();
        if top_chars.len() < 2 || !top_chars.iter().copied().all(is_cjk) {
            return None;
        }
        let mut base_scores = vec![top.base_score];
        let mut words = vec![top_chars];
        for candidate in candidates {
            let Some(entry) = candidate else { break };
            if (top.base_score - entry.base_score).abs() > AMBIGUITY_MARGIN {
                break;
            }
            let chars: Vec<_> = entry.text.chars().collect();
            if chars.len() != words[0].len() || !chars.iter().copied().all(is_cjk) {
                break;
            }
            base_scores.push(entry.base_score);
            words.push(chars);
        }
        if words.len() < 2 {
            return None;
        }

        let top_chars = &words[0];
        let diff_positions: Vec<_> = (0..top_chars.len())
            .filter(|&pos| words[1..].iter().any(|chars| chars[pos] != top_chars[pos]))
            .take(3)
            .collect();
        if !(1..=2).contains(&diff_positions.len()) {
            return None;
        }
        let mut context_tail: Vec<_> = left_context
            .iter()
            .rev()
            .copied()
            .filter(|ch| !ch.is_whitespace())
            .take(MAX_CONTEXT)
            .collect();
        context_tail.reverse();
        let sentence = top_chars
            .iter()
            .copied()
            .enumerate()
            .map(|(pos, ch)| (!diff_positions.contains(&pos)).then_some(ch))
            .collect();
        let chars = words
            .iter()
            .map(|word| diff_positions.iter().map(|&pos| word[pos]).collect())
            .collect();
        Some(Self {
            masked: MaskedSentence {
                context_tail,
                sentence,
                diff_positions,
            },
            chars,
            base_scores,
        })
    }

    pub(super) fn apply_cached(
        &self,
        cache: &Cache,
        mut swap: impl FnMut(usize, usize),
    ) -> Result<bool, MissingScores> {
        let mut scores = Vec::with_capacity(self.chars.len());
        let mut missing = Vec::new();
        for chars in &self.chars {
            match cache
                .entries
                .iter()
                .find(|entry| entry.masked == self.masked && entry.chars == *chars)
            {
                Some(entry) => scores.push(entry.score),
                None => missing.push(chars.clone()),
            }
        }
        if !missing.is_empty() {
            return Err(MissingScores(missing));
        }

        let top_mlm = scores[0];
        let mut order: Vec<_> = self
            .base_scores
            .iter()
            .zip(scores)
            .enumerate()
            .map(|(index, (base_score, score))| {
                let mlm_delta = (score - top_mlm).clamp(-6.0, 6.0);
                (index, base_score + NEURAL_ALPHA * mlm_delta)
            })
            .collect();
        order.sort_by(|(_, a), (_, b)| b.total_cmp(a));

        let mut current: [_; MAX_POOL] = std::array::from_fn(|index| index);
        let mut changed = false;
        for (dest, (wanted, _)) in order.into_iter().enumerate() {
            if current[dest] != wanted {
                let source = current.iter().position(|&index| index == wanted).unwrap();
                swap(dest, source);
                current.swap(dest, source);
                changed = true;
            }
        }
        Ok(changed)
    }
}

impl Cache {
    pub(super) fn bind(&mut self, reranker_dir: Option<&Path>) -> u64 {
        if self.reranker_dir.as_deref() != reranker_dir {
            self.invalidate();
            self.reranker_dir = reranker_dir.map(Path::to_path_buf);
        }
        self.generation
    }

    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn store(&mut self, pool: &Pool, missing: MissingScores, scores: Vec<f64>) {
        for (chars, score) in missing.0.into_iter().zip(scores) {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.masked == pool.masked && entry.chars == chars)
            {
                entry.score = score;
                continue;
            }
            if self.entries.len() == CACHE_CAPACITY {
                // Keep any hits from this pool available for the immediate reorder.
                let oldest = self
                    .entries
                    .iter()
                    .position(|entry| {
                        entry.masked != pool.masked || !pool.chars.contains(&entry.chars)
                    })
                    .unwrap();
                self.entries.remove(oldest);
            }
            self.entries.push_back(CacheEntry {
                masked: pool.masked.clone(),
                chars,
                score,
            });
        }
    }
}

impl MissingScores {
    fn score(
        &self,
        masked: &MaskedSentence,
        score_batch: impl FnOnce(&[Option<char>], &[usize], &[Vec<char>]) -> Option<MaskedScores>,
    ) -> Option<Vec<f64>> {
        let input: Vec<_> = masked
            .context_tail
            .iter()
            .copied()
            .map(Some)
            .chain(masked.sentence.iter().copied())
            .collect();
        let positions: Vec<_> = masked
            .diff_positions
            .iter()
            .map(|pos| 1 + masked.context_tail.len() + pos)
            .collect();
        let chars_per_slot: Vec<_> = (0..positions.len())
            .map(|slot| self.0.iter().map(|chars| chars[slot]).collect())
            .collect();
        let batch = score_batch(&input, &positions, &chars_per_slot)?;
        if batch.len() != positions.len() || batch.iter().any(|row| row.len() != self.0.len()) {
            return None;
        }
        (0..self.0.len())
            .map(|column| {
                batch
                    .iter()
                    .map(|row| row[column].filter(|score| score.is_finite()).map(f64::from))
                    .sum()
            })
            .collect()
    }
}

#[cfg(test)]
pub(super) fn rerank<T>(
    candidates: &mut [T],
    left_context: &[char],
    cache: &mut Cache,
    candidate: impl Fn(&T) -> Option<Candidate<'_>>,
    score_batch: impl FnOnce(&[Option<char>], &[usize], &[Vec<char>]) -> Option<MaskedScores>,
) -> bool {
    Pool::new(candidates.iter().map(candidate), left_context)
        .is_some_and(|pool| pool.rerank(candidates, cache, score_batch))
}

#[cfg(test)]
impl Pool {
    fn rerank<T>(
        &self,
        candidates: &mut [T],
        cache: &mut Cache,
        score_batch: impl FnOnce(&[Option<char>], &[usize], &[Vec<char>]) -> Option<MaskedScores>,
    ) -> bool {
        match self.apply_cached(cache, |a, b| candidates.swap(a, b)) {
            Ok(changed) => changed,
            Err(missing) => {
                let Some(scores) = missing.score(&self.masked, score_batch) else {
                    return false;
                };
                cache.store(self, missing, scores);
                self.apply_cached(cache, |a, b| candidates.swap(a, b))
                    .unwrap_or_default()
            }
        }
    }
}

#[cfg(feature = "native-onnx")]
fn score_with_bundle(
    bundle: &HwOnnxBundle,
    input: &[Option<char>],
    positions: &[usize],
    chars_per_slot: &[Vec<char>],
) -> Option<MaskedScores> {
    let tokenizer = bundle.tokenizer.as_ref()?;
    let tokens: Vec<_> = std::iter::once(tokenizer.cls_id)
        .chain(input.iter().map(|slot| match slot {
            Some(ch) => tokenizer.char_token_id(*ch).unwrap_or(tokenizer.unk_id),
            None => tokenizer.mask_id,
        }))
        .chain(std::iter::once(tokenizer.sep_id))
        .collect();
    bundle.try_score_rbt3_masked_batch_if_warm(
        &vec![tokens; positions.len()],
        positions,
        chars_per_slot,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eng::{CandidateItem, CandidateOrigin, EngineConfig, EngineState, SchemaKind};
    use crate::ret::RetentionEngine;

    #[derive(Clone, Copy)]
    struct Entry<'a> {
        text: &'a str,
        base_score: f64,
        pinned: bool,
        personal: bool,
    }

    fn entry(text: &str, base_score: f64) -> Entry<'_> {
        Entry {
            text,
            base_score,
            pinned: false,
            personal: false,
        }
    }

    fn candidate<'a, 'b>(entry: &'a Entry<'b>) -> Option<Candidate<'a>> {
        (!entry.pinned && !entry.personal).then_some(Candidate {
            text: entry.text,
            base_score: entry.base_score,
        })
    }

    fn pair() -> [Entry<'static>; 2] {
        [entry("他跑的很快", 10.0), entry("他跑得很快", 9.0)]
    }

    fn favors_second() -> Option<MaskedScores> {
        Some(vec![vec![Some(-4.0), Some(-1.0)]])
    }

    #[test]
    fn one_slot_uses_right_context_and_reuses_scores_after_reordering() {
        let mut entries = pair();
        let mut cache = Cache::default();
        assert!(rerank(
            &mut entries,
            &[],
            &mut cache,
            candidate,
            |input, positions, chars| {
                assert_eq!(
                    input,
                    [Some('他'), Some('跑'), None, Some('很'), Some('快')]
                );
                assert_eq!(positions, [3]);
                assert_eq!(chars, [vec!['的', '得']]);
                favors_second()
            }
        ));
        assert_eq!(entries[0].text, "他跑得很快");
        assert!(!rerank(
            &mut entries,
            &[],
            &mut cache,
            candidate,
            |_, _, _| { panic!("cached scores must follow the candidates") }
        ));
        assert_eq!(entries[0].text, "他跑得很快");
    }

    #[test]
    fn two_slots_are_both_masked_with_the_nonblank_context_tail() {
        let mut entries = [entry("他做玩了", 10.0), entry("他作完了", 9.0)];
        let context: Vec<_> = "头一二三四五六七八九 十上下\t".chars().collect();
        assert!(rerank(
            &mut entries,
            &context,
            &mut Cache::default(),
            candidate,
            |input, positions, chars| {
                assert!(input[..12]
                    .iter()
                    .copied()
                    .flatten()
                    .eq("一二三四五六七八九十上下".chars()));
                assert_eq!(input[12..], [Some('他'), None, None, Some('了')]);
                assert_eq!(positions, [14, 15]);
                assert_eq!(chars, [vec!['做', '作'], vec!['玩', '完']]);
                Some(vec![
                    vec![Some(-1.0), Some(-3.0)],
                    vec![Some(-6.0), Some(-1.0)],
                ])
            }
        ));
        assert_eq!(entries[0].text, "他作完了");
    }

    #[test]
    fn guards_skip_confident_short_or_locked_candidates_and_large_divergence() {
        let [first, second] = pair();
        for mut entries in [
            [
                first,
                entry(second.text, first.base_score - AMBIGUITY_MARGIN - 0.01),
            ],
            [
                Entry {
                    pinned: true,
                    ..first
                },
                second,
            ],
            [
                first,
                Entry {
                    personal: true,
                    ..second
                },
            ],
            [first, entry("他跑得快", 9.0)],
            [first, entry("你走得很快", 9.0)],
            [entry("的", 10.0), entry("得", 9.0)],
            [entry("用AI", 10.0), entry("用爱", 9.0)],
        ] {
            assert!(!rerank(
                &mut entries,
                &[],
                &mut Cache::default(),
                candidate,
                |_, _, _| { panic!("the candidate pool must be rejected before scoring") }
            ));
        }
    }

    #[test]
    fn unknown_character_aborts_without_caching_any_scores() {
        let mut entries = [entry("我在家", 10.0), entry("我𰻞家", 9.0)];
        let mut cache = Cache::default();
        for _ in 0..2 {
            assert!(!rerank(
                &mut entries,
                &[],
                &mut cache,
                candidate,
                |_, _, chars| {
                    assert_eq!(chars, [vec!['在', '𰻞']]);
                    Some(vec![vec![Some(-1.0), None]])
                }
            ));
            assert_eq!(entries[0].text, "我在家");
            assert!(cache.entries.is_empty());
        }
    }

    #[test]
    fn cache_holds_128_candidate_scores_and_evicts_oldest() {
        let mut cache = Cache::default();
        let check = |cache: &mut Cache, ch, expected_calls| {
            let mut calls = 0;
            assert!(rerank(&mut pair(), &[ch], cache, candidate, |_, _, _| {
                calls += 1;
                favors_second()
            }));
            assert_eq!(calls, expected_calls);
        };
        for ch in '一'..='丿' {
            check(&mut cache, ch, 1);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        check(&mut cache, '一', 0);
        check(&mut cache, '乀', 1);
        check(&mut cache, '丿', 0);
        check(&mut cache, '一', 1);
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
    }

    fn test_engine() -> EngineState {
        EngineState::new(EngineConfig {
            assets_dir: "/nonexistent/vitral-test-assets".into(),
            schema: SchemaKind::RimeIce,
            ..Default::default()
        })
    }

    fn portable(
        text: &str,
        comment: &str,
        consumed_len: usize,
        rank_score: f64,
    ) -> (CandidateItem, CandidateOrigin) {
        let decoded = crate::eng::pure_rust::DecodedCandidate::language(
            text,
            text.into(),
            comment,
            consumed_len,
            None,
        )
        .unwrap();
        let (item, mut origin) = decoded.into_portable_pair();
        if let CandidateOrigin::PureRust {
            rank_score: score, ..
        } = &mut origin
        {
            *score = rank_score;
        }
        (item, origin)
    }

    #[test]
    fn final_display_language_and_corrections_are_boundaries() {
        let engine = test_engine();
        let code = "tapaodehenkuai";
        let reading = "ta pao de hen kuai";
        for comment in ["英文", "中英", "纠错"] {
            let entries = [
                portable("他跑的很快", reading, code.len(), 10.0),
                portable("他跑得很快", comment, code.len(), 9.5),
                portable("他跑德很快", reading, code.len(), 9.0),
            ];
            assert!(engine
                .neural_rerank_pool(&entries, code, &RetentionEngine::default(), 0)
                .is_none());
        }
    }

    #[test]
    fn final_display_partial_prefix_ends_the_pool() {
        let engine = test_engine();
        let code = "tapaodehenkuai";
        let reading = "ta pao de hen kuai";
        let mut entries = [
            portable("他跑的很快", reading, code.len(), 10.0),
            portable("他跑得很快", reading, code.len(), 9.0),
            portable("他跑", "ta pao", 5, 0.0),
            portable("他跑德很快", reading, code.len(), 9.5),
        ];
        let pool = engine
            .neural_rerank_pool(&entries, code, &RetentionEngine::default(), 0)
            .unwrap();
        assert!(
            pool.rerank(&mut entries, &mut Cache::default(), |_, _, chars| {
                assert_eq!(chars, [vec!['的', '得']]);
                favors_second()
            })
        );
        assert_eq!(
            entries.map(|(item, _)| item.text),
            ["他跑得很快", "他跑的很快", "他跑", "他跑德很快"],
        );
    }

    #[test]
    fn cache_is_invalidated_when_the_engine_changes_reranker_directory() {
        let mut engine = test_engine();
        let mut cache = Cache::default();
        engine.model_paths.reranker = Some("first-model".into());
        let generation = cache.bind(engine.model_paths.reranker.as_deref());
        assert!(rerank(
            &mut pair(),
            &[],
            &mut cache,
            candidate,
            |_, _, _| favors_second()
        ));
        assert_eq!(
            cache.bind(engine.model_paths.reranker.as_deref()),
            generation
        );
        assert!(rerank(
            &mut pair(),
            &[],
            &mut cache,
            candidate,
            |_, _, _| { panic!("the same model should reuse cached scores") }
        ));

        engine.model_paths.reranker = Some("second-model".into());
        assert_ne!(
            cache.bind(engine.model_paths.reranker.as_deref()),
            generation
        );
        let mut entries = pair();
        assert!(!rerank(
            &mut entries,
            &[],
            &mut cache,
            candidate,
            |_, _, chars| {
                assert_eq!(chars, [vec!['的', '得']]);
                Some(vec![vec![Some(-1.0), Some(-4.0)]])
            }
        ));
        assert_eq!(entries[0].text, "他跑的很快");
        cache.bind(None);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn expired_left_context_preserves_sentence_internal_scoring() {
        let mut engine = test_engine();
        engine.extend_commit_context("上一个词");
        engine.last_committed_word = Some("词".into());
        engine.last_committed_ms = 1000;
        let code = "tapaodehenkuai";
        let reading = "ta pao de hen kuai";
        let entries = [
            portable("他跑的很快", reading, code.len(), 10.0),
            portable("他跑得很快", reading, code.len(), 9.0),
        ];
        let retention = RetentionEngine::default();
        let mut cache = Cache::default();
        for (now_ms, expected) in [(7000, "他跑的很快"), (7001, "他跑得很快")] {
            let mut entries = entries.clone();
            let pool = engine
                .neural_rerank_pool(&entries, code, &retention, now_ms)
                .unwrap();
            pool.rerank(&mut entries, &mut cache, |input, positions, _| {
                let sentence = [Some('他'), Some('跑'), None, Some('很'), Some('快')];
                if now_ms == 7000 {
                    assert_eq!(input[..4], [Some('上'), Some('一'), Some('个'), Some('词')]);
                    assert_eq!(input[4..], sentence);
                    assert_eq!(positions, [7]);
                    Some(vec![vec![Some(-1.0), Some(-4.0)]])
                } else {
                    assert_eq!(input, sentence);
                    assert_eq!(positions, [3]);
                    favors_second()
                }
            });
            assert_eq!(entries[0].0.text, expected);
        }
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn late_result_only_updates_cache_and_old_model_results_are_discarded() {
        let mut engine = test_engine();
        let code = "tapaodehenkuai";
        let reading = "ta pao de hen kuai";
        let retention = RetentionEngine::default();
        let queue = |engine: &mut EngineState, dir: &str| {
            engine.model_paths.reranker = Some(dir.into());
            let entries = vec![
                portable("他跑的很快", reading, code.len(), 10.0),
                portable("他跑得很快", reading, code.len(), 9.0),
            ];
            let pool = engine
                .neural_rerank_pool(&entries, code, &retention, 0)
                .unwrap();
            let cache_generation = engine
                .neural_rerank_cache
                .bind(engine.model_paths.reranker.as_deref());
            let missing = pool
                .apply_cached(&engine.neural_rerank_cache, |_, _| unreachable!())
                .err()
                .unwrap();
            engine.set_current_candidates(entries);
            Pending {
                model_paths: engine.model_paths.clone(),
                cache_generation,
                pool,
                missing,
            }
        };

        let task = queue(&mut engine, "first-model");
        engine.clear_composition_state();
        engine.apply_neural_rerank(task, vec![-4.0, -1.0]);
        assert!(engine.current_candidates.is_empty());
        assert_eq!(engine.neural_rerank_cache.entries.len(), 2);

        let task = queue(&mut engine, "second-model");
        engine.model_paths.reranker = Some("third-model".into());
        engine.apply_neural_rerank(task, vec![-4.0, -1.0]);
        assert_eq!(engine.current_candidates[0].text, "他跑的很快");
        assert!(engine.neural_rerank_cache.entries.is_empty());
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn real_rbt3_reranks_ambiguous_sentences_only_when_warm() {
        use std::{path::Path, time::Instant};
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        if !assets.join("reranker/model_int8.onnx").is_file() {
            return;
        }
        let paths = crate::models::ModelPaths {
            reranker: Some(assets.join("reranker/../reranker")),
            ..Default::default()
        };
        let bundle = HwOnnxBundle::load_paths(&paths);
        let score = |input: &[Option<char>], positions: &[usize], chars: &[Vec<char>]| {
            score_with_bundle(&bundle, input, positions, chars)
        };
        let mut entries = [entry("我再家", 10.0), entry("我在家", 9.0)];
        let mut cache = Cache::default();
        let start = Instant::now();
        assert!(!rerank(&mut entries, &[], &mut cache, candidate, score));
        eprintln!("rbt3 cold skip: {:.3?}", start.elapsed());
        assert_eq!(entries[0].text, "我再家");
        assert!(cache.entries.is_empty());

        assert!(bundle.has_reranker_onnx());
        for (wrong, right) in [
            ("我再家", "我在家"),
            ("明天带再家里", "明天待在家里"),
            ("他的工作座完了", "他的工作做完了"),
        ] {
            let mut entries = [entry(wrong, 10.0), entry(right, 9.0)];
            let start = Instant::now();
            assert!(
                rerank(&mut entries, &[], &mut cache, candidate, score),
                "{wrong} → {right}"
            );
            eprintln!(
                "rbt3 warm rescore {wrong} → {right}: {:.3?}",
                start.elapsed()
            );
            assert_eq!(entries[0].text, right);
        }
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn vitral_core_refreshes_from_warm_rbt3_and_skips_cold_or_busy_sessions() {
        use crate::{SwipeDir, VitralCore};
        use std::time::{Duration, Instant};

        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        if !assets.join("reranker/model_int8.onnx").is_file()
            || !assets.join("rime/cn_dicts/base.dict.yaml").is_file()
            || !assets.join("rime/wanxiang-lts-zh-hans.gram").is_file()
        {
            return;
        }

        let config = EngineConfig {
            assets_dir: assets.to_string_lossy().into_owned(),
            schema: SchemaKind::RimeIce,
            syllable_split: true,
            adaptive_learning: false,
            pure_offline: true,
            ..Default::default()
        };
        let core = VitralCore::init(&serde_json::to_string(&config).unwrap()).unwrap();
        // Isolate this test's lazy session from other consumers of the bundled model.
        let paths = ModelPaths {
            reranker: Some(assets.join("reranker/../reranker/../reranker")),
            ..Default::default()
        };
        let bundle = HwOnnxBundle::load_paths(&paths);
        let prepare = || {
            let mut state = core.input.lock();
            state.retention = RetentionEngine::default();
            let engine = &mut state.engine;
            engine.clear_composition_state();
            engine.model_paths = paths.clone();
            engine.context_window.clear();
            engine.extend_commit_context("这件事让我很");
            engine.last_committed_word = Some("很".into());
            engine.last_committed_ms = RetentionEngine::now_epoch_ms();
            engine.set_raw_input("fanga".into());
            engine.raw_cursor = engine.raw_input.len();
        };
        let key = |key: char| core.process_key(key as u32, 0, SwipeDir::None).unwrap();
        let wait_for_worker = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if !core.input.lock().neural_rerank_worker_running {
                    break;
                }
                assert!(Instant::now() < deadline, "the rerank worker must finish");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let assert_base_unchanged = || {
            let state = core.input.lock();
            assert_eq!(state.engine.current_candidates[0].text, "方案");
            assert!(state.engine.neural_rerank_cache.entries.is_empty());
        };

        prepare();
        assert_eq!(key('n').candidates[0].text, "方案");
        wait_for_worker();
        assert_base_unchanged();
        assert!(bundle.has_reranker_onnx());

        prepare();
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            bundle.with_locked_reranker_for_test(|| {
                let key = &key;
                scope.spawn(move || tx.send(key('n')).unwrap());
                let action = rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the keystroke must finish while the model is locked");
                assert_eq!(action.candidates[0].text, "方案");
                wait_for_worker();
                assert_base_unchanged();
            });
        });

        prepare();
        let first = key('n');
        assert_eq!(first.candidates[0].text, "方案");
        assert_eq!(first.preedit.text, "fangan");
        wait_for_worker();
        {
            // Background completion populates the cache without mutating the already-displayed page.
            let state = core.input.lock();
            let engine = &state.engine;
            assert_eq!(engine.current_candidates[0].text, "方案");
            assert_eq!(engine.canonical_at(0), Some("方案"));
            assert!(!engine.neural_rerank_cache.entries.is_empty());
            assert_eq!(engine.raw_input, "fangan");
            assert_eq!(engine.composing, "fangan");
        }
        // Selecting slot 0 (or pressing Space) commits what is displayed on screen ("方案").
        assert_eq!(core.select_candidate(0).unwrap().commit_text.as_deref(), Some("方案"));

        // Clear learned preference so the next candidate generation exercises the populated neural cache.
        core.input.lock().retention.clear_learned_habits();
        prepare();
        bundle.with_locked_reranker_for_test(|| {
            let cached = key('n');
            assert_eq!(cached.candidates[0].text, "反感");
            assert!(!core.input.lock().neural_rerank_worker_running);
        });
        assert_eq!(key(' ').commit_text.as_deref(), Some("反感"));

        prepare();
        core.input.lock().engine.model_paths.reranker =
            Some(assets.join("reranker/../reranker/../reranker/../reranker"));
        assert_eq!(key('n').candidates[0].text, "方案");
        wait_for_worker();
        assert_base_unchanged();
    }
}
