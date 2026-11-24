use crate::error::VitralError;
use crate::offline_guard::OfflineGuard;
#[cfg(feature = "native-voice")]
mod sensevoice;

use serde::{Deserialize, Serialize};
pub mod local_llm;
#[cfg(feature = "native-voice")]
mod qwen3_asr;
pub mod qwen_bpe;
#[cfg(any(feature = "native-onnx", feature = "native-voice"))]
mod qwen_onnx;

#[derive(Debug, Clone, Default)]
pub struct SpeechContext {
    pub hotwords: Vec<String>,
    pub context_before: String,
}

#[cfg(feature = "native-voice")]
type SpeechModel<M> = std::sync::Arc<parking_lot::Mutex<Option<M>>>;

#[cfg(feature = "native-voice")]
struct SpeechModelCache<K, M>(parking_lot::Mutex<Option<(K, SpeechModel<M>)>>);

#[cfg(feature = "native-voice")]
impl<K: PartialEq, M> SpeechModelCache<K, M> {
    const fn new() -> Self {
        Self(parking_lot::Mutex::new(None))
    }

    fn model(&self, key: K) -> SpeechModel<M> {
        let mut cache = self.0.lock();
        if let Some((_, model)) = cache.as_ref().filter(|(cached, _)| *cached == key) {
            return std::sync::Arc::clone(model);
        }
        let model = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let previous = cache.replace((key, std::sync::Arc::clone(&model)));
        drop(cache);
        drop(previous);
        model
    }

    fn clear(&self) {
        let removed = self.0.lock().take();
        drop(removed);
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.lock().is_none()
    }
}

pub struct SpeechRecognizer;
impl SpeechRecognizer {
    pub fn transcribe(
        samples: &[i16],
        paths: &crate::models::ModelPaths,
        ctx: &SpeechContext,
    ) -> Result<(String, String), VitralError> {
        #[cfg(feature = "native-voice")]
        {
            if let Some(dir) = &paths.voice_qwen3_asr {
                return qwen3_asr::transcribe(samples, dir, ctx);
            }
            if let Some(dir) = &paths.voice {
                return sensevoice::transcribe(samples, Some(dir), paths.grammar.as_deref());
            }
        }
        #[cfg(not(feature = "native-voice"))]
        let _ = (samples, paths, ctx);
        Err(VitralError::ModelUnavailable(
            "尚未下载或已禁用离线语音识别模型，请在模型中心下载并启用".into(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiMode {
    Translate,
    Polish,
    Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceResult {
    pub transcript: String,
    pub language: String,
    pub duration_ms: u64,
    pub sample_count: usize,
}

impl VoiceResult {
    pub(crate) fn empty(sample_count: usize) -> Self {
        Self {
            transcript: String::new(),
            language: "auto".into(),
            duration_ms: sample_count as u64 / 16,
            sample_count,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiInlineResult {
    pub mode: AiMode,
    pub source_text: String,
    pub output_text: String,
    pub target_lang: String,
}

#[derive(Debug, Default)]
pub struct VoiceSession {
    pub active: bool,
    pub samples: Vec<i16>,
    pub model_paths: crate::models::ModelPaths,
}

impl VoiceSession {
    pub fn start(&mut self) {
        self.active = true;
        self.samples.clear();
    }

    pub fn push_pcm(&mut self, pcm: &[i16]) -> Result<(), VitralError> {
        if !self.active {
            return Err(VitralError::NotInitialized);
        }
        self.samples.extend_from_slice(pcm);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<VoiceResult, VitralError> {
        let samples = self.finish()?;
        recognize_pcm(&samples, &self.model_paths, &SpeechContext::default())
    }

    pub(crate) fn preview_slice(&self) -> &[i16] {
        const MAX_PREVIEW_SAMPLES: usize = 15 * 16_000;
        if !self.active {
            return &[];
        }
        &self.samples[self.samples.len().saturating_sub(MAX_PREVIEW_SAMPLES)..]
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<i16>, VitralError> {
        if !self.active {
            return Err(VitralError::NotInitialized);
        }
        self.active = false;
        Ok(std::mem::take(&mut self.samples))
    }
}

pub(crate) fn trim_silence_vad(samples: &[i16]) -> Option<&[i16]> {
    const FRAME: usize = 320;
    const PADDING: usize = 8 * FRAME;
    const MIN_SPEECH_RMS: f64 = 150.0;
    const MIN_PEAK_RMS: f64 = 250.0;

    let mut first = None;
    let mut end = 0;
    let mut speech_frames = 0;
    let mut peak_rms = 0.0_f64;
    for (index, frame) in samples.chunks(FRAME).enumerate() {
        let power = frame.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        // Treat an incomplete final frame as zero-padded.
        let rms = (power / FRAME as f64).sqrt();
        peak_rms = peak_rms.max(rms);
        if rms >= MIN_SPEECH_RMS {
            let start = index * FRAME;
            first.get_or_insert(start);
            end = start + frame.len();
            speech_frames += 1;
        }
    }
    if speech_frames < 3 || peak_rms < MIN_PEAK_RMS {
        return None;
    }
    let start = first?.saturating_sub(PADDING);
    Some(&samples[start..end.saturating_add(PADDING).min(samples.len())])
}

pub(crate) fn sanitize_asr_transcript(raw: &str) -> String {
    let text = raw.trim();
    let content: Vec<_> = text.chars().filter(|c| c.is_alphanumeric()).collect();
    let repeated = (1..=3).any(|len| {
        content.len() >= 6 * len && content.chunks(len).all(|part| part == &content[..len])
    });
    if content.is_empty() || repeated {
        String::new()
    } else {
        text.to_owned()
    }
}

pub(crate) fn recognize_pcm(
    samples: &[i16],
    paths: &crate::models::ModelPaths,
    ctx: &SpeechContext,
) -> Result<VoiceResult, VitralError> {
    let mut result = VoiceResult::empty(samples.len());
    // Keep each inference bounded even for continuous recorder input.
    for chunk in samples.chunks(30 * 16_000) {
        let Some(chunk) = trim_silence_vad(chunk) else {
            continue;
        };
        let (text, lang) = SpeechRecognizer::transcribe(chunk, paths, ctx)?;
        let text = sanitize_asr_transcript(&text);
        if !text.is_empty() {
            if result
                .transcript
                .ends_with(|c: char| c.is_ascii_alphanumeric())
                && text.starts_with(|c: char| c.is_ascii_alphanumeric())
            {
                result.transcript.push(' ');
            }
            result.transcript.push_str(&text);
            if result.language == "auto" {
                result.language = lang;
            }
        }
    }
    result.transcript = sanitize_asr_transcript(&result.transcript);
    if result.transcript.is_empty() {
        result.language = "auto".into();
    }
    Ok(result)
}

#[derive(Default)]
pub struct AiClient {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub prefer_local: bool,
}

impl From<&crate::eng::EngineConfig> for AiClient {
    fn from(cfg: &crate::eng::EngineConfig) -> Self {
        Self {
            endpoint: cfg.ai_endpoint.clone(),
            model: cfg.ai_model.clone(),
            api_key: cfg.ai_api_key.clone(),
            timeout_ms: cfg.ai_timeout_ms,
            prefer_local: cfg.ai_prefer_local,
        }
    }
}

impl AiClient {
    pub fn run(
        &self,
        guard: &OfflineGuard,
        mode: AiMode,
        text: &str,
        target_lang: &str,
    ) -> Result<AiInlineResult, VitralError> {
        self.run_with_local(guard, None, mode, text, target_lang)
    }

    pub(crate) fn run_with_local(
        &self,
        guard: &OfflineGuard,
        model: Option<&crate::models::LocalLlmModel>,
        mode: AiMode,
        text: &str,
        target_lang: &str,
    ) -> Result<AiInlineResult, VitralError> {
        if text.trim().is_empty() {
            return Err(VitralError::InvalidArgument("text cannot be empty".into()));
        }
        let prompt = match mode {
            AiMode::Translate => format!("Translate the user's text into {target_lang}. Return only the translation."),
            AiMode::Polish => "Polish the user's spoken text into clear written prose in its original language. Return only the edited text.".into(),
            AiMode::Summary => "Summarize the transcript as a numbered list in its original language.".into(),
        };
        let output_text = self.chat_with_local(guard, model, &prompt, text)?;
        Ok(AiInlineResult {
            mode,
            source_text: text.into(),
            output_text,
            target_lang: target_lang.into(),
        })
    }

    pub(crate) fn uses_local(
        &self,
        guard: &OfflineGuard,
        model: Option<&crate::models::LocalLlmModel>,
    ) -> bool {
        local_llm::LocalLlmEngine::available(model)
            && (guard.is_pure_offline()
                || self.prefer_local
                || self.endpoint.as_ref().is_none_or(|s| s.trim().is_empty()))
    }
    pub(crate) fn chat_with_local(
        &self,
        guard: &OfflineGuard,
        model: Option<&crate::models::LocalLlmModel>,
        system: &str,
        prompt: &str,
    ) -> Result<String, VitralError> {
        if self.uses_local(guard, model) {
            return local_engine().generate(
                model.unwrap(),
                &local_llm::LocalLlmRequest {
                    system: system.into(),
                    prompt: prompt.into(),
                    max_new_tokens: 512,
                    stop: vec!["<|im_end|>".into()],
                    cancel: None,
                },
            );
        }
        self.chat(guard, system, prompt)
    }

    pub(crate) fn chat(
        &self,
        guard: &OfflineGuard,
        prompt: &str,
        text: &str,
    ) -> Result<String, VitralError> {
        guard.require_online("ai_chat")?;
        let base = self
            .endpoint
            .as_deref()
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| VitralError::InvalidArgument("ai_endpoint is required".into()))?;
        let mut url = reqwest::Url::parse(base)
            .map_err(|_| VitralError::InvalidArgument("invalid ai_endpoint URL".into()))?;
        let path = url.path().trim_end_matches('/');
        let openai = path.ends_with("/v1") || path.ends_with("/chat/completions");
        let path = if path.ends_with("/chat/completions") || path.ends_with("/api/chat") {
            path.to_owned()
        } else if openai {
            format!("{path}/chat/completions")
        } else {
            format!("{path}/api/chat")
        };
        url.set_path(&path);
        let model = self.model.as_deref().unwrap_or("qwen2.5:7b");
        let body = serde_json::json!({
            "model": model, "stream": false,
            "messages": [{"role":"system", "content":prompt}, {"role":"user", "content":text}]
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_millis(
                self.timeout_ms.unwrap_or(30_000),
            )))
            .max_redirects(0)
            .build()
            .into();
        let mut request = agent
            .post(url.as_str())
            .header("Content-Type", "application/json");
        if let Some(key) = self.api_key.as_deref() {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        guard.require_online("ai_inline")?;
        let mut response = request
            .send(body.to_string().as_bytes())
            .map_err(|e| VitralError::Io(format!("AI request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(VitralError::Io(format!(
                "AI endpoint returned {}",
                response.status()
            )));
        }
        let raw = response
            .body_mut()
            .read_to_string()
            .map_err(|e| VitralError::Io(e.to_string()))?;
        let json: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| VitralError::Parse(e.to_string()))?;
        let pointer = if openai {
            "/choices/0/message/content"
        } else {
            "/message/content"
        };
        let output_text = json
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| VitralError::Parse("AI response is missing message content".into()))?;
        Ok(output_text.to_owned())
    }
}

pub fn run_ai_inline(
    guard: &OfflineGuard,
    mode: AiMode,
    text: &str,
    target_lang: &str,
) -> Result<AiInlineResult, VitralError> {
    AiClient::default().run(guard, mode, text, target_lang)
}

pub(crate) fn clear_model_cache() {
    #[cfg(feature = "native-voice")]
    {
        sensevoice::clear_cache();
        qwen3_asr::clear_cache();
    }
    unload_local_llm();
}

pub(crate) fn local_engine() -> &'static local_llm::LocalLlmEngine {
    static ENGINE: std::sync::OnceLock<local_llm::LocalLlmEngine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(local_llm::LocalLlmEngine::default)
}
pub fn unload_local_llm() {
    local_engine().unload();
}

pub fn speech_context(
    retention: &crate::ret::RetentionEngine,
    app: Option<&str>,
    context_before: String,
) -> SpeechContext {
    let mut words: Vec<String> = retention.pinned_words.iter().cloned().collect();
    words.sort();
    let top = |map: &std::collections::HashMap<String, f64>, n: usize| {
        let mut entries: Vec<_> = map
            .iter()
            .filter(|(_, s)| s.is_finite() && **s > 0.0)
            .collect();
        entries.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
        entries
            .into_iter()
            .take(n)
            .map(|(w, _)| w.clone())
            .collect::<Vec<_>>()
    };
    if let Some(map) =
        app.and_then(|app| retention.app_frecency.get(&app.trim().to_ascii_lowercase()))
    {
        words.extend(top(map, 32));
    }
    words.extend(top(&retention.frecency, 32));
    let mut english: Vec<_> = retention.user_english.values().cloned().collect();
    english.sort();
    english.dedup();
    english.sort_by(|a, b| {
        retention
            .frecency
            .get(b)
            .unwrap_or(&0.0)
            .total_cmp(retention.frecency.get(a).unwrap_or(&0.0))
            .then_with(|| a.cmp(b))
    });
    words.extend(english.into_iter().take(16));
    let mut seen = std::collections::HashSet::new();
    words.retain(|w| !w.trim().is_empty() && seen.insert(w.clone()));
    SpeechContext {
        hotwords: words,
        context_before,
    }
}

/// Correct exact pinyin homophones only with positive left-context evidence.
/// Limit replacements to personal 2–6 Hanzi phrases and 25% of the transcript's Hanzi.
pub fn phonetic_correct_transcript(
    text: &str,
    retention: &crate::ret::RetentionEngine,
    gram: Option<&crate::eng::gram::RimeGramDb>,
) -> String {
    use pinyin::ToPinyin;
    let phonetic = |chars: &[char]| -> Option<Vec<String>> {
        chars
            .iter()
            .map(|c| c.to_pinyin().map(|p| p.plain().to_owned()))
            .collect()
    };
    let mut phrases: Vec<_> = retention
        .pinned_words
        .iter()
        .chain(
            retention
                .frecency
                .iter()
                .filter(|(_, s)| **s >= 3.0)
                .map(|(w, _)| w),
        )
        .collect();
    phrases.sort();
    phrases.dedup();
    let mut phrases: Vec<_> = phrases
        .into_iter()
        .filter_map(|w| {
            let chars: Vec<_> = w.chars().collect();
            if !(2..=6).contains(&chars.len()) {
                return None;
            }
            let py = phonetic(&chars)?;
            Some((w, chars, py))
        })
        .collect();
    phrases.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then_with(|| {
                retention
                    .pinned_words
                    .contains(b.0)
                    .cmp(&retention.pinned_words.contains(a.0))
            })
            .then_with(|| {
                retention
                    .frecency
                    .get(b.0)
                    .unwrap_or(&0.0)
                    .total_cmp(retention.frecency.get(a.0).unwrap_or(&0.0))
            })
            .then_with(|| a.0.cmp(b.0))
    });
    let mut chars: Vec<_> = text.chars().collect();
    let total = chars.iter().filter(|c| c.to_pinyin().is_some()).count();
    let budget = total / 4;
    let mut edits = 0;
    let mut at = 0;
    while at < chars.len() {
        let mut replaced = false;
        for (word, replacement, py) in &phrases {
            let n = replacement.len();
            if at + n > chars.len() {
                continue;
            }
            let source = &chars[at..at + n];
            let diff = source
                .iter()
                .zip(replacement)
                .filter(|(a, b)| a != b)
                .count();
            if diff == 0 || edits + diff > budget || phonetic(source).as_ref() != Some(py) {
                continue;
            }
            let original: String = source.iter().collect();
            if retention.pinned_words.contains(&original) {
                continue;
            }
            let start = chars[..at]
                .iter()
                .rposition(|&ch| !crate::ret::is_cjk(ch))
                .map_or(0, |index| index + 1);
            let context: String = chars[start..at].iter().collect();
            let personal = context.char_indices().find_map(|(start, _)| {
                let following = retention.context_bigrams.get(&context[start..])?;
                let old = following.get(&original).copied();
                let new = following.get(*word).copied();
                (old.is_some() || new.is_some()).then_some((old, new))
            });
            let personal_advantage = personal.is_some_and(|(old, new)| {
                let old = old.unwrap_or(0.0);
                new.is_some_and(|new| new >= 2.0 && new >= old + 1.0 && new >= old * 1.5)
            });
            let gram_advantage = || {
                gram.is_some_and(|gram| {
                    let start = context.char_indices().rev().nth(2).map_or(0, |(start, _)| start);
                    context[start..]
                        .char_indices()
                        .find_map(|(offset, _)| {
                            let prefix = &context[start + offset..];
                            let old = gram.lookup(prefix, &original);
                            let new = gram.lookup(prefix, word);
                            (old.is_some() || new.is_some()).then_some((old, new))
                        })
                        .is_some_and(|(old, new)| {
                            new.is_some_and(|new| old.map_or(new > 0.0, |old| new > old + 2.0))
                        })
                })
            };
            if !personal_advantage && !gram_advantage() {
                continue;
            }
            chars[at..at + n].copy_from_slice(replacement);
            edits += diff;
            at += n;
            replaced = true;
            break;
        }
        if !replaced {
            at += 1;
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod qwen_tests {
    use super::*;

    #[test]
    fn vad_rejects_silence_weak_noise_and_short_spikes() {
        assert!(trim_silence_vad(&[]).is_none());
        assert!(trim_silence_vad(&[0; 16_000]).is_none());
        let noise: Vec<_> = (0..16_000).map(|i| (i % 161) as i16 - 80).collect();
        assert!(trim_silence_vad(&noise).is_none());
        assert!(trim_silence_vad(&[180; 16_000]).is_none());
        let mut spike = [0; 16_000];
        spike[320..960].fill(i16::MIN);
        assert!(trim_silence_vad(&spike).is_none());
    }

    #[test]
    fn vad_preserves_padding_and_internal_pauses() {
        const FRAME: usize = 320;
        let mut samples = vec![0; 60 * FRAME];
        samples[12 * FRAME..15 * FRAME].fill(900);
        samples[30 * FRAME..33 * FRAME].fill(-900);
        let trimmed = trim_silence_vad(&samples).unwrap();
        let expected = &samples[4 * FRAME..41 * FRAME];
        assert_eq!(trimmed.as_ptr(), expected.as_ptr());
        assert_eq!(trimmed, expected);

        let unpadded = &samples[12 * FRAME..33 * FRAME];
        assert_eq!(trim_silence_vad(unpadded), Some(unpadded));
    }

    #[test]
    fn transcript_filter_removes_empty_output_and_short_token_loops() {
        for raw in [
            "  \t\n",
            "。，？！…「」",
            "嗯嗯嗯嗯嗯嗯",
            "你好，你好，你好，你好，你好，你好。",
            "abc abc abc abc abc abc!",
        ] {
            assert!(sanitize_asr_transcript(raw).is_empty(), "{raw:?}");
        }
        for raw in ["嗯嗯嗯嗯嗯", "今天想想，明天再说。", "重复 重复 重复 重复 重复 重复 完毕"] {
            assert_eq!(sanitize_asr_transcript(raw), raw);
        }
        assert_eq!(sanitize_asr_transcript("  你好，Vitral！ \n"), "你好，Vitral！");
    }

    #[test]
    fn voice_preview_slice_borrows_only_the_latest_fifteen_seconds() {
        let mut session = VoiceSession::default();
        session.start();
        session.push_pcm(&[1, 2]).unwrap();
        session.push_pcm(&vec![3; 240_000]).unwrap();
        let preview = session.preview_slice();
        assert_eq!(preview.len(), 240_000);
        assert!(preview.iter().all(|&sample| sample == 3));
        assert!(session.active);
        assert_eq!(&session.samples[..2], &[1, 2]);
        assert_eq!(session.finish().unwrap().len(), 240_002);
        assert!(session.preview_slice().is_empty());
    }

    #[cfg(feature = "native-voice")]
    #[test]
    fn speech_cache_clear_keeps_active_handle_alive() {
        let cache = SpeechModelCache::new();
        let old = cache.model("model");
        *old.lock() = Some(String::from("loaded"));
        let ref_to_old = std::sync::Arc::downgrade(&old);
        cache.clear();
        assert!(cache.is_empty());
        let new = cache.model("model");
        assert!(new.lock().is_none());
        assert_eq!(old.lock().as_deref(), Some("loaded"));
        drop(old);
        assert!(ref_to_old.upgrade().is_none());
    }

    #[cfg(all(target_os = "linux", feature = "native-voice", feature = "native-onnx"))]
    #[test]
    fn llm_unload_drops_cached_handwriting_and_voice_and_allows_reload() {
        use crate::hw::onnx_models::HwOnnxBundle;
        use crate::models::ModelPaths;
        use std::path::Path;
        use std::sync::Arc;

        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let paths = ModelPaths::bundled(Some(&assets));
        let core = crate::VitralCore::init(r#"{"pure_offline":true}"#).unwrap();
        let hw = HwOnnxBundle::load_paths(&paths);
        assert!(hw.has_hw_onnx());
        let context = ['你', '好'];
        let encoded = hw.encode_rbt3_context(&context).unwrap();

        let wav = include_bytes!("../../../tests/fixtures/audio/mandarin_short_16k.wav");
        let pcm: Vec<i16> = wav[44..].chunks_exact(2)
            .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
            .collect();
        let transcribe = || SpeechRecognizer::transcribe(&pcm, &paths, &SpeechContext::default()).unwrap();
        let speech = transcribe();
        assert_eq!(speech.1, "zh");
        assert!(!speech.0.is_empty());
        assert!(!sensevoice::cache_is_empty());

        let old = Arc::downgrade(&hw);
        let response = crate::commands::execute(&core, "llm-unload", &serde_json::json!({})).unwrap();
        assert_eq!(response, serde_json::json!({"ok":true}));
        assert!(sensevoice::cache_is_empty());
        let reloaded = HwOnnxBundle::load_paths(&paths);
        assert!(!Arc::ptr_eq(&hw, &reloaded));
        assert_eq!(hw.encode_rbt3_context(&context).unwrap(), encoded);
        drop(hw);
        assert!(old.upgrade().is_none());
        assert!(reloaded.has_hw_onnx());
        assert_eq!(reloaded.encode_rbt3_context(&context).unwrap(), encoded);
        assert_eq!(transcribe(), speech);
        assert!(!sensevoice::cache_is_empty());
        crate::commands::execute(&core, "llm-unload", &serde_json::json!({})).unwrap();
    }

    #[test]
    fn asr_keeps_common_words_without_positive_context() {
        let mut r = crate::ret::RetentionEngine::default();
        r.pinned_words.insert("指导".into());
        r.frecency.insert("知道".into(), 4.0);
        let transcript = "我知道这件事情";
        assert_eq!(phonetic_correct_transcript(transcript, &r, None), transcript);
        r.context_bigrams
            .insert("我".into(), [("指导".into(), 1.0)].into());
        assert_eq!(phonetic_correct_transcript(transcript, &r, None), transcript);

        let path = crate::imp::polyphone::default_assets_dir_candidates()
            .unwrap()
            .join("rime/wanxiang-lts-zh-hans.gram");
        let gram = crate::eng::gram::RimeGramDb::open_cached(&path).unwrap();
        assert!(gram.lookup("我", "知道").is_some());
        assert!(gram.lookup("我", "指导").is_none());
        assert_eq!(
            phonetic_correct_transcript(transcript, &r, Some(&gram)),
            transcript
        );
    }

    #[test]
    fn personal_homophones_require_context_and_respect_the_edit_budget() {
        let mut r = crate::ret::RetentionEngine::default();
        r.pinned_words.insert("维创".into());
        r.context_bigrams
            .insert("明天联系".into(), [("维创".into(), 4.0)].into());
        assert_eq!(
            phonetic_correct_transcript("明天联系伟创讨论项目", &r, None),
            "明天联系维创讨论项目"
        );
        assert_eq!(
            phonetic_correct_transcript("明天拜访伟创讨论项目", &r, None),
            "明天拜访伟创讨论项目"
        );
        assert_eq!(phonetic_correct_transcript("伟创", &r, None), "伟创");
        assert_eq!(
            phonetic_correct_transcript("明天联系微软讨论项目", &r, None),
            "明天联系微软讨论项目"
        );
        r.context_bigrams
            .insert("创".into(), [("维创".into(), 4.0)].into());
        assert_eq!(
            phonetic_correct_transcript("伟创伟创伟创伟创", &r, None),
            "伟创维创维创伟创"
        );
    }

    #[test]
    fn speech_hotwords_use_current_app_and_personal_vocabulary() {
        let mut r = crate::ret::RetentionEngine::default();
        r.pinned_words.insert("置顶".into());
        r.app_frecency.insert(
            "editor".into(),
            std::collections::HashMap::from([("当前项目".into(), 9.0)]),
        );
        r.frecency.insert("全局常用".into(), 5.0);
        r.user_english.insert("vitral".into(), "Vitral".into());
        let ctx = speech_context(&r, Some("Editor"), "前文".into());
        assert_eq!(ctx.hotwords, vec!["置顶", "当前项目", "全局常用", "Vitral"]);
        assert_eq!(ctx.context_before, "前文");
    }
    #[test]
    fn offline_local_route() {
        let c = AiClient::from(&crate::eng::EngineConfig::default());
        let guard = OfflineGuard::new(true);
        assert!(!c.uses_local(&guard, None));
        let model = crate::models::LocalLlmModel {
            id: "qwen".into(),
            dir: "missing".into(),
            model_file: "missing".into(),
            is_fp16_kv: false,
        };
        assert_eq!(
            c.uses_local(&guard, Some(&model)),
            cfg!(feature = "native-onnx")
        );
        let remote = AiClient {
            endpoint: Some("https://example.test/v1".into()),
            prefer_local: false,
            ..Default::default()
        };
        assert!(!remote.uses_local(&OfflineGuard::new(false), Some(&model)));
        assert_eq!(
            remote.uses_local(&guard, Some(&model)),
            cfg!(feature = "native-onnx")
        );
        let unconfigured = AiClient::default();
        assert_eq!(
            unconfigured.uses_local(&OfflineGuard::new(false), Some(&model)),
            cfg!(feature = "native-onnx")
        );
        #[cfg(feature = "native-onnx")]
        assert!(matches!(
            c.run_with_local(&guard, Some(&model), AiMode::Translate, "你好", "en"),
            Err(VitralError::Parse(_))
        ));
    }
}
