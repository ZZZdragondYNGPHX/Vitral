//! Native implementation of sherpa-onnx's Qwen3-ASR export contract.
use super::{
    qwen_bpe::QwenBpe,
    qwen_onnx::{self, error},
    SpeechContext, SpeechModelCache,
};
use crate::error::VitralError;
use ort::{
    session::Session,
    value::{DynTensorValueType, Tensor},
};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

fn mel_features(samples: &[i16]) -> Vec<f32> {
    let samples: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
    let frames = kaldi_fbank::compute_fbank(&kaldi_fbank::FbankOptions::whisper(128), &samples);
    // torch.stft(center=true)[..., :-1]: drop the final centered frame.
    let frames = &frames[..frames.len().saturating_sub(1)];
    let mut features: Vec<f32> = frames
        .iter()
        .flatten()
        .map(|&f| f.max(1e-10).log10())
        .collect();
    let max = features.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    for x in &mut features {
        *x = (x.max(max - 8.0) + 4.0) / 4.0;
    }
    features
}
fn audio_tokens(frames: usize) -> usize {
    (frames / 100) * 13 + (frames % 100).div_ceil(8)
}
fn prompt(hotwords: &[String], context_before: &str, n: usize) -> String {
    // The official system segment contains space-separated hotwords.
    let clean = |s: &str| s.replace(['<', '>', '\r', '\n'], " ");
    let mut hints = hotwords
        .iter()
        .map(|s| clean(s).chars().take(48).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ");
    if !context_before.trim().is_empty() {
        hints.push(' ');
        hints.push_str(
            &clean(context_before)
                .chars()
                .take(128)
                .collect::<String>(),
        );
    }
    format!("<|im_start|>system\n{hints}<|im_end|>\n<|im_start|>user\n<|audio_start|>{}<|audio_end|><|im_end|>\n<|im_start|>assistant\n","<|audio_pad|>".repeat(n))
}

fn encode_prompt(
    ctx: &SpeechContext,
    valid: usize,
    max_len: usize,
    encode: impl Fn(&str) -> Result<Vec<i64>, VitralError>,
) -> Result<Vec<i64>, VitralError> {
    let prompt_budget = max_len.saturating_sub(128);
    let hotwords = &ctx.hotwords[..ctx.hotwords.len().min(80)];
    let encode_hints = |count, before| encode(&prompt(&hotwords[..count], before, valid));
    let ids = encode_hints(hotwords.len(), &ctx.context_before)?;
    if ids.len() <= prompt_budget {
        return Ok(ids);
    }
    if !ctx.context_before.trim().is_empty() {
        let ids = encode_hints(hotwords.len(), "")?;
        if ids.len() <= prompt_budget {
            return Ok(ids);
        }
    }

    let mut best = encode_hints(0, "")?;
    if best.len() > max_len {
        return Err(error("音频超出语音模型上下文，请缩短录音"));
    }
    // Let a long audio-only prompt use the remaining slots when 128 are unavailable.
    if best.len() > prompt_budget {
        return Ok(best);
    }
    let (mut kept, mut rejected) = (0, hotwords.len());
    while rejected > kept + 1 {
        let mid = kept + (rejected - kept) / 2;
        let ids = encode_hints(mid, "")?;
        if ids.len() <= prompt_budget {
            kept = mid;
            best = ids;
        } else {
            rejected = mid;
        }
    }
    Ok(best)
}
fn parse_transcript(text: &str) -> (String, String) {
    let (prefix, body) = text.split_once("<asr_text>").unwrap_or(("", text));
    let lang = prefix
        .trim()
        .trim_start_matches("<|im_start|>")
        .trim()
        .strip_prefix("language ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("auto");
    let lang = match lang.to_ascii_lowercase().as_str() {
        "chinese" => "zh",
        "english" => "en",
        "japanese" => "ja",
        "korean" => "ko",
        "cantonese" => "yue",
        _ => lang,
    };
    (
        body.split("<|im_end|>")
            .next()
            .unwrap_or(body)
            .replace("<|endoftext|>", "")
            .trim()
            .to_owned(),
        lang.to_owned(),
    )
}
struct Model {
    conv: Session,
    encoder: Session,
    decoder: Session,
    tokenizer: QwenBpe,
}
impl Model {
    fn load(dir: &Path) -> Result<Self, VitralError> {
        Ok(Self {
            conv: qwen_onnx::session(&dir.join("conv_frontend.onnx"))?,
            encoder: qwen_onnx::session(&dir.join("encoder.int8.onnx"))?,
            decoder: qwen_onnx::session(&dir.join("decoder.int8.onnx"))?,
            tokenizer: QwenBpe::from_dir(&dir.join("tokenizer"))?,
        })
    }
    fn decode(
        &mut self,
        samples: &[i16],
        ctx: &SpeechContext,
    ) -> Result<(String, String), VitralError> {
        let features = mel_features(samples);
        let frames = features.len() / 128;
        if frames < 2 {
            return Ok((String::new(), "auto".into()));
        }
        let mut outputs = self
            .conv
            .run(ort::inputs![Tensor::from_array((
                [1, frames, 128],
                features
            ))
            .map_err(error)?])
            .map_err(error)?;
        let name = outputs
            .keys()
            .next()
            .ok_or_else(|| error("missing conv output"))?
            .to_owned();
        let conv = outputs
            .remove(&name)
            .ok_or_else(|| error("missing conv output"))?;
        let shape = conv
            .dtype()
            .tensor_shape()
            .ok_or_else(|| error("invalid conv shape"))?;
        if shape.len() != 3 || shape[0] != 1 || shape[1] < 1 {
            return Err(error("invalid conv dimensions"));
        }
        let n = shape[1] as usize;
        let valid = audio_tokens(frames).min(n);
        let mut outputs = self
            .encoder
            .run(ort::inputs![
                conv,
                Tensor::from_array(([1, n], (0..n).map(|i| i < valid).collect::<Vec<_>>()))
                    .map_err(error)?
            ])
            .map_err(error)?;
        let name = outputs
            .keys()
            .next()
            .ok_or_else(|| error("missing encoder output"))?
            .to_owned();
        let audio = outputs
            .remove(&name)
            .ok_or_else(|| error("missing encoder output"))?;
        let audio = audio.downcast::<DynTensorValueType>().map_err(error)?;
        let shape = audio.shape();
        if shape.len() != 3 || shape[0] != 1 || shape[1] < valid as i64 || shape[2] <= 0 {
            return Err(error("invalid audio feature shape"));
        }
        let dim = shape[2];
        let ty = *audio.data_type();
        let mut trimmed = qwen_onnx::zeros(ty, vec![1, valid as i64, dim])?
            .downcast::<DynTensorValueType>()
            .map_err(error)?;
        let bytes = ty
            .byte_size(valid * dim as usize)
            .ok_or_else(|| error("invalid audio type"))?;
        if bytes > 0 {
            // Copy only valid frames, preserving the encoder's FP16/FP32 type.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    audio.data_ptr().cast::<u8>(),
                    trimmed.data_ptr_mut().cast::<u8>(),
                    bytes,
                );
            }
        }
        let audio = trimmed.into_dyn();
        let input_count = self.decoder.inputs().len();
        if input_count < 6
            || (input_count - 4) % 2 != 0
            || self.decoder.outputs().len() != input_count - 3
        {
            return Err(error("unsupported ASR decoder interface"));
        }
        let mut caches = Vec::new();
        let mut max_len = usize::MAX;
        for input in self.decoder.inputs().iter().skip(4) {
            let shape = input
                .dtype()
                .tensor_shape()
                .ok_or_else(|| error("missing cache shape"))?;
            if shape.len() != 4 || shape[2] <= 0 || shape[3] <= 0 {
                return Err(error("invalid cache dimensions"));
            }
            let capacity = if shape[1] > 0 { shape[1] } else { 1024 };
            max_len = max_len.min(capacity as usize);
            caches.push(qwen_onnx::zeros(
                input
                    .dtype()
                    .tensor_type()
                    .ok_or_else(|| error("missing KV type"))?,
                vec![1, capacity, shape[2], shape[3]],
            )?);
        }
        let mut ids = encode_prompt(ctx, valid, max_len, |text| self.tokenizer.encode(text))?;
        let mut generated = Vec::new();
        let mut position = 0;
        for _ in 0..512 {
            let n = ids.len();
            if position + n > max_len {
                break;
            }
            let mut inputs: Vec<ort::session::SessionInputValue<'_>> = vec![
                qwen_onnx::ids([1, n], ids)?.into(),
                audio.view().into(),
                qwen_onnx::ids([1, n], vec![1; n])?.into(),
                qwen_onnx::ids([n], (position..position + n).map(|n| n as i64).collect())?.into(),
            ];
            inputs.extend(caches.iter().map(|c| c.view().into()));
            let mut outputs = self.decoder.run(inputs.as_slice()).map_err(error)?;
            let names: Vec<_> = outputs.keys().map(str::to_owned).collect();
            let logits = outputs
                .remove(&names[0])
                .ok_or_else(|| error("missing ASR logits"))?;
            let next = qwen_onnx::greedy(&logits, &[], 1.0)?;
            for (cache, name) in caches.iter_mut().zip(&names[1..]) {
                qwen_onnx::apply_delta(
                    cache,
                    outputs.get(name).ok_or_else(|| error("missing KV delta"))?,
                    position,
                    n,
                )?;
            }
            position += n;
            if Some(next) == self.tokenizer.token_id("<|im_end|>")
                || Some(next) == self.tokenizer.token_id("<|endoftext|>")
            {
                break;
            }
            generated.push(next);
            ids = vec![next];
            if generated.len() >= 32
                && generated[generated.len() - 16..]
                    == generated[generated.len() - 32..generated.len() - 16]
            {
                generated.truncate(generated.len() - 16);
                break;
            }
        }
        Ok(parse_transcript(&self.tokenizer.decode(&generated, false)))
    }
}
static MODEL: OnceLock<SpeechModelCache<PathBuf, Model>> = OnceLock::new();
pub(super) fn transcribe(
    samples: &[i16],
    dir: &Path,
    ctx: &SpeechContext,
) -> Result<(String, String), VitralError> {
    MODEL.get_or_init(SpeechModelCache::new).with_model(dir.to_path_buf(), |model| {
        if model.is_none() {
            #[cfg(feature = "native-onnx")]
            if super::current_heavy_memory_policy().mutual_exclusion {
                super::local_engine().try_unload_if_idle();
            }
            *model = Some(Model::load(dir)?);
        }
        model.as_mut().unwrap().decode(samples, ctx)
    })
}
pub(super) fn evict_if_idle() {
    if let Some(cache) = MODEL.get() {
        cache.try_unload_if_idle();
    }
}

pub(super) fn clear_cache() {
    if let Some(cache) = MODEL.get() {
        cache.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn whisper_features_and_audio_length() {
        let f = mel_features(&vec![0; 16000]);
        assert_eq!(f.len(), 100 * 128);
        assert!(f.iter().all(|&v| (v + 1.5).abs() < 1e-6));
        assert_eq!(audio_tokens(101), 14);
        let signal: Vec<i16> = (0..16000)
            .map(|n| ((n as f32 * 0.1).sin() * 12000.0) as i16)
            .collect();
        let f = mel_features(&signal);
        assert!(f.iter().all(|v| v.is_finite()));
        let (min, max) = f
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &v| {
                (a.min(v), b.max(v))
            });
        assert!(max > min && max - min <= 2.00001);
    }
    #[test]
    fn prompt_and_parse() {
        let p = prompt(&["Vitral".into(), "伟创".into()], "项目", 2);
        assert!(p.contains("system\nVitral 伟创 项目<|im_end|>"));
        assert_eq!(p.matches("<|audio_pad|>").count(), 2);
        assert_eq!(
            parse_transcript("language Chinese<asr_text>你好<|im_end|>"),
            ("你好".into(), "zh".into())
        );
        assert_eq!(
            parse_transcript("<|im_start|>language Chinese<asr_text>你好"),
            ("你好".into(), "zh".into())
        );
        assert_eq!(parse_transcript("hello"), ("hello".into(), "auto".into()));
    }

    #[test]
    fn prompt_budget_removes_context_then_low_priority_hotwords() {
        let encode = |text: &str| {
            let (system, _) = text
                .strip_prefix("<|im_start|>system\n")
                .unwrap()
                .split_once("<|im_end|>")
                .unwrap();
            let audio_and_markers = text.matches("<|audio_pad|>").count() + 12;
            let mut tokens: Vec<_> = system.bytes().map(i64::from).collect();
            tokens.resize(tokens.len() + audio_and_markers, 0);
            Ok(tokens)
        };
        let valid = 48;
        let audio_only_len = valid + 12;
        let hotwords = vec!["primary".into(), "backup".into()];
        let context = SpeechContext {
            hotwords: hotwords.clone(),
            context_before: "previous utterance".into(),
        };
        let fit_all = encode_prompt(&context, valid, 256, encode).unwrap();
        assert_eq!(fit_all, encode(&prompt(&hotwords, "previous utterance", valid)).unwrap());

        let max_len = audio_only_len + 128 + "primary backup".len();
        let kept = encode(&prompt(&hotwords, "", valid)).unwrap();
        assert_eq!(encode_prompt(&context, valid, max_len, encode).unwrap(), kept);
        let mut overloaded = context;
        overloaded.hotwords.extend((0..78).map(|_| "noisy hotword".repeat(4)));
        assert_eq!(encode_prompt(&overloaded, valid, max_len, encode).unwrap(), kept);
        assert_eq!(kept.len(), max_len - 128);

        let bare = encode(&prompt(&[], "", valid)).unwrap();
        assert_eq!(
            encode_prompt(&overloaded, valid, audio_only_len + 128, encode).unwrap(),
            bare
        );
        assert_eq!(encode_prompt(&overloaded, valid, audio_only_len, encode).unwrap(), bare);
        assert!(encode_prompt(&overloaded, valid, audio_only_len - 1, encode).is_err());
    }

    #[test]
    #[ignore = "set VITRAL_QWEN_ASR_SMOKE_DIR to an installed Qwen3-ASR model directory"]
    fn installed_asr_transcribes_mandarin() {
        let dir = PathBuf::from(
            std::env::var_os("VITRAL_QWEN_ASR_SMOKE_DIR").expect("VITRAL_QWEN_ASR_SMOKE_DIR"),
        );
        let wav = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/audio/mandarin_short_16k.wav"
        ))
        .unwrap();
        let mut at = 12;
        let mut pcm = None;
        while at + 8 <= wav.len() {
            let len = u32::from_le_bytes(wav[at + 4..at + 8].try_into().unwrap()) as usize;
            if &wav[at..at + 4] == b"data" {
                pcm = Some(
                    wav[at + 8..at + 8 + len]
                        .chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]]))
                        .collect::<Vec<_>>(),
                );
                break;
            }
            at += 8 + len + (len % 2);
        }
        let (text, language) = transcribe(
            &pcm.unwrap(),
            &dir,
            &SpeechContext {
                hotwords: vec!["语音识别".into()],
                context_before: String::new(),
            },
        )
        .unwrap();
        eprintln!("Real Qwen3-ASR output [{language}]: {text}");
        assert_eq!(language, "zh");
        assert!(text.contains("天气") || text.contains("你好"));
        assert!(!text.contains("<asr_text>"));
        clear_cache();
        assert!(MODEL.get().unwrap().is_empty());
    }
}
