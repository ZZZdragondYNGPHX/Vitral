use parking_lot::Mutex;
#[cfg(feature = "native-onnx")]
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[cfg(feature = "native-onnx")]
use crate::onnx_env::get_ort_env;
#[cfg(feature = "native-onnx")]
use ort::{session::Session, value::TensorRef};

#[derive(Debug, Clone)]
pub struct BertTokenizer {
    vocab: HashMap<String, i64>,
    pub id_to_char: Vec<Option<char>>,
    pub cls_id: i64,
    pub sep_id: i64,
    pub mask_id: i64,
    pub unk_id: i64,
    pub pad_id: i64,
}

impl BertTokenizer {
    pub fn from_vocab_file(path: &Path) -> Option<Self> {
        let content = fs::read_to_string(path).ok()?;
        let mut vocab = HashMap::with_capacity(22_000);
        let mut id_to_char = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            let tok = line.trim_end_matches(&['\r', '\n'][..]);
            id_to_char.push(
                tok.chars()
                    .next()
                    .filter(|&ch| tok.chars().count() == 1 && crate::ret::is_cjk(ch)),
            );
            if !tok.is_empty() {
                vocab.insert(tok.to_string(), idx as i64);
            }
        }
        let cls_id = *vocab.get("[CLS]").unwrap_or(&101);
        let sep_id = *vocab.get("[SEP]").unwrap_or(&102);
        let mask_id = *vocab.get("[MASK]").unwrap_or(&103);
        let unk_id = *vocab.get("[UNK]").unwrap_or(&100);
        let pad_id = *vocab.get("[PAD]").unwrap_or(&0);
        Some(Self {
            vocab,
            id_to_char,
            cls_id,
            sep_id,
            mask_id,
            unk_id,
            pad_id,
        })
    }

    pub fn token_id(&self, tok: &str) -> Option<i64> {
        if let Some(&id) = self.vocab.get(tok) {
            return Some(id);
        }
        let lower = tok.to_lowercase();
        self.vocab.get(&lower).copied()
    }

    pub fn char_token_id(&self, ch: char) -> Option<i64> {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.token_id(s)
    }

    pub fn encode_char_slots(&self, chars: &[char], mask_idx: Option<usize>) -> (Vec<i64>, usize) {
        let mut ids = Vec::with_capacity(chars.len() + 2);
        ids.push(self.cls_id);
        let mut mask_pos = 0usize;
        for (i, &ch) in chars.iter().enumerate() {
            if mask_idx == Some(i) {
                mask_pos = ids.len();
                ids.push(self.mask_id);
            } else {
                let id = self.char_token_id(ch).unwrap_or(self.unk_id);
                ids.push(id);
            }
        }
        ids.push(self.sep_id);
        (ids, mask_pos)
    }
}

#[derive(Debug, Clone)]
pub struct VisualPrediction {
    pub ch: char,
    pub prob: f32,
    pub raw_logit: f32,
}

pub struct HwOnnxBundle {
    pub hw_classes: Vec<char>,
    pub hw_class_set: HashSet<char>,
    pub tokenizer: Option<BertTokenizer>,
    #[cfg(feature = "native-onnx")]
    hw_session: LazySession,
    #[cfg(feature = "native-onnx")]
    rerank_session: LazySession,
}

#[derive(Clone, Copy)]
pub(super) enum RerankerLock {
    Wait,
    Try,
}

pub(super) struct RerankerBusy;
type MaskedScores = Vec<Vec<Option<f32>>>;

#[cfg(feature = "native-onnx")]
#[derive(Default)]
struct LazySession {
    path: Option<PathBuf>,
    session: Mutex<OnceCell<Option<Session>>>,
}

#[cfg(feature = "native-onnx")]
impl LazySession {
    fn lock(
        &self,
        mode: RerankerLock,
    ) -> Result<Option<parking_lot::MappedMutexGuard<'_, Session>>, RerankerBusy> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let guard = match mode {
            RerankerLock::Wait => self.session.lock(),
            RerankerLock::Try => self.session.try_lock().ok_or(RerankerBusy)?,
        };
        Ok(parking_lot::MutexGuard::try_map(guard, |session| {
            session.get_or_init(|| {
                let env = get_ort_env()?;
                Session::builder(env)
                    .ok()?
                    .with_intra_threads(1)
                    .ok()?
                    .commit_from_file(path)
                    .ok()
            });
            session.get_mut()?.as_mut()
        })
        .ok())
    }
}

impl std::fmt::Debug for HwOnnxBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HwOnnxBundle")
            .field("hw_classes", &self.hw_classes.len())
            .field("has_tokenizer", &self.tokenizer.is_some())
            .finish()
    }
}

static BUNDLE_CACHE: OnceLock<Mutex<HashMap<crate::models::ModelPaths, Arc<HwOnnxBundle>>>> =
    OnceLock::new();

pub fn resolve_reranker_dir(assets_dir: &Path) -> Option<PathBuf> {
    let primary = assets_dir.join("reranker");
    if primary.join("model_int8.onnx").is_file() {
        return Some(primary);
    }
    let fallback = assets_dir.join("models").join("reranker").join("rbt3-ONNX");
    if fallback.join("model_int8.onnx").is_file() {
        return Some(fallback);
    }
    if primary.is_dir() {
        return Some(primary);
    }
    None
}

impl HwOnnxBundle {
    pub fn load_cached(assets_dir: Option<&Path>) -> Arc<Self> {
        Self::load_paths(&crate::models::ModelPaths::bundled(assets_dir))
    }

    pub fn clear_cache() {
        if let Some(cache) = BUNDLE_CACHE.get() {
            let removed = std::mem::take(&mut *cache.lock());
            drop(removed);
        }
    }

    pub fn load_paths(paths: &crate::models::ModelPaths) -> Arc<Self> {
        let cache = BUNDLE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        Arc::clone(
            cache
                .lock()
                .entry(paths.clone())
                .or_insert_with(|| Arc::new(Self::load(paths))),
        )
    }

    fn load(paths: &crate::models::ModelPaths) -> Self {
        let mut hw_classes = Vec::new();
        let mut hw_class_set = HashSet::new();
        if let Some(dir) = &paths.handwriting {
            if let Ok(raw) = fs::read_to_string(dir.join("hw_model.classes.json")) {
                if let Ok(ords) = serde_json::from_str::<Vec<u32>>(&raw) {
                    hw_classes.reserve(ords.len());
                    for cp in ords {
                        let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                        hw_classes.push(ch);
                        if ch != '\u{FFFD}' {
                            hw_class_set.insert(ch);
                        }
                    }
                }
            }
        }
        let tokenizer = paths
            .reranker
            .as_ref()
            .and_then(|dir| BertTokenizer::from_vocab_file(&dir.join("vocab.txt")));
        #[cfg(feature = "native-onnx")]
        let hw_session = LazySession {
            path: paths
                .handwriting
                .as_ref()
                .filter(|_| !hw_classes.is_empty())
                .map(|dir| dir.join("hw_model.int8.onnx")),
            ..LazySession::default()
        };
        #[cfg(feature = "native-onnx")]
        let rerank_session = LazySession {
            path: paths
                .reranker
                .as_ref()
                .filter(|_| tokenizer.is_some())
                .map(|dir| dir.join("model_int8.onnx")),
            ..LazySession::default()
        };

        Self {
            hw_classes,
            hw_class_set,
            tokenizer,
            #[cfg(feature = "native-onnx")]
            hw_session,
            #[cfg(feature = "native-onnx")]
            rerank_session,
        }
    }

    pub fn has_hw_onnx(&self) -> bool {
        #[cfg(feature = "native-onnx")]
        {
            self.hw_session
                .lock(RerankerLock::Wait)
                .ok()
                .flatten()
                .is_some()
        }
        #[cfg(not(feature = "native-onnx"))]
        {
            false
        }
    }

    pub fn has_reranker_onnx(&self) -> bool {
        #[cfg(feature = "native-onnx")]
        {
            self.rerank_session
                .lock(RerankerLock::Wait)
                .ok()
                .flatten()
                .is_some()
        }
        #[cfg(not(feature = "native-onnx"))]
        {
            false
        }
    }

    /// Encode only committed context; candidate queries consume the cached projection.
    pub fn encode_rbt3_context(
        &self,
        context_chars: &[char],
    ) -> Option<([i8; 32], HashMap<char, f32>)> {
        #[cfg(not(feature = "native-onnx"))]
        {
            let _ = context_chars;
            None
        }
        #[cfg(feature = "native-onnx")]
        {
            if context_chars.is_empty() {
                return None;
            }
            let tok = self.tokenizer.as_ref()?;
            let chars = &context_chars[context_chars.len().saturating_sub(16)..];
            let mask_pos = 1 + chars.len();
            let mut ids = Vec::with_capacity(chars.len() + 3);
            ids.push(tok.cls_id);
            ids.extend(
                chars
                    .iter()
                    .map(|&ch| tok.char_token_id(ch).unwrap_or(tok.unk_id)),
            );
            ids.extend([tok.mask_id, tok.sep_id]);
            let len = ids.len();
            let attention = vec![1i64; len];
            let types = vec![0i64; len];
            let logits = {
                let mut session = self.rerank_session.lock(RerankerLock::Wait).ok()??;
                let mut outputs = session.run(ort::inputs![
                    "input_ids" => TensorRef::from_array_view(([1usize, len], ids.as_slice())).ok()?,
                    "attention_mask" => TensorRef::from_array_view(([1usize, len], attention.as_slice())).ok()?,
                    "token_type_ids" => TensorRef::from_array_view(([1usize, len], types.as_slice())).ok()?
                ]).ok()?;
                outputs.remove("logits")?
            };
            let (shape, logits) = logits.try_extract_tensor::<f32>().ok()?;
            if shape.len() != 3 || shape[0] != 1 || shape[1] as usize != len {
                return None;
            }
            let vocab_size = shape[2] as usize;
            let row = &logits[mask_pos * vocab_size..(mask_pos + 1) * vocab_size];
            let max_z = row.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
            let log_norm = max_z
                + row
                    .iter()
                    .map(|&z| (z as f64 - max_z).exp())
                    .sum::<f64>()
                    .ln();
            const TOP_CONTEXT: usize = 64;
            let by_score = |a: &(usize, char, f32), b: &(usize, char, f32)| {
                b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0))
            };
            let mut top = Vec::with_capacity(2 * TOP_CONTEXT);
            for (tid, (ch, &z)) in tok.id_to_char.iter().zip(row).enumerate() {
                if let Some(ch) = ch.filter(|_| tid as i64 != tok.unk_id) {
                    top.push((tid, ch, z));
                    if top.len() == 2 * TOP_CONTEXT {
                        top.select_nth_unstable_by(TOP_CONTEXT - 1, by_score);
                        top.truncate(TOP_CONTEXT);
                    }
                }
            }
            if top.len() > TOP_CONTEXT {
                top.select_nth_unstable_by(TOP_CONTEXT - 1, by_score);
                top.truncate(TOP_CONTEXT);
            }
            if top.is_empty() {
                return None;
            }
            top.sort_by_key(|t| t.0);
            let mut vector = [0.0; 32];
            let mut priors = HashMap::with_capacity(top.len());
            for (tid, ch, z) in top {
                let prob = (z as f64 - log_norm).exp() as f32;
                priors.insert(ch, prob);
                crate::ret::project_token(&mut vector, tid as u64, prob);
            }
            Some((crate::ret::quantize_context(vector), priors))
        }
    }

    /// Run `hw_model` ONNX on a flattened `[B, 3, 48, 48]` DirectMap batch.
    pub fn classify_directmap_batch(
        &self,
        directmaps_flat: &[f32],
        batch_size: usize,
        top_k: usize,
    ) -> Option<Vec<Vec<VisualPrediction>>> {
        #[cfg(not(feature = "native-onnx"))]
        {
            let _ = (directmaps_flat, batch_size, top_k);
            None
        }
        #[cfg(feature = "native-onnx")]
        {
            if batch_size == 0 || directmaps_flat.len() != batch_size * 3 * 48 * 48 {
                return None;
            }
            let num_classes = self.hw_classes.len();
            if num_classes == 0 {
                return None;
            }
            let input_ref = TensorRef::from_array_view((
                [batch_size, 3usize, 48usize, 48usize],
                directmaps_flat,
            ))
            .ok()?;

            let mut sess = self.hw_session.lock(RerankerLock::Wait).ok()??;
            let outputs = sess.run(ort::inputs!["directmap" => input_ref]).ok()?;
            let (shape, logits) = outputs["logits"].try_extract_tensor::<f32>().ok()?;
            if shape.len() != 2
                || shape[0] as usize != batch_size
                || shape[1] as usize != num_classes
            {
                return None;
            }

            let mut batch_out = Vec::with_capacity(batch_size);
            for b in 0..batch_size {
                let row = &logits[b * num_classes..(b + 1) * num_classes];
                let max_z = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let mut sum_exp = 0.0f64;
                for &z in row {
                    sum_exp += ((z - max_z) as f64).exp();
                }
                let log_norm = (max_z as f64) + sum_exp.ln();

                // Partial selection of top_k indices
                let k = top_k.min(num_classes);
                let mut indexed: Vec<(usize, f32)> = row.iter().copied().enumerate().collect();
                indexed.select_nth_unstable_by(k.saturating_sub(1), |a, b| b.1.total_cmp(&a.1));
                let mut top_slice = indexed[..k].to_vec();
                top_slice.sort_by(|a, b| b.1.total_cmp(&a.1));

                let preds = top_slice
                    .into_iter()
                    .map(|(idx, z)| {
                        let prob = ((z as f64) - log_norm).exp() as f32;
                        VisualPrediction {
                            ch: self.hw_classes[idx],
                            prob,
                            raw_logit: z,
                        }
                    })
                    .collect();
                batch_out.push(preds);
            }
            Some(batch_out)
        }
    }

    /// Score candidate characters at `[MASK]` positions using `rbt3-ONNX` (`BertForMaskedLM`).
    /// Returns `None` for any candidate that maps to `[UNK]` (not in `vocab.txt`).
    pub fn score_rbt3_masked_batch(
        &self,
        sequences: &[Vec<i64>],
        mask_positions: &[usize],
        cands_per_item: &[Vec<char>],
    ) -> Option<MaskedScores> {
        self.score_rbt3_with_lock(
            sequences,
            mask_positions,
            cands_per_item,
            RerankerLock::Wait,
        )
        .ok()
        .flatten()
    }

    pub(super) fn score_rbt3_with_lock(
        &self,
        sequences: &[Vec<i64>],
        mask_positions: &[usize],
        cands_per_item: &[Vec<char>],
        lock: RerankerLock,
    ) -> Result<Option<MaskedScores>, RerankerBusy> {
        #[cfg(not(feature = "native-onnx"))]
        {
            let _ = (sequences, mask_positions, cands_per_item, lock);
            Ok(None)
        }
        #[cfg(feature = "native-onnx")]
        {
            let Some(mut session) = self.rerank_session.lock(lock)? else {
                return Ok(None);
            };
            let logits =
                self.run_masked_batch(&mut session, sequences, mask_positions, cands_per_item);
            drop(session);
            Ok(logits.and_then(|(max_len, logits)| {
                let tok = self.tokenizer.as_ref()?;
                let (shape, logits) = logits.try_extract_tensor::<f32>().ok()?;
                let bsz = sequences.len();
                if shape.len() != 3 || shape[0] as usize != bsz || shape[1] as usize != max_len {
                    return None;
                }
                let vocab_size = shape[2] as usize;
                let scores = mask_positions
                    .iter()
                    .zip(cands_per_item)
                    .enumerate()
                    .map(|(batch, (&pos, candidates))| {
                        if pos >= max_len {
                            return vec![None; candidates.len()];
                        }
                        let start = (batch * max_len + pos) * vocab_size;
                        score_masked_row(&logits[start..start + vocab_size], tok, candidates)
                    })
                    .collect();
                Some(scores)
            }))
        }
    }

    #[cfg(feature = "native-onnx")]
    fn run_masked_batch(
        &self,
        session: &mut Session,
        sequences: &[Vec<i64>],
        mask_positions: &[usize],
        cands_per_item: &[Vec<char>],
    ) -> Option<(usize, ort::value::DynValue)> {
        let tok = self.tokenizer.as_ref()?;
        let bsz = sequences.len();
        if bsz == 0 || bsz != mask_positions.len() || bsz != cands_per_item.len() {
            return None;
        }
        let max_len = sequences.iter().map(|s| s.len()).max().unwrap_or(0);
        if max_len == 0 {
            return None;
        }

        let mut input_ids = vec![tok.pad_id; bsz * max_len];
        let mut attention_mask = vec![0i64; bsz * max_len];
        let token_type_ids = vec![0i64; bsz * max_len];
        for (b, seq) in sequences.iter().enumerate() {
            let base = b * max_len;
            for (t, &id) in seq.iter().enumerate() {
                input_ids[base + t] = id;
                attention_mask[base + t] = 1;
            }
        }

        let t_ids = TensorRef::from_array_view(([bsz, max_len], input_ids.as_slice())).ok()?;
        let t_att = TensorRef::from_array_view(([bsz, max_len], attention_mask.as_slice())).ok()?;
        let t_typ = TensorRef::from_array_view(([bsz, max_len], token_type_ids.as_slice())).ok()?;

        let mut outputs = session
            .run(ort::inputs![
                "input_ids" => t_ids,
                "attention_mask" => t_att,
                "token_type_ids" => t_typ
            ])
            .ok()?;
        Some((max_len, outputs.remove("logits")?))
    }
}

#[cfg(feature = "native-onnx")]
fn score_masked_row(row: &[f32], tok: &BertTokenizer, candidates: &[char]) -> Vec<Option<f32>> {
    let max_z = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !max_z.is_finite() {
        return vec![None; candidates.len()];
    }
    let sum_exp: f64 = row.iter().map(|&z| f64::from(z - max_z).exp()).sum();
    if !sum_exp.is_finite() || sum_exp <= 0.0 {
        return vec![None; candidates.len()];
    }
    let log_sum_exp = sum_exp.ln();
    candidates
        .iter()
        .map(|&ch| {
            let id = tok.char_token_id(ch).filter(|&id| id != tok.unk_id)?;
            let z = *row.get(id as usize)?;
            if !z.is_finite() {
                return None;
            }
            let score = (f64::from(z) - f64::from(max_z) - log_sum_exp) as f32;
            score.is_finite().then_some(score)
        })
        .collect()
}

#[cfg(all(test, feature = "native-onnx"))]
mod context_tests {
    use super::*;

    #[test]
    fn masked_candidate_scores_match_full_vocabulary_softmax() {
        let row: Vec<f32> = (0..21_128)
            .map(|i| 50.0 + (i as f32 * 0.129).sin() * 13.0)
            .collect();
        let tok = BertTokenizer {
            vocab: [
                ("甲".into(), 5),
                ("乙".into(), 19_430),
                ("丙".into(), 21_127),
                ("丁".into(), 100),
            ]
            .into(),
            id_to_char: Vec::new(),
            cls_id: 101,
            sep_id: 102,
            mask_id: 103,
            unk_id: 100,
            pad_id: 0,
        };
        let max = f64::from(row.iter().copied().fold(f32::NEG_INFINITY, f32::max));
        let mut full_softmax: Vec<f64> = row.iter().map(|&z| (f64::from(z) - max).exp()).collect();
        let norm: f64 = full_softmax.iter().sum();
        full_softmax.iter_mut().for_each(|p| *p /= norm);
        let scores = score_masked_row(&row, &tok, &['甲', '乙', '丙', '丁', '戊']);
        for (score, id) in scores.iter().zip([5, 19_430, 21_127]) {
            assert!((f64::from(score.unwrap()) - full_softmax[id].ln()).abs() < 2e-6);
        }
        assert_eq!(scores[3..], [None, None]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn independent_lazy_sessions_and_busy_preview_retries_on_end() {
        use crate::hw::{HandwritingSession, HwDecodeContext, HwMode, StrokePoint};
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let paths = crate::models::ModelPaths::bundled(Some(&assets));
        let semantic_only = HwOnnxBundle::load(&paths);
        assert!(semantic_only.hw_session.session.lock().get().is_none());
        assert!(semantic_only.rerank_session.session.lock().get().is_none());
        assert!(semantic_only.encode_rbt3_context(&['你', '好']).is_some());
        assert!(semantic_only.hw_session.session.lock().get().is_none());
        let visual_only = HwOnnxBundle::load(&paths);
        assert!(visual_only.has_hw_onnx());
        assert!(visual_only.rerank_session.session.lock().get().is_none());

        let bundle = HwOnnxBundle::load_paths(&paths);
        let imported = crate::imp::ImportedLexiconIndex::new(None);
        let retention = crate::ret::RetentionEngine::default();
        let ctx = HwDecodeContext {
            imported: &imported,
            retention: &retention,
            gram: None,
            tier3: None,
            offline_guard: None,
        };
        let stroke = [
            StrokePoint {
                x: 0.1,
                y: 0.5,
                timestamp_ms: 0,
            },
            StrokePoint {
                x: 0.8,
                y: 0.5,
                timestamp_ms: 25,
            },
        ];
        let new_session = || {
            let mut session = HandwritingSession::default();
            session.set_model_paths(paths.clone());
            session.begin(HwMode::ContinuousLattice);
            session
        };
        let mut expected = new_session();
        let full = expected
            .push_stroke_with_context(&stroke, true, &ctx)
            .unwrap();

        let mut session = new_session();
        let (tx, rx) = std::sync::mpsc::channel();
        let preview = std::thread::scope(|scope| {
            let guard = bundle.rerank_session.session.lock();
            scope.spawn(|| {
                tx.send(session.push_stroke_with_context(&stroke, true, &ctx))
                    .unwrap();
            });
            let preview = rx.recv_timeout(std::time::Duration::from_secs(5));
            drop(guard);
            preview
                .expect("preview must finish while rbt3 is locked")
                .unwrap()
        });
        let score = |payload| {
            let crate::hw::HandwritingPayload::Mode2(result) = payload else {
                panic!("expected Mode2")
            };
            result.slots[0]
                .top8
                .iter()
                .map(|c| c.joint_score)
                .collect::<Vec<_>>()
        };
        let preview = score(preview);
        let full = score(full);
        assert_ne!(preview, full);
        assert_eq!(score(session.end_with_context(&ctx).unwrap()), full);
    }

    #[test]
    fn real_rbt3_context_encoding_reuses_frozen_mlm() {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        if !assets.join("reranker/model_int8.onnx").is_file() {
            return;
        }
        let bundle = HwOnnxBundle::load_cached(Some(&assets));
        assert!(bundle.encode_rbt3_context(&[]).is_none());
        let encode = |text: &str| {
            bundle
                .encode_rbt3_context(&text.chars().collect::<Vec<_>>())
                .expect("available rbt3 model must encode context")
        };
        let (a, priors) = encode("今天晚上想吃");
        let (b, _) = encode("今天晚上要吃");
        assert!(a.iter().any(|&v| v != 0));
        assert!(!priors.is_empty() && priors.len() <= 64);
        assert!(priors
            .iter()
            .all(|(&ch, &p)| crate::ret::is_cjk(ch) && p.is_finite() && p > 0.0 && p <= 1.0));
        let dot = a
            .iter()
            .zip(b)
            .map(|(&x, y)| x as f64 * y as f64)
            .sum::<f64>();
        let norm = |v: &[i8; 32]| v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>().sqrt();
        let cosine = dot / (norm(&a) * norm(&b));
        eprintln!(
            "rbt3 similar-context cosine={cosine:.4}, priors={}",
            priors.len()
        );
        assert!(cosine > 0.65, "similar contexts: {cosine}");
    }
}
