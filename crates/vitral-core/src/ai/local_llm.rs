use crate::{error::VitralError, models::LocalLlmModel};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(feature = "native-onnx")]
use {
    super::{
        qwen_bpe::QwenBpe,
        qwen_onnx::{self, error},
    },
    parking_lot::Mutex,
    std::time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct LocalLlmRequest {
    pub system: String,
    pub prompt: String,
    pub max_new_tokens: usize,
    pub stop: Vec<String>,
    pub cancel: Option<Arc<AtomicBool>>,
}
impl LocalLlmRequest {
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
pub fn chatml(system: &str, prompt: &str) -> String {
    format!("<|im_start|>system\n{system}<|im_end|>\n<|im_start|>user\n{prompt}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n")
}
#[cfg(feature = "native-onnx")]
struct Loaded {
    model: LocalLlmModel,
    tokenizer: QwenBpe,
    session: ort::session::Session,
    config: serde_json::Value,
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
                let mut state = state.lock();
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
        if state
            .as_ref()
            .is_some_and(|s| s.last_used.elapsed() >= Duration::from_secs(300))
        {
            *state = None;
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
    ) -> Result<Option<String>, VitralError> {
        self.generate_impl(model, request, false)
    }

    pub fn generate(
        &self,
        model: &LocalLlmModel,
        request: &LocalLlmRequest,
    ) -> Result<String, VitralError> {
        self.generate_impl(model, request, true)
            .map(Option::unwrap_or_default)
    }

    fn generate_impl(
        &self,
        model: &LocalLlmModel,
        request: &LocalLlmRequest,
        wait: bool,
    ) -> Result<Option<String>, VitralError> {
        request.check_cancel()?;
        if request.max_new_tokens == 0 {
            return Ok(Some(String::new()));
        }
        #[cfg(not(feature = "native-onnx"))]
        {
            let _ = (model, wait);
            Err(VitralError::ModelUnavailable(
                "本地 Qwen3 需要 native-onnx".into(),
            ))
        }
        #[cfg(feature = "native-onnx")]
        {
            let mut state = if wait {
                self.state.lock()
            } else if let Some(state) = self.state.try_lock() {
                state
            } else {
                return Ok(None);
            };
            request.check_cancel()?;
            if state.as_ref().is_none_or(|s| s.model != *model) {
                *state = None;
                *state = Some(Loaded {
                    model: model.clone(),
                    tokenizer: QwenBpe::from_tokenizer_json(&model.dir.join("tokenizer.json"))?,
                    session: qwen_onnx::session(&model.model_file)?,
                    config: serde_json::from_slice(
                        &std::fs::read(model.dir.join("config.json")).map_err(error)?,
                    )
                    .map_err(error)?,
                    last_used: Instant::now(),
                });
            }
            let loaded = state.as_mut().unwrap();
            let result = loaded.generate(request);
            loaded.last_used = Instant::now();
            result.map(Some)
        }
    }
}
#[cfg(feature = "native-onnx")]
impl Loaded {
    fn generate(&mut self, request: &LocalLlmRequest) -> Result<String, VitralError> {
        use ort::value::DynValue;
        let mut ids = self
            .tokenizer
            .encode(&chatml(&request.system, &request.prompt))?;
        let limit = self.config["max_position_embeddings"]
            .as_u64()
            .unwrap_or(32768) as usize;
        if request.max_new_tokens > limit.saturating_sub(ids.len()) || ids.len() >= limit {
            return Err(error("文本过长，请缩短输入"));
        }
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
        let mut generated = Vec::new();
        let mut past = 0usize;
        let eos = self.config["eos_token_id"].as_i64().unwrap_or(151645);
        let mut output = String::new();
        for _ in 0..request.max_new_tokens {
            request.check_cancel()?;
            let n = ids.len();
            let mut inputs: Vec<DynValue> = vec![
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
            let next = qwen_onnx::greedy(&logits, &generated, 1.05)?;
            caches = outputs.into_iter().map(|(_, v)| v).collect();
            past += n;
            if next == eos
                || Some(next) == self.tokenizer.token_id("<|im_end|>")
                || Some(next) == self.tokenizer.token_id("<|endoftext|>")
            {
                break;
            }
            generated.push(next);
            output = self.tokenizer.decode(&generated, true);
            if let Some(pos) = request
                .stop
                .iter()
                .filter(|s| !s.is_empty())
                .filter_map(|s| output.find(s))
                .min()
            {
                output.truncate(pos);
                break;
            }
            ids = vec![next];
        }
        request.check_cancel()?;
        Ok(output.trim().to_owned())
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
