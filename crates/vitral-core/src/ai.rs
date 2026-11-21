use crate::error::VitralError;
use crate::offline_guard::OfflineGuard;
#[cfg(feature = "native-voice")]
mod sensevoice;

use serde::{Deserialize, Serialize};
use std::path::Path;

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
        let assets = crate::eng::resolve_assets_dir("");
        recognize_pcm(&samples, assets.as_deref())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<i16>, VitralError> {
        if !self.active {
            return Err(VitralError::NotInitialized);
        }
        self.active = false;
        Ok(std::mem::take(&mut self.samples))
    }
}

pub(crate) fn recognize_pcm(
    samples: &[i16],
    assets: Option<&Path>,
) -> Result<VoiceResult, VitralError> {
    let mut result = VoiceResult {
        transcript: String::new(),
        language: "auto".into(),
        duration_ms: samples.len() as u64 / 16,
        sample_count: samples.len(),
    };
    // Keep each inference bounded even for continuous recorder input.
    for chunk in samples.chunks(30 * 16_000) {
        if chunk.len() < 400 || chunk.iter().all(|s| s.unsigned_abs() < 64) {
            continue;
        }
        let (text, lang) = transcribe(chunk, assets)?;
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
    Ok(result)
}

#[cfg(feature = "native-voice")]
use sensevoice::transcribe;

#[cfg(not(feature = "native-voice"))]
fn transcribe(_: &[i16], _: Option<&Path>) -> Result<(String, String), VitralError> {
    Err(VitralError::ModelUnavailable(
        "SenseVoice requires native-voice".into(),
    ))
}

#[derive(Default)]
pub struct AiClient {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: Option<u64>,
}

impl From<&crate::eng::EngineConfig> for AiClient {
    fn from(cfg: &crate::eng::EngineConfig) -> Self {
        Self {
            endpoint: cfg.ai_endpoint.clone(),
            model: cfg.ai_model.clone(),
            api_key: cfg.ai_api_key.clone(),
            timeout_ms: cfg.ai_timeout_ms,
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
        guard.require_online("ai_inline")?;
        if text.trim().is_empty() {
            return Err(VitralError::InvalidArgument("text cannot be empty".into()));
        }
        let prompt = match mode {
            AiMode::Translate => format!("Translate the user's text into {target_lang}. Return only the translation."),
            AiMode::Polish => "Polish the user's spoken text into clear written prose in its original language. Return only the edited text.".into(),
            AiMode::Summary => "Summarize the transcript as a numbered list in its original language.".into(),
        };
        let output_text = self.chat(guard, &prompt, text)?;
        Ok(AiInlineResult {
            mode,
            source_text: text.into(),
            output_text,
            target_lang: target_lang.into(),
        })
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
