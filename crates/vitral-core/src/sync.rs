use crate::clip::ClipboardEntry;
use crate::eng::{LangMode, OpenCcMode, SchemaKind};
use crate::error::VitralError;
use crate::imp::ImportedLexiconEntry;
use crate::ret::RetentionEngine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Cursor;

const ENVELOPE_MAGIC: &[u8; 5] = b"VTRL1";
const NONCE_LEN: usize = 24;
const DEFAULT_SYNC_PASSPHRASE: &[u8] = b"vitral-default-sync-key-v1";

pub(crate) fn merge_alias(v: &mut serde_json::Value, key: &str, alias: &str) -> Result<(), VitralError> {
    if let Some(fields) = v.as_object_mut() {
        if let Some(legacy) = fields.remove(alias) {
            if fields.get(key).is_some_and(|current| current != &legacy) {
                return Err(VitralError::InvalidArgument(format!("conflicting {key} and {alias}")));
            }
            fields.entry(key).or_insert(legacy);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceTier {
    Lan = 0,
    Wan = 1,
    Offline = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayDevice {
    #[serde(alias = "id")]
    pub device_id: String,
    pub device_name: String,
    pub tier: DeviceTier,
    #[serde(default, alias = "transfer_count")]
    pub usage_count: u64,
    #[serde(default)]
    pub last_used_ms: u64,
    #[serde(default)]
    pub lan_online: Option<bool>,
    #[serde(default)]
    pub wan_online: Option<bool>,
    #[serde(default)]
    pub localsend_url: Option<String>,
}

impl RelayDevice {
    pub fn effective_tier(&self) -> DeviceTier {
        match (self.lan_online, self.wan_online) {
            (Some(true), _) => DeviceTier::Lan,
            (Some(false), Some(true)) => DeviceTier::Wan,
            (Some(false), Some(false)) => DeviceTier::Offline,
            _ => self.tier,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaySendResult {
    pub transfer_id: String,
    pub device_id: String,
    pub transport: String,
    pub sha256: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayReceivedItem {
    pub transfer_id: String,
    pub raw_bytes: Vec<u8>,
    pub is_file: bool,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayPollResult {
    pub received: Vec<String>,
    pub deleted_count: usize,
    #[serde(default)]
    pub received_items: Vec<RelayReceivedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupResult {
    pub purged_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreResult {
    pub restored: bool,
    pub cloud_root: String,
    pub snapshots_applied: usize,
}

pub fn sort_devices_mru(devices: &mut [RelayDevice]) {
    devices.sort_by(|a, b| {
        a.effective_tier()
            .cmp(&b.effective_tier())
            .then_with(|| b.last_used_ms.cmp(&a.last_used_ms))
            .then_with(|| b.usage_count.cmp(&a.usage_count))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileBackup {
    #[serde(default = "crate::shortcuts::defaults")]
    pub shortcuts: Vec<crate::shortcuts::ShortcutBinding>,
    pub schema_version: u32,
    pub schema: SchemaKind,
    pub opencc_mode: OpenCcMode,
    pub clipboard_ring_capacity: usize,
    pub frecency_half_life_sec: f64,
    pub app_lang_memory: HashMap<String, LangMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexiconSnapshotPayload {
    pub schema_version: u32,
    pub retention: RetentionEngine,
    pub imported_entries: Vec<ImportedLexiconEntry>,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn derive_key(sync_key: Option<&str>) -> [u8; 32] {
    let raw = match sync_key.map(str::trim) {
        Some(k) if !k.is_empty() => k.as_bytes(),
        _ => DEFAULT_SYNC_PASSPHRASE,
    };
    let digest = Sha256::digest(raw);
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

pub fn encrypt_envelope(plaintext: &[u8], sync_key: Option<&str>) -> Result<Vec<u8>, VitralError> {
    let key_bytes = derive_key(sync_key);
    let cipher = XChaCha20Poly1305::new(&Key::from(key_bytes));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| VitralError::Internal(format!("CSPRNG nonce failed: {e}")))?;
    let ciphertext = cipher
        .encrypt(&XNonce::from(nonce_bytes), plaintext)
        .map_err(|e| VitralError::Integrity(format!("XChaCha20Poly1305 encrypt failed: {e}")))?;

    let mut out = Vec::with_capacity(ENVELOPE_MAGIC.len() + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(ENVELOPE_MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn decrypt_envelope(data: &[u8], sync_key: Option<&str>) -> Result<Vec<u8>, VitralError> {
    if data.len() < ENVELOPE_MAGIC.len() + NONCE_LEN + 16 || &data[..5] != ENVELOPE_MAGIC {
        return Err(VitralError::Integrity(
            "invalid or corrupt encrypted envelope header".into(),
        ));
    }
    let key_bytes = derive_key(sync_key);
    let cipher = XChaCha20Poly1305::new(&Key::from(key_bytes));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&data[5..5 + NONCE_LEN]);
    let ct = &data[5 + NONCE_LEN..];
    cipher
        .decrypt(&XNonce::from(nonce_bytes), ct)
        .map_err(|_| VitralError::Integrity("XChaCha20Poly1305 authentication failed".into()))
}

pub fn zstd_compress(data: &[u8]) -> Result<Vec<u8>, VitralError> {
    zstd::bulk::compress(data, 3)
        .map_err(|e| VitralError::Internal(format!("zstd compress failed: {e}")))
}

pub fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>, VitralError> {
    zstd::stream::decode_all(Cursor::new(data))
        .map_err(|e| VitralError::Integrity(format!("zstd decompress failed: {e}")))
}

pub fn encode_clipboard_ring_jsonl(entries: &[ClipboardEntry]) -> Result<Vec<u8>, VitralError> {
    let mut buf = Vec::new();
    for entry in entries {
        let line = serde_json::to_vec(entry)
            .map_err(|e| VitralError::Internal(format!("clipboard JSONL encode failed: {e}")))?;
        buf.extend_from_slice(&line);
        buf.push(b'\n');
    }
    Ok(buf)
}

pub fn decode_clipboard_ring_jsonl(bytes: &[u8]) -> Result<Vec<ClipboardEntry>, VitralError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| VitralError::Integrity(format!("clipboard JSONL invalid UTF-8: {e}")))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: ClipboardEntry = serde_json::from_str(trimmed)
            .map_err(|e| VitralError::Integrity(format!("clipboard JSONL parse failed: {e}")))?;
        out.push(entry);
    }
    Ok(out)
}

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(input: &[u8]) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_ALPHABET[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>, VitralError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(VitralError::Integrity(
            "invalid base64 length".to_string(),
        ));
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for (idx, chunk) in bytes.chunks_exact(4).enumerate() {
        let is_last = idx + 1 == bytes.len() / 4;
        let mut vals = [0u8; 4];
        let mut pad = 0usize;
        for (j, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                if !is_last || j < 2 || (j == 2 && chunk[3] != b'=') {
                    return Err(VitralError::Integrity(
                        "invalid base64 padding".to_string(),
                    ));
                }
                pad += 1;
                vals[j] = 0;
            } else {
                vals[j] = match b {
                    b'A'..=b'Z' => b - b'A',
                    b'a'..=b'z' => b - b'a' + 26,
                    b'0'..=b'9' => b - b'0' + 52,
                    b'+' => 62,
                    b'/' => 63,
                    _ => {
                        return Err(VitralError::Integrity(format!(
                            "invalid base64 byte 0x{b:02x}"
                        )))
                    }
                };
            }
        }
        let n = ((vals[0] as u32) << 18)
            | ((vals[1] as u32) << 12)
            | ((vals[2] as u32) << 6)
            | (vals[3] as u32);
        out.push(((n >> 16) & 0xFF) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if pad == 0 {
            out.push((n & 0xFF) as u8);
        }
    }
    Ok(out)
}
