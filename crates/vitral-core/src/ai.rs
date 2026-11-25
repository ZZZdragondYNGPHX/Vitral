use crate::error::VitralError;
use crate::offline_guard::OfflineGuard;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};
#[cfg(feature = "native-voice")]
use {
    parking_lot::Mutex,
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
};
#[cfg(feature = "native-voice")]
mod sensevoice;

use serde::{Deserialize, Serialize};
mod inline;
pub mod local_llm;
pub use inline::AiClient;
pub(crate) use inline::{user_text_prompt, AiChat, InlineRequest, InlineTasks, PrivacyAudit};
pub use inline::{InlineStart, InlineUpdate};
#[cfg(feature = "native-voice")]
mod qwen3_asr;
pub mod qwen_bpe;
#[cfg(any(feature = "native-onnx", feature = "native-voice"))]
mod qwen_onnx;

static AI_POWER_SAVE: AtomicBool = AtomicBool::new(false);
static LOCAL_ENGINE: OnceLock<local_llm::LocalLlmEngine> = OnceLock::new();

#[cfg(any(feature = "native-onnx", feature = "native-voice", test))]
#[derive(Debug, PartialEq, Eq)]
struct HeavyMemoryPolicy {
    mutual_exclusion: bool,
    evict_sensevoice: bool,
    idle_unload_secs: u64,
    max_prefix_entries: usize,
    prefix_kv_budget_bytes: usize,
}

#[cfg(any(feature = "native-onnx", feature = "native-voice", test))]
fn heavy_memory_policy(ram_mb: u64, power_save: bool) -> HeavyMemoryPolicy {
    let tier = crate::models::ram_tier(ram_mb);
    let evict_sensevoice = power_save || tier == "low";
    let (idle_unload_secs, max_prefix_entries, prefix_budget_mib) = if evict_sensevoice {
        (90, 1, 24)
    } else if tier == "standard" {
        (180, 2, 48)
    } else {
        (300, 3, 64)
    };
    HeavyMemoryPolicy {
        mutual_exclusion: evict_sensevoice || tier == "standard",
        evict_sensevoice,
        idle_unload_secs,
        max_prefix_entries,
        prefix_kv_budget_bytes: prefix_budget_mib * 1024 * 1024,
    }
}

#[cfg(any(feature = "native-onnx", feature = "native-voice", test))]
fn current_heavy_memory_policy() -> HeavyMemoryPolicy {
    static RAM_MB: OnceLock<u64> = OnceLock::new();
    let ram_mb = *RAM_MB.get_or_init(crate::models::total_ram_mb);
    heavy_memory_policy(ram_mb, AI_POWER_SAVE.load(Ordering::Acquire))
}

pub(crate) fn set_ai_power_save(enabled: bool) {
    AI_POWER_SAVE.store(enabled, Ordering::Release);
    #[cfg(feature = "native-onnx")]
    if let Some(engine) = LOCAL_ENGINE.get() {
        engine.try_resize_prefix_cache();
    }
}

pub(crate) fn evict_idle_speech_models() {
    #[cfg(feature = "native-voice")]
    {
        qwen3_asr::evict_if_idle();
        if current_heavy_memory_policy().evict_sensevoice {
            sensevoice::evict_if_idle();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SpeechContext {
    pub hotwords: Vec<String>,
    pub context_before: String,
}

#[cfg(feature = "native-voice")]
struct CachedSpeechModel<M> {
    model: Option<M>,
    last_used: Instant,
}

#[cfg(feature = "native-voice")]
type SpeechModel<M> = Arc<Mutex<CachedSpeechModel<M>>>;

#[cfg(feature = "native-voice")]
type SpeechCacheState<K, M> = Mutex<Option<(K, SpeechModel<M>)>>;

#[cfg(feature = "native-voice")]
struct SpeechModelCache<K, M>(Arc<SpeechCacheState<K, M>>);

#[cfg(feature = "native-voice")]
impl<K: PartialEq + Send + 'static, M: Send + 'static> SpeechModelCache<K, M> {
    fn new() -> Self {
        let state = Arc::new(SpeechCacheState::new(None));
        let weak = Arc::downgrade(&state);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(30));
            let Some(state) = weak.upgrade() else { break };
            let idle = Duration::from_secs(current_heavy_memory_policy().idle_unload_secs);
            let removed = state
                .try_lock()
                .and_then(|mut cache| Self::take_idle(&mut cache, Instant::now(), idle));
            drop(removed);
        });
        Self(state)
    }

    fn take_idle(
        cache: &mut Option<(K, SpeechModel<M>)>,
        now: Instant,
        min_idle: Duration,
    ) -> Option<(K, SpeechModel<M>)> {
        let idle = cache
            .as_ref()
            // A caller may have cloned the handle and still be waiting to lock it.
            .filter(|(_, model)| Arc::strong_count(model) == 1)
            .and_then(|(_, model)| model.try_lock())
            .is_some_and(|model| now.saturating_duration_since(model.last_used) >= min_idle);
        if idle {
            cache.take()
        } else {
            None
        }
    }

    fn try_unload_if_idle(&self) -> bool {
        self.0
            .try_lock()
            .and_then(|mut cache| Self::take_idle(&mut cache, Instant::now(), Duration::ZERO))
            .is_some()
    }

    fn model(&self, key: K) -> SpeechModel<M> {
        let mut cache = self.0.lock();
        if let Some((_, model)) = cache.as_ref().filter(|(cached, _)| *cached == key) {
            return Arc::clone(model);
        }
        let model = Arc::new(Mutex::new(CachedSpeechModel {
            model: None,
            last_used: Instant::now(),
        }));
        let previous = cache.replace((key, Arc::clone(&model)));
        drop(cache);
        drop(previous);
        model
    }

    fn with_model<T>(&self, key: K, transcribe: impl FnOnce(&mut Option<M>) -> T) -> T {
        let handle = self.model(key);
        let mut model = handle.lock();
        let result = transcribe(&mut model.model);
        model.last_used = Instant::now();
        result
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
    #[serde(default)]
    pub raw_transcript: String,
    #[serde(default)]
    pub cleaned: bool,
    pub language: String,
    pub duration_ms: u64,
    pub sample_count: usize,
}

impl VoiceResult {
    pub(crate) fn empty(sample_count: usize) -> Self {
        Self {
            transcript: String::new(),
            raw_transcript: String::new(),
            cleaned: false,
            language: "auto".into(),
            duration_ms: sample_count as u64 / 16,
            sample_count,
        }
    }

    pub(crate) fn clean(
        &mut self,
        mode: &str,
        correct: impl FnOnce(&str) -> String,
        polish: impl FnOnce(&str) -> Option<String>,
    ) {
        let raw_asr = self.transcript.clone();
        self.transcript = correct(&raw_asr);
        if mode != "off" {
            let light = cleanup_spoken_transcript(&self.transcript);
            if !light.is_empty() {
                self.transcript = light;
            }
            if mode == "polish" && !self.transcript.trim().is_empty() {
                if let Some(text) = polish(&self.transcript).filter(|s| !s.trim().is_empty()) {
                    self.transcript = text;
                }
            }
        }
        self.raw_transcript = raw_asr;
        self.cleaned = !self.raw_transcript.is_empty() && self.transcript != self.raw_transcript;
    }
}

fn spoken_pause(ch: char) -> bool {
    ch.is_whitespace() || "，、,；;：:…".contains(ch)
}

fn spoken_sentence_end(ch: char) -> bool {
    "。！？.!?\n".contains(ch)
}

fn spoken_number_char(ch: char) -> bool {
    ch.is_ascii_digit() || "零〇一二两三四五六七八九十百千万亿".contains(ch)
}

const SPOKEN_PAUSE_MARKERS: &[&str] = &["[停顿]", "（停顿）", "(停顿)", "<pause>"];

fn strip_spoken_pause(text: &str) -> &str {
    let mut rest = text.trim_start_matches(spoken_pause);
    while let Some(after) = SPOKEN_PAUSE_MARKERS
        .iter()
        .find_map(|marker| rest.strip_prefix(marker))
    {
        rest = after.trim_start_matches(spoken_pause);
    }
    rest
}

/// Remove unambiguous disfluencies while retaining the speaker's wording.
pub fn cleanup_spoken_transcript(raw: &str) -> String {
    const FILLERS: &[&str] = &["就是那个", "然后呢", "就是说", "那个", "呃", "嗯", "啊"];
    // Identical lexical verbs can be intentional (e.g. 讨论讨论).
    const STUTTERS: &[&str] = &[
        "这个", "那个", "然后", "就是", "因为", "所以", "但是", "如果", "我们", "你们", "他们",
        "它们", "其实", "可能", "还有", "已经",
    ];
    let mut text = String::with_capacity(raw.len());
    let mut rest = raw;
    let mut at_pause = true;
    while let Some(ch) = rest.chars().next() {
        if at_pause {
            if let Some(after) = FILLERS.iter().find_map(|word| {
                let tail = rest.strip_prefix(word)?;
                let tail = if matches!(*word, "呃" | "嗯" | "啊") {
                    tail.trim_start_matches(ch)
                } else {
                    tail
                };
                let after = strip_spoken_pause(tail);
                (after.len() < tail.len()).then_some(after)
            }) {
                // Retain the separator already preceding the filler.
                rest = after;
                continue;
            }
        }
        if let Some(after) = SPOKEN_PAUSE_MARKERS
            .iter()
            .find_map(|marker| rest.strip_prefix(marker))
        {
            rest = strip_spoken_pause(after);
            if !text.is_empty() && !text.ends_with(spoken_pause) {
                text.push('，');
            }
            at_pause = true;
            continue;
        }
        if let Some(word) = STUTTERS.iter().find(|&&word| {
            rest.strip_prefix(word)
                .is_some_and(|tail| tail.starts_with(word))
        }) {
            rest = rest.trim_start_matches(word);
            text.push_str(word);
            at_pause = false;
            continue;
        }
        rest = &rest[ch.len_utf8()..];
        if (crate::ret::is_cjk(ch) || matches!(ch, '呃' | '嗯' | '啊'))
            && !spoken_number_char(ch)
            && rest
                .strip_prefix(ch)
                .is_some_and(|tail| tail.starts_with(ch))
        {
            rest = rest.trim_start_matches(ch);
        }
        let separator = spoken_pause(ch) || spoken_sentence_end(ch);
        if ch.is_whitespace() {
            if ch == '\n' && !text.is_empty() && !text.ends_with('\n') {
                if text.ends_with(' ') {
                    text.pop();
                }
                text.push('\n');
            } else if !text.is_empty() && !text.ends_with([' ', '\n']) {
                text.push(' ');
            }
        } else if !separator || (!text.is_empty() && !text.ends_with(ch)) {
            text.push(ch);
        }
        at_pause = separator;
    }
    merge_spoken_repairs(text.trim_end()).replace('\n', " ")
}

fn spoken_value_kind(word: &str) -> Option<&str> {
    if word
        .strip_prefix("星期")
        .or_else(|| word.strip_prefix('周'))
        .is_some_and(|day| matches!(day, "一" | "二" | "三" | "四" | "五" | "六" | "日" | "天"))
    {
        return Some("weekday");
    }
    let (number, unit) = [
        "点半", "点钟", "分钟", "小时", "点", "时", "分", "秒", "号", "日", "月", "年", "元", "个",
    ]
    .into_iter()
    .find_map(|unit| word.strip_suffix(unit).map(|number| (number, unit)))
    .unwrap_or((word, ""));
    (!number.is_empty() && number.chars().all(spoken_number_char))
    .then_some(match unit {
        "点半" | "点钟" | "点" | "时" => "hour",
        "分钟" | "分" => "minute",
        "号" | "日" => "day",
        unit => unit,
    })
}

fn merge_spoken_repairs(mut rest: &str) -> String {
    const MARKERS: &[&str] = &[
        "不对，改成",
        "不对,改成",
        "不对，是",
        "不对,是",
        "不对不对，",
        "不对不对,",
        "不是，是",
        "不是,是",
        "应该是",
    ];
    let mut output = String::with_capacity(rest.len());
    while let Some(comma) = rest.find(['，', ',']) {
        let (before, punctuation_and_after) = rest.split_at(comma);
        let comma_len = punctuation_and_after.chars().next().unwrap().len_utf8();
        let after_comma = &punctuation_and_after[comma_len..];
        if let Some((marker, after)) = MARKERS.iter().find_map(|&marker| {
            after_comma
                .trim_start()
                .strip_prefix(marker)
                .map(|after| (marker, after.trim_start()))
        }) {
            let lhs = before.trim_end();
            let rhs_end = after
                .find(|ch| spoken_pause(ch) || spoken_sentence_end(ch))
                .unwrap_or(after.len());
            let rhs = &after[..rhs_end];
            // Typed values can be adjacent to verbs, as in “定在周三，不对，是周四开会”.
            let typed = lhs
                .char_indices()
                .rev()
                .take(12)
                .filter_map(|(start, _)| {
                    let kind = spoken_value_kind(&lhs[start..])?;
                    if kind != "weekday"
                        && lhs[..start].ends_with(|ch: char| {
                            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '．' | '-' | '+')
                        })
                    {
                        return None;
                    }
                    let end = rhs
                        .char_indices()
                        .map(|(i, ch)| i + ch.len_utf8())
                        .take(12)
                        .filter(|&end| spoken_value_kind(&rhs[..end]) == Some(kind))
                        .last()?;
                    if after[end..].starts_with(|ch: char| ch.is_ascii_digit())
                        || (kind.is_empty() && after[end..].starts_with(['.', '点']))
                    {
                        return None;
                    }
                    Some((start, end))
                })
                .last();
            // For an untyped name, require a whole short phrase or a bounded name after an explicit cue.
            let cue_end = [
                "联系人是",
                "名字是",
                "名称是",
                "地点是",
                "城市是",
                "改成",
                "联系",
                "叫",
            ]
            .into_iter()
            .filter_map(|cue| lhs.rfind(cue).map(|start| start + cue.len()))
            .max();
            let entity_start = cue_end
                .or_else(|| {
                    lhs.rfind(spoken_sentence_end)
                        .map(|i| i + lhs[i..].chars().next().unwrap().len_utf8())
                })
                .unwrap_or(0);
            let entity = |word: &str| {
                (1..=4).contains(&word.chars().count())
                    && word.chars().all(crate::ret::is_cjk)
                    && !word.starts_with([
                        '我', '你', '他', '她', '它', '这', '那', '很', '也', '没', '不',
                        '别', '非', '未', '并',
                    ])
                    && !word.ends_with(['了', '吗', '呢', '吧', '的'])
                    && spoken_value_kind(word).is_none()
            };
            let repair = typed.or_else(|| {
                let left = &lhs[entity_start..];
                (marker != "应该是"
                    && entity(left)
                    && entity(rhs)
                    && (cue_end.is_some()
                        || (left.chars().count() <= 2 && rhs.chars().count() <= 2)))
                    .then_some((entity_start, rhs_end))
            });
            if let Some((start, end)) = repair.filter(|&(start, _)| {
                let prefix = lhs[..start]
                    .rsplit(['，', '。', '！', '？', '；', '、', '\n', ',', '!', '?', ';'])
                    .next()
                    .unwrap_or_default();
                !prefix.contains(['别', '没', '未'])
                    && !prefix.contains("并非")
                    && !prefix.trim_end().ends_with('非')
                    && !prefix
                        .match_indices('不')
                        .any(|(at, marker)| !prefix[at + marker.len()..].starts_with('对'))
            }) {
                output.push_str(&lhs[..start]);
                // Leave the replacement in the stream so a subsequent correction can replace it again.
                rest = after;
                debug_assert!(end > 0);
                continue;
            }
        }
        output.push_str(&rest[..comma + comma_len]);
        rest = after_comma;
    }
    output.push_str(rest);
    output
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiInlineResult {
    pub mode: AiMode,
    pub source_text: String,
    pub output_text: String,
    pub target_lang: String,
    #[serde(default)]
    pub route: String,
}

#[derive(Default)]
pub(super) struct AiTextStream {
    pending: String,
    output: String,
    delivered: usize,
    thinking: usize,
}

impl AiTextStream {
    fn starts_with(text: &str, prefix: &str) -> bool {
        text.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    }

    pub(super) fn push(&mut self, text: &str, on_delta: &mut impl FnMut(&str)) {
        self.push_stopping(text, &[], on_delta);
    }

    pub(super) fn push_stopping(
        &mut self,
        text: &str,
        stops: &[String],
        on_delta: &mut impl FnMut(&str),
    ) -> bool {
        self.pending.push_str(text);
        let mut rest = self.pending.as_str();
        while !rest.is_empty() {
            let boundary = rest.find('<').unwrap_or(rest.len());
            if self.thinking == 0 {
                self.output.push_str(&rest[..boundary]);
            }
            rest = &rest[boundary..];
            if rest.is_empty() {
                break;
            }
            if Self::starts_with(rest, "<think>") {
                self.thinking += 1;
                rest = &rest[7..];
            } else if Self::starts_with(rest, "</think>") {
                self.thinking = self.thinking.saturating_sub(1);
                rest = &rest[8..];
            } else if Self::starts_with("<think>", rest) || Self::starts_with("</think>", rest) {
                break;
            } else {
                if self.thinking == 0 {
                    self.output.push('<');
                }
                rest = &rest[1..];
            }
        }
        let consumed = self.pending.len() - rest.len();
        self.pending.drain(..consumed);
        self.emit(stops, on_delta)
    }

    fn emit(&mut self, stops: &[String], on_delta: &mut impl FnMut(&str)) -> bool {
        if self.delivered == 0 {
            let start = self.output.len() - self.output.trim_start().len();
            self.output.drain(..start);
        }
        let undelivered = &self.output[self.delivered..];
        let stop = stops
            .iter()
            .filter(|stop| !stop.is_empty())
            .filter_map(|stop| undelivered.find(stop))
            .min();
        let end = if let Some(offset) = stop {
            self.output.truncate(self.delivered + offset);
            self.pending.clear();
            self.output.trim_end().len()
        } else {
            let partial = stops
                .iter()
                .flat_map(|stop| stop.char_indices().skip(1).map(|(end, _)| &stop[..end]))
                .filter(|prefix| undelivered.ends_with(prefix))
                .map(str::len)
                .max()
                .unwrap_or(0);
            self.output[..self.output.len() - partial].trim_end().len()
        };
        if end > self.delivered {
            on_delta(&self.output[self.delivered..end]);
            self.delivered = end;
        }
        stop.is_some()
    }

    pub(super) fn finish(mut self, on_delta: &mut impl FnMut(&str)) -> String {
        if self.thinking == 0 {
            self.output.push_str(&self.pending);
        }
        self.emit(&[], on_delta);
        self.output.truncate(self.delivered);
        self.output
    }
}

pub(crate) fn strip_thinking_tags(raw: &str) -> String {
    let mut stream = AiTextStream::default();
    stream.push(raw, &mut |_| {});
    stream.finish(&mut |_| {})
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

const SPEECH_FRAME: usize = 320;

fn frame_rms(frame: &[i16], frame_size: usize) -> f64 {
    let power = frame.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
    (power / frame_size as f64).sqrt()
}

pub(crate) fn trim_silence_vad(samples: &[i16]) -> Option<&[i16]> {
    const PADDING: usize = 10 * SPEECH_FRAME;

    let rms: Vec<_> = samples
        .chunks(SPEECH_FRAME)
        // Treat an incomplete final frame as zero-padded.
        .map(|frame| frame_rms(frame, SPEECH_FRAME))
        .collect();
    // Exclude the zero-padded tail from the noise estimate to avoid artificial modulation.
    let mut distribution = rms[..samples.len() / SPEECH_FRAME].to_vec();
    let last = distribution.len().checked_sub(1)?;
    let (_, p10, _) = distribution.select_nth_unstable_by(last / 10, f64::total_cmp);
    let p10 = *p10;
    let (_, p50, _) = distribution.select_nth_unstable_by(last / 2, f64::total_cmp);
    let peak_rms = rms.iter().copied().fold(0.0_f64, f64::max);
    if peak_rms < 220.0 || peak_rms < p10 * 1.4 {
        return None;
    }
    let (speech_thresh, peak_thresh) = if (80.0..=220.0).contains(&p10) && *p50 <= p10 * 1.25 {
        let speech_thresh = (p10 * 2.5).clamp(120.0, 550.0);
        (speech_thresh, (speech_thresh * 1.4).max(220.0))
    } else {
        (120.0, 220.0)
    };
    if peak_rms < peak_thresh {
        return None;
    }

    let mut first = None;
    let mut end = 0;
    let mut speech_frames = 0;
    for (index, rms) in rms.into_iter().enumerate() {
        if rms >= speech_thresh {
            let start = index * SPEECH_FRAME;
            first.get_or_insert(start);
            end = (start + SPEECH_FRAME).min(samples.len());
            speech_frames += 1;
        }
    }
    if speech_frames < 3 {
        return None;
    }
    let start = first?.saturating_sub(PADDING);
    Some(&samples[start..end.saturating_add(PADDING).min(samples.len())])
}

fn speech_chunk_end(samples: &[i16]) -> usize {
    const MIN_CHUNK: usize = 20 * 16_000;
    const MAX_CHUNK: usize = 30 * 16_000;
    if samples.len() <= MAX_CHUNK {
        return samples.len();
    }
    let mut best = MAX_CHUNK;
    let mut lowest = f64::INFINITY;
    for cut in (MIN_CHUNK..=MAX_CHUNK).step_by(SPEECH_FRAME) {
        let half = SPEECH_FRAME / 2;
        let frame = &samples[cut - half..(cut + half).min(samples.len())];
        let rms = frame_rms(frame, frame.len());
        if rms <= lowest {
            best = cut;
            lowest = rms;
        }
    }
    best
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
    let mut remaining = samples;
    while !remaining.is_empty() {
        let (chunk, rest) = remaining.split_at(speech_chunk_end(remaining));
        remaining = rest;
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
    result.raw_transcript.clone_from(&result.transcript);
    Ok(result)
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
    LOCAL_ENGINE.get_or_init(local_llm::LocalLlmEngine::default)
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
        for amplitude in [400, 500] {
            assert!(trim_silence_vad(&[amplitude; 16_000]).is_none());
            assert!(trim_silence_vad(&[amplitude; 3 * SPEECH_FRAME + 1]).is_none());
        }
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
        let expected = &samples[2 * FRAME..43 * FRAME];
        assert_eq!(trimmed.as_ptr(), expected.as_ptr());
        assert_eq!(trimmed, expected);

        let unpadded = &samples[12 * FRAME..33 * FRAME];
        assert_eq!(trim_silence_vad(unpadded), Some(unpadded));
    }

    #[test]
    fn vad_keeps_soft_onsets_and_raises_threshold_above_background() {
        const FRAME: usize = SPEECH_FRAME;
        let mut samples = vec![40; 60 * FRAME];
        samples[12 * FRAME..20 * FRAME].fill(90);
        samples[20 * FRAME..24 * FRAME].fill(145);
        samples[24 * FRAME..25 * FRAME].fill(230);
        assert_eq!(
            trim_silence_vad(&samples),
            Some(&samples[10 * FRAME..35 * FRAME])
        );

        let mut samples = vec![180; 60 * FRAME];
        samples[20 * FRAME..25 * FRAME].fill(480);
        assert!(trim_silence_vad(&samples).is_none());
        samples[24 * FRAME..25 * FRAME].fill(650);
        assert_eq!(
            trim_silence_vad(&samples),
            Some(&samples[10 * FRAME..35 * FRAME])
        );
    }

    #[test]
    fn vad_keeps_continuous_quiet_speech() {
        let wav = include_bytes!("../../../tests/fixtures/audio/long_20s_16k.wav");
        let pcm: Vec<_> = wav[44..]
            .chunks_exact(2)
            .skip(271_040)
            .take(8_000)
            .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
            .collect();
        assert_eq!(pcm.len(), 8_000);
        for divisor in [4, 6, 8] {
            let quiet: Vec<_> = pcm.iter().map(|&sample| sample / divisor).collect();
            assert!(
                trim_silence_vad(&quiet).is_some_and(|speech| !speech.is_empty()),
                "speech at 1/{divisor} amplitude"
            );
        }
    }

    #[test]
    fn long_recording_splits_at_silence_before_the_thirty_second_limit() {
        const RATE: usize = 16_000;
        let mut samples = vec![800; 75 * RATE];
        for time in [25, 51] {
            let at = time * RATE;
            samples[at - SPEECH_FRAME..at + SPEECH_FRAME].fill(0);
        }
        let cut = speech_chunk_end(&samples);
        assert_eq!(cut, 25 * RATE);
        assert_eq!(samples[cut - 1], 0);
        assert_eq!(samples[cut], 0);
        assert_eq!(speech_chunk_end(&samples[cut..]), 26 * RATE);
        assert_eq!(speech_chunk_end(&samples[51 * RATE..]), 24 * RATE);
        assert_eq!(speech_chunk_end(&samples[..30 * RATE]), 30 * RATE);
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

    #[test]
    fn heavy_model_memory_policy_by_ram_and_power_save() {
        let mib = 1024 * 1024;
        let low = HeavyMemoryPolicy {
            mutual_exclusion: true,
            evict_sensevoice: true,
            idle_unload_secs: 90,
            max_prefix_entries: 1,
            prefix_kv_budget_bytes: 24 * mib,
        };
        let standard = HeavyMemoryPolicy {
            mutual_exclusion: true,
            evict_sensevoice: false,
            idle_unload_secs: 180,
            max_prefix_entries: 2,
            prefix_kv_budget_bytes: 48 * mib,
        };
        let high = HeavyMemoryPolicy {
            mutual_exclusion: false,
            evict_sensevoice: false,
            idle_unload_secs: 300,
            max_prefix_entries: 3,
            prefix_kv_budget_bytes: 64 * mib,
        };
        for (ram_mb, normal) in [
            (0, &high),
            (4096, &low),
            (6143, &low),
            (6144, &standard),
            (8192, &standard),
            (12287, &standard),
            (12288, &high),
            (16384, &high),
        ] {
            assert_eq!(&heavy_memory_policy(ram_mb, false), normal);
            assert_eq!(heavy_memory_policy(ram_mb, true), low);
        }
    }

    #[test]
    fn core_power_save_updates_global_heavy_model_policy() {
        struct RestorePowerSave(bool);
        impl Drop for RestorePowerSave {
            fn drop(&mut self) {
                set_ai_power_save(self.0);
            }
        }
        let _restore = RestorePowerSave(AI_POWER_SAVE.load(Ordering::Acquire));
        let core = crate::VitralCore::init(r#"{"pure_offline":true}"#).unwrap();
        core.set_power_save(true);
        assert!(core.is_power_save());
        assert!(AI_POWER_SAVE.load(Ordering::Acquire));
        assert_eq!(current_heavy_memory_policy(), heavy_memory_policy(0, true));
        core.set_power_save(false);
        assert_eq!(
            current_heavy_memory_policy(),
            heavy_memory_policy(crate::models::total_ram_mb(), false)
        );
    }

    #[cfg(feature = "native-voice")]
    #[test]
    fn speech_cache_clear_keeps_active_handle_alive() {
        let cache = SpeechModelCache::new();
        let old = cache.model("model");
        old.lock().model = Some(String::from("loaded"));
        let ref_to_old = std::sync::Arc::downgrade(&old);
        cache.clear();
        assert!(cache.is_empty());
        let new = cache.model("model");
        assert!(new.lock().model.is_none());
        assert_eq!(old.lock().model.as_deref(), Some("loaded"));
        drop(old);
        assert!(ref_to_old.upgrade().is_none());
    }

    #[cfg(feature = "native-voice")]
    #[test]
    fn speech_cache_expires_after_policy_timeout_when_no_handles_are_borrowed() {
        let cache = SpeechModelCache::new();
        cache.with_model("model", |model| *model = Some(String::from("loaded")));
        let old = cache.model("model");
        let last_used = old.lock().last_used;
        let idle = Duration::from_secs(heavy_memory_policy(8192, false).idle_unload_secs);
        let deadline = last_used + idle;
        let take_idle = |now| SpeechModelCache::take_idle(&mut cache.0.lock(), now, idle);
        assert!(take_idle(deadline).is_none());
        {
            let active = old.lock();
            assert!(take_idle(deadline).is_none());
            assert_eq!(active.model.as_deref(), Some("loaded"));
        }
        drop(old);
        assert!(take_idle(deadline - Duration::from_millis(1)).is_none());
        let (_, expired) = take_idle(deadline).unwrap();
        assert!(cache.is_empty());
        assert_eq!(expired.lock().model.as_deref(), Some("loaded"));
        assert!(cache.model("model").lock().model.is_none());

        let recent = cache.model("model");
        recent.lock().last_used = Instant::now() - idle;
        drop(recent);
        cache.with_model("model", |_| {});
        assert!(take_idle(Instant::now()).is_none());
    }

    #[cfg(feature = "native-voice")]
    #[test]
    fn speech_cache_only_evicts_unborrowed_models_without_waiting() {
        fn check_busy(cache: &SpeechModelCache<&'static str, &'static str>, lock: impl Sized) {
            std::thread::scope(|scope| {
                let (tx, rx) = std::sync::mpsc::channel();
                scope.spawn(move || tx.send(cache.try_unload_if_idle()).unwrap());
                let result = rx.recv_timeout(Duration::from_secs(2));
                drop(lock);
                assert_eq!(result, Ok(false));
            });
        }
        let cache = SpeechModelCache::new();
        cache.with_model("model", |model| *model = Some("loaded"));
        let handle = cache.model("model");
        let weak = Arc::downgrade(&handle);
        assert!(!cache.try_unload_if_idle());
        check_busy(&cache, handle.lock());
        drop(handle);
        check_busy(&cache, cache.0.lock());
        assert!(cache.try_unload_if_idle());
        assert!(weak.upgrade().is_none());
        assert!(cache.is_empty());
        assert!(!cache.try_unload_if_idle());
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
            c.run_inline(
                AiChat { guard: &guard, local_model: Some(&model), activity: None },
                InlineRequest { mode: AiMode::Translate, text: "你好", target_lang: "en", app_tone: None },
            ),
            Err(VitralError::Parse(_))
        ));
    }
}

#[cfg(test)]
mod p1_tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_clear_fillers_and_stutters() {
        for (raw, expected) in [
            (
                "呃，嗯，然后呢，我我我想，啊，明天开会。",
                "我想，明天开会。",
            ),
            ("就是说   我们，那个、下周再聊", "我们，下周再聊"),
            ("嗯嗯嗯[停顿]先讨论[停顿]啊，下周再聊", "先讨论，下周再聊"),
            (
                "这个这个方案，然后然后我们看看，谢谢。",
                "这个方案，然后我们看看，谢谢。",
            ),
            (
                "那个方案，然后去开会，嗯好，就是这样。",
                "那个方案，然后去开会，嗯好，就是这样。",
            ),
            (
                "好好讨论讨论，试试，慢慢看看，刚刚说过",
                "好好讨论讨论，试试，慢慢看看，刚刚说过",
            ),
            (
                "，、  先讨论，，再安排。。  OpenAI   API",
                "先讨论，再安排。 OpenAI API",
            ),
            ("货号 AAA 和 111 不变", "货号 AAA 和 111 不变"),
        ] {
            assert_eq!(cleanup_spoken_transcript(raw), expected, "{raw}");
        }
    }

    #[test]
    fn cleanup_merges_explicit_repairs_without_deleting_uncertain_clauses() {
        for (raw, expected) in [
            ("周三，不对，是周四", "周四"),
            ("会议安排在周三，不对，是周四", "会议安排在周四"),
            ("非洲团队会议安排在周三，应该是周四", "非洲团队会议安排在周四"),
            ("下午三点，不对，改成四点", "下午四点"),
            (
                "之前没确定；会议安排在周三，不对，是周四",
                "之前没确定；会议安排在周四",
            ),
            (
                "之前没确定\n会议安排在周三，不对，是周四",
                "之前没确定 会议安排在周四",
            ),
            ("会议定在星期三，不对，改成周四开会。", "会议定在周四开会。"),
            ("十点，不对不对，十一点。", "十一点。"),
            ("十点，应该是十一点。", "十一点。"),
            ("周三，不是，是周四，不对，是周五", "周五"),
            ("地点是北京市，不对，改成上海市。", "地点是上海市。"),
            ("北京，不对，是上海。", "上海。"),
            ("今天开会，不对，是等通知。", "今天开会，不对，是等通知。"),
            ("周三，不对，应该继续确认。", "周三，不对，应该继续确认。"),
            (
                "请核对十点，不对，是会议时间。",
                "请核对十点，不对，是会议时间。",
            ),
            ("登记2.5，不对，是3。", "登记2.5，不对，是3。"),
        ] {
            assert_eq!(cleanup_spoken_transcript(raw), expected, "{raw}");
        }
    }

    #[test]
    fn cleanup_preserves_repeated_digits_and_negated_values() {
        for raw in [
            "编号三三三七",
            "编号〇〇〇、零零零、111和１１１",
            "会议不是周三，应该是周四",
            "不要周三，应该是周四",
            "会议没有定周三，应该是周四",
            "会议没有安排在周三，应该是周四",
            "会议不安排在周三，应该是周四",
            "会议不要临时安排在周三，应该是周四",
            "并非十点，不对，是十一点",
            "未周三，应该是周四",
        ] {
            assert_eq!(cleanup_spoken_transcript(raw), raw);
        }
    }

    #[test]
    fn voice_result_keeps_asr_text_when_only_homophones_change() {
        let mut retention = crate::ret::RetentionEngine::default();
        retention.pinned_words.insert("维创".into());
        retention.context_bigrams
            .insert("明天联系".into(), [("维创".into(), 4.0)].into());
        for mode in ["off", "light"] {
            let mut result = VoiceResult::empty(0);
            result.transcript = "明天联系伟创讨论项目".into();
            result.clean(
                mode,
                |raw| phonetic_correct_transcript(raw, &retention, None),
                |_| panic!("unexpected polish"),
            );
            assert_eq!(result.transcript, "明天联系维创讨论项目");
            assert_eq!(result.raw_transcript, "明天联系伟创讨论项目");
            assert!(result.cleaned);
        }
    }

    #[test]
    fn thinking_filter_keeps_partial_tags_as_visible_body() {
        for raw in ["示例：<th", "示例：</th", "示例：</t"] {
            let mut text = AiTextStream::default();
            let mut output = String::new();
            let mut emit = |delta: &str| output.push_str(delta);
            text.push("示例：", &mut emit);
            text.push(raw.strip_prefix("示例：").unwrap(), &mut emit);
            assert_eq!(text.finish(&mut emit), raw);
            assert_eq!(output, raw);
        }
    }

    #[test]
    fn voice_result_preserves_raw_and_supports_legacy_json() {
        let mut legacy: VoiceResult = serde_json::from_str(
            r#"{"transcript":"呃，开会","language":"zh","duration_ms":400,"sample_count":6400}"#,
        )
        .unwrap();
        assert!(legacy.raw_transcript.is_empty());
        assert!(!legacy.cleaned);
        let mut off = legacy.clone();
        off.clean("off", str::to_owned, |_| panic!("off cannot invoke polish"));
        assert_eq!(off.transcript, "呃，开会");
        assert_eq!(off.raw_transcript, off.transcript);
        assert!(!off.cleaned);

        let mut light = legacy.clone();
        light.clean("light", str::to_owned, |_| panic!("light cannot invoke polish"));
        assert_eq!(light.transcript, "开会");
        assert_eq!(light.raw_transcript, "呃，开会");
        assert!(light.cleaned);

        let mut fallback = legacy.clone();
        fallback.clean("polish", str::to_owned, |_| None);
        assert_eq!(fallback.transcript, light.transcript);
        assert!(fallback.cleaned);
        legacy.clean("polish", str::to_owned, |input| {
            assert_eq!(input, "开会");
            Some("开会。".into())
        });
        assert_eq!(legacy.transcript, "开会。");
        assert_eq!(legacy.raw_transcript, "呃，开会");
        assert!(legacy.cleaned);

        let mut only_filler = VoiceResult::empty(0);
        only_filler.transcript = "嗯，".into();
        only_filler.clean("light", str::to_owned, |_| None);
        assert_eq!(only_filler.transcript, "嗯，");
        assert!(!only_filler.cleaned);
    }

    #[test]
    fn strips_thinking_blocks_including_nested_and_unfinished() {
        assert_eq!(
            strip_thinking_tags(" <think>a<think>b</think>c</think>答案<think>余下"),
            "答案"
        );
        assert_eq!(
            strip_thinking_tags("开头<think>思考</think>结尾<THINK>no</THINK>"),
            "开头结尾"
        );
        assert_eq!(strip_thinking_tags("<think>秘密"), "");
        assert_eq!(strip_thinking_tags("1 < 2"), "1 < 2");
        let legacy: AiInlineResult = serde_json::from_str(
            r#"{"mode":"polish","source_text":"原文","output_text":"正文","target_lang":""}"#,
        )
        .unwrap();
        assert!(legacy.route.is_empty());
    }

    #[test]
    fn local_stream_stop_markers_never_leak_as_deltas() {
        let mut text = AiTextStream::default();
        let mut output = String::new();
        let mut delta = |part: &str| output.push_str(part);
        let stops = ["终止".into(), "。".into()];
        assert!(!text.push_stopping("<th", &stops, &mut delta));
        assert!(!text.push_stopping("ink>隐藏。", &stops, &mut delta));
        assert!(!text.push_stopping("</think>可见终", &stops, &mut delta));
        assert!(text.push_stopping("止多余", &stops, &mut delta));
        assert_eq!(text.finish(&mut delta), "可见");
        assert_eq!(output, "可见");
    }
}
