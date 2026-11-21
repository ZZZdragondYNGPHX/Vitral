use crate::eng::gram::RimeGramDb;
use std::sync::Arc;
use crate::{error::VitralError, onnx_env::get_ort_env};
use kaldi_fbank::{compute_fbank, FbankOptions, WindowType};
use ort::{session::Session, value::TensorRef};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};

const MEL_BINS: usize = 80;
const WINDOW: usize = 7;
const SHIFT: usize = 6;
const DIM: usize = MEL_BINS * WINDOW;

struct Model {
    session: Session,
    tokens: Vec<String>,
    neg_mean: Vec<f32>,
    inv_stddev: Vec<f32>,
    lang_auto: i32,
    with_itn: i32,
    grammar: Option<Arc<RimeGramDb>>,
}

fn model_error(e: impl std::fmt::Display) -> VitralError {
    VitralError::ModelUnavailable(format!("SenseVoice: {e}"))
}

impl Model {
    fn load(dir: &Path, assets: &Path) -> Result<Self, VitralError> {
        let env = get_ort_env().ok_or_else(|| model_error("ONNX Runtime unavailable"))?;
        let session = Session::builder(env)
            .map_err(model_error)?
            .with_intra_threads(1)
            .map_err(model_error)?
            .commit_from_file(dir.join("model.int8.onnx"))
            .map_err(model_error)?;
        let meta = session.metadata().map_err(model_error)?;
        let field = |key: &str| {
            meta.custom(key)
                .ok_or_else(|| model_error(format!("missing {key}")))
        };
        let number = |key: &str| field(key)?.parse::<i32>().map_err(model_error);
        if number("lfr_window_size")? != WINDOW as i32
            || number("lfr_window_shift")? != SHIFT as i32
        {
            return Err(model_error("unsupported LFR dimensions"));
        }
        let norm = |key: &str| {
            let values = field(key)?
                .split(',')
                .map(str::parse)
                .collect::<Result<Vec<f32>, _>>()
                .map_err(model_error)?;
            if values.len() != DIM {
                return Err(model_error(format!("invalid {key} dimension")));
            }
            Ok(values)
        };
        let neg_mean = norm("neg_mean")?;
        let inv_stddev = norm("inv_stddev")?;
        let lang_auto = number("lang_auto")?;
        let with_itn = number("with_itn")?;
        drop(meta);
        let mut tokens = Vec::new();
        for line in std::fs::read_to_string(dir.join("tokens.txt"))
            .map_err(model_error)?
            .lines()
        {
            let (piece, id) = line
                .rsplit_once(' ')
                .ok_or_else(|| model_error("invalid token"))?;
            if id.parse::<usize>().map_err(model_error)? != tokens.len() {
                return Err(model_error("invalid token index"));
            }
            tokens.push(piece.to_owned());
        }
        Ok(Self {
            session,
            tokens,
            neg_mean,
            inv_stddev,
            lang_auto,
            with_itn,
            grammar: RimeGramDb::open_cached(&assets.join("rime/wanxiang-lts-zh-hans.gram")).ok(),
        })
    }

    fn decode(&mut self, pcm: &[i16]) -> Result<(String, String, f64), VitralError> {
        let mut opts = FbankOptions::default();
        opts.frame_opts.window_type = WindowType::Hamming;
        opts.frame_opts.dither = 0.;
        opts.mel_opts.num_bins = MEL_BINS as i32;
        opts.mel_opts.high_freq = 0.;
        let samples: Vec<f32> = pcm.iter().map(|&v| f32::from(v)).collect();
        let frames = compute_fbank(&opts, &samples);
        let mut features = Vec::with_capacity(frames.len().div_ceil(SHIFT) * DIM);
        for center in (0..frames.len()).step_by(SHIFT) {
            for offset in 0..WINDOW {
                let index = (center + offset)
                    .saturating_sub(WINDOW / 2)
                    .min(frames.len() - 1);
                for (bin, &x) in frames[index].iter().enumerate() {
                    let j = offset * MEL_BINS + bin;
                    features.push((x + self.neg_mean[j]) * self.inv_stddev[j]);
                }
            }
        }
        let n = features.len() / DIM;
        let x =
            TensorRef::from_array_view(([1, n, DIM], features.as_slice())).map_err(model_error)?;
        let scalar =
            |value: i32| ort::value::Tensor::from_array(([1], vec![value])).map_err(model_error);
        let inputs = ort::inputs![
            "x" => x,
            "x_length" => scalar(n as i32)?,
            "language" => scalar(self.lang_auto)?,
            "text_norm" => scalar(self.with_itn)?
        ];
        let outputs = self.session.run(inputs).map_err(model_error)?;
        let (shape, logits) = outputs
            .get("logits")
            .ok_or_else(|| model_error("missing logits"))?
            .try_extract_tensor::<f32>()
            .map_err(model_error)?;
        let vocab = self.tokens.len();
        if vocab == 0 || shape.len() != 3 || shape[0] != 1 || shape[2] != vocab as i64 {
            return Err(model_error("invalid logits dimensions"));
        }
        let mut beam = vec![(String::new(), 0.0)];
        let mut language = "auto".to_owned();
        let mut previous = 0;
        let mut confidence = 0.0;
        let mut emitted = 0;
        for (frame, row) in logits.chunks_exact(vocab).enumerate() {
            let id = row
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            let token = &self.tokens[id];
            if frame == 0 {
                if let Some(tag) = token.strip_prefix("<|").and_then(|t| t.strip_suffix("|>")) {
                    language = tag.to_owned();
                }
            }
            // The first four logits carry language, emotion, event and normalization tags.
            if frame >= 4 && id != previous && id > 2 && !token.starts_with("<|") {
                let norm: f64 = row.iter().map(|s| f64::from(*s-row[id]).exp()).sum();
                confidence += 1.0 / norm;
                emitted += 1;
                if let Some(grammar) = self.grammar.as_deref().filter(|_| hanzi(token)) {
                    let mut alternatives: Vec<_> = row.iter().enumerate()
                        .filter(|(i,score)| row[id] - **score < 4.5 && hanzi(&self.tokens[*i]))
                        .collect();
                    alternatives.sort_unstable_by(|a,b| b.1.total_cmp(a.1));
                    let mut next = Vec::new();
                    for (prefix,score) in &beam {
                        let mut tail: Vec<char> = prefix.chars().rev().take(3).collect();
                        tail.reverse();
                        for &(alt,logit) in alternatives.iter().take(20) {
                            let piece = &self.tokens[alt];
                            let mut context_bonus = 0.0_f64;
                            for len in 1..=tail.len() {
                                let ctx: String = tail[tail.len()-len..].iter().collect();
                                if let Some(weight) = grammar.lookup(&ctx,piece) {
                                    let baseline = 11.2 + 2.8 * (len-1) as f64;
                                    context_bonus = context_bonus.max((2.0 + weight-baseline).clamp(0.0,4.0));
                                }
                            }
                            let mut text=prefix.clone();
                            text.push_str(piece);
                            next.push((text,score+f64::from(*logit-row[id])+context_bonus));
                        }
                    }
                    next.sort_unstable_by(|a,b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                    next.truncate(24);
                    beam=next;
                } else {
                    for (prefix,_) in &mut beam { prefix.push_str(token); }
                }
            }
            previous = id;
        }
        let text = &beam[0].0;
        Ok((text.replace('▁', " ").trim().into(), language, confidence / f64::from(emitted.max(1))))
    }
}

fn hanzi(token:&str)->bool {
    let mut chars=token.chars();
    matches!(chars.next(),Some('\u{3400}'..='\u{9fff}')) && chars.next().is_none()
}

static MODEL: Mutex<Option<(PathBuf, Model)>> = Mutex::new(None);

pub(super) fn transcribe(
    samples: &[i16],
    assets: Option<&Path>,
) -> Result<(String, String), VitralError> {
    let assets=assets.ok_or_else(|| model_error("missing assets directory"))?;
    let dir=assets.join("voice/sensevoice-small");
    let mut cache = MODEL.lock();
    if cache.as_ref().is_none_or(|(p, _)| p != &dir) {
        let model = Model::load(&dir, assets)?;
        *cache = Some((dir, model));
    }
    let model=&mut cache.as_mut().unwrap().1;
    let mut best=model.decode(samples)?;
    // Low-confidence utterances benefit from a second acoustic view. Speed
    // perturbation changes speaker/formant scale. Compare normalized token
    // posteriors so differing frame counts do not bias the selection.
    if !best.0.is_empty() && best.2 < 0.65 {
        for ratio in [0.9, 1.1] {
            let size=(samples.len() as f64 / ratio) as usize;
            let mut adjusted=Vec::with_capacity(size);
            for i in 0..size {
                let at=i as f64*ratio;
                let j=at as usize;
                let first=f64::from(samples[j]);
                let second=f64::from(samples[(j+1).min(samples.len()-1)]);
                adjusted.push((first+(second-first)*at.fract()).round() as i16);
            }
            let candidate=model.decode(&adjusted)?;
            if !candidate.0.is_empty() && candidate.1==best.1 && candidate.2>best.2 {
                best=candidate;
            }
        }
    }
    Ok((best.0,best.1))
}
