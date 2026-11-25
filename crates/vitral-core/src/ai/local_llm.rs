use crate::{error::VitralError, models::LocalLlmModel};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;
#[cfg(feature = "native-onnx")]
use {
    super::{
        current_heavy_memory_policy, evict_idle_speech_models,
        qwen_bpe::QwenBpe,
        qwen_onnx::{self, error},
        AiTextStream,
    },
    ort::value::DynValue,
    parking_lot::Mutex,
    std::{collections::VecDeque, time::Duration},
};

#[derive(Clone, Default)]
pub struct LocalLlmRequest {
    pub system: String,
    pub prompt: String,
    pub max_new_tokens: usize,
    pub stop: Vec<String>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub deadline: Option<Instant>,
}
impl LocalLlmRequest {
    fn timed_out(&self) -> bool {
        self.deadline.is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn check_cancel(&self) -> Result<(), VitralError> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            Err(VitralError::InvalidArgument("本地 AI 生成已取消".into()))
        } else {
            Ok(())
        }
    }
}
const CHATML_SYSTEM_START: &str = "<|im_start|>system\n";
const CHATML_END: &str = "<|im_end|>";

pub fn chatml(system: &str, prompt: &str) -> String {
    format!("{CHATML_SYSTEM_START}{system}{CHATML_END}\n<|im_start|>user\n{prompt}{CHATML_END}\n<|im_start|>assistant\n<think>\n\n</think>\n")
}

#[cfg(feature = "native-onnx")]
fn cacheable_prefix(tokenizer: &QwenBpe, system: &str, full_ids: &[i64]) -> Option<Vec<i64>> {
    if system.is_empty() {
        return None;
    }
    let ids = tokenizer
        .encode(&format!("{CHATML_SYSTEM_START}{system}{CHATML_END}"))
        .ok()?;
    (ids.len() >= 8
        && ids.last().copied() == tokenizer.token_id(CHATML_END)
        && full_ids.len() > ids.len()
        && full_ids.starts_with(&ids))
    .then_some(ids)
}

#[cfg(feature = "native-onnx")]
struct PrefixKvEntry {
    ids: Vec<i64>,
    tensors: Vec<qwen_onnx::KvSnapshot>,
    bytes: usize,
}

#[cfg(feature = "native-onnx")]
#[derive(Default)]
struct PrefixKvCache {
    entries: VecDeque<PrefixKvEntry>,
    bytes: usize,
    max_entries: usize,
    budget_bytes: usize,
}

#[cfg(feature = "native-onnx")]
impl PrefixKvCache {
    fn enabled(&self) -> bool {
        self.max_entries > 0 && self.budget_bytes > 0
    }

    fn resize(&mut self, max_entries: usize, budget_bytes: usize) {
        self.max_entries = max_entries;
        self.budget_bytes = budget_bytes;
        self.trim(if self.enabled() { max_entries } else { 0 }, budget_bytes);
    }

    fn trim(&mut self, max_entries: usize, budget_bytes: usize) {
        while self.entries.len() > max_entries || self.bytes > budget_bytes {
            self.bytes -= self.entries.pop_front().unwrap().bytes;
        }
    }

    fn get(&mut self, ids: &[i64]) -> Option<Vec<DynValue>> {
        let index = self.entries.iter().position(|entry| entry.ids == ids)?;
        let entry = self.entries.remove(index).unwrap();
        let restored: Result<Vec<_>, _> = entry
            .tensors
            .iter()
            .map(qwen_onnx::restore_kv_tensor)
            .collect();
        match restored {
            Ok(tensors) => {
                self.entries.push_back(entry);
                Some(tensors)
            }
            Err(_) => {
                self.bytes -= entry.bytes;
                None
            }
        }
    }

    fn insert(&mut self, ids: Vec<i64>, tensors: &[DynValue]) {
        if !self.enabled() {
            return;
        }
        // Check the cost before copying any tensor or evicting a usable entry.
        let Some(bytes) = tensors.iter().try_fold(0usize, |bytes, tensor| {
            bytes.checked_add(qwen_onnx::kv_tensor_byte_len(tensor).ok()?)
        }) else {
            return;
        };
        if bytes > self.budget_bytes {
            return;
        }
        self.trim(self.max_entries - 1, self.budget_bytes - bytes);
        if let Ok(tensors) = tensors.iter().map(qwen_onnx::snapshot_kv_tensor).collect() {
            self.entries.push_back(PrefixKvEntry {
                ids,
                tensors,
                bytes,
            });
            self.bytes += bytes;
        }
    }
}

#[cfg(feature = "native-onnx")]
struct Loaded {
    model: LocalLlmModel,
    tokenizer: QwenBpe,
    session: ort::session::Session,
    config: serde_json::Value,
    prefix_cache: PrefixKvCache,
    last_used: Instant,
}
#[derive(Clone)]
pub struct LocalLlmEngine {
    #[cfg(feature = "native-onnx")]
    state: Arc<Mutex<Option<Loaded>>>,
}
impl Default for LocalLlmEngine {
    fn default() -> Self {
        #[cfg(feature = "native-onnx")]
        {
            let state: Arc<Mutex<Option<Loaded>>> = Arc::new(Mutex::new(None));
            let weak = Arc::downgrade(&state);
            // A weak owner permits engine drop without keeping sessions alive.
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(30));
                let Some(state) = weak.upgrade() else { break };
                let Some(mut state) = state.try_lock() else { continue };
                Self::expire_idle(&mut state);
            });
            Self { state }
        }
        #[cfg(not(feature = "native-onnx"))]
        Self {}
    }
}
impl LocalLlmEngine {
    #[cfg(feature = "native-onnx")]
    fn expire_idle(state: &mut Option<Loaded>) {
        let idle = Duration::from_secs(current_heavy_memory_policy().idle_unload_secs);
        if state
            .as_ref()
            .is_some_and(|s| s.last_used.elapsed() >= idle)
        {
            *state = None;
        }
    }

    #[cfg(all(feature = "native-onnx", any(feature = "native-voice", test)))]
    pub(super) fn try_unload_if_idle(&self) -> bool {
        self.state
            .try_lock()
            .is_some_and(|mut state| state.take().is_some())
    }

    #[cfg(feature = "native-onnx")]
    pub(super) fn try_resize_prefix_cache(&self) {
        if let Some(mut state) = self.state.try_lock() {
            if let Some(loaded) = state.as_mut() {
                loaded.resize_prefix_cache();
            }
        }
    }

    pub fn unload(&self) {
        #[cfg(feature = "native-onnx")]
        {
            *self.state.lock() = None;
        }
    }
    pub fn available(model: Option<&LocalLlmModel>) -> bool {
        cfg!(feature = "native-onnx") && model.is_some()
    }
    pub(crate) fn is_busy(&self) -> bool {
        #[cfg(feature = "native-onnx")]
        {
            self.state.is_locked()
        }
        #[cfg(not(feature = "native-onnx"))]
        {
            false
        }
    }

    pub(crate) fn try_generate(
        &self,
        model: &LocalLlmModel,
        request: &LocalLlmRequest,
        on_start: impl FnOnce(),
    ) -> Result<Option<String>, VitralError> {
        self.generate_impl(model, request, false, on_start, &mut |_| {})
    }

    pub fn generate(
        &self,
        model: &LocalLlmModel,
        request: &LocalLlmRequest,
    ) -> Result<String, VitralError> {
        self.generate_impl(model, request, true, || {}, &mut |_| {})
            .map(Option::unwrap_or_default)
    }

    pub fn generate_streaming(
        &self,
        model: &LocalLlmModel,
        request: &LocalLlmRequest,
        mut on_delta: impl FnMut(&str),
    ) -> Result<String, VitralError> {
        let output = self.generate_impl(model, request, true, || {}, &mut on_delta)?;
        if request.timed_out() {
            return Err(VitralError::Io("本地 AI 生成超时".into()));
        }
        Ok(output.unwrap_or_default())
    }

    fn generate_impl(
        &self,
        model: &LocalLlmModel,
        request: &LocalLlmRequest,
        wait: bool,
        on_start: impl FnOnce(),
        on_delta: &mut impl FnMut(&str),
    ) -> Result<Option<String>, VitralError> {
        request.check_cancel()?;
        if request.max_new_tokens == 0 || request.timed_out() {
            return Ok(Some(String::new()));
        }
        #[cfg(not(feature = "native-onnx"))]
        {
            let _ = (model, wait, on_start, on_delta);
            Err(VitralError::ModelUnavailable(
                "本地 Qwen3 需要 native-onnx".into(),
            ))
        }
        #[cfg(feature = "native-onnx")]
        {
            let state = if wait {
                if request.cancel.is_some() {
                    loop {
                        request.check_cancel()?;
                        if request.timed_out() {
                            return Ok(Some(String::new()));
                        }
                        let tick = Instant::now() + Duration::from_millis(25);
                        let until = request.deadline.map_or(tick, |deadline| deadline.min(tick));
                        if let Some(state) = self.state.try_lock_until(until) {
                            break Some(state);
                        }
                    }
                } else if let Some(deadline) = request.deadline {
                    self.state.try_lock_until(deadline)
                } else {
                    Some(self.state.lock())
                }
            } else {
                self.state.try_lock()
            };
            if request.timed_out() {
                return Ok(Some(String::new()));
            }
            let Some(mut state) = state else {
                return Ok(None);
            };
            request.check_cancel()?;
            on_start();
            if state.as_ref().is_none_or(|s| s.model != *model) {
                *state = None;
                if current_heavy_memory_policy().mutual_exclusion {
                    evict_idle_speech_models();
                }
                *state = Some(Loaded {
                    model: model.clone(),
                    tokenizer: QwenBpe::from_tokenizer_json(&model.dir.join("tokenizer.json"))?,
                    session: qwen_onnx::session(&model.model_file)?,
                    config: serde_json::from_slice(
                        &std::fs::read(model.dir.join("config.json")).map_err(error)?,
                    )
                    .map_err(error)?,
                    prefix_cache: PrefixKvCache::default(),
                    last_used: Instant::now(),
                });
            }
            let loaded = state.as_mut().unwrap();
            let result = loaded.generate(request, on_delta);
            loaded.resize_prefix_cache();
            loaded.last_used = Instant::now();
            result.map(Some)
        }
    }
}
#[cfg(feature = "native-onnx")]
impl Loaded {
    fn resize_prefix_cache(&mut self) {
        let policy = current_heavy_memory_policy();
        self.prefix_cache
            .resize(policy.max_prefix_entries, policy.prefix_kv_budget_bytes);
    }

    fn empty_caches(&self) -> Result<Vec<DynValue>, VitralError> {
        let kv_heads = self.config["num_key_value_heads"]
            .as_i64()
            .ok_or_else(|| error("missing num_key_value_heads"))?;
        let head_dim = self.config["head_dim"].as_i64().unwrap_or(128);
        let mut caches = Vec::new();
        for input in self.session.inputs().iter().skip(3) {
            let ty = input
                .dtype()
                .tensor_type()
                .ok_or_else(|| error("invalid KV type"))?;
            let shape = input
                .dtype()
                .tensor_shape()
                .ok_or_else(|| error("missing KV shape"))?;
            if shape.len() != 4 {
                return Err(error("unsupported KV rank"));
            }
            let shape = vec![
                1,
                if shape[1] > 0 { shape[1] } else { kv_heads },
                0,
                if shape[3] > 0 { shape[3] } else { head_dim },
            ];
            caches.push(qwen_onnx::zeros(ty, shape)?);
        }
        if caches.is_empty() || self.session.inputs()[0].name() != "input_ids" {
            return Err(error("unsupported text model interface"));
        }
        Ok(caches)
    }

    fn run_step(
        &mut self,
        ids: Vec<i64>,
        past: usize,
        mut caches: Vec<DynValue>,
    ) -> Result<(DynValue, Vec<DynValue>), VitralError> {
        let n = ids.len();
        let mut inputs = vec![
            qwen_onnx::ids([1, n], ids)?,
            qwen_onnx::ids([1, past + n], vec![1; past + n])?,
            qwen_onnx::ids([1, n], (past..past + n).map(|n| n as i64).collect())?,
        ];
        inputs.append(&mut caches);
        let mut outputs = self
            .session
            .run(
                inputs
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<ort::session::SessionInputValue<'_>>>()
                    .as_slice(),
            )
            .map_err(error)?;
        let logits_name = outputs
            .keys()
            .next()
            .ok_or_else(|| error("missing logits"))?
            .to_owned();
        let logits = outputs
            .remove(&logits_name)
            .ok_or_else(|| error("missing logits"))?;
        Ok((logits, outputs.into_iter().map(|(_, v)| v).collect()))
    }

    fn generate(
        &mut self,
        request: &LocalLlmRequest,
        on_delta: &mut impl FnMut(&str),
    ) -> Result<String, VitralError> {
        request.check_cancel()?;
        if request.timed_out() {
            return Ok(String::new());
        }
        let mut ids = self
            .tokenizer
            .encode(&chatml(&request.system, &request.prompt))?;
        let limit = self.config["max_position_embeddings"]
            .as_u64()
            .unwrap_or(32768) as usize;
        if request.max_new_tokens > limit.saturating_sub(ids.len()) || ids.len() >= limit {
            return Err(error("文本过长，请缩短输入"));
        }
        self.resize_prefix_cache();
        let prefix = if self.prefix_cache.enabled() {
            cacheable_prefix(&self.tokenizer, &request.system, &ids)
        } else {
            None
        };
        let (mut caches, mut past) = if let Some(prefix) = prefix {
            let past = prefix.len();
            let caches = if let Some(caches) = self.prefix_cache.get(&prefix) {
                caches
            } else {
                request.check_cancel()?;
                if request.timed_out() {
                    return Ok(String::new());
                }
                let empty = self.empty_caches()?;
                let (_, caches) = self.run_step(prefix.clone(), 0, empty)?;
                request.check_cancel()?;
                if request.timed_out() {
                    return Ok(String::new());
                }
                self.resize_prefix_cache();
                self.prefix_cache.insert(prefix, &caches);
                caches
            };
            ids = ids.split_off(past);
            (caches, past)
        } else {
            (self.empty_caches()?, 0)
        };
        let mut generated = Vec::new();
        let eos = self.config["eos_token_id"].as_i64().unwrap_or(151645);
        let mut text = AiTextStream::default();
        let mut bytes = Vec::new();
        let mut stopped = false;
        let think = self.tokenizer.token_id("<think>");
        let end_think = self.tokenizer.token_id("</think>");
        for _ in 0..request.max_new_tokens {
            request.check_cancel()?;
            if request.timed_out() {
                return Ok(String::new());
            }
            let n = ids.len();
            let (logits, next_caches) = self.run_step(ids, past, caches)?;
            request.check_cancel()?;
            if request.timed_out() {
                return Ok(String::new());
            }
            let next = qwen_onnx::greedy(&logits, &generated, 1.05)?;
            caches = next_caches;
            past += n;
            if next == eos
                || Some(next) == self.tokenizer.token_id("<|im_end|>")
                || Some(next) == self.tokenizer.token_id("<|endoftext|>")
            {
                break;
            }
            generated.push(next);
            self.tokenizer.append_token_bytes(
                next,
                Some(next) != think && Some(next) != end_think,
                &mut bytes,
            );
            // A single token can end part way through a UTF-8 character.
            let mut consumed = 0;
            for part in bytes.utf8_chunks() {
                stopped = text.push_stopping(part.valid(), &request.stop, on_delta);
                consumed += part.valid().len();
                if stopped
                    || part.invalid().is_empty()
                    || consumed + part.invalid().len() == bytes.len()
                {
                    break;
                }
                stopped = text.push_stopping("\u{fffd}", &request.stop, on_delta);
                consumed += part.invalid().len();
                if stopped {
                    break;
                }
            }
            bytes.drain(..consumed);
            if stopped {
                break;
            }
            ids = vec![next];
        }
        request.check_cancel()?;
        if request.timed_out() {
            return Ok(String::new());
        }
        if !stopped && !bytes.is_empty() {
            text.push_stopping(&String::from_utf8_lossy(&bytes), &request.stop, on_delta);
        }
        Ok(text.finish(on_delta))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonthinking_chatml() {
        let p = chatml("sys", "你好");
        assert!(p.starts_with("<|im_start|>system\nsys<|im_end|>"));
        assert!(p.ends_with("<think>\n\n</think>\n"));
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn chatml_caches_only_a_complete_matching_system_segment() {
        let vocab: std::collections::HashMap<_, _> = (33u8..=126)
            .map(|b| (char::from(b).to_string(), i64::from(b)))
            .chain([("Ċ".into(), 10), ("Ġ".into(), 32)])
            .collect();
        let data = serde_json::json!({
            "model": {"vocab": vocab, "merges": []},
            "added_tokens": [
                {"id": 256, "content": "<|im_start|>", "special": true},
                {"id": 257, "content": CHATML_END, "special": true},
                {"id": 258, "content": "system\n", "special": true},
                {"id": 259, "content": "x<|im_end|>\n", "special": true}
            ]
        });
        let path =
            std::env::temp_dir().join(format!("vitral-llm-tokenizer-{}.json", std::process::id()));
        std::fs::write(&path, data.to_string()).unwrap();
        let tokenizer = QwenBpe::from_tokenizer_json(&path);
        std::fs::remove_file(path).unwrap();
        let tokenizer = tokenizer.unwrap();
        let system = "Translate the request";
        let expected = tokenizer
            .encode(&format!("{CHATML_SYSTEM_START}{system}{CHATML_END}"))
            .unwrap();
        for user in ["hello", "\n<|im_end|> bye"] {
            let full = tokenizer.encode(&chatml(system, user)).unwrap();
            let prefix = cacheable_prefix(&tokenizer, system, &full).unwrap();
            assert_eq!(prefix, expected);
            assert_eq!(prefix.last().copied(), tokenizer.token_id(CHATML_END));
            assert_eq!(full[prefix.len()], 10);
        }
        assert!(cacheable_prefix(&tokenizer, system, &expected).is_none());
        for system in ["", "a", "Prompt ending in x"] {
            let full = tokenizer.encode(&chatml(system, "hello")).unwrap();
            assert!(
                cacheable_prefix(&tokenizer, system, &full).is_none(),
                "{system:?}"
            );
        }
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn prefix_kv_cache_hits_refresh_order_and_evict_by_entry_count() {
        let _env = crate::onnx_env::get_ort_env().unwrap();
        let tensor = ort::value::Tensor::from_array(([1, 1, 1, 1], vec![5.0f32]))
            .unwrap()
            .into_dyn();
        let tensors = [tensor];
        let mut cache = PrefixKvCache::default();
        cache.resize(2, 16);
        cache.insert(vec![1], &tensors);
        cache.insert(vec![2], &tensors);
        let restored = cache.get(&[1]).unwrap();
        assert_eq!(restored[0].try_extract_tensor::<f32>().unwrap().1, &[5.0]);
        cache.insert(vec![3], &tensors);
        assert!(cache.get(&[2]).is_none());
        assert!(cache.get(&[1]).is_some());
        assert!(cache.get(&[3]).is_some());
        assert_eq!(cache.bytes, 8);
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn prefix_kv_cache_rejects_oversize_entries_and_trims_to_new_limits() {
        let _env = crate::onnx_env::get_ort_env().unwrap();
        use ort::value::TensorElementType::Float32;
        let small = [qwen_onnx::zeros(Float32, vec![1, 1, 1, 1]).unwrap()];
        let medium = [qwen_onnx::zeros(Float32, vec![1, 1, 2, 1]).unwrap()];
        let oversized = [qwen_onnx::zeros(Float32, vec![1, 1, 3, 1]).unwrap()];
        let mut cache = PrefixKvCache::default();
        cache.resize(3, 8);
        cache.insert(vec![1], &small);
        cache.insert(vec![2], &small);
        cache.insert(vec![3], &oversized);
        assert!(cache.get(&[3]).is_none());
        assert!(cache.get(&[1]).is_some());
        assert!(cache.get(&[2]).is_some());
        cache.insert(vec![4], &medium);
        assert!(cache.get(&[1]).is_none());
        assert!(cache.get(&[2]).is_none());
        assert!(cache.get(&[4]).is_some());
        assert_eq!(cache.bytes, 8);

        cache.resize(3, 16);
        cache.insert(vec![5], &small);
        cache.insert(vec![6], &small);
        cache.resize(3, 8);
        assert!(cache.get(&[4]).is_none());
        assert_eq!(cache.bytes, 8);
        cache.resize(1, 8);
        assert!(cache.get(&[5]).is_none());
        assert!(cache.get(&[6]).is_some());
        assert_eq!(cache.bytes, 4);
        cache.resize(1, 0);
        cache.insert(vec![7], &small);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[cfg(feature = "native-onnx")]
    #[test]
    fn try_unload_if_idle_does_not_wait_for_a_busy_engine() {
        let engine = LocalLlmEngine::default();
        let lock = engine.state.lock();
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let engine = &engine;
            scope.spawn(move || tx.send(engine.try_unload_if_idle()).unwrap());
            let result = rx.recv_timeout(Duration::from_secs(2));
            drop(lock);
            assert_eq!(result, Ok(false));
        });
    }

    #[cfg(all(target_os = "linux", feature = "native-onnx", feature = "native-voice"))]
    #[test]
    fn first_sensevoice_load_evicts_idle_llm_in_power_save() {
        use super::super::{local_engine, sensevoice, set_ai_power_save, AI_POWER_SAVE};
        use std::path::Path;

        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
        let tokenizer_path =
            std::env::temp_dir().join(format!("vitral-sensevoice-tokenizer-{}.json", std::process::id()));
        std::fs::write(&tokenizer_path, r#"{"model":{"vocab":{},"merges":[]}}"#).unwrap();
        let tokenizer = QwenBpe::from_tokenizer_json(&tokenizer_path);
        std::fs::remove_file(tokenizer_path).unwrap();
        let model_file = assets.join("handwriting/hw_model.int8.onnx");
        let engine = local_engine();
        *engine.state.lock() = Some(Loaded {
            session: qwen_onnx::session(&model_file).unwrap(),
            model: LocalLlmModel {
                id: "test".into(),
                dir: assets.join("handwriting"),
                model_file,
                is_fp16_kv: false,
            },
            tokenizer: tokenizer.unwrap(),
            config: serde_json::Value::Null,
            prefix_cache: PrefixKvCache::default(),
            last_used: Instant::now(),
        });

        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                local_engine().unload();
                sensevoice::clear_cache();
                set_ai_power_save(self.0);
            }
        }
        let _restore = Restore(AI_POWER_SAVE.load(Ordering::Acquire));
        set_ai_power_save(true);
        sensevoice::clear_cache();
        let wav = include_bytes!("../../../../tests/fixtures/audio/mandarin_short_16k.wav");
        let pcm: Vec<_> = wav[44..]
            .chunks_exact(2)
            .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
            .collect();

        let (_, language) =
            sensevoice::transcribe(&pcm, Some(&assets.join("voice/sensevoice-small")), None).unwrap();
        assert_eq!(language, "zh");
        assert!(!sensevoice::cache_is_empty());
        assert!(engine.state.lock().is_none());
    }

    #[test]
    fn cancel_before_model_load_and_unload() {
        let e = LocalLlmEngine::default();
        let r = LocalLlmRequest {
            cancel: Some(Arc::new(AtomicBool::new(true))),
            max_new_tokens: 8,
            ..Default::default()
        };
        let m = LocalLlmModel {
            id: "test".into(),
            dir: "missing".into(),
            model_file: "missing".into(),
            is_fp16_kv: false,
        };
        assert!(e.generate(&m, &r).unwrap_err().to_string().contains("取消"));
        e.unload();
        e.unload();
    }

    #[test]
    fn deadline_before_model_load_returns_no_output() {
        let engine = LocalLlmEngine::default();
        let request = LocalLlmRequest {
            deadline: Some(Instant::now()),
            max_new_tokens: 8,
            ..Default::default()
        };
        let model = LocalLlmModel {
            id: "missing".into(),
            dir: "missing".into(),
            model_file: "missing".into(),
            is_fp16_kv: false,
        };
        assert_eq!(engine.try_generate(&model, &request, || {}).unwrap(), Some(String::new()));
    }

    /// Optional real-weight regression; never downloads assets in the test suite.
    #[cfg(feature = "native-onnx")]
    #[test]
    #[ignore = "set VITRAL_QWEN_SMOKE_DIR to an installed Qwen3 model directory"]
    fn installed_model_generation_and_idle_unload() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("VITRAL_QWEN_SMOKE_DIR").expect("VITRAL_QWEN_SMOKE_DIR"),
        );
        let fp16 = dir.join("model_q4f16.onnx").is_file();
        let model = LocalLlmModel {
            id: "smoke".into(),
            model_file: dir.join(if fp16 {
                "model_q4f16.onnx"
            } else {
                "model_int8.onnx"
            }),
            dir,
            is_fp16_kv: fp16,
        };
        let engine = LocalLlmEngine::default();
        let request = LocalLlmRequest {
            system: "Translate into English. Return only the translation.".into(),
            prompt: "你好，世界！".into(),
            max_new_tokens: 24,
            ..Default::default()
        };
        let text = engine.generate(&model, &request).unwrap();
        eprintln!("Real Qwen output: {text}");
        assert!(text.to_lowercase().contains("hello") && text.to_lowercase().contains("world"));
        assert!(!text.contains("<think>"));
        assert!(engine.state.lock().is_some());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cancelling = request.clone();
        cancelling.cancel = Some(cancel.clone());
        let worker_engine = engine.clone();
        let worker_model = model.clone();
        let worker = std::thread::spawn(move || worker_engine.generate(&worker_model, &cancelling));
        std::thread::sleep(Duration::from_millis(50));
        cancel.store(true, Ordering::Relaxed);
        assert!(worker
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("取消"));
        let mut stopped = request.clone();
        stopped.stop = vec!["world".into(), "World".into()];
        let stopped = engine.generate(&model, &stopped).unwrap();
        assert!(!stopped.to_lowercase().contains("world"));
        engine.unload();
        assert!(engine.state.lock().is_none());
        let _ = engine.generate(&model, &request).unwrap();
        let mut state = engine.state.lock();
        state.as_mut().unwrap().last_used = Instant::now() - Duration::from_secs(301);
        LocalLlmEngine::expire_idle(&mut state);
        assert!(state.is_none());
        drop(state);
        let data =
            std::env::temp_dir().join(format!("vitral-llm-core-test-{}", std::process::id()));
        struct Clean(std::path::PathBuf);
        impl Drop for Clean {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _clean = Clean(data.clone());
        let core=crate::VitralCore::init(&serde_json::json!({"pure_offline":true,"assets_dir":data.join("no-assets"),"data_dir":data}).to_string()).unwrap();
        core.input.lock().engine.model_paths.local_llm = Some(model);
        let result = core
            .ai_inline(super::super::AiMode::Translate, "你好，世界！", "English")
            .unwrap();
        eprintln!("Real offline VitralCore::ai_inline: {}", result.output_text);
        assert!(result.output_text.to_lowercase().contains("hello"));
        super::super::unload_local_llm();
    }
}
